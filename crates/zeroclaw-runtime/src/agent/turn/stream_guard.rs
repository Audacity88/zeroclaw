//! Streaming-text guards: protocol-fragment buffering and `<think>` tag stripping.

use super::protocol_detect::{
    complete_json_fence_protocol_state, complete_non_protocol_json,
    find_embedded_protocol_candidate_start, find_incomplete_protocol_candidate_start,
    json_fence_has_trailing_text, longest_suffix_matching_prefix,
    starts_suspicious_protocol_prefix, starts_suspicious_tag_or_fence_prefix,
};
use std::collections::HashSet;
use zeroclaw_tool_call_parser::{
    TERMINAL_MARKERS, ToolProtocolEnvelopeKind, classify_tool_protocol_envelope,
    contains_tool_protocol_tag_call, looks_like_malformed_tool_protocol_envelope_for_known_tools,
    looks_like_tool_protocol_envelope, looks_like_tool_protocol_example,
    strip_trailing_terminal_markers, tool_protocol_envelope_mentions_known_tool,
};

/// Which guard detector suppressed a candidate and where the candidate
/// began, so a suppression can be diagnosed from the trace log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProtocolSuppressionDiagnostic {
    /// One of `tool_result`, `function_call`, `tagged`, `malformed`,
    /// `active_tool_json`.
    pub(crate) detector: &'static str,
    /// Byte offset of the candidate into the released-then-pending text.
    pub(crate) candidate_offset: usize,
}

#[derive(Debug, Default)]
pub(crate) struct StreamTextGuard {
    // Chunks can split `"toolcalls"` / `<tool_call>` and other protocol
    // shapes across deltas, so a chunk that may contain a candidate is
    // buffered whole (prose ahead of the candidate stays releasable) and
    // candidate text keeps accumulating once one is seeded. A quoted span
    // can hold part of this buffer until its closer arrives or the stream
    // finishes.
    pending: String,
    pending_candidate_start: Option<usize>,
    known_tool_names: HashSet<String>,
    has_active_tools: bool,
    // Text already delivered to the caller before the current candidate was
    // established. Prose released ahead of a candidate makes the candidate
    // embedded: the model is quoting protocol, not emitting it.
    released_bytes: usize,
    released_prose: bool,
    pub(crate) suppress_forwarding: bool,
    pub(crate) suppressed_protocol: bool,
    pub(crate) suppression: Option<ProtocolSuppressionDiagnostic>,
}

impl StreamTextGuard {
    pub(crate) fn new(available_tools: Option<&[crate::tools::ToolSpec]>) -> Self {
        let available_tools = available_tools.unwrap_or(&[]);
        let known_tool_names = available_tools
            .iter()
            .map(|tool| tool.name.to_ascii_lowercase())
            .collect();
        Self {
            known_tool_names,
            has_active_tools: !available_tools.is_empty(),
            ..Self::default()
        }
    }

    pub(crate) fn push(&mut self, chunk: &str) -> Option<String> {
        if self.suppress_forwarding || chunk.is_empty() {
            return None;
        }

        if self.pending.is_empty() && !starts_suspicious_protocol_prefix(chunk) {
            // Buffer the whole chunk with the candidate offset intact: the
            // prose ahead of the candidate is not protocol and stays
            // releasable if the candidate itself is suppressed or resolved.
            if let Some(start) = find_embedded_protocol_candidate_start(chunk) {
                self.pending_candidate_start = Some(start);
                self.pending.push_str(chunk);
                return self.evaluate_pending(false);
            }
            if let Some(start) = find_incomplete_protocol_candidate_start(chunk) {
                self.pending_candidate_start = Some(start);
                self.pending.push_str(chunk);
                return None;
            }
            self.note_released(chunk);
            return Some(chunk.to_string());
        }

        self.pending.push_str(chunk);
        self.evaluate_pending(false)
    }

