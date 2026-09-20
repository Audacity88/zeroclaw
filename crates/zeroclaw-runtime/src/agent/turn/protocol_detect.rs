//! Heuristics for detecting tool-protocol fragments in streamed model text.

use std::collections::HashSet;
use zeroclaw_tool_call_parser::{
    ParsedToolCall, ToolProtocolEnvelopeKind, classify_tool_protocol_envelope,
    contains_tool_protocol_tag_call, looks_like_malformed_tool_protocol_envelope,
    looks_like_malformed_tool_protocol_envelope_for_known_tools, looks_like_tool_protocol_envelope,
    looks_like_tool_protocol_example, tool_protocol_envelope_mentions_known_tool,
};

pub(crate) fn longest_suffix_matching_prefix(text: &str, pattern: &str) -> usize {
    (1..pattern.len())
        .rev()
        .find(|&len| text.ends_with(&pattern[..len]))
        .unwrap_or(0)
}

/// Fence-opener patterns a candidate finder recognizes. A match anywhere
/// inside a longer backtick run still identifies that run's fence, so a
/// found index is walked back to the run's first backtick.
const FENCE_PATTERNS: [&str; 3] = ["```tool", "```invoke", "```json"];

/// Walk a fence-pattern match at `idx` back to the first backtick of the
/// run it sits in. A candidate seeded at a fence opener must begin at the
/// start of the opening run: a finder that matched only the last three
/// backticks of a four-backtick opener hands the guard a shortened fence
/// that a three-backtick line can then close.
fn fence_pattern_run_start(text: &str, idx: usize) -> usize {
    let mut start = idx;
    while start > 0 && text.as_bytes()[start - 1] == b'`' {
        start -= 1;
    }
    start
}

pub(crate) fn find_embedded_protocol_candidate_start(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let mut earliest: Option<usize> = None;

    for pattern in [
        "<tool_call",
        "<toolcall",
        "<tool-call",
        "<invoke",
        "<function",
    ] {
        if let Some(idx) = lower.find(pattern) {
            earliest = Some(earliest.map_or(idx, |current| current.min(idx)));
        }
    }

    for pattern in FENCE_PATTERNS {
        if let Some(idx) = lower.find(pattern) {
            let start = fence_pattern_run_start(text, idx);
            earliest = Some(earliest.map_or(start, |current| current.min(start)));
        }
    }

    for key in ["\"tool_calls\"", "\"toolcalls\"", "\"function_call\""] {
        if let Some(key_idx) = lower.find(key)
            && let Some(json_start) = text[..key_idx].rfind(['{', '['])
        {
            earliest = Some(earliest.map_or(json_start, |current| current.min(json_start)));
        }
    }

    earliest
}

pub(crate) fn find_incomplete_protocol_candidate_start(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let mut earliest: Option<usize> = None;

    for pattern in ["<tool", "<invoke", "<function"] {
        if let Some(idx) = lower.rfind(pattern) {
            earliest = Some(earliest.map_or(idx, |current| current.min(idx)));
        }
    }

    for pattern in FENCE_PATTERNS {
        if let Some(idx) = lower.rfind(pattern) {
            let start = fence_pattern_run_start(text, idx);
            earliest = Some(earliest.map_or(start, |current| current.min(start)));
        }
    }

    for delimiter in ['{', '['] {
        if let Some(idx) = text.rfind(delimiter) {
            let tail = &lower[idx..];
            if tail.contains("\"tool")
                || tail.contains("\"function")
                || tail.contains("\"call")
                || tail.len() <= 16
            {
                earliest = Some(earliest.map_or(idx, |current| current.min(idx)));
            }
        }
    }

    earliest
}

