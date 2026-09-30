//! Live progress for background delegates.
//!
//! A background delegate's sub-loop reports into a [`DelegateProgressSink`]
//! instead of discarding its observer events. The sink keeps a bounded
//! [`TaskProgress`] in memory; one flusher task per delegate writes it to the
//! task row, which is where `check_result` reads it behind the same
//! visibility check as the task's output. Tool arguments, tool results,
//! error text and prompt content never enter the record.

use crate::control_plane::{TaskProgress, TaskProgressTool, TaskRegistry};
use crate::observability::traits::{Observer, ObserverEvent, ObserverMetric};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Finished tool calls kept in [`TaskProgress::recent_tools`].
pub(crate) const RECENT_TOOLS_LIMIT: usize = 5;
/// Receipts kept in [`TaskProgress::receipt_tail`].
pub(crate) const RECEIPT_TAIL_LIMIT: usize = 5;
/// Characters kept of a tool name.
pub(crate) const TOOL_NAME_LIMIT: usize = 64;
/// Interval at which the flusher rewrites the current state even without new
/// events, so the row's heartbeat stays fresh through one long tool call or a
/// nested synchronous delegate. Well under the reaper's heartbeat-age limit.
pub(crate) const PROGRESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

pub(crate) struct DelegateProgressSink {
    state: Mutex<TaskProgress>,
    changed: tokio::sync::Notify,
    /// The background task's own receipt collector (never the launching
    /// turn's); `None` when receipts are off.
    receipts: Option<Arc<std::sync::Mutex<Vec<String>>>>,
}

impl DelegateProgressSink {
    pub(crate) fn new(receipts: Option<Arc<std::sync::Mutex<Vec<String>>>>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(TaskProgress::default()),
            changed: tokio::sync::Notify::new(),
            receipts,
        })
    }

    /// Record the wall-clock budget the delegated loop runs under.
    pub(crate) fn set_timeout_budget(&self, secs: u64) {
        self.state.lock().timeout_budget_secs = Some(secs);
        self.changed.notify_one();
    }

    /// Current progress, with the receipt tail read from the collector.
    pub(crate) fn snapshot(&self) -> TaskProgress {
        let mut progress = self.state.lock().clone();
        if let Some(receipts) = &self.receipts {
            let receipts = receipts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let skip = receipts.len().saturating_sub(RECEIPT_TAIL_LIMIT);
            progress.receipt_tail = receipts[skip..].to_vec();
        }
        progress
    }

    fn apply(&self, event: &ObserverEvent) {
        let now = chrono::Utc::now().to_rfc3339();
        let mut state = self.state.lock();
        match event {
            ObserverEvent::LlmRequest { .. } => {
                state.iterations = state.iterations.saturating_add(1);
            }
            ObserverEvent::LlmResponse { .. } => {}
            ObserverEvent::ToolCallStart { tool, .. } => {
                state.last_tool = Some(TaskProgressTool {
                    name: bounded_tool_name(tool),
                    started_at: Some(now.clone()),
                    finished_at: None,
                    success: None,
                });
            }
            ObserverEvent::ToolCall { tool, success, .. } => {
                let name = bounded_tool_name(tool);
                state.tools_completed = state.tools_completed.saturating_add(1);
                let started_at = match &state.last_tool {
                    Some(last) if last.name == name && last.finished_at.is_none() => {
                        last.started_at.clone()
                    }
                    _ => None,
                };
                let finished = TaskProgressTool {
                    name,
                    started_at,
                    finished_at: Some(now.clone()),
                    success: Some(*success),
                };
                state.last_tool = Some(finished.clone());
                state.recent_tools.push(finished);
                let excess = state.recent_tools.len().saturating_sub(RECENT_TOOLS_LIMIT);
                state.recent_tools.drain(..excess);
            }
            _ => return,
        }
        state.last_activity_at = Some(now);
        drop(state);
        self.changed.notify_one();
    }
}

impl Observer for DelegateProgressSink {
    fn record_event(&self, event: &ObserverEvent) {
        self.apply(event);
    }

    fn record_metric(&self, _metric: &ObserverMetric) {}

