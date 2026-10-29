//! Review evidence from production input, RPC decoding and app renderers.
//! These fixtures describe daemon replies, not a real daemon or network run.

#[cfg(test)] // test-only module: lets the keymap guard skip review-evidence key input
use super::*;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use zeroclaw_api::agent::{DelegateActivity, DelegateActivityPhase};

struct DelegateFrame {
    chat: Chat,
    code: crate::acp::Acp,
    rpc: Arc<RpcOutbound>,
    rx: mpsc::Receiver<String>,
    prompt: serde_json::Value,
    width: u16,
    height: u16,
}

impl DelegateFrame {
    async fn running() -> Self {
        let (tx, rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let mut chat = local_code_chat_with_session(&rpc, "sess-1", "/review/project");
        chat.pane_kind = PaneKind::Chat;
        let code = crate::acp::Acp::new(Arc::clone(&chat.rpc));
        let mut term = key_term();
        let prompt_text = "Ask a helper to inspect the docs.";
        for ch in prompt_text.chars() {
            chat.handle_key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut term,
            )
            .await;
        }
        chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut term)
            .await;
        let mut fixture = Self {
            chat,
            code,
            rpc,
            rx,
            prompt: serde_json::Value::Null,
            width: 100,
            height: 30,
        };
        fixture.prompt = next_rpc_request(&mut fixture.rx, "Enter must send the prompt").await;
        assert_eq!(fixture.prompt["method"], method::SESSION_PROMPT);
        assert_eq!(fixture.prompt["params"]["prompt"], prompt_text);
        assert_eq!(fixture.prompt["params"]["session_id"], "sess-1");
        assert!(active_state(&mut fixture.chat).turn_in_flight);
        fixture.notify(serde_json::json!({
            "type": "tool_call", "tool_call_id": "parent", "name": "delegate",
            "raw_input": {"agent": "researcher", "prompt": "Inspect the docs"}
        }));
        fixture.start("call_1", 0, "command: inspect-docs");
        fixture
    }

    fn notify(&mut self, mut params: serde_json::Value) {
        params["session_id"] = "sess-1".into();
        self.chat
            .rpc
            .push_notification_for_test("session/update", params);
        self.chat.tick_transport_events();
    }

    fn activity(&mut self, id: &str, depth: u32, phase: DelegateActivityPhase) {
        let mut params = serde_json::to_value(DelegateActivity {
            parent_tool_call_id: "parent".into(),
            agent: "researcher".into(),
            depth,
            tool_call_id: id.into(),
            tool_name: "shell".into(),
            phase,
        })
        .unwrap();
        params["type"] = "delegate_activity".into();
        self.notify(params);
    }

    fn start(&mut self, id: &str, depth: u32, summary: &str) {
        self.activity(
            id,
            depth,
            DelegateActivityPhase::Started {
                arguments_summary: summary.into(),
            },
        );
    }

    fn finish_child(&mut self, id: &str, depth: u32, success: bool) {
        self.activity(
            id,
            depth,
            DelegateActivityPhase::Finished {
                success,
                // The current UI intentionally does not render this wire field.
                output_preview: "scrubbed child output".into(),
            },
        );
    }

    fn parent_result(&mut self) {
        self.notify(serde_json::json!({
            "type": "tool_result", "tool_call_id": "parent",
            "raw_output": "Inspection complete"
        }));
    }

    async fn finish_turn(&mut self, outcome: &str) {
        self.notify(serde_json::json!({
            "type": "turn_complete", "outcome": outcome,
            "client_turn_generation": self.prompt["params"]["client_turn_generation"]
        }));
        respond_ok(
            &self.rpc,
            &self.prompt,
            serde_json::json!({"stop_reason": outcome}),
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.chat.prompt_completion_rx.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the prompt response must reach the pane");
        self.chat.tick_transport_events();
        assert!(!active_state(&mut self.chat).turn_in_flight);
    }

    fn rows(&mut self) -> (Vec<String>, Rect) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(self.width, self.height))
                .unwrap();
        let connection = self.chat.rpc.connection_state();
        let mut content = Rect::default();
        terminal
            .draw(|frame| {
                content = crate::app::draw_chat_evidence_frame(
                    frame,
                    &mut self.chat,
                    &mut self.code,
                    &connection,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..self.height)
            .map(|y| {
                (0..self.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        (rows, content)
    }

    async fn expand(&mut self) {
        let (_, content) = self.rows();
        let header = active_state(&mut self.chat)
            .tool_header_rects
            .first()
            .expect("the real renderer must expose the delegate header hit target")
            .1;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            self.chat
                .handle_mouse(
                    MouseEvent {
                        kind,
                        column: header.x + 1,
                        row: header.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    content,
                )
                .await;
        }
        assert_eq!(
            active_state(&mut self.chat).tool_disclosures.get("parent"),
            Some(&ToolDisclosure::Full)
        );
    }

    // This stdout is the requested export format, confined to the evidence helper.
    #[allow(clippy::print_stdout)]
    fn capture(&mut self, slug: &str, expected: &[&str], absent: &[&str]) {
        let (rows, _) = self.rows();
        let text = rows.join("\n");
        for key in expected {
            assert!(text.contains(key), "{slug}: missing {key:?}\n{text}");
        }
        for key in absent {
            assert!(!text.contains(key), "{slug}: unexpected {key:?}\n{text}");
        }
        println!(
            "=== EVIDENCE BEGIN {slug} ({}x{}) ===",
            self.width, self.height
        );
        for row in rows {
            println!("{row}");
        }
        println!("=== EVIDENCE END {slug} ===");
    }
}

#[tokio::test]
async fn evidence_frame_delegate_collapsed_running() {
    let _lock = env_test_lock_async().await;
    let mut f = DelegateFrame::running().await;
    f.capture(
        "delegate-collapsed-running",
        &[
            "researcher",
            "Waiting for response",
            "1 child tool calls",
            "now: shell",
        ],
        &["command: inspect-docs", "Result not recorded"],
    );
    f.finish_turn("completed").await;
}

#[tokio::test]
async fn evidence_frame_delegate_expanded_running() {
    let _lock = env_test_lock_async().await;
    let mut f = DelegateFrame::running().await;
    f.expand().await;
    f.capture(
        "delegate-expanded-running",
        &[
            "Waiting for response",
            "1 child tool calls",
            "now: shell",
            "… researcher: shell (command: inspect-docs)",
        ],
        &["Result not recorded", "scrubbed child output"],
    );
    f.finish_turn("completed").await;
}

#[tokio::test]
async fn evidence_frame_delegate_expanded_finished() {
    let _lock = env_test_lock_async().await;
    let mut f = DelegateFrame::running().await;
    f.expand().await;
    f.finish_child("call_1", 0, true);
    f.parent_result();
    f.finish_turn("completed").await;
    f.capture(
        "delegate-expanded-finished",
        &[
            "Inspection complete",
            "1 child tool calls",
            "✓ researcher: shell (command: inspect-docs)",
        ],
        &["now:", "Waiting for response", "scrubbed child output"],
    );
}

#[tokio::test]
async fn evidence_frame_delegate_reverse_finishes_keep_failure_with_arguments() {
    let _lock = env_test_lock_async().await;
    let mut f = DelegateFrame::running().await;
    f.start("call_2", 0, "command: inspect-tests");
    f.finish_child("call_2", 0, false);
    f.finish_child("call_1", 0, true);
    f.parent_result();
    f.finish_turn("completed").await;
    f.expand().await;
    f.capture(
        "delegate-reverse-finishes",
        &[
            "2 child tool calls",
            "✓ researcher: shell (command: inspect-docs)",
            "✗ researcher: shell (command: inspect-tests)",
        ],
        &["now:", "✓ researcher: shell (command: inspect-tests)"],
    );
}

#[tokio::test]
async fn evidence_frame_delegate_reused_ids_and_nested_depth() {
    let _lock = env_test_lock_async().await;
    let mut f = DelegateFrame::running().await;
    f.start("call_1", 1, "command: nested-check");
    f.finish_child("call_1", 0, false);
    f.finish_child("call_1", 1, true);
    f.start("call_1", 0, "command: second-pass");
    f.finish_child("call_1", 0, true);
    // An extra finish must not overwrite an already-settled reused id.
    f.finish_child("call_1", 0, false);
    f.parent_result();
    f.finish_turn("completed").await;
    f.expand().await;
    f.capture(
        "delegate-reused-ids-and-depth",
        &[
            "3 child tool calls",
            "✗ researcher: shell (command: inspect-docs)",
            "✓ researcher: shell (command: nested-check)",
            "✓ researcher: shell (command: second-pass)",
        ],
        &["now:", "✗ researcher: shell (command: second-pass)"],
    );
}

#[tokio::test]
async fn evidence_frame_delegate_cancelled_without_child_finish() {
    let _lock = env_test_lock_async().await;
    let mut f = DelegateFrame::running().await;
    f.expand().await;
    let mut term = key_term();
    let answer = async {
        let request = next_rpc_request(&mut f.rx, "Escape must request cancellation").await;
        assert_eq!(request["method"], method::SESSION_CANCEL);
        assert_eq!(request["params"]["session_id"], "sess-1");
        respond_ok(
            &f.rpc,
            &request,
            serde_json::json!({"session_id": "sess-1", "cancelled": true}),
        );
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            f.chat
                .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut term),
            answer,
        );
    })
    .await
    .expect("cancellation RPC must complete");
    f.finish_turn("cancelled").await;
    f.capture(
        "delegate-cancelled-unfinished-child",
        &[
            "1 child tool calls",
            "Result not recorded",
            "· researcher: shell (command: inspect-docs) · Result not recorded",
        ],
        &["now:", "Waiting for response", "… researcher: shell"],
    );
}