/// Whether lowercased, trimmed text begins with a fence opener: a run of
/// three or more backticks followed by the `json`, `tool`, or `invoke`
/// language label. An opening run of any length from three up is a fence,
/// so prefix routing must not recognize only the exact three-backtick
/// spelling and let a longer opener pass as ordinary text.
fn starts_with_suspicious_fence_opener(lower: &str) -> bool {
    let run = lower.chars().take_while(|&ch| ch == '`').count();
    if run < 3 {
        return false;
    }
    let after_run = &lower[run..];
    after_run.starts_with("json")
        || after_run.starts_with("tool")
        || after_run.starts_with("invoke")
}

pub(crate) fn starts_suspicious_protocol_prefix(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with('{')
        || lower.starts_with('[')
        || lower.starts_with("<tool")
        || lower.starts_with("<invoke")
        || lower.starts_with("<function")
        || starts_with_suspicious_fence_opener(&lower)
}

pub(crate) fn starts_suspicious_tag_or_fence_prefix(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    lower.starts_with("<tool")
        || lower.starts_with("<invoke")
        || lower.starts_with("<function")
        || starts_with_suspicious_fence_opener(&lower)
        || lower.starts_with("[tool_call]")
}

pub(crate) fn complete_non_protocol_json(text: &str, known_tool_names: &HashSet<String>) -> bool {
    let trimmed = text.trim();
    (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
        && (!looks_like_tool_protocol_envelope(trimmed)
            || !tool_protocol_envelope_mentions_known_tool(trimmed, known_tool_names))
}

pub(crate) fn complete_json_fence_protocol_state(
    text: &str,
    known_tool_names: &HashSet<String>,
) -> Option<bool> {
    let trimmed = text.trim();
    let body = json_fence_body(trimmed)?;
    Some(
        looks_like_tool_protocol_envelope(body)
            && tool_protocol_envelope_mentions_known_tool(body, known_tool_names),
    )
}

pub(crate) fn detect_internal_protocol_without_tools(response: &str) -> Option<String> {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return None;
    }
    if looks_like_tool_protocol_example(trimmed) {
        return None;
    }

    (looks_like_malformed_tool_protocol_envelope(trimmed)
        || contains_tool_protocol_tag_call(trimmed)
        || classify_tool_protocol_envelope(trimmed)
            .is_some_and(|kind| matches!(kind, ToolProtocolEnvelopeKind::TaggedToolCall))
        || (classify_tool_protocol_envelope(trimmed).is_none()
            && looks_like_tool_protocol_envelope(trimmed)))
    .then(|| {
        "response resembled an internal tool protocol envelope but no tools were enabled".into()
    })
}

pub(crate) fn detect_tool_call_parse_issue_for_known_tools(
    response: &str,
    parsed_calls: &[ParsedToolCall],
    known_tool_names: &HashSet<String>,
) -> Option<String> {
    if !parsed_calls.is_empty() {
        return None;
    }

    let trimmed = response.trim();
    if trimmed.is_empty() || looks_like_tool_protocol_example(trimmed) {
        return None;
    }

    let message = "response resembled an internal tool protocol envelope but no valid tool call could be parsed";

    if looks_like_malformed_tool_protocol_envelope_for_known_tools(trimmed, known_tool_names)
        || contains_tool_protocol_tag_call(trimmed)
    {
        return Some(message.into());
    }

    if let Some(kind) = classify_tool_protocol_envelope(trimmed) {
        return (matches!(
            kind,
            ToolProtocolEnvelopeKind::TaggedToolCall | ToolProtocolEnvelopeKind::ToolResult
        ) || tool_protocol_envelope_mentions_known_tool(trimmed, known_tool_names))
        .then(|| message.into());
    }

    looks_like_tool_protocol_envelope(trimmed).then(|| message.into())
}