    fn name(&self) -> &str {
        "delegate-progress"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn bounded_tool_name(name: &str) -> String {
    name.chars().take(TOOL_NAME_LIMIT).collect()
}

/// Write `sink`'s progress to the task row whenever it changes and at least
/// every [`PROGRESS_HEARTBEAT_INTERVAL`], one write at a time, until `stop`
/// fires; then write once more and return. Store errors never fail the
/// delegation; the first one per task is logged.
pub(crate) async fn run_progress_flusher(
    sink: Arc<DelegateProgressSink>,
    store: Arc<dyn TaskRegistry>,
    task_id: String,
    owner_boot_id: String,
    stop: CancellationToken,
) {
    let mut warned = false;
    let mut tick = tokio::time::interval(PROGRESS_HEARTBEAT_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            () = sink.changed.notified() => {}
            _ = tick.tick() => {}
        }
        flush_progress(&sink, store.as_ref(), &task_id, &owner_boot_id, &mut warned).await;
    }
    flush_progress(&sink, store.as_ref(), &task_id, &owner_boot_id, &mut warned).await;
}

async fn flush_progress(
    sink: &DelegateProgressSink,
    store: &dyn TaskRegistry,
    task_id: &str,
    owner_boot_id: &str,
    warned: &mut bool,
) {
    let Err(error) = store
        .record_progress(task_id, owner_boot_id, &sink.snapshot())
        .await
    else {
        return;
    };
    if std::mem::replace(warned, true) {
        return;
    }
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({
                "task_id": task_id,
                "error": format!("{error:#}"),
            })),
        "delegate progress could not be recorded; the delegation continues"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_start(tool: &str, arguments: &str) -> ObserverEvent {
        ObserverEvent::ToolCallStart {
            tool: tool.into(),
            tool_call_id: None,
            arguments: Some(arguments.into()),
            channel: None,
            agent_alias: None,
            parent_agent_alias: None,
            turn_id: None,
        }
    }

    fn tool_done(tool: &str, arguments: &str, result: &str) -> ObserverEvent {
        ObserverEvent::ToolCall {
            tool: tool.into(),
            tool_call_id: None,
            duration: Duration::from_millis(3),
            success: true,
            arguments: Some(arguments.into()),
            result: Some(result.into()),
            channel: None,
            agent_alias: None,
            parent_agent_alias: None,
            turn_id: None,
        }
    }

    #[test]
    fn progress_is_bounded_and_never_records_arguments_or_results() {
        let receipts = Arc::new(std::sync::Mutex::new(
            (0..7)
                .map(|i| format!("zc-receipt-{i}"))
                .collect::<Vec<_>>(),
        ));
        let sink = DelegateProgressSink::new(Some(Arc::clone(&receipts)));
        let secret = "SECRET-MARKER-10531";
        let long_name = "t".repeat(200);
        sink.record_event(&ObserverEvent::LlmRequest {
            model_provider: "p".into(),
            model: "m".into(),
            messages_count: 2,
            channel: None,
            agent_alias: None,
            parent_agent_alias: None,
            turn_id: None,
        });
        for i in 0..8 {
            let name = if i == 7 {
                long_name.clone()
            } else {
                format!("tool_{i}")
            };
            sink.record_event(&tool_start(&name, secret));
            sink.record_event(&tool_done(&name, secret, secret));
        }
        sink.record_event(&tool_start("shell", secret));

        let progress = sink.snapshot();
        assert_eq!(progress.iterations, 1, "one LlmRequest counted");
        assert_eq!(progress.tools_completed, 8, "every finished call counted");
        assert_eq!(
            progress.recent_tools.len(),
            RECENT_TOOLS_LIMIT,
            "recent tools are capped"
        );
        assert_eq!(
            progress.recent_tools[0].name, "tool_3",
            "oldest are dropped first"
        );
        let truncated = &progress.recent_tools[RECENT_TOOLS_LIMIT - 1];
        assert_eq!(truncated.name.chars().count(), TOOL_NAME_LIMIT);
        assert!(
            truncated.started_at.is_some() && truncated.finished_at.is_some(),
            "a finished call keeps the start time of its matching start: {truncated:?}"
        );
        let last = progress.last_tool.as_ref().expect("last tool");
        assert_eq!(last.name, "shell");
        assert!(last.finished_at.is_none() && last.success.is_none());
        assert_eq!(
            progress.receipt_tail,
            (2..7)
                .map(|i| format!("zc-receipt-{i}"))
                .collect::<Vec<_>>(),
            "receipt tail keeps the newest receipts"
        );
        assert!(progress.last_activity_at.is_some());
        let encoded = serde_json::to_string(&progress).unwrap();
        assert!(
            !encoded.contains(secret),
            "arguments and results must never reach the stored progress: {encoded}"
        );
    }

    #[test]
    fn progress_without_receipt_scope_has_an_empty_tail() {
        let sink = DelegateProgressSink::new(None);
        sink.record_event(&tool_start("shell", "{}"));
        sink.set_timeout_budget(300);
        let progress = sink.snapshot();
        assert!(progress.receipt_tail.is_empty());
        assert_eq!(progress.timeout_budget_secs, Some(300));
    }
}
