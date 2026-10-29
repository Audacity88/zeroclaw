//! Review evidence from real input dispatch, RPC decoding, and app rendering.
//! History replies are synthetic daemon data, not proof of store persistence.

use super::*;
use crossterm::event::{KeyCode, KeyModifiers};

const SESSION: &str = "evidence-session";

fn history_message(role: &str, content: &str, at: String) -> serde_json::Value {
    serde_json::json!({"role": role, "content": content, "created_at": at})
}

fn day_at(days_ago: u64, hour: u32, minute: u32, second: u32) -> String {
    use chrono::TimeZone;
    let day = chrono::Local::now()
        .date_naive()
        .checked_sub_days(chrono::Days::new(days_ago))
        .unwrap();
    chrono::Local
        .from_local_datetime(&day.and_hms_opt(hour, minute, second).unwrap())
        .earliest()
        .expect("fixture time exists in the local zone")
        .to_rfc3339()
}

fn turn_history() -> Vec<serde_json::Value> {
    vec![
        history_message("user", "yesterday task", day_at(1, 12, 0, 0)),
        history_message("assistant", "older answer", day_at(1, 12, 0, 5)),
        history_message("user", "long task", day_at(0, 1, 0, 0)),
        history_message("assistant", "slow answer", day_at(0, 1, 31, 40)),
        history_message("user", "quick follow-up", day_at(0, 1, 35, 0)),
        history_message("assistant", "quick answer", day_at(0, 1, 35, 20)),
        history_message("user", "after a break", day_at(0, 2, 0, 0)),
        history_message("assistant", "done", day_at(0, 2, 0, 5)),
    ]
}

/// Select an untracked persisted ACP session through Ctrl+S and Enter. The
/// production attach path must actually request and deserialize its history.
async fn open_history(
    messages: Vec<serde_json::Value>,
) -> (Chat, Arc<RpcOutbound>, mpsc::Receiver<String>) {
    let (tx, mut rx) = mpsc::channel::<String>(16);
    let rpc = Arc::new(RpcOutbound::new(tx));
    let mut chat = local_code_chat_with_session(&rpc, "evidence-home", native_absolute_root());
    let mut term = key_term();
    let input = async {
        chat.handle_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut term,
        )
        .await;
        chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut term)
            .await;
    };
    let replies = async {
        answer_picker(&rpc, &mut rx, SESSION).await;
        let request = next_rpc_request(&mut rx, "picker attaches persisted session").await;
        assert_eq!(request["method"], method::SESSION_NEW);
        assert_eq!(request["params"]["session_id"], SESSION);
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({"session_id": SESSION, "workspace_dir": native_absolute_root()}),
        );
        let request = next_rpc_request(&mut rx, "attach refreshes model identity").await;
        assert_eq!(request["method"], method::CONFIG_LIST);
        respond_ok(&rpc, &request, serde_json::json!([]));
        let request = next_rpc_request(&mut rx, "attach fetches actual history").await;
        assert_eq!(request["method"], method::SESSION_MESSAGES);
        assert_eq!(request["params"]["session_id"], SESSION);
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({"total": messages.len(), "start": 0, "messages": messages}),
        );
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(input, replies);
    })
    .await
    .expect("keyboard selection and RPC attach complete");
    assert_eq!(chat.current_session_id(), Some(SESSION));
    (chat, rpc, rx)
}

async fn answer_picker(rpc: &RpcOutbound, rx: &mut mpsc::Receiver<String>, session: &str) {
    let request = next_rpc_request(rx, "Ctrl+S opens the ACP session picker").await;
    assert_eq!(request["method"], method::SESSION_LIST_ACP);
    respond_ok(
        rpc,
        &request,
        serde_json::json!({"sessions": [{
            "session_id": session, "session_key": session, "agent_alias": "alpha",
            "created_at": "2026-10-01T00:00:00Z",
            "last_activity": "2026-10-01T00:00:05Z", "message_count": 0
        }]}),
    );
}