/// The start of the body of the `json` fence that leads `text` (just past
/// its opening line) and the length of the opener's backtick run. `None`
/// when `text` does not begin with a fence opener of three or more
/// backticks labeled `json`.
fn json_fence_opener(text: &str) -> Option<(usize, usize)> {
    let leading_whitespace = text.len() - text.trim_start().len();
    let rest = &text[leading_whitespace..];
    let opener_run = rest.chars().take_while(|&ch| ch == '`').count();
    if opener_run < 3 {
        return None;
    }
    let after_run = &rest[opener_run..];
    let opener_line_end = after_run.find('\n')?;
    let language = after_run[..opener_line_end].trim().trim_end_matches('\r');
    if !language.eq_ignore_ascii_case("json") {
        return None;
    }
    Some((
        leading_whitespace + opener_run + opener_line_end + 1,
        opener_run,
    ))
}

/// Byte offset just past the real closing fence line of a `json` fence that
/// leads `text`. The close is a line whose entire content is a backtick run
/// at least as long as the opener's run, so a run of backticks inside a
/// JSON string is string content, never a close. `None` when the fence is
/// unterminated: no such line has arrived, and the stream may simply be cut.
pub(crate) fn json_fence_close_end(text: &str) -> Option<usize> {
    let (body_start, opener_run) = json_fence_opener(text)?;
    let mut line_start = body_start;
    for line in text[body_start..].split_inclusive('\n') {
        let content = line.trim();
        if !content.is_empty() && content.chars().all(|ch| ch == '`') && content.len() >= opener_run
        {
            let run_start = line_start + (line.len() - line.trim_start().len());
            return Some(run_start + content.len());
        }
        line_start += line.len();
    }
    None
}

/// Whether a JSON-fenced block in `text` carries non-fence text after its
/// real closing fence line. A candidate that begins with a `json` fence but
/// is followed by ordinary text is quoted material inside a larger message,
/// not a whole-message envelope. The close is a line of backticks at least
/// as long as the opener; an unterminated fence is not trailing text, since
/// the stream may simply be cut and the fence is all there is so far.
pub(crate) fn json_fence_has_trailing_text(text: &str) -> bool {
    json_fence_close_end(text).is_some_and(|close_end| !text[close_end..].trim().is_empty())
}

/// The body of a `json` fence that leads `text` and is closed by a real
/// closing fence line with nothing but whitespace after it. The close is
/// the line rule `json_fence_close_end` applies, a backtick run at least
/// as long as the opener alone on its line, so a longer opener cannot be
/// closed by a shorter run and a run inside a JSON string is string
/// content, never the close.
pub(crate) fn json_fence_body(trimmed: &str) -> Option<&str> {
    let (body_start, _) = json_fence_opener(trimmed)?;
    let close_end = json_fence_close_end(trimmed)?;
    if !trimmed[close_end..].trim().is_empty() {
        return None;
    }
    let close_line_start = trimmed[..close_end]
        .rfind('\n')
        .map_or(body_start, |newline| newline + 1);
    Some(trimmed[body_start..close_line_start].trim())
}

/// The body of the `json` fence that leads a candidate, for the guard's
/// detectors to judge: the text after the opening line through the real
/// closing fence line when it has arrived, or to the end of the candidate
/// while the fence is still open. Unlike [`json_fence_body`] this neither
/// requires a close nor rejects a tail: an unterminated fence still has a
/// body, and text past an arrived close belongs to the re-scanned
/// remainder, not to the body.
pub(crate) fn json_fence_candidate_body(candidate: &str) -> Option<&str> {
    let (body_start, _) = json_fence_opener(candidate)?;
    let body_end = match json_fence_close_end(candidate) {
        Some(close_end) => candidate[..close_end]
            .rfind('\n')
            .map_or(body_start, |newline| newline + 1),
        None => candidate.len(),
    };
    Some(candidate[body_start..body_end].trim())
}

#[cfg(test)]
mod tests {
    use super::{
        find_embedded_protocol_candidate_start, find_incomplete_protocol_candidate_start,
        json_fence_has_trailing_text,
    };

    #[test]
    fn fence_with_text_after_close_has_trailing_text() {
        assert!(json_fence_has_trailing_text(
            "```json\n{\"a\": 1}\n```\nand then prose"
        ));
    }