#[tokio::test]
async fn evidence_frame_delegate_failed_turn_without_child_finish() {
    let _lock = env_test_lock_async().await;
    let mut f = DelegateFrame::running().await;
    f.finish_turn("failed").await;
    f.expand().await;
    f.capture(
        "delegate-failed-turn-unfinished-child",
        &[
            "1 child tool calls",
            "Result not recorded",
            "· researcher: shell (command: inspect-docs) · Result not recorded",
        ],
        &["now:", "Waiting for response", "… researcher: shell"],
    );
}

#[tokio::test]
async fn evidence_frame_delegate_completed_without_child_finish() {
    let _lock = env_test_lock_async().await;
    let mut f = DelegateFrame::running().await;
    f.parent_result();
    f.finish_turn("completed").await;
    f.expand().await;
    f.capture(
        "delegate-completed-unfinished-child",
        &[
            "Inspection complete",
            "1 child tool calls",
            "· researcher: shell (command: inspect-docs) · Result not recorded",
        ],
        &["now:", "Waiting for response", "… researcher: shell"],
    );
}

#[tokio::test]
async fn evidence_frame_delegate_restored_history_has_no_live_activity() {
    let _lock = env_test_lock_async().await;
    let mut f = DelegateFrame::running().await;
    f.finish_turn("cancelled").await;
    // Re-enter through session/new and session/messages, rather than building
    // a restored card or clearing its activity log in the test.
    f.chat = Chat::new(Arc::clone(&f.chat.rpc), PaneKind::Chat);
    f.chat
        .set_resume_sessions(vec![resume_entry("sess-1", "alpha", true)]);
    let answer = async {
        for method_name in [
            method::SESSION_NEW,
            method::CONFIG_LIST,
            method::SESSION_MESSAGES,
        ] {
            let request =
                next_rpc_request(&mut f.rx, "resume must load its retained history").await;
            assert_eq!(request["method"], method_name);
            let result = match method_name {
                method::SESSION_NEW => {
                    assert_eq!(request["params"]["session_id"], "sess-1");
                    serde_json::json!({"session_id": "sess-1", "workspace_dir": "/review/project"})
                }
                method::CONFIG_LIST => serde_json::json!([]),
                method::SESSION_MESSAGES => serde_json::json!({
                    "messages": [{
                        "role": "assistant", "kind": "tool_call", "content": "",
                        "tool_call_id": "parent", "tool_name": "delegate",
                        "tool_input": {"agent": "researcher", "prompt": "Inspect the docs"}
                    }], "total": 1
                }),
                _ => unreachable!(),
            };
            respond_ok(&f.rpc, &request, result);
        }
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(f.chat.start_session("alpha", None), answer);
    })
    .await
    .expect("resume must finish loading history");
    f.expand().await;
    f.capture(
        "delegate-restored-history",
        &["researcher", "Inspect the docs", "Result not recorded"],
        &[
            "child tool calls",
            "now:",
            "Waiting for response",
            "inspect-docs",
        ],
    );
}
