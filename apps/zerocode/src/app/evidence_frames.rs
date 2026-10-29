//! Review evidence from real Config input handlers and the production app frame.
//! The RPC peer supplies fixture data; it does not replace any UI renderer.

#[cfg(test)]
#[cfg(test)] // test-only module: lets the keymap guard skip review-evidence key input
use super::*;

use crate::jsonrpc::RpcOutbound;
use crate::keymap::{Chord, RebindableActions, overrides};

struct EvidenceUi {
    dir: tempfile::TempDir,
    config: config_manager::App,
    chat: chat::Chat,
    code: acp::Acp,
    dock: ConversationDock,
    rpc: Arc<RpcClient>,
    peer: tokio::task::JoinHandle<()>,
    requests: Arc<Mutex<Vec<String>>>,
    input: config_manager::Term,
    original: Vec<u8>,
}

impl EvidenceUi {
    async fn new() -> Self {
        // Refuse ambient settings rather than risking private URI/route text in
        // stdout. The parent can run this filter in a clean environment.
        assert!(
            !std::env::vars_os().any(|(name, _)| name.to_string_lossy().starts_with("ZEROCODE_")),
            "run evidence tests without ZEROCODE_* environment overrides"
        );
        let dir = tempfile::tempdir().unwrap();
        crate::i18n::init("en", dir.path());
        let (tx, mut rx) = mpsc::channel::<String>(16);
        let outbound = Arc::new(RpcOutbound::new(tx));
        let responder = Arc::clone(&outbound);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let peer = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let method = request["method"].as_str().unwrap();
                let result = match method {
                    "config/sections" => serde_json::json!({"sections": []}),
                    "config/templates" => serde_json::json!({"templates": []}),
                    "agents/status" => serde_json::json!({"agents": [
                        {"alias": "sample-agent", "enabled": true},
                        {"alias": "disabled-agent", "enabled": false}
                    ]}),
                    "locales/list" => serde_json::json!({"locales": [
                        {"code": "en", "label": "English"}
                    ]}),
                    _ => panic!("unexpected RPC method in settings evidence"),
                };
                recorded.lock().unwrap().push(method.to_string());
                responder.dispatch_response(request["id"].as_str().unwrap(), Some(result), None);
            }
        });
        let rpc = Arc::new(RpcClient::with_rpc(outbound));
        let mut config = config_manager::App::new(Arc::clone(&rpc), dir.path());
        tokio::time::timeout(Duration::from_secs(2), config.init())
            .await
            .expect("initial Config RPC replies")
            .unwrap();
        let original = std::fs::read(config::config_path(dir.path())).unwrap();
        let input = ratatui::Terminal::with_options(
            crate::terminal_backend::WideCellCleanupBackend::new(std::io::stdout()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 100, 30)),
            },
        )
        .unwrap();
        Self {
            chat: chat::Chat::new(Arc::clone(&rpc), chat::PaneKind::Chat),
            code: acp::Acp::new(Arc::clone(&rpc)),
            dock: ConversationDock::from_config_dir(dir.path()),
            dir,
            config,
            rpc,
            peer,
            requests,
            input,
            original,
        }
    }

    async fn press(&mut self, code: KeyCode) {
        self.key(KeyEvent::new(code, KeyModifiers::NONE)).await;
    }

    async fn key(&mut self, key: KeyEvent) {
        // Use the same query/global arbitration as the event loop before
        // forwarding the event to Config's production handler.
        let global = binding_query_global_action(
            GlobalAction::from_chord(&key),
            self.config.binding_query_claims_key(&key),
        );
        if self.config.binding_query_claims_key(&key) {
            assert!(global.is_none(), "query text must own printable globals");
        }
        let quit = tokio::time::timeout(
            Duration::from_secs(2),
            self.config.handle_key(key, &mut self.input),
        )
        .await
        .expect("Config key and any lazy RPC reply must complete")
        .unwrap();
        assert!(!quit);
    }

    async fn type_text(&mut self, text: &str) {
        for character in text.chars() {
            self.press(KeyCode::Char(character)).await;
        }
    }

    async fn open_page(&mut self, page: usize, detail: bool) {
        self.press(KeyCode::Tab).await;
        for _ in 0..page {
            self.press(KeyCode::Down).await;
        }
        if detail {
            self.press(KeyCode::Enter).await;
        }
    }

    fn rows(&mut self, width: u16, height: u16) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let Self {
            config,
            chat,
            code,
            dock,
            rpc,
            ..
        } = self;
        terminal
            .draw(|frame| {
                draw_app_frame(frame, Mode::Config, chat, code, |frame, chunks, _, _| {
                    draw_mode_bar(frame, chunks[0], Mode::Config, None);
                    let layout = dock.layout(chunks[1], Mode::Config, false);
                    dock.draw_shell(frame);
                    config.draw_into(frame, layout.conversation);
                    dock.draw_resize_handles(frame);
                    draw_status_bar(
                        frame,
                        chunks[2],
                        &ConnectionState::Connected,
                        rpc.tui_id(),
                        CtxBar::new(None, None, None, None),
                        false,
                        false,
                    );
                });
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
                row.trim_end().to_string()
            })
            .collect()
    }

    #[allow(clippy::print_stdout)]
    fn capture(&mut self, slug: &str, width: u16, height: u16, body: &[&str], footer: &[&str]) {
        let rows = self.rows(width, height);
        let text = rows.join("\n");
        for expected in body {
            assert!(
                text.contains(expected),
                "{slug}: missing {expected:?}\n{text}"
            );
        }
        // The Config footer must occupy the last content row, immediately
        // above the app status bar. Finding the same text in a list is weaker.
        for expected in footer {
            assert!(
                rows[usize::from(height - 2)].contains(expected),
                "{slug}: footer missing {expected:?}\n{text}"
            );
        }
        assert!(rows[0].contains("Config"), "{slug}: real app mode bar");
        assert!(rows[usize::from(height - 1)].contains("Connected"));
        println!("=== EVIDENCE BEGIN {slug} ({width}x{height}) ===");
        for row in rows {
            println!("{row}");
        }
        println!("=== EVIDENCE END {slug} ===");
    }

    fn assert_unchanged(&self) {
        assert_eq!(
            std::fs::read(config::config_path(self.dir.path())).unwrap(),
            self.original,
            "navigation/search/capture cancellation must not save settings"
        );
    }
}