    pub(crate) fn finish(&mut self) -> Option<String> {
        if self.suppress_forwarding || self.pending.is_empty() {
            return None;
        }
        if let Some(release) = self.evaluate_pending(true) {
            return Some(release);
        }
        if self.suppressed_protocol || self.pending.is_empty() {
            return None;
        }
        // A malformed verdict on the whole buffer can only apply to a
        // leading candidate: prose around the buffer head (or a fenced
        // block with text after it) makes this quoted material, which the
        // embedded gate inside `candidate_is_embedded` recognizes.
        if !self.candidate_is_embedded(&self.pending)
            && looks_like_malformed_tool_protocol_envelope_for_known_tools(
                &self.pending,
                &self.known_tool_names,
            )
        {
            return self.suppress_protocol("malformed");
        }
        let release = std::mem::take(&mut self.pending);
        self.note_released(&release);
        Some(release)
    }

    fn evaluate_pending(&mut self, finalizing: bool) -> Option<String> {
        let candidate_start = self.pending_candidate_start.unwrap_or(0);
        let candidate = self.pending.get(candidate_start..).unwrap_or(&self.pending);

        if !finalizing && starts_suspicious_tag_or_fence_prefix(candidate) {
            return None;
        }

        if let Some(detector) = self.protocol_suppression_detector(candidate) {
            return self.suppress_protocol(detector);
        }

        if let Some(is_protocol) =
            complete_json_fence_protocol_state(candidate, &self.known_tool_names)
        {
            // A fence carrying a known-tool envelope is an internal protocol
            // leak only when the fence is the whole message; prose around it
            // makes it quoted material.
            if is_protocol && self.has_active_tools && !self.candidate_is_embedded(candidate) {
                return self.suppress_protocol("function_call");
            }
            self.pending_candidate_start = None;
            let release = std::mem::take(&mut self.pending);
            self.note_released(&release);
            return Some(release);
        }

        if complete_non_protocol_json(candidate, &self.known_tool_names) {
            self.pending_candidate_start = None;
            let release = std::mem::take(&mut self.pending);
            self.note_released(&release);
            return Some(release);
        }

        None
    }

    /// Text delivered to the caller before a later candidate appears: it is
    /// part of the message, not protocol, and it positions any later
    /// candidate past the start of the message.
    fn note_released(&mut self, text: &str) {
        self.released_bytes += text.len();
        self.released_prose |= !text.trim().is_empty();
    }

    /// A candidate is embedded (quoted material, not a leaked envelope) when
    /// prose was already released or is buffered ahead of it, or when it sits
    /// in a fenced block that carries text beyond its closing fence.
    fn candidate_is_embedded(&self, candidate: &str) -> bool {
        let candidate_start = self.pending_candidate_start.unwrap_or(0);
        if self.released_prose {
            return true;
        }
        if self
            .pending
            .get(..candidate_start)
            .is_some_and(|prefix| !prefix.trim().is_empty())
        {
            return true;
        }
        json_fence_has_trailing_text(candidate)
    }

    fn suppress_protocol(&mut self, detector: &'static str) -> Option<String> {
        let candidate_start = self.pending_candidate_start.unwrap_or(0);
        self.suppression = Some(ProtocolSuppressionDiagnostic {
            detector,
            candidate_offset: self.released_bytes + candidate_start,
        });
        // The text buffered ahead of the candidate is ordinary prose, not
        // protocol: deliver it and withhold from the candidate onward. Only
        // a candidate that starts at offset 0 of the pending buffer (or the
        // split-prefix buffer, which has no pre-candidate text) clears the
        // whole buffer.
        let release = (candidate_start > 0).then(|| self.pending[..candidate_start].to_string());
        self.pending.clear();
        self.pending_candidate_start = None;
        self.suppress_forwarding = true;
        self.suppressed_protocol = true;
        release
    }

    fn looks_like_active_tool_json(&self, text: &str) -> bool {
        if self.known_tool_names.is_empty() {
            return false;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
            return false;
        };

        match value {
            serde_json::Value::Array(items) => {
                !items.is_empty() && items.iter().all(|item| self.is_known_tool_payload(item))
            }
            serde_json::Value::Object(_) => self.is_known_tool_payload(&value),
            _ => false,
        }
    }

