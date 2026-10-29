//! Review evidence drawn by the real app widgets after RPC-backed transitions.
//! These buffers are interface evidence, not proof of a real daemon restart.

use super::*;
use crate::agent_sidebar::{AgentSidebar, SidebarEvent};
use crate::chat::{MAX_TRACKED_SESSIONS_PER_PANE, PaneKind, ResumeEntry};
use crate::client::{Transport, method};
use crate::jsonrpc::RpcOutbound;
use crate::open_sessions::{Snapshot, Store};
use serde_json::{Value, json};

const WIDTH: u16 = 100;
const HEIGHT: u16 = 30;
const TARGET: &str = "local:evidence-daemon";
const SESSIONS: [(&str, &str, PaneKind); 4] = [
    ("evidence-code-a", "code-a", PaneKind::Acp),
    ("evidence-code-b", "code-b", PaneKind::Acp),
    ("evidence-chat-c", "chat-c", PaneKind::Chat),
    ("evidence-window-d", "window-d", PaneKind::Chat),
];

/// Reply only to the RPCs exercised by these scenarios. Unexpected calls fail
/// rather than silently substituting a permissive reply for an untested path.
struct EvidenceRpc {
    client: Arc<RpcClient>,
    outbound: Arc<RpcOutbound>,
    requests: mpsc::Receiver<String>,
    attached: Vec<String>,
}

impl EvidenceRpc {
    fn new() -> Self {
        let (tx, requests) = mpsc::channel(32);
        let outbound = Arc::new(RpcOutbound::new(tx));
        Self {
            client: Arc::new(RpcClient::with_rpc_transport(
                outbound.clone(),
                Transport::Local,
            )),
            outbound,
            requests,
            attached: Vec::new(),
        }
    }

    async fn drive<T>(&mut self, action: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::pin!(action);
            loop {
                tokio::select! {
                    result = &mut action => return result,
                    line = self.requests.recv() => {
                        let request: Value = serde_json::from_str(
                            &line.expect("RPC writer stays open"),
                        ).expect("JSON-RPC request");
                        let result = self.reply(&request);
                        self.outbound.dispatch_response(
                            request["id"].as_str().expect("request id"),
                            Some(result),
                            None,
                        );
                    }
                }
            }
        })
        .await
        .expect("interface transition completes")
    }

    fn reply(&mut self, request: &Value) -> Value {
        match request["method"].as_str().expect("RPC method") {
            method::AGENTS_STATUS => json!({"agents": SESSIONS.iter().map(|(_, alias, _)| {
                json!({"alias": alias, "enabled": true, "persisted_sessions": 1})
            }).collect::<Vec<_>>()}),
            method::SESSION_LIST_ACP => json!({"sessions": SESSIONS.iter()
                .filter(|(_, _, pane)| *pane == PaneKind::Acp)
                .map(|(id, alias, _)| json!({
                    "session_id": id, "session_key": id, "agent_alias": alias,
                    "created_at": "2026-10-29T00:00:00Z",
                    "last_activity": "2026-10-29T00:00:00Z", "message_count": 1,
                })).collect::<Vec<_>>()}),
            method::SESSION_NEW => {
                let alias = request["params"]["agent_alias"]
                    .as_str()
                    .expect("agent alias");
                let (id, _, pane) = SESSIONS
                    .iter()
                    .find(|(_, a, _)| *a == alias)
                    .expect("known synthetic agent");
                if !request["params"]["session_id"].is_null() {
                    assert_eq!(request["params"]["session_id"], *id);
                    assert!(
                        request["params"]["cwd"].is_null(),
                        "resume keeps daemon cwd"
                    );
                }
                assert_eq!(
                    request["params"]["exclude_memory"]
                        .as_bool()
                        .unwrap_or(false),
                    *pane == PaneKind::Acp
                );
                self.attached.push((*id).to_string());
                json!({"session_id": id, "message_count": if *id == "evidence-window-d" { 0 } else { 1 }})
            }
            method::CONFIG_LIST => json!([]),
            method::SESSION_MESSAGES => {
                let id = request["params"]["session_id"]
                    .as_str()
                    .expect("history id");
                let (_, alias, _) = SESSIONS
                    .iter()
                    .find(|(sid, _, _)| *sid == id)
                    .expect("history belongs to a synthetic session");
                if id == "evidence-window-d" {
                    return json!({"messages": [], "total": 0});
                }
                json!({"messages": [{"role": "assistant",
                    "content": format!("Saved transcript for {alias}.")}], "total": 1})
            }
            method::COST_QUERY => json!({"session_cost_usd": 0.0, "daily_cost_usd": 0.0,
                "monthly_cost_usd": 0.0, "total_tokens": 0, "request_count": 0}),
            method::TUI_LIST => json!({"tuis": []}),
            other => panic!("unexpected evidence RPC: {other}"),
        }
    }
}