fn draw_frame(
    code: &mut crate::acp::Acp,
    client: Arc<RpcClient>,
    width: u16,
    height: u16,
) -> Vec<String> {
    let connection = client.connection_state();
    let mut chat = Chat::new(client, PaneKind::Chat);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("evidence terminal");
    terminal
        .draw(|frame| crate::app::draw_code_evidence_frame(frame, &mut chat, code, &connection))
        .expect("draw real app frame");
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

// This helper is the only stdout-producing code in the evidence tests.
#[allow(clippy::print_stdout, clippy::disallowed_macros)]
fn print_frame(slug: &str, width: u16, height: u16, rows: &[String]) {
    println!("=== EVIDENCE BEGIN {slug} ({width}x{height}) ===");
    for row in rows {
        println!("{row}");
    }
    println!("=== EVIDENCE END {slug} ===");
}

fn assert_turn_frame(rows: &[String]) {
    let text = rows.join("\n");
    for expected in [
        "Yesterday 12:00",
        "older answer",
        "Today 01:00",
        "01:00 → 01:31 · 31m 40s",
        "slow answer",
        "quick follow-up",
        "quick answer",
        "Today 02:00",
        "after a break",
    ] {
        assert!(text.contains(expected), "missing {expected:?}:\n{text}");
    }
    assert_eq!(text.matches("── Today ").count(), 2, "{text}");
    assert_eq!(text.matches("─── ").count(), 1, "{text}");
    let footer = rows.iter().position(|row| row.contains("31m 40s")).unwrap();
    let slow = rows
        .iter()
        .position(|row| row.contains("slow answer"))
        .unwrap();
    let quick = rows
        .iter()
        .position(|row| row.contains("quick follow-up"))
        .unwrap();
    assert!(slow < footer && footer < quick, "{text}");
}

#[tokio::test]
async fn evidence_frame_today_yesterday_and_dated_history() {
    let _lock = env_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let _config = ConfigDirGuard::set(dir.path());
    let mut messages = Vec::new();
    for (days, prompt) in [(3, "dated task"), (1, "yesterday task"), (0, "today task")] {
        messages.push(history_message("user", prompt, day_at(days, 12, 0, 0)));
        messages.push(history_message(
            "assistant",
            "short answer",
            day_at(days, 12, 0, 5),
        ));
    }
    let (chat, _, _) = open_history(messages).await;
    let client = chat.rpc.clone();
    let mut code = crate::acp::Acp::from_chat_for_test(chat);
    let rows = draw_frame(&mut code, client, 100, 30);
    let text = rows.join("\n");
    for expected in [
        "Today 12:00",
        "Yesterday 12:00",
        "dated task",
        "yesterday task",
        "today task",
    ] {
        assert!(text.contains(expected), "missing {expected:?}:\n{text}");
    }
    let date = chrono::DateTime::parse_from_rfc3339(&day_at(3, 12, 0, 0))
        .unwrap()
        .format("%Y-%m-%d 12:00")
        .to_string();
    assert!(text.contains(&date), "{text}");
    assert!(
        !text.contains("─── "),
        "short turns have no footer:\n{text}"
    );
    print_frame("today-yesterday-dated-history", 100, 30, &rows);
}

#[tokio::test]
async fn evidence_frame_long_turn_quick_followup_and_gap() {
    let _lock = env_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let _config = ConfigDirGuard::set(dir.path());
    let (chat, _, _) = open_history(turn_history()).await;
    let client = chat.rpc.clone();
    let mut code = crate::acp::Acp::from_chat_for_test(chat);
    let rows = draw_frame(&mut code, client, 100, 30);
    assert_turn_frame(&rows);
    print_frame("long-turn-quick-followup-gap", 100, 30, &rows);
}

#[tokio::test]
async fn evidence_frame_narrow_long_turn_quick_followup_and_gap() {
    let _lock = env_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let _config = ConfigDirGuard::set(dir.path());
    let (chat, _, _) = open_history(turn_history()).await;
    let client = chat.rpc.clone();
    let mut code = crate::acp::Acp::from_chat_for_test(chat);
    let rows = draw_frame(&mut code, client, 60, 30);
    assert_turn_frame(&rows);
    print_frame("narrow-long-turn-quick-followup-gap", 60, 30, &rows);
}

#[tokio::test]
async fn evidence_frame_older_daemon_without_timestamps() {
    let _lock = env_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let _config = ConfigDirGuard::set(dir.path());
    let (chat, _, _) = open_history(vec![
        serde_json::json!({"role": "user", "content": "legacy prompt"}),
        serde_json::json!({"role": "assistant", "content": "legacy answer"}),
    ])
    .await;
    let client = chat.rpc.clone();
    let mut code = crate::acp::Acp::from_chat_for_test(chat);
    let rows = draw_frame(&mut code, client, 100, 30);
    let text = rows.join("\n");
    assert!(
        text.contains("legacy prompt") && text.contains("legacy answer"),
        "{text}"
    );
    assert!(
        !text.contains("── Today ") && !text.contains("── Yesterday ") && !text.contains("─── "),
        "{text}"
    );
    print_frame("older-daemon-no-timestamps", 100, 30, &rows);
}

async fn switch_tracked_session(
    code: &mut crate::acp::Acp,
    rpc: &RpcOutbound,
    rx: &mut mpsc::Receiver<String>,
    session: &str,
) {
    let mut term = key_term();
    let keys = async {
        code.handle_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut term,
        )
        .await;
        code.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut term)
            .await;
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(keys, answer_picker(rpc, rx, session));
    })
    .await
    .expect("keyboard focus switch completes");
    assert_eq!(code.current_session_id(), Some(session));
    assert!(
        rx.try_recv().is_err(),
        "a tracked focus switch retains cached history"
    );
}