    fn is_known_tool_payload(&self, value: &serde_json::Value) -> bool {
        let Some(object) = value.as_object() else {
            return false;
        };

        let (name, has_args) =
            if let Some(function) = object.get("function").and_then(|value| value.as_object()) {
                (
                    function
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| object.get("name").and_then(serde_json::Value::as_str)),
                    function.contains_key("arguments")
                        || function.contains_key("parameters")
                        || object.contains_key("arguments")
                        || object.contains_key("parameters"),
                )
            } else {
                (
                    object.get("name").and_then(serde_json::Value::as_str),
                    object.contains_key("arguments") || object.contains_key("parameters"),
                )
            };

        let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
            return false;
        };

        has_args && self.known_tool_names.contains(&name.to_ascii_lowercase())
    }

    /// Which detector (if any) marks `text` as an internal tool-protocol
    /// envelope that must be withheld from channel output.
    ///
    /// Tagged tool-call markup is a machine directive, never legitimate
    /// prose: it is withheld wherever it appears. Every other detector is a
    /// JSON-envelope verdict, and those only apply to a candidate that leads
    /// the message: a candidate embedded in surrounding prose (text released
    /// or buffered ahead of it, or a fenced block that is not the whole
    /// message) is the model quoting the protocol, not leaking it.
    fn protocol_suppression_detector(&self, text: &str) -> Option<&'static str> {
        if looks_like_tool_protocol_example(text) {
            return None;
        }

        if contains_tool_protocol_tag_call(text) {
            return Some("tagged");
        }

        if let Some(kind) = classify_tool_protocol_envelope(text)
            && matches!(kind, ToolProtocolEnvelopeKind::TaggedToolCall)
        {
            return Some("tagged");
        }

        if self.candidate_is_embedded(text) {
            return None;
        }

        if looks_like_malformed_tool_protocol_envelope_for_known_tools(text, &self.known_tool_names)
        {
            return Some("malformed");
        }

        if let Some(kind) = classify_tool_protocol_envelope(text)
            && self.has_active_tools
        {
            if matches!(kind, ToolProtocolEnvelopeKind::ToolResult) {
                return Some("tool_result");
            }
            if tool_protocol_envelope_mentions_known_tool(text, &self.known_tool_names) {
                return Some("function_call");
            }
        }

        // Parsed JSON that carries protocol-only fields but cannot yield a valid
        // tool call is an internal protocol failure, not user-facing text.
        if looks_like_tool_protocol_envelope(text) {
            return Some("malformed");
        }

        self.looks_like_active_tool_json(text)
            .then_some("active_tool_json")
    }
}

#[cfg(test)]
mod stream_text_guard_tests {
    use super::{ProtocolSuppressionDiagnostic, StreamTextGuard};

    fn guard_with_tool() -> StreamTextGuard {
        StreamTextGuard::new(Some(&[crate::tools::ToolSpec::new(
            "shell",
            "run a command",
            serde_json::json!({"type": "object"}),
        )]))
    }

    fn push_all(guard: &mut StreamTextGuard, chunks: &[&str]) -> String {
        let mut forwarded = String::new();
        for chunk in chunks {
            if let Some(text) = guard.push(chunk) {
                forwarded.push_str(&text);
            }
        }
        if let Some(tail) = guard.finish() {
            forwarded.push_str(&tail);
        }
        forwarded
    }

    /// Regression for the issue: prose, then a code span quoting an object
    /// with the tool_call_id and content keys, then more prose, streamed so
    /// the split lands inside the object. The quoted snippet is prose, not
    /// a leaked envelope: every byte is forwarded and nothing is flagged.
    #[test]
    fn prose_code_span_object_split_inside_is_forwarded() {
        let mut guard = guard_with_tool();
        let message = "The history message looks like `{\"tool_call_id\": \"call_1\", \"content\": \"ok\"}` and that is the whole shape.";
        let forwarded = push_all(
            &mut guard,
            &[
                "The history message looks like `{",
                "\"tool_call_id\": \"call_1\",",
                " \"content\": \"ok\"}` and that is the whole shape.",
            ],
        );
        assert_eq!(forwarded, message);
        assert!(
            !guard.suppressed_protocol,
            "an embedded quoted object must not be suppressed"
        );
        assert!(guard.suppression.is_none());
    }