struct EvidenceWindow {
    client: Arc<RpcClient>,
    chat: chat::Chat,
    code: acp::Acp,
    dashboard: dashboard::Dashboard,
    sidebar: AgentSidebar,
    dock: ConversationDock,
    mode: Mode,
    mode_bar: ModeBarLayout,
}

struct EvidenceFrame {
    rows: Vec<String>,
    sidebar_rows: Vec<String>,
}

impl EvidenceWindow {
    async fn init(
        dir: &std::path::Path,
        rpc: &mut EvidenceRpc,
        saved: Option<&Snapshot>,
        chat_entries: Vec<ResumeEntry>,
        code_entries: Vec<ResumeEntry>,
    ) -> Self {
        let client = rpc.client.clone();
        let mut dashboard = dashboard::Dashboard::new(client.clone(), TARGET, false);
        rpc.drive(dashboard.init()).await.unwrap();
        let mut code = acp::Acp::new(client.clone());
        code.set_resume_sessions(code_entries);
        rpc.drive(code.init()).await.unwrap();
        let mut chat = chat::Chat::new(client.clone(), PaneKind::Chat);
        chat.set_resume_sessions(chat_entries);
        rpc.drive(chat.init()).await.unwrap();
        let mut sidebar = AgentSidebar::new();
        if let Some(saved) = saved {
            sidebar.restore_order(saved.sidebar_order());
        }
        let mode = match saved.and_then(Snapshot::start_pane) {
            Some(PaneKind::Acp) => Mode::Acp,
            Some(PaneKind::Chat) => Mode::Chat,
            None => Mode::Dashboard,
        };
        Self {
            client,
            chat,
            code,
            dashboard,
            sidebar,
            dock: ConversationDock::from_config_dir(dir),
            mode,
            mode_bar: ModeBarLayout::default(),
        }
    }

    async fn restore(
        dir: &std::path::Path,
        rpc: &mut EvidenceRpc,
        saved: Option<&Snapshot>,
    ) -> Self {
        let (chat_entries, code_entries) = saved
            .map(|s| s.resume_entries(MAX_TRACKED_SESSIONS_PER_PANE))
            .unwrap_or_default();
        Self::init(dir, rpc, saved, chat_entries, code_entries).await
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot::from_rows(
            &session_summaries_in_sidebar_order(
                &self.sidebar,
                self.code.session_summaries(),
                self.chat.session_summaries(),
            ),
            match self.mode {
                Mode::Acp => Some(PaneKind::Acp),
                Mode::Chat => Some(PaneKind::Chat),
                _ => None,
            },
        )
    }