impl Drop for EvidenceUi {
    fn drop(&mut self) {
        self.rpc.shutdown();
        self.peer.abort();
    }
}

struct ResetKeymap;

impl Drop for ResetKeymap {
    fn drop(&mut self) {
        overrides::reset();
    }
}

#[derive(Clone, Copy)]
enum State {
    BindingsSections,
    BindingsIdle,
    QueryEditing,
    QueryNoMatches,
    QueryConfirmed,
    QueryCleared,
    QueryByKey,
    FilteredRebind,
    FilteredReset,
    Capture,
    CaptureOverride,
    AgentPicking,
    AgentAssigning,
    ConnectionBool,
    ConnectionUri,
    ConnectionRoutes,
    NarrowSections,
    NarrowBindings80,
    NarrowBindings60,
    Theme,
    Presets,
    Locale,
    Tracker,
    MouseDeparture,
    KeyboardDeparture,
}

/// `i18n::init` caches both locale and config directory in process-wide
/// OnceLocks. A mutex cannot restore them for sibling tests after our temporary
/// directory disappears, so initialize the real UI in a fresh test process.
#[allow(clippy::print_stdout)]
fn run_in_isolated_process() -> bool {
    const CHILD: &str = "ZEROCLAW_EVIDENCE_FRAME_CHILD";
    let thread = std::thread::current();
    let test_name = thread
        .name()
        .expect("libtest names the current test thread");
    let child = {
        let _environment = crate::test_support::env_test_lock();
        std::env::var_os(CHILD)
    };
    if child.as_deref() == Some(std::ffi::OsStr::new(test_name)) {
        return false;
    }

    let executable = std::env::current_exe().expect("current test executable");
    let output = {
        // Match the lock taken by synchronous env tests and the async Chat
        // helper while the child inherits its environment. Release it before
        // reporting a child failure, so no parent shared lock is poisoned.
        let _environment = crate::test_support::env_test_lock();
        std::process::Command::new(executable)
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD, test_name)
            .output()
    }
    .expect("run isolated evidence test");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    assert!(
        output.status.success(),
        "isolated evidence test {test_name} failed: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("=== EVIDENCE BEGIN ") && stdout.contains("=== EVIDENCE END "),
        "isolated test {test_name} must execute and emit a real frame"
    );
    true
}