    /// The same object as the entire message is a genuine whole-message
    /// envelope: suppressed, with the detector and candidate offset recorded.
    #[test]
    fn whole_message_tool_result_object_is_suppressed() {
        let mut guard = guard_with_tool();
        let forwarded = push_all(
            &mut guard,
            &["{\"tool_call_id\": \"call_1\",", " \"content\": \"ok\"}"],
        );
        assert_eq!(forwarded, "");
        assert!(guard.suppressed_protocol);
        assert_eq!(
            guard.suppression,
            Some(ProtocolSuppressionDiagnostic {
                detector: "tool_result",
                candidate_offset: 0,
            })
        );
    }

    /// A fenced block containing the object, surrounded by prose that never
    /// uses an example word: the fence is not the whole message, so the
    /// quoted envelope is forwarded like any other prose.
    #[test]
    fn fenced_object_with_surrounding_prose_is_forwarded() {
        let mut guard = guard_with_tool();
        let message = "The wire shape is:\n```json\n{\"tool_call_id\": \"call_1\", \"content\": \"ok\"}\n```\nand nothing else carries protocol.";
        let forwarded = push_all(
            &mut guard,
            &[
                "The wire shape is:\n```json\n{\"tool_",
                "call_id\": \"call_1\", \"content\": \"ok\"}\n```\nand nothing else carries protocol.",
            ],
        );
        assert_eq!(forwarded, message);
        assert!(!guard.suppressed_protocol);
        assert!(guard.suppression.is_none());
    }

    /// A message that is nothing but a json fence whose body is the object
    /// stays suppressed: that is a whole-message envelope, not quoted prose.
    #[test]
    fn fence_only_message_with_object_body_is_suppressed() {
        let mut guard = guard_with_tool();
        let message = "```json\n{\"tool_call_id\": \"call_1\", \"content\": \"ok\"}\n```";
        let forwarded = push_all(&mut guard, &[message]);
        assert_eq!(forwarded, "");
        assert!(guard.suppressed_protocol);
        assert_eq!(
            guard.suppression,
            Some(ProtocolSuppressionDiagnostic {
                detector: "tool_result",
                candidate_offset: 0,
            })
        );
    }

    /// Tagged tool-call markup is never legitimate prose: it suppresses
    /// wherever it appears, but the prose buffered ahead of the candidate is
    /// ordinary text and must still be delivered.
    #[test]
    fn tagged_call_suppression_releases_buffered_prose_prefix() {
        let mut guard = guard_with_tool();
        let prefix = "Here is the call: ";
        let call =
            "<tool_call>{\"name\": \"shell\", \"arguments\": {\"command\": \"ls\"}}</tool_call>";
        let tagged_text = format!("{prefix}{call}");
        let forwarded = push_all(&mut guard, &[tagged_text.as_str()]);
        assert_eq!(
            forwarded, prefix,
            "prose buffered ahead of a suppressed candidate must be released"
        );
        assert!(guard.suppressed_protocol);
        assert_eq!(
            guard.suppression,
            Some(ProtocolSuppressionDiagnostic {
                detector: "tagged",
                candidate_offset: prefix.len(),
            })
        );
    }
}

#[derive(Debug, Default)]
pub(crate) struct StreamThinkTagStripper {
    pending: String,
    in_think: bool,
}

impl StreamThinkTagStripper {
    const START_TAG: &'static str = "<think>";
    const END_TAG: &'static str = "</think>";

    pub(crate) fn push(&mut self, chunk: &str) -> String {
        if chunk.is_empty() {
            return String::new();
        }

        let mut input = std::mem::take(&mut self.pending);
        input.push_str(chunk);
        let mut visible = String::new();

        loop {
            if self.in_think {
                if let Some(end) = input.find(Self::END_TAG) {
                    input = input[end + Self::END_TAG.len()..].to_string();
                    self.in_think = false;
                    continue;
                }

                let keep_len = longest_suffix_matching_prefix(&input, Self::END_TAG);
                if keep_len > 0 {
                    self.pending = input[input.len() - keep_len..].to_string();
                }
                return visible;
            }

            if let Some(start) = input.find(Self::START_TAG) {
                visible.push_str(&input[..start]);
                input = input[start + Self::START_TAG.len()..].to_string();
                self.in_think = true;
                continue;
            }

            let keep_len = longest_suffix_matching_prefix(&input, Self::START_TAG);
            if keep_len > 0 {
                let emit_len = input.len() - keep_len;
                visible.push_str(&input[..emit_len]);
                self.pending = input[emit_len..].to_string();
            } else {
                visible.push_str(&input);
            }
            return visible;
        }
    }