    /// Same widgets, geometry, and draw order as the conversation branch of
    /// app::run. Nothing below paints evidence labels or replacement widgets.
    fn draw(&mut self) -> EvidenceFrame {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(WIDTH, HEIGHT))
                .expect("evidence terminal");
        let rows = session_summaries_in_sidebar_order(
            &self.sidebar,
            self.code.session_summaries(),
            self.chat.session_summaries(),
        );
        let active_pane = match self.mode {
            Mode::Acp => Some(PaneKind::Acp),
            Mode::Chat => Some(PaneKind::Chat),
            _ => None,
        };
        let ctx = crate::agent_sidebar::SidebarCtx {
            active_pane,
            quickstart_active: false,
            connected: true,
        };
        let plan_visible = match self.mode {
            Mode::Acp => self.code.plan_visible(),
            Mode::Chat => self.chat.plan_visible(),
            _ => false,
        };
        let mut statuses = self.chat.terminal_statuses();
        statuses.extend(self.code.terminal_statuses());
        terminal
            .draw(|frame| {
                draw_app_frame(
                    frame,
                    self.mode,
                    &mut self.chat,
                    &mut self.code,
                    |frame, chunks, chat, code| {
                        self.mode_bar = draw_mode_bar(frame, chunks[0], self.mode, None);
                        let layout = self.dock.layout(chunks[1], self.mode, plan_visible);
                        self.dock.draw_shell(frame);
                        if let Some(area) = layout.sessions {
                            self.sidebar.draw(frame, area, &rows, &ctx);
                        } else {
                            self.sidebar.clear_geometry();
                        }
                        match self.mode {
                            Mode::Acp => code.draw_with_dock(
                                frame,
                                layout.conversation,
                                layout.queue,
                                layout.plan,
                            ),
                            Mode::Chat => chat.draw_with_dock(
                                frame,
                                layout.conversation,
                                layout.queue,
                                layout.plan,
                            ),
                            Mode::Dashboard => self.dashboard.draw(
                                frame,
                                layout.conversation,
                                None,
                                None,
                                code.current_cwd(),
                                chat.current_cwd(),
                                code.focused_runtime_context(),
                                chat.focused_runtime_context(),
                                &statuses,
                            ),
                            _ => unreachable!("only supported evidence modes"),
                        }
                        self.dock.draw_resize_handles(frame);
                        match self.mode {
                            Mode::Acp => code.draw_dock_overlay(frame),
                            Mode::Chat => chat.draw_dock_overlay(frame),
                            _ => {}
                        }
                        let (input, source, max, window) = match self.mode {
                            Mode::Acp => code.ctx_tokens(),
                            Mode::Chat => chat.ctx_tokens(),
                            _ => (None, None, None, None),
                        };
                        let browse = match self.mode {
                            Mode::Acp => code.in_browse_mode(),
                            Mode::Chat => chat.in_browse_mode(),
                            _ => false,
                        };
                        draw_status_bar(
                            frame,
                            chunks[2],
                            &self.client.connection_state(),
                            self.client.tui_id(),
                            CtxBar::new(input, source, max, window),
                            false,
                            browse,
                        );
                    },
                );
                self.sidebar.draw_picker(frame, frame.area());
            })
            .expect("real app frame draws");
        let buffer = terminal.backend().buffer();
        let rows = (0..HEIGHT)
            .map(|y| {
                (0..WIDTH)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let sidebar_rows = self
            .dock
            .layout
            .sessions
            .map(|area| {
                (area.y..area.bottom())
                    .map(|y| {
                        (area.x..area.right())
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect()
            })
            .unwrap_or_default();
        EvidenceFrame { rows, sidebar_rows }
    }

    fn click_mode(&mut self, target: Mode) {
        let entry = self
            .mode_bar
            .entries
            .iter()
            .find(|entry| entry.mode == target)
            .expect("requested mode is visible");
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: entry.hit_rect.x,
            row: entry.hit_rect.y,
            modifiers: KeyModifiers::NONE,
        };
        self.mode = self
            .mode_bar
            .mode_at(mouse.column, mouse.row)
            .expect("real mode-bar hit");
        assert_eq!(self.mode, target);
    }

    async fn add_second_window_session(&mut self, rpc: &mut EvidenceRpc) {
        self.draw();
        self.click_mode(Mode::Chat);
        self.draw();
        let area = self.dock.layout.sessions.expect("Sessions geometry");
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.right() - 3,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        let (consumed, event) = route_agent_sidebar_mouse(
            self.mode,
            &mut self.chat,
            &mut self.code,
            &mut self.sidebar,
            &mouse,
        );
        assert!(consumed);
        assert_eq!(event, Some(SidebarEvent::OpenPicker));
        self.sidebar
            .open_picker(PaneKind::Chat, Default::default(), &self.client);
        rpc.drive(async {
            loop {
                self.sidebar.drain_picker_fetch();
                if self.draw().rows.iter().any(|row| row.contains("window-d")) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        for _ in 0..3 {
            assert_eq!(
                self.sidebar
                    .handle_picker_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                None
            );
        }
        let event = self
            .sidebar
            .handle_picker_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Some(SidebarEvent::PickAgent { pane, alias }) = event else {
            panic!("Enter selects a real picker row");
        };
        assert_eq!(pane, PaneKind::Chat);
        assert_eq!(alias, "window-d");
        rpc.drive(self.chat.add_agent_session(&alias)).await;
        assert_eq!(self.chat.current_session_id(), Some("evidence-window-d"));
    }

    async fn mouse(&mut self, kind: MouseEventKind, ordinal: u16) {
        let area = self
            .dock
            .layout
            .sessions
            .expect("draw exposes Sessions geometry");
        let mouse = MouseEvent {
            kind,
            column: area.x + 2,
            row: area.y + 1 + ordinal,
            modifiers: KeyModifiers::NONE,
        };
        let (consumed, event) = route_agent_sidebar_mouse(
            self.mode,
            &mut self.chat,
            &mut self.code,
            &mut self.sidebar,
            &mouse,
        );
        assert!(consumed, "real sidebar receives the gesture");
        if let Some(event) = event {
            match event {
                SidebarEvent::FocusSession {
                    pane: PaneKind::Acp,
                    session_id,
                } => {
                    assert!(self.code.focus_session(&session_id).await);
                    self.mode = Mode::Acp;
                }
                SidebarEvent::FocusSession {
                    pane: PaneKind::Chat,
                    session_id,
                } => {
                    assert!(self.chat.focus_session(&session_id).await);
                    self.mode = Mode::Chat;
                }
                other => panic!("unexpected mouse action: {other:?}"),
            }
        }
    }
}

// Only this buffer printer writes stdout. Production logging is unchanged.
#[allow(clippy::print_stdout)]
fn print_frame(slug: &str, frame: &EvidenceFrame) {
    println!("=== EVIDENCE BEGIN {slug} ({WIDTH}x{HEIGHT}) ===");
    for row in &frame.rows {
        println!("{}", row.trim_end());
    }
    println!("=== EVIDENCE END {slug} ===");
}

fn assert_sidebar(frame: &EvidenceFrame, aliases: &[&str]) {
    let visible: Vec<_> = frame
        .sidebar_rows
        .iter()
        .filter_map(|row| aliases.iter().find(|alias| row.contains(**alias)).copied())
        .collect();
    assert_eq!(visible, aliases, "rendered sidebar order");
    assert!(frame.rows.iter().any(|row| row.contains("Connected")));
}

fn store(dir: &std::path::Path) -> Store {
    Store::for_launch(dir, TARGET, Transport::Local)
        .unwrap()
        .expect("free local slot")
}

async fn three_sessions(dir: &std::path::Path, rpc: &mut EvidenceRpc) -> EvidenceWindow {
    // Existing daemon sessions enter through the same resume handoff as startup.
    let mut window = EvidenceWindow::init(
        dir,
        rpc,
        None,
        vec![ResumeEntry::saved(
            "evidence-chat-c".into(),
            "chat-c".into(),
            true,
        )],
        vec![
            ResumeEntry::saved("evidence-code-a".into(), "code-a".into(), true),
            ResumeEntry::saved("evidence-code-b".into(), "code-b".into(), false),
        ],
    )
    .await;
    window.draw();
    window.click_mode(Mode::Acp);
    window.draw();
    window
        .mouse(MouseEventKind::Down(MouseButton::Left), 1)
        .await;
    window.mouse(MouseEventKind::Up(MouseButton::Left), 1).await;
    assert_eq!(window.code.current_session_id(), Some("evidence-code-b"));
    window
}

async fn dragged_sessions(dir: &std::path::Path, rpc: &mut EvidenceRpc) -> EvidenceWindow {
    let mut window = three_sessions(dir, rpc).await;
    window.draw();
    // Drag Chat between Code A and Code B, then focus Code B again.
    window
        .mouse(MouseEventKind::Down(MouseButton::Left), 2)
        .await;
    window.draw();
    window
        .mouse(MouseEventKind::Drag(MouseButton::Left), 1)
        .await;
    window.mouse(MouseEventKind::Up(MouseButton::Left), 1).await;
    window.draw();
    window
        .mouse(MouseEventKind::Down(MouseButton::Left), 2)
        .await;
    window.mouse(MouseEventKind::Up(MouseButton::Left), 2).await;
    window
}

// Preserve a caller's config override as well as serializing with sibling tests.
struct ConfigDirGuard(Option<std::ffi::OsString>);

impl ConfigDirGuard {
    fn set(name: &'static str, value: &str) -> Self {
        assert_eq!(name, "ZEROCLAW_CONFIG_DIR");
        let previous = std::env::var_os(name);
        // SAFETY: every evidence test holds the shared environment lock.
        unsafe { std::env::set_var(name, value) };
        Self(previous)
    }
}

impl Drop for ConfigDirGuard {
    fn drop(&mut self) {
        // SAFETY: the test still holds the shared environment lock.
        unsafe {
            match &self.0 {
                Some(value) => std::env::set_var("ZEROCLAW_CONFIG_DIR", value),
                None => std::env::remove_var("ZEROCLAW_CONFIG_DIR"),
            }
        }
    }
}

async fn env_lock() -> (
    tokio::sync::MutexGuard<'static, ()>,
    std::sync::MutexGuard<'static, ()>,
) {
    static ASYNC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let guard = ASYNC_LOCK.lock().await;
    (guard, crate::test_support::env_test_lock())
}

#[tokio::test]
async fn evidence_frame_three_sessions_with_second_code_focused() {
    let _lock = env_lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _config = ConfigDirGuard::set("ZEROCLAW_CONFIG_DIR", dir.path().to_str().unwrap());
    let mut rpc = EvidenceRpc::new();
    let mut window = three_sessions(dir.path(), &mut rpc).await;
    let frame = window.draw();
    assert_sidebar(&frame, &["code-a", "code-b", "chat-c"]);
    assert!(
        frame
            .rows
            .iter()
            .any(|row| row.contains("Saved transcript for code-b."))
    );
    print_frame("three-sessions-second-code-focused", &frame);
}

#[tokio::test]
async fn evidence_frame_dragged_cross_pane_order_before_restart() {
    let _lock = env_lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _config = ConfigDirGuard::set("ZEROCLAW_CONFIG_DIR", dir.path().to_str().unwrap());
    let mut rpc = EvidenceRpc::new();
    let mut window = dragged_sessions(dir.path(), &mut rpc).await;
    let frame = window.draw();
    assert_sidebar(&frame, &["code-a", "chat-c", "code-b"]);
    assert_eq!(window.code.current_session_id(), Some("evidence-code-b"));
    assert!(
        frame
            .rows
            .iter()
            .any(|row| row.contains("Saved transcript for code-b."))
    );
    print_frame("dragged-cross-pane-order-before-restart", &frame);
}

#[tokio::test]
async fn evidence_frame_restored_cross_pane_order_and_focus() {
    let _lock = env_lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _config = ConfigDirGuard::set("ZEROCLAW_CONFIG_DIR", dir.path().to_str().unwrap());
    let mut rpc = EvidenceRpc::new();
    let mut before = dragged_sessions(dir.path(), &mut rpc).await;
    assert_sidebar(&before.draw(), &["code-a", "chat-c", "code-b"]);
    let saved = before.snapshot();
    let mut first_store = store(dir.path());
    first_store.save_if_changed(&saved).unwrap();
    drop(before);
    drop(first_store);

    // New client, panes and sidebar, using the actual disk and launch path.
    let mut rpc = EvidenceRpc::new();
    let mut reopened_store = store(dir.path());
    let client = rpc.client.clone();
    let outcome = rpc.drive(reopened_store.start(&client, true)).await;
    assert!(outcome.publish_error.is_none());
    assert_eq!(outcome.snapshot.as_ref(), Some(&saved));
    let mut restored =
        EvidenceWindow::restore(dir.path(), &mut rpc, outcome.snapshot.as_ref()).await;
    assert_eq!(restored.mode, Mode::Acp);
    assert_eq!(restored.code.current_session_id(), Some("evidence-code-b"));
    assert_eq!(rpc.attached.len(), 3);
    let frame = restored.draw();
    assert_sidebar(&frame, &["code-a", "chat-c", "code-b"]);
    assert!(
        frame
            .rows
            .iter()
            .any(|row| row.contains("Saved transcript for code-b."))
    );
    assert_eq!(restored.snapshot(), saved);
    print_frame("restored-cross-pane-order-and-focus", &frame);

    // The same restored Chat transcript is visible when its real row is clicked.
    restored
        .mouse(MouseEventKind::Down(MouseButton::Left), 1)
        .await;
    restored
        .mouse(MouseEventKind::Up(MouseButton::Left), 1)
        .await;
    let frame = restored.draw();
    assert!(
        frame
            .rows
            .iter()
            .any(|row| row.contains("Saved transcript for chat-c."))
    );
    print_frame("restored-chat-transcript", &frame);
}

#[tokio::test]
async fn evidence_frame_fresh_starts_with_empty_sessions() {
    let _lock = env_lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _config = ConfigDirGuard::set("ZEROCLAW_CONFIG_DIR", dir.path().to_str().unwrap());
    let mut rpc = EvidenceRpc::new();
    let mut prior = three_sessions(dir.path(), &mut rpc).await;
    prior.draw();
    let mut prior_store = store(dir.path());
    prior_store.save_if_changed(&prior.snapshot()).unwrap();
    drop(prior);
    drop(prior_store);

    let mut fresh_store = store(dir.path());
    let launch = OpenSessionsLaunch::new(true, true);
    assert!(launch.save);
    assert!(!launch.restore);
    let mut rpc = EvidenceRpc::new();
    let client = rpc.client.clone();
    let outcome = rpc.drive(fresh_store.start(&client, launch.restore)).await;
    assert!(outcome.snapshot.is_none());
    assert!(outcome.publish_error.is_none());
    assert_eq!(fresh_store.load(), Some(Snapshot::default()));
    let mut fresh = EvidenceWindow::restore(dir.path(), &mut rpc, outcome.snapshot.as_ref()).await;
    assert!(rpc.attached.is_empty(), "--fresh attaches no saved session");
    assert_eq!(fresh.mode, Mode::Dashboard);
    let frame = fresh.draw();
    assert!(frame.rows.iter().any(|row| row.contains("Overview")));
    print_frame("fresh-initial-dashboard", &frame);

    // Dashboard does not display the Sessions dock. Click Chat to expose the
    // empty dock without creating a session, just as a user can after launch.
    fresh.click_mode(Mode::Chat);
    let frame = fresh.draw();
    assert_sidebar(&frame, &[]);
    assert!(
        frame
            .sidebar_rows
            .iter()
            .any(|row| row.contains("No active agents"))
    );
    assert!(fresh.chat.session_summaries().is_empty());
    assert!(fresh.code.session_summaries().is_empty());
    assert!(rpc.attached.is_empty());
    print_frame("fresh-empty-sessions", &frame);
}

async fn independent_windows(
    dir: &std::path::Path,
) -> (EvidenceWindow, Store, EvidenceWindow, Store, EvidenceRpc) {
    let mut first_rpc = EvidenceRpc::new();
    let mut first = three_sessions(dir, &mut first_rpc).await;
    first.draw();
    let mut first_store = store(dir);
    first_store.save_if_changed(&first.snapshot()).unwrap();

    let mut second_store = store(dir);
    let mut second_rpc = EvidenceRpc::new();
    let client = second_rpc.client.clone();
    let outcome = second_rpc.drive(second_store.start(&client, true)).await;
    assert!(outcome.snapshot.is_none(), "a new slot has no saved list");
    assert!(outcome.publish_error.is_none());
    let second = EvidenceWindow::restore(dir, &mut second_rpc, outcome.snapshot.as_ref()).await;
    assert!(
        second_rpc.attached.is_empty(),
        "new slot does not steal first-window sessions"
    );
    (first, first_store, second, second_store, second_rpc)
}

#[tokio::test]
async fn evidence_frame_independent_second_window() {
    let _lock = env_lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _config = ConfigDirGuard::set("ZEROCLAW_CONFIG_DIR", dir.path().to_str().unwrap());
    let (mut first, mut first_store, mut second, mut second_store, mut rpc) =
        independent_windows(dir.path()).await;
    let first_snapshot = first.snapshot();
    let frame = second.draw();
    assert_eq!(second.mode, Mode::Dashboard);
    assert!(frame.rows.iter().any(|row| row.contains("Overview")));
    print_frame("second-window-initial-dashboard", &frame);

    second.add_second_window_session(&mut rpc).await;
    let frame = second.draw();
    assert_sidebar(&frame, &["window-d"]);
    assert!(
        frame
            .sidebar_rows
            .iter()
            .all(|row| !row.contains("code-a") && !row.contains("chat-c"))
    );
    second_store.save_if_changed(&second.snapshot()).unwrap();
    assert_eq!(first.snapshot(), first_snapshot);
    assert_eq!(first_store.load(), Some(first_snapshot));
    print_frame("second-window-independent-session", &frame);

    let frame = first.draw();
    assert_sidebar(&frame, &["code-a", "code-b", "chat-c"]);
    assert_eq!(first.code.current_session_id(), Some("evidence-code-b"));
    assert!(
        frame
            .rows
            .iter()
            .any(|row| row.contains("Saved transcript for code-b."))
    );
    print_frame("first-window-unchanged", &frame);
}

#[tokio::test]
async fn evidence_frame_reopened_second_window() {
    let _lock = env_lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _config = ConfigDirGuard::set("ZEROCLAW_CONFIG_DIR", dir.path().to_str().unwrap());
    let (mut first, mut first_store, mut second, mut second_store, mut rpc) =
        independent_windows(dir.path()).await;
    let first_snapshot = first.snapshot();
    second.add_second_window_session(&mut rpc).await;
    second.draw();
    let second_snapshot = second.snapshot();
    second_store.save_if_changed(&second_snapshot).unwrap();
    drop(second);
    drop(second_store);
    drop(rpc);

    // The first slot stays locked throughout. Only the second slot is released.
    let mut reopened_store = store(dir.path());
    let mut rpc = EvidenceRpc::new();
    let client = rpc.client.clone();
    let outcome = rpc.drive(reopened_store.start(&client, true)).await;
    assert_eq!(outcome.snapshot.as_ref(), Some(&second_snapshot));
    assert!(outcome.publish_error.is_none());
    let mut reopened =
        EvidenceWindow::restore(dir.path(), &mut rpc, outcome.snapshot.as_ref()).await;
    let frame = reopened.draw();
    assert_sidebar(&frame, &["window-d"]);
    assert_eq!(reopened.mode, Mode::Chat);
    assert_eq!(rpc.attached, ["evidence-window-d"]);
    assert_eq!(reopened.snapshot(), second_snapshot);
    print_frame("reopened-second-window-own-session", &frame);

    assert_eq!(first.snapshot(), first_snapshot);
    assert_eq!(first_store.load(), Some(first_snapshot));
    let frame = first.draw();
    assert_sidebar(&frame, &["code-a", "code-b", "chat-c"]);
    assert_eq!(first.code.current_session_id(), Some("evidence-code-b"));
    print_frame("first-window-after-second-reopens", &frame);
}

#[tokio::test]
async fn evidence_frame_restored_original_order_and_focus() {
    let _lock = env_lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _config = ConfigDirGuard::set("ZEROCLAW_CONFIG_DIR", dir.path().to_str().unwrap());
    let mut rpc = EvidenceRpc::new();
    let mut before = three_sessions(dir.path(), &mut rpc).await;
    before.draw();
    let saved = before.snapshot();
    let mut prior_store = store(dir.path());
    prior_store.save_if_changed(&saved).unwrap();
    drop(before);
    drop(prior_store);

    let mut rpc = EvidenceRpc::new();
    let mut reopened_store = store(dir.path());
    let client = rpc.client.clone();
    let outcome = rpc.drive(reopened_store.start(&client, true)).await;
    assert!(outcome.publish_error.is_none());
    assert_eq!(outcome.snapshot.as_ref(), Some(&saved));
    let mut restored =
        EvidenceWindow::restore(dir.path(), &mut rpc, outcome.snapshot.as_ref()).await;
    assert_eq!(restored.mode, Mode::Acp);
    assert_eq!(restored.code.current_session_id(), Some("evidence-code-b"));
    let frame = restored.draw();
    assert_sidebar(&frame, &["code-a", "code-b", "chat-c"]);
    assert!(
        frame
            .rows
            .iter()
            .any(|row| row.contains("Saved transcript for code-b."))
    );
    assert_eq!(restored.snapshot(), saved);
    print_frame("restored-original-order-and-focus", &frame);

    // Both remaining transcripts must be reachable through their real rows.
    restored
        .mouse(MouseEventKind::Down(MouseButton::Left), 0)
        .await;
    restored
        .mouse(MouseEventKind::Up(MouseButton::Left), 0)
        .await;
    let frame = restored.draw();
    assert!(
        frame
            .rows
            .iter()
            .any(|row| row.contains("Saved transcript for code-a."))
    );
    print_frame("restored-first-code-transcript", &frame);
    restored
        .mouse(MouseEventKind::Down(MouseButton::Left), 2)
        .await;
    restored
        .mouse(MouseEventKind::Up(MouseButton::Left), 2)
        .await;
    let frame = restored.draw();
    assert!(
        frame
            .rows
            .iter()
            .any(|row| row.contains("Saved transcript for chat-c."))
    );
    print_frame("restored-original-chat-transcript", &frame);
}