// These current-thread tests serialize process-global environment/keymap/theme
// reads in the child through the existing production-boundary test guards.
#[allow(clippy::await_holding_lock)]
async fn render_state(state: State) {
    if run_in_isolated_process() {
        return;
    }
    let _environment = crate::test_support::env_test_lock();
    let _keys = overrides::TEST_GUARD
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    overrides::reset();
    let _reset = ResetKeymap;
    let _theme = theme::set_active_for_test(theme::default_theme());
    if matches!(state, State::CaptureOverride) {
        overrides::set_row("capture", "cancel", vec![Chord::key(KeyCode::F(8))]);
    }
    let mut ui = EvidenceUi::new().await;
    match state {
        State::BindingsSections | State::NarrowSections => {
            ui.open_page(3, false).await;
            let width = if matches!(state, State::NarrowSections) {
                60
            } else {
                100
            };
            let slug = if width == 60 {
                "keybindings-sections-narrow"
            } else {
                "keybindings-sections"
            };
            ui.capture(
                slug,
                width,
                30,
                &["Search:"],
                &["ZeroCode Sections/Keybindings", "Open section"],
            );
        }
        State::BindingsIdle | State::NarrowBindings80 | State::NarrowBindings60 => {
            ui.open_page(3, true).await;
            let width = match state {
                State::NarrowBindings80 => 80,
                State::NarrowBindings60 => 60,
                _ => 100,
            };
            let slug = match width {
                80 => "keybindings-detail-80",
                60 => "keybindings-detail-60",
                _ => "keybindings-detail-idle",
            };
            let quit_key = GlobalAction::Quit.resolved()[0].display();
            ui.capture(
                slug,
                width,
                30,
                &["Search:", &quit_key],
                &["ZeroCode Detail/Keybindings", "rebind", "back"],
            );
        }
        State::QueryEditing
        | State::QueryNoMatches
        | State::QueryConfirmed
        | State::QueryCleared
        | State::QueryByKey => {
            ui.open_page(3, true).await;
            ui.press(KeyCode::Char('/')).await;
            let query = match state {
                State::QueryNoMatches => "no-such-action-11505",
                State::QueryByKey => "Ctrl+z",
                _ => "clear input",
            };
            ui.type_text(query).await;
            assert!(ui.config.wants_text_input());
            match state {
                State::QueryEditing => ui.capture(
                    "keybindings-query-editing",
                    120,
                    30,
                    &["Search: clear input", "█"],
                    &["Detail/Keybindings", "browse matches", "clear filter"],
                ),
                State::QueryNoMatches => {
                    ui.press(KeyCode::Enter).await;
                    assert!(!ui.config.wants_text_input());
                    let rows = ui.rows(100, 30);
                    assert!(!rows[28].contains("rebind"));
                    assert!(!rows[28].contains("reset"));
                    ui.capture(
                        "keybindings-no-matches",
                        100,
                        30,
                        &["No matching actions", "(0/"],
                        &["Detail/Keybindings", "search", "clear filter"],
                    );
                }
                State::QueryConfirmed => {
                    ui.press(KeyCode::Enter).await;
                    assert!(!ui.config.wants_text_input());
                    ui.capture(
                        "keybindings-query-confirmed",
                        100,
                        30,
                        &["Search: clear input"],
                        &["Detail/Keybindings", "rebind", "clear filter"],
                    );
                }
                State::QueryByKey => {
                    ui.press(KeyCode::Enter).await;
                    assert!(!ui.config.wants_text_input());
                    ui.capture(
                        "keybindings-query-by-effective-key",
                        120,
                        30,
                        &["Search: Ctrl+z", "input_bar.undo"],
                        &["Detail/Keybindings", "rebind", "clear filter"],
                    );
                }
                State::QueryCleared => {
                    ui.press(KeyCode::Esc).await;
                    assert!(!ui.config.wants_text_input());
                    let rows = ui.rows(100, 30);
                    assert!(!rows.iter().any(|row| row.contains("Search: clear input")));
                    assert!(rows.iter().any(|row| row.contains("› global.quit")));
                    ui.capture(
                        "keybindings-query-cleared",
                        100,
                        30,
                        &["Search:"],
                        &["Detail/Keybindings", "rebind", "back"],
                    );
                }
                _ => unreachable!(),
            }
        }
        State::FilteredRebind | State::FilteredReset => {
            use crate::keymap::InputBarAction;
            ui.open_page(3, true).await;
            ui.press(KeyCode::Char('/')).await;
            ui.type_text("Ctrl+z").await;
            ui.press(KeyCode::Enter).await;
            ui.press(KeyCode::Enter).await;
            assert!(
                ui.rows(120, 30)
                    .join("\n")
                    .contains("Rebind: input_bar.undo")
            );
            ui.press(KeyCode::F(5)).await;
            let saved = config::load_persisted(ui.dir.path())
                .unwrap()
                .resolve_keybindings()
                .unwrap();
            assert_eq!(saved["input_bar"]["undo"], vec![Chord::key(KeyCode::F(5))]);
            if matches!(state, State::FilteredReset) {
                ui.press(KeyCode::Char('d')).await;
                let reset = config::load_persisted(ui.dir.path())
                    .unwrap()
                    .resolve_keybindings()
                    .unwrap();
                assert_eq!(reset["input_bar"]["undo"], InputBarAction::Undo.defaults());
                assert!(!reset.contains_key("global"));
                ui.capture(
                    "keybindings-filtered-reset-target",
                    120,
                    30,
                    &["Reset input_bar.undo", "› input_bar.undo"],
                    &["Detail/Keybindings", "rebind", "back"],
                );
            } else {
                assert!(!saved.contains_key("global"));
                ui.capture(
                    "keybindings-filtered-rebind-target",
                    120,
                    30,
                    &["input_bar.undo -> F5", "› input_bar.undo"],
                    &["Detail/Keybindings", "rebind", "back"],
                );
            }
            // Successful capture clears the key query while keeping the edited
            // action selected, so the next reset cannot silently target Quit.
            assert!(
                !ui.rows(120, 30)
                    .iter()
                    .any(|row| row.contains("Search: Ctrl+z"))
            );
        }
        State::Capture | State::CaptureOverride => {
            ui.open_page(3, true).await;
            ui.press(KeyCode::Char('/')).await;
            ui.type_text("clear input").await;
            ui.press(KeyCode::Enter).await;
            ui.press(KeyCode::Enter).await;
            let custom = matches!(state, State::CaptureOverride);
            let cancel = if custom { "F8" } else { "Esc" };
            ui.capture(
                if custom {
                    "keybindings-capture-effective-cancel"
                } else {
                    "keybindings-capture"
                },
                120,
                30,
                &[
                    "Assign Key",
                    "Rebind: input_bar.clear_input",
                    &format!("{cancel} to cancel"),
                ],
                &[&format!("{cancel}=Cancel capture")],
            );
            ui.press(if custom { KeyCode::F(8) } else { KeyCode::Esc })
                .await;
            assert!(!ui.rows(120, 30).join("\n").contains("Assign Key"));
        }
        State::AgentPicking | State::AgentAssigning => {
            ui.open_page(1, true).await;
            assert!(
                ui.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|method| method == "agents/status")
            );
            if matches!(state, State::AgentAssigning) {
                ui.press(KeyCode::Enter).await;
                ui.capture(
                    "agent-theme-assigning",
                    120,
                    30,
                    &["Theme → sample-agent"],
                    &["Detail/Agent Themes", "assign theme", "back"],
                );
            } else {
                ui.capture(
                    "agent-theme-picking",
                    120,
                    30,
                    &["sample-agent"],
                    &[
                        "Detail/Agent Themes",
                        "choose theme",
                        "back",
                        "clear override",
                    ],
                );
                assert!(!ui.rows(120, 30).join("\n").contains("disabled-agent"));
            }
        }
        State::ConnectionBool | State::ConnectionUri | State::ConnectionRoutes => {
            ui.open_page(5, true).await;
            match state {
                State::ConnectionBool => {
                    ui.press(KeyCode::Down).await;
                    ui.press(KeyCode::Enter).await;
                    assert!(
                        config::load_persisted(ui.dir.path())
                            .unwrap()
                            .connection
                            .wss
                            .tls
                            .skip_verify
                    );
                    ui.capture(
                        "connection-bool-toggle",
                        100,
                        30,
                        &["› TLS skip verify", "true", "Saved"],
                        &["Detail/Connection", "edit/toggle", "back"],
                    );
                }
                State::ConnectionUri => {
                    ui.press(KeyCode::Enter).await;
                    ui.type_text("wss://example.invalid/rpc").await;
                    ui.capture(
                        "connection-uri-editing",
                        120,
                        30,
                        &["WSS URI", "Pending edits", "wss://example.invalid/rpc"],
                        &["Detail/Connection", "save", "cancel"],
                    );
                }
                State::ConnectionRoutes => {
                    ui.press(KeyCode::Down).await;
                    ui.press(KeyCode::Down).await;
                    ui.press(KeyCode::Enter).await;
                    ui.type_text("example.invalid:443").await;
                    ui.press(KeyCode::Enter).await;
                    ui.type_text("localhost:42617").await;
                    ui.capture(
                        "connection-routes-editing",
                        120,
                        30,
                        &[
                            "Skip-verify routes",
                            "Pending edits",
                            "example.invalid:443",
                            "localhost:42617",
                        ],
                        &["Detail/Connection", "new line", "cancel", "save"],
                    );
                }
                _ => unreachable!(),
            }
        }
        State::Theme | State::Presets | State::Locale | State::Tracker => {
            let (page, slug, panel, context, action) = match state {
                State::Theme => (0, "theme-detail", "Theme", "Detail/Theme", "apply theme"),
                State::Presets => (
                    2,
                    "presets-detail",
                    "Keybinding Presets",
                    "Detail/Presets",
                    "apply preset",
                ),
                State::Locale => (
                    4,
                    "locale-detail",
                    "English",
                    "Detail/Locale",
                    "select/download",
                ),
                _ => (
                    6,
                    "todo-tracker-detail",
                    "Todo tracker ([todotracker])",
                    "Detail/Todo tracker",
                    "toggle",
                ),
            };
            ui.open_page(page, true).await;
            ui.capture(slug, 120, 30, &[panel], &[context, action, "back"]);
        }
        State::MouseDeparture | State::KeyboardDeparture => {
            ui.open_page(3, true).await;
            ui.press(KeyCode::Char('/')).await;
            ui.type_text("clear input").await;
            if matches!(state, State::MouseDeparture) {
                // Restrict the search to the left section list. The right
                // panel title also says Keybindings, above the Theme row.
                let rows = ui.rows(120, 30);
                let row = rows
                    .iter()
                    .position(|row| {
                        row.chars()
                            .take(22)
                            .collect::<String>()
                            .contains("Keybindings")
                    })
                    .expect("Keybindings row in the real left section list");
                ui.config
                    .handle_mouse(
                        MouseEvent {
                            kind: MouseEventKind::Down(MouseButton::Left),
                            column: 3,
                            row: row as u16,
                            modifiers: KeyModifiers::NONE,
                        },
                        Rect::new(0, 0, 120, 30),
                        &mut ui.input,
                    )
                    .await
                    .unwrap();
            } else {
                ui.press(KeyCode::Tab).await;
                ui.press(KeyCode::Tab).await;
            }
            assert!(!ui.config.wants_text_input());
            let context = if matches!(state, State::MouseDeparture) {
                "Sections/Keybindings"
            } else {
                "Detail/Keybindings"
            };
            ui.capture(
                if matches!(state, State::MouseDeparture) {
                    "keybindings-mouse-departure"
                } else {
                    "keybindings-keyboard-departure"
                },
                120,
                30,
                &["Search: clear input"],
                &[context],
            );
        }
    }
    if !matches!(
        state,
        State::ConnectionBool | State::FilteredRebind | State::FilteredReset
    ) {
        ui.assert_unchanged();
    }
}