    pub(crate) fn finish(&mut self) -> String {
        if self.in_think {
            self.pending.clear();
            return String::new();
        }
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod terminal_marker_stripper_tests {
    use super::{StreamTerminalMarkerStripper, StreamTextGuard};
    use std::collections::HashSet;
    use zeroclaw_tool_call_parser::{TERMINAL_MARKERS, strip_trailing_terminal_markers};

    #[test]
    fn strips_single_marker_at_end() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        // The safe prefix streams live; only the marker is held and discarded
        // on finish.
        assert_eq!(stripper.push("Summary<eom>"), "Summary");
        assert_eq!(stripper.finish(), "");
    }

    #[test]
    fn strips_pipe_eom_marker_at_end() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        assert_eq!(stripper.push("Summary<|eom|>"), "Summary");
        assert_eq!(stripper.finish(), "");
    }

    #[test]
    fn strips_stacked_markers_at_end() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        assert_eq!(stripper.push("Summary<eom><|eom|>"), "Summary");
        assert_eq!(stripper.finish(), "");
    }

    #[test]
    fn preserves_inline_marker_with_text_after() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        assert_eq!(
            stripper.push("Text <eom> more text"),
            "Text <eom> more text"
        );
        assert_eq!(stripper.finish(), "");
    }

    #[test]
    fn handles_marker_split_across_chunks() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        assert_eq!(stripper.push("Summary<"), "Summary");
        assert_eq!(stripper.push("eom>"), "");
        assert_eq!(stripper.finish(), "");
    }

    #[test]
    fn handles_pipe_marker_split_across_chunks() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        assert_eq!(stripper.push("Summary<|"), "Summary");
        assert_eq!(stripper.push("eom|>"), "");
        assert_eq!(stripper.finish(), "");
    }

    #[test]
    fn handles_stacked_markers_split_across_chunks() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        assert_eq!(stripper.push("Summary<eom>"), "Summary");
        assert_eq!(stripper.push("<|"), "");
        assert_eq!(stripper.push("eom|>"), "");
        assert_eq!(stripper.finish(), "");
    }

    #[test]
    fn preserves_inline_marker_then_strips_terminal() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        assert_eq!(stripper.push("Text <eom> inline<eom>"), "Text <eom> inline");
        assert_eq!(stripper.finish(), "");
    }

    #[test]
    fn handles_whitespace_after_marker() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        assert_eq!(stripper.push("Summary<eom>\n"), "Summary");
        assert_eq!(stripper.finish(), "");
    }

    #[test]
    fn handles_long_whitespace_after_stacked_markers() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        assert_eq!(stripper.push("Summary<eom>           <|eom|>"), "Summary");
        assert_eq!(stripper.finish(), "");
    }

    #[test]
    fn empty_chunk_returns_empty() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        assert_eq!(stripper.push(""), "");
        assert_eq!(stripper.finish(), "");
    }

    #[test]
    fn no_marker_passes_through() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        assert_eq!(stripper.push("Normal text"), "Normal text");
        assert_eq!(stripper.finish(), "");
    }

    /// Regression for the live-streaming timing bug: a provider that sends the
    /// whole answer plus a terminal marker in ONE delta must still forward the
    /// answer immediately. The old implementation held the entire chunk in
    /// `pending` and only released it on `finish()`, so nothing streamed until
    /// the provider's `Final` event.
    #[test]
    fn push_releases_safe_prefix_and_holds_marker_suffix() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        assert_eq!(
            stripper.push("A large answer<eom>"),
            "A large answer",
            "the safe prefix must stream immediately; only the marker is held"
        );
        assert_eq!(
            stripper.finish(),
            "",
            "the held marker is terminal and must be discarded on finish"
        );
    }

    /// Regression for the incomplete-marker-prefix data loss: `push` holds a
    /// possible split marker (`<eom`) plus the ordinary trailing space that
    /// follows it, and `finish` must preserve that whitespace verbatim because
    /// no complete terminal marker was produced. The non-streaming helper keeps
    /// the same input, so the two paths must agree.
    #[test]
    fn finish_preserves_whitespace_after_incomplete_marker_prefix() {
        let mut stripper = StreamTerminalMarkerStripper::new();
        // `<eom` is held as a possible split marker; the trailing space is
        // ordinary text, not part of a completed marker suffix.
        assert_eq!(stripper.push("Answer<eom "), "Answer");
        assert_eq!(
            stripper.finish(),
            "<eom ",
            "no complete marker was stripped, so the trailing space is kept"
        );
        assert_eq!(
            strip_trailing_terminal_markers("Answer<eom "),
            "Answer<eom ",
            "the non-streaming helper must preserve the same input"
        );
    }

    /// The streaming stripper and the non-streaming
    /// `strip_trailing_terminal_markers` helper must agree on the same inputs.
    /// This guards the shared-marker-vocabulary invariant: a change to
    /// [`TERMINAL_MARKERS`] or to one path that is not mirrored in the other
    /// fails here.
    #[test]
    fn streaming_matches_non_streaming_on_complete_input() {
        let cases = [
            ("Summary<eom>", "Summary"),
            ("Summary<|eom|>", "Summary"),
            ("Summary<eom><|eom|>", "Summary"),
            ("Summary<eom>  \n", "Summary"),
            ("Summary<eom>           <|eom|>", "Summary"),
            ("Text with <eom> inline", "Text with <eom> inline"),
            ("<eom>", ""),
            ("<eom>\n<|eom|>", ""),
            ("Answer<eom ", "Answer<eom "),
            ("", ""),
        ];
        for (input, expected) in cases {
            let non_streaming = strip_trailing_terminal_markers(input);
            assert_eq!(
                non_streaming, expected,
                "non-streaming helper diverged for {input:?}"
            );
            let mut stripper = StreamTerminalMarkerStripper::new();
            let live = stripper.push(input);
            let flushed = stripper.finish();
            let streamed = format!("{live}{flushed}");
            assert_eq!(
                streamed, expected,
                "streaming stripper diverged from the non-streaming helper for {input:?}"
            );
        }

        // Guard parity on the same contract for the protocol guard: prose
        // quoting a tool-result-shaped object in a code span. The
        // non-streaming parse-issue detector raises nothing on this text,
        // so the streaming guard must deliver it byte-for-byte too.
        let known_tool_names = HashSet::from(["shell".to_string()]);
        let guard_tools = [crate::tools::ToolSpec::new(
            "shell",
            "run a command",
            serde_json::json!({"type": "object"}),
        )];
        let quoted = "The history message looks like `{\"tool_call_id\": \"call_1\", \"content\": \"ok\"}` and that is the whole shape.";
        assert!(
            super::super::protocol_detect::detect_tool_call_parse_issue_for_known_tools(
                quoted,
                &[],
                &known_tool_names
            )
            .is_none(),
            "the non-streaming detector must not flag quoted protocol prose"
        );
        let mut guard = StreamTextGuard::new(Some(&guard_tools));
        let live = guard.push(quoted);
        let flushed = guard.finish();
        let streamed = format!(
            "{}{}",
            live.unwrap_or_default(),
            flushed.unwrap_or_default()
        );
        assert_eq!(
            streamed, quoted,
            "the streaming guard must forward quoted protocol prose byte-for-byte"
        );
        assert!(!guard.suppressed_protocol);
    }

    /// The canonical marker vocabulary must stay aligned between the streaming
    /// state machine and the non-streaming helper. If the table is duplicated
    /// again or a marker is added to only one path, this pin fails.
    #[test]
    fn marker_vocabulary_is_shared_with_non_streaming_path() {
        assert_eq!(
            TERMINAL_MARKERS,
            ["<|eom|>", "<eom>"],
            "the canonical marker table must match the documented spellings"
        );
        for marker in TERMINAL_MARKERS {
            assert_eq!(
                strip_trailing_terminal_markers(&format!("Summary{marker}")),
                "Summary",
                "non-streaming helper must strip the shared marker {marker:?}"
            );
            let mut stripper = StreamTerminalMarkerStripper::new();
            assert_eq!(
                stripper.push(&format!("Summary{marker}")),
                "Summary",
                "streaming stripper must recognize the shared marker {marker:?}"
            );
            assert_eq!(stripper.finish(), "");
        }
    }
}