#[tokio::test]
async fn evidence_frame_same_session_after_messages_reload() {
    let _lock = env_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let _config = ConfigDirGuard::set(dir.path());
    let messages = turn_history();
    let (chat, rpc, mut rx) = open_history(messages.clone()).await;
    let client = chat.rpc.clone();
    let mut code = crate::acp::Acp::from_chat_for_test(chat);
    let before = draw_frame(&mut code, client.clone(), 100, 30);
    assert_turn_frame(&before);
    print_frame("same-session-before-reload", 100, 30, &before);

    // Real keyboard focus round-trip. At this head it intentionally retains
    // tracked history, so this alone must not be presented as an RPC reload.
    switch_tracked_session(&mut code, &rpc, &mut rx, "evidence-home").await;
    switch_tracked_session(&mut code, &rpc, &mut rx, SESSION).await;
    let switched = draw_frame(&mut code, client, 100, 30);
    assert_eq!(
        switched, before,
        "focus round-trip preserves the visible frame"
    );
    print_frame("same-session-after-keyboard-switch", 100, 30, &switched);

    // Use the app's canonical reconnect snapshot. It carries session ids and
    // client-owned queues, never transcript entries or their timestamps.
    let entries = code.resume_entries();
    let (tx, mut reconnect_rx) = mpsc::channel::<String>(16);
    let reconnect_rpc = Arc::new(RpcOutbound::new(tx));
    let reconnect_client = Arc::new(RpcClient::with_rpc_transport(
        reconnect_rpc.clone(),
        crate::client::Transport::Local,
    ));
    let mut rebuilt = crate::acp::Acp::new(reconnect_client.clone());
    rebuilt.set_resume_sessions(entries);
    let mut loaded_sessions = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        let init = rebuilt.init();
        tokio::pin!(init);
        loop {
            tokio::select! {
                result = &mut init => {
                    result.expect("production reconnect initialization");
                    break;
                }
                raw = reconnect_rx.recv() => {
                    let request: serde_json::Value = serde_json::from_str(
                        &raw.expect("reconnect request channel stays open")
                    ).unwrap();
                    match request["method"].as_str() {
                        Some(method::AGENTS_STATUS) => respond_ok(&reconnect_rpc, &request,
                            serde_json::json!({"agents": [{"alias": "alpha", "enabled": true,
                                "live_sessions": 2, "persisted_sessions": 2}]})),
                        Some(method::SESSION_NEW) => {
                            let sid = request["params"]["session_id"].as_str().unwrap();
                            assert!([SESSION, "evidence-home"].contains(&sid));
                            respond_ok(&reconnect_rpc, &request, serde_json::json!({
                                "session_id": sid, "workspace_dir": native_absolute_root()
                            }));
                        }
                        Some(method::CONFIG_LIST) => respond_ok(&reconnect_rpc, &request, serde_json::json!([])),
                        Some(method::SESSION_MESSAGES) => {
                            let sid = request["params"]["session_id"].as_str().unwrap();
                            let history = if sid == SESSION { messages.clone() } else {
                                assert_eq!(sid, "evidence-home");
                                Vec::new()
                            };
                            loaded_sessions.push(sid.to_string());
                            respond_ok(&reconnect_rpc, &request, serde_json::json!({
                                "total": history.len(), "start": 0, "messages": history
                            }));
                        }
                        other => panic!("unexpected reconnect request: {other:?}"),
                    }
                }
            }
        }
    }).await.expect("reconnect reload completes");
    assert_eq!(loaded_sessions.len(), 2);
    assert!(loaded_sessions.iter().any(|sid| sid == SESSION));
    assert_eq!(rebuilt.current_session_id(), Some(SESSION));
    let after = draw_frame(&mut rebuilt, reconnect_client, 100, 30);
    assert_turn_frame(&after);
    assert_eq!(
        after, before,
        "session/messages reload preserves every visible row"
    );
    print_frame("same-session-after-messages-reload", 100, 30, &after);
}