    #[test]
    fn fence_that_is_the_whole_text_has_no_trailing_text() {
        assert!(!json_fence_has_trailing_text("```json\n{\"a\": 1}\n```"));
        assert!(!json_fence_has_trailing_text(
            "```json\n{\"a\": 1}\n```  \n"
        ));
    }

    #[test]
    fn unterminated_fence_has_no_trailing_text() {
        // The body so far is all there is; the stream may simply be cut.
        assert!(!json_fence_has_trailing_text("```json\n{\"a\":"));
        // A backtick run inside a JSON string is string content, not a close.
        assert!(!json_fence_has_trailing_text(
            "```json\n{\"name\": \"shell\", \"arguments\": {\"command\": \"echo ```done\"}}"
        ));
        // Backticks sharing their line with other text do not close either.
        assert!(!json_fence_has_trailing_text(
            "```json\n{\"a\": 1}\n``` and then prose"
        ));
    }

    #[test]
    fn inner_backtick_run_with_real_close_and_text_after_has_trailing_text() {
        // The string's inner run is skipped; the real closing fence line is
        // the close, and the text after it is trailing.
        assert!(json_fence_has_trailing_text(
            "```json\n{\"command\": \"echo ```done\"}\n```\nand then prose"
        ));
    }

    #[test]
    fn four_backtick_opener_ignores_shorter_close_run() {
        // A three-backtick line is too short to close a four-backtick fence.
        assert!(!json_fence_has_trailing_text(
            "````json\n{\"a\": 1}\n```\nstill inside the fence\ntrailing"
        ));
    }

    #[test]
    fn four_backtick_opener_closes_with_matching_run() {
        assert!(json_fence_has_trailing_text(
            "````json\n{\"a\": 1}\n````\nand then prose"
        ));
        assert!(!json_fence_has_trailing_text("````json\n{\"a\": 1}\n````"));
    }

    #[test]
    fn non_fence_text_has_no_trailing_text() {
        assert!(!json_fence_has_trailing_text("plain prose"));
        assert!(!json_fence_has_trailing_text("{\"a\": 1}"));
        assert!(!json_fence_has_trailing_text(
            "```tool_call\nx\n``` trailing"
        ));
    }

    #[test]
    fn incomplete_finder_selects_object_after_prose() {
        let text = "see the shape {\"tool_call_id\": \"x\"} here";
        assert_eq!(find_incomplete_protocol_candidate_start(text), Some(14));
    }

    #[test]
    fn embedded_finder_selects_container_object_start() {
        let text = "nope {\"tool_calls\": []}";
        assert_eq!(find_embedded_protocol_candidate_start(text), Some(5));
    }

    #[test]
    fn finders_seed_four_backtick_opener_at_its_run_start() {
        // The pattern match lands on the last three backticks of the
        // four-backtick run; the candidate must begin at the first one so
        // a three-backtick line can never close this fence.
        assert_eq!(
            find_embedded_protocol_candidate_start("````json\n{\"a\": 1}\n```\nstill inside"),
            Some(0)
        );
        assert_eq!(
            find_embedded_protocol_candidate_start("prose\n````json\n{\"a\": 1}"),
            Some(6)
        );
        assert_eq!(
            find_incomplete_protocol_candidate_start("````json\n{\"a\""),
            Some(0)
        );
        assert_eq!(
            find_incomplete_protocol_candidate_start("prose ````json\n{\"a\""),
            Some(6)
        );
    }

    #[test]
    fn finders_keep_exact_three_backtick_opener_start() {
        assert_eq!(
            find_embedded_protocol_candidate_start("```json\n{\"a\": 1}"),
            Some(0)
        );
        assert_eq!(
            find_embedded_protocol_candidate_start("prose\n```json\n{\"a\": 1}"),
            Some(6)
        );
        assert_eq!(
            find_incomplete_protocol_candidate_start("```json\n{\"a\""),
            Some(0)
        );
    }
}