/// Streaming-safe terminal marker stripper.
///
/// Strips trailing terminal markers ([`TERMINAL_MARKERS`]) from streaming text
/// chunks. Handles markers split across multiple chunks, stacked markers, and
/// markers with arbitrary whitespace between them.
///
/// # State machine
///
/// The stripper maintains a `pending` buffer that accumulates text. When a
/// complete marker is found at the end, only the possible marker/whitespace
/// suffix is held in `pending`; the safe prefix is emitted immediately so a
/// single delta that ends in a terminal marker still streams live instead of
/// buffering the whole chunk until `finish()`. If the next chunk is non-empty
/// and turns the held suffix into inline text, the suffix is released as inline
/// text. If `finish()` is called, the marker is discarded as terminal.
#[derive(Debug, Default)]
pub(crate) struct StreamTerminalMarkerStripper {
    pending: String,
}

/// Length of the longest [`TERMINAL_MARKERS`] prefix that `text` ends with, if
/// any. Used to hold a marker that is split across chunk boundaries (e.g. a
/// chunk ending in `<` or `<|`) until the rest of the marker arrives.
fn longest_terminal_marker_prefix(text: &str) -> Option<usize> {
    TERMINAL_MARKERS
        .iter()
        .flat_map(|marker| (1..marker.len()).map(move |len| &marker[..len]))
        .filter(|prefix| text.ends_with(prefix))
        .map(str::len)
        .max()
}