// Fluent uses process-wide OnceLocks. Run just this test in a fresh process
// to exercise the actual French catalogue without changing siblings' locale.
#[allow(clippy::print_stdout, clippy::disallowed_macros)]
#[tokio::test]
async fn evidence_frame_french_transcript() {
    if std::env::var("ZEROCODE_EVIDENCE_LOCALE").as_deref() != Ok("fr") {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "chat::tests::evidence_frames::evidence_frame_french_transcript",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("ZEROCODE_EVIDENCE_LOCALE", "fr")
            .output()
            .expect("run isolated French interface test");
        assert!(
            output.status.success(),
            "French evidence failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("=== EVIDENCE END french-transcript ==="),
            "the isolated test must actually run and print a frame"
        );
        print!("{}", String::from_utf8_lossy(&output.stdout));
        return;
    }
    let _lock = env_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let _config = ConfigDirGuard::set(dir.path());
    let locale_dir = dir.path().join("data/ftl/fr");
    std::fs::create_dir_all(&locale_dir).unwrap();
    std::fs::write(
        locale_dir.join("zerocode.ftl"),
        include_str!("../../../locales/fr/zerocode.ftl"),
    )
    .unwrap();
    crate::i18n::init("fr", dir.path());
    let (chat, _, _) = open_history(vec![
        history_message("user", "ancienne question", day_at(1, 12, 0, 0)),
        history_message("assistant", "ancienne réponse", day_at(1, 12, 0, 5)),
        history_message("user", "longue question", day_at(0, 12, 0, 0)),
        history_message("assistant", "réponse finale", day_at(0, 12, 31, 40)),
    ])
    .await;
    let client = chat.rpc.clone();
    let mut code = crate::acp::Acp::from_chat_for_test(chat);
    let rows = draw_frame(&mut code, client, 100, 30);
    let text = rows.join("\n");
    for expected in [
        "Aujourd’hui 12:00",
        "Hier 12:00",
        "31 min 40 s",
        "réponse finale",
    ] {
        assert!(text.contains(expected), "missing {expected:?}:\n{text}");
    }
    assert!(
        !text.contains("Today ") && !text.contains("Yesterday "),
        "{text}"
    );
    print_frame("french-transcript", 100, 30, &rows);
}