#[tokio::test]
async fn evidence_frame_keybindings_sections() {
    render_state(State::BindingsSections).await;
}
#[tokio::test]
async fn evidence_frame_keybindings_detail_idle() {
    render_state(State::BindingsIdle).await;
}
#[tokio::test]
async fn evidence_frame_keybindings_query_editing() {
    render_state(State::QueryEditing).await;
}
#[tokio::test]
async fn evidence_frame_keybindings_no_matches() {
    render_state(State::QueryNoMatches).await;
}
#[tokio::test]
async fn evidence_frame_keybindings_query_confirmed() {
    render_state(State::QueryConfirmed).await;
}
#[tokio::test]
async fn evidence_frame_keybindings_query_cleared() {
    render_state(State::QueryCleared).await;
}
#[tokio::test]
async fn evidence_frame_keybindings_query_by_key() {
    render_state(State::QueryByKey).await;
}
#[tokio::test]
async fn evidence_frame_filtered_rebind_target() {
    render_state(State::FilteredRebind).await;
}
#[tokio::test]
async fn evidence_frame_filtered_reset_target() {
    render_state(State::FilteredReset).await;
}
#[tokio::test]
async fn evidence_frame_capture_modal() {
    render_state(State::Capture).await;
}
#[tokio::test]
async fn evidence_frame_capture_effective_cancel() {
    render_state(State::CaptureOverride).await;
}
#[tokio::test]
async fn evidence_frame_agent_theme_picking() {
    render_state(State::AgentPicking).await;
}
#[tokio::test]
async fn evidence_frame_agent_theme_assigning() {
    render_state(State::AgentAssigning).await;
}
#[tokio::test]
async fn evidence_frame_connection_bool() {
    render_state(State::ConnectionBool).await;
}
#[tokio::test]
async fn evidence_frame_connection_uri() {
    render_state(State::ConnectionUri).await;
}
#[tokio::test]
async fn evidence_frame_connection_routes() {
    render_state(State::ConnectionRoutes).await;
}
#[tokio::test]
async fn evidence_frame_narrow_sections_footer() {
    render_state(State::NarrowSections).await;
}
#[tokio::test]
async fn evidence_frame_narrow_bindings_80() {
    render_state(State::NarrowBindings80).await;
}
#[tokio::test]
async fn evidence_frame_narrow_bindings_60() {
    render_state(State::NarrowBindings60).await;
}
#[tokio::test]
async fn evidence_frame_theme_detail() {
    render_state(State::Theme).await;
}
#[tokio::test]
async fn evidence_frame_presets_detail() {
    render_state(State::Presets).await;
}
#[tokio::test]
async fn evidence_frame_locale_detail() {
    render_state(State::Locale).await;
}
#[tokio::test]
async fn evidence_frame_todo_tracker_detail() {
    render_state(State::Tracker).await;
}
#[tokio::test]
async fn evidence_frame_query_mouse_departure() {
    render_state(State::MouseDeparture).await;
}
#[tokio::test]
async fn evidence_frame_query_keyboard_departure() {
    render_state(State::KeyboardDeparture).await;
}