impl StreamTerminalMarkerStripper {
    pub(crate) fn new() -> Self {
        Self {
            pending: String::new(),
        }
    }

    /// Push a chunk of text and return the visible text with terminal markers stripped.
    ///
    /// The safe prefix is emitted immediately and only the possible
    /// marker/whitespace suffix (including a marker split across chunk
    /// boundaries) is held, so a provider that sends a full answer plus a
    /// terminal marker in one delta still streams the answer live instead of
    /// buffering the whole chunk until [`Self::finish`]. A complete marker
    /// followed by a partial marker is held as one unit: it may resolve into
    /// stacked terminal markers.
    pub(crate) fn push(&mut self, chunk: &str) -> String {
        if chunk.is_empty() {
            return String::new();
        }

        // Append the new chunk
        self.pending.push_str(chunk);

        // From the end, strip the trailing run of whitespace / complete
        // markers / partial marker prefixes. What remains is the safe prefix.
        let mut hold_start = self.pending.len();
        loop {
            let before = hold_start;
            let ws_trimmed = self.pending[..hold_start].trim_end().len();

            let mut stripped = false;
            for marker in TERMINAL_MARKERS {
                if self.pending[..ws_trimmed].ends_with(marker) {
                    hold_start = ws_trimmed - marker.len();
                    stripped = true;
                    break;
                }
            }
            if !stripped
                && let Some(prefix_len) =
                    longest_terminal_marker_prefix(&self.pending[..ws_trimmed])
            {
                hold_start = ws_trimmed - prefix_len;
                stripped = true;
            }
            if !stripped {
                // No marker or partial prefix in the tail: the trailing
                // whitespace (if any) belongs to normal text — emit it.
                hold_start = before;
                break;
            }
            if hold_start == 0 {
                // Everything is a possible terminal marker — hold it all until
                // the next chunk decides inline vs terminal.
                return String::new();
            }
        }

        if hold_start == self.pending.len() {
            // No marker at all — release everything.
            let result = std::mem::take(&mut self.pending);
            return result;
        }

        let result = self.pending[..hold_start].to_string();
        self.pending.drain(..hold_start);
        result
    }

    /// Finish the stream and return any remaining text.
    /// Discards any trailing terminal markers.
    pub(crate) fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }

        // Delegate to the shared non-streaming helper so both paths apply the
        // same policy: only whitespace that follows a *complete* recognized
        // marker is trimmed. An incomplete marker prefix plus ordinary trailing
        // whitespace (e.g. `<eom␠` with no closing `>`) is preserved verbatim —
        // it is user-visible prose, not a terminal marker suffix.
        strip_trailing_terminal_markers(&self.pending)
    }
}
