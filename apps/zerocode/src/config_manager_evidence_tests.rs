//! Review evidence from real Config input/RPC handling and app-frame rendering.
//! Fixtures contain synthetic names only. Run with --nocapture --test-threads=1.

use super::*;
use crate::jsonrpc::{JsonRpcError, RpcOutbound};
use crate::wire::AliasSource;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::mpsc;

struct EvidenceApp {
    manager: App,
    term: Term,
    requests: Arc<Mutex<Vec<Value>>>,
    responder: tokio::task::JoinHandle<()>,
    _config_dir: tempfile::TempDir,
    _keymap_guard: MutexGuard<'static, ()>,
}

impl Drop for EvidenceApp {
    fn drop(&mut self) {
        self.responder.abort();
        crate::keymap::overrides::reset();
    }
}

impl EvidenceApp {
    // Load section metadata and values through the real RPC client, then enter
    // the section, alias, and field with the same key handler used by app::run.
    async fn open(
        path: &str,
        source: AliasSource,
        current: &[&str],
        aliases: Option<Vec<&str>>,
    ) -> Self {
        let keymap_guard = crate::keymap::overrides::TEST_GUARD
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::keymap::overrides::reset();
        let config_dir = tempfile::tempdir().unwrap();
        crate::i18n::init("en", config_dir.path());
        let (tx, mut rx) = mpsc::channel::<String>(32);
        let outbound = Arc::new(RpcOutbound::new(tx));
        let rpc = Arc::new(RpcClient::with_rpc(outbound.clone()));
        let manager = App::new(rpc, config_dir.path());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let path = path.to_string();
        let mut parts = path.split('.');
        let section_key = parts.next().unwrap().to_string();
        let alias = parts.next().unwrap().to_string();
        let prefix = format!("{section_key}.{alias}");
        let section = json!({
            "key": section_key,
            "label": section_key,
            "help": "",
            "completed": false,
            "group": "",
            "group_key": "",
            "shape": SectionShape::OneTierAliasMap,
            "cost_category": "",
        });
        let mut entry = ConfigFieldEntry {
            path: path.clone(),
            category: String::new(),
            kind: PropKind::StringArray,
            type_hint: String::new(),
            value: Some(json!(toml::Value::try_from(current).unwrap().to_string())),
            populated: true,
            is_secret: false,
            is_env_overridden: false,
            enum_variants: Vec::new(),
            description: String::new(),
            section: None,
            tab: if section_key == "agents" {
                ConfigTab::Bundles
            } else {
                ConfigTab::None
            },
            alias_source: Some(source),
            setup: None,
        };
        let aliases =
            aliases.map(|names| names.into_iter().map(str::to_string).collect::<Vec<_>>());
        let responder = tokio::spawn(async move {
            while let Some(raw) = rx.recv().await {
                let request: Value = serde_json::from_str(&raw).unwrap();
                let id = request["id"].as_str().unwrap();
                recorded.lock().unwrap().push(request.clone());
                let result = match request["method"].as_str().unwrap() {
                    "config/sections" => json!({"sections": [section]}),
                    "config/templates" => json!({"templates": []}),
                    "config/map-keys" => {
                        assert_eq!(request["params"]["path"], section_key);
                        json!({"keys": [alias]})
                    }
                    "config/list" => {
                        if request["params"]["prefix"] == prefix {
                            json!({"entries": [entry]})
                        } else {
                            assert_eq!(request["params"]["prefix"], format!("{prefix}.enabled"));
                            json!({"entries": []})
                        }
                    }
                    "personality/list" => json!({"files": [], "max_chars": 20000}),
                    "config/resolve-alias-source" => {
                        assert_eq!(
                            request["params"]["source"],
                            serde_json::to_value(source).unwrap()
                        );
                        let Some(values) = &aliases else {
                            outbound.dispatch_response(
                                id,
                                None,
                                Some(JsonRpcError {
                                    code: -32601,
                                    message: "Synthetic lookup unavailable".into(),
                                    data: None,
                                }),
                            );
                            continue;
                        };
                        json!({"values": values})
                    }
                    "config/set" => {
                        assert_eq!(request["params"]["prop"], path);
                        assert!(request["params"]["value"].is_array());
                        // Echo the daemon's canonical TOML display on readback.
                        entry.value = Some(json!(
                            toml::Value::try_from(&request["params"]["value"])
                                .unwrap()
                                .to_string()
                        ));
                        json!({"revision": {"epoch": "evidence", "sequence": 1}})
                    }
                    other => panic!("unexpected evidence RPC method: {other}"),
                };
                outbound.dispatch_response(id, Some(result), None);
            }
        });
        let term = Terminal::with_options(
            WideCellCleanupBackend::new(io::stdout()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 120, 40)),
            },
        )
        .unwrap();
        let mut app = Self {
            manager,
            term,
            requests,
            responder,
            _config_dir: config_dir,
            _keymap_guard: keymap_guard,
        };
        tokio::time::timeout(Duration::from_secs(2), app.manager.init())
            .await
            .expect("initial Config RPCs complete")
            .unwrap();
        // Section focus -> alias list -> field list -> picker/text editor.
        app.key(KeyCode::Enter).await;
        app.key(KeyCode::Enter).await;
        app.key(KeyCode::Enter).await;
        assert!(matches!(app.manager.screen, Screen::FieldEdit { .. }));
        app
    }

    async fn skills(current: &[&str]) -> Self {
        Self::open(
            "agents.review-agent.skill_bundles",
            AliasSource::SkillBundles,
            current,
            Some(vec!["alpha", "beta", "gamma"]),
        )
        .await
    }

    async fn event(&mut self, event: KeyEvent) {
        tokio::time::timeout(
            Duration::from_secs(2),
            self.manager.handle_key(event, &mut self.term),
        )
        .await
        .expect("Config input completes")
        .unwrap();
    }

    async fn key(&mut self, code: KeyCode) {
        self.event(KeyEvent::new(code, KeyModifiers::NONE)).await;
    }

    async fn save(&mut self) {
        self.event(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        assert!(matches!(self.manager.screen, Screen::FieldList { .. }));
    }

    async fn search(&mut self, query: &str) {
        self.key(KeyCode::Char('/')).await;
        for c in query.chars() {
            self.key(KeyCode::Char(c)).await;
        }
    }

    fn writes(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "config/set")
            .map(|request| request["params"]["value"].clone())
            .collect()
    }

    fn rows(&mut self, width: u16, height: u16, help: bool) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let rpc = self.manager.rpc.clone();
        terminal
            .draw(|frame| {
                crate::app::draw_config_evidence_frame(frame, &mut self.manager, rpc, help);
            })
            .unwrap();
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

    fn frame(&mut self, slug: &str, expected: &[&str]) {
        self.frame_at(slug, 120, 40, false, expected);
    }

    // Only the evidence-output function may print. No production lint changes.
    #[allow(clippy::print_stdout, clippy::disallowed_macros)]
    fn frame_at(&mut self, slug: &str, width: u16, height: u16, help: bool, expected: &[&str]) {
        let rows = self.rows(width, height, help);
        let text = rows.join("\n");
        for needle in expected {
            assert!(text.contains(needle), "{slug}: missing {needle:?}\n{text}");
        }
        println!("=== EVIDENCE BEGIN {slug} ({width}x{height}) ===");
        for row in rows {
            println!("{row}");
        }
        println!("=== EVIDENCE END {slug} ===");
    }
}

#[tokio::test]
async fn evidence_frame_mixed_selection_and_footer() {
    let mut app = EvidenceApp::skills(&[]).await;
    app.key(KeyCode::Enter).await;
    assert!(app.writes().is_empty());
    app.frame(
        "mixed-selection-footer",
        &[
            "skill_bundles",
            "[x] alpha",
            "[ ] beta",
            "Toggle selection",
            "save",
            "cancel",
            "Pending edits",
        ],
    );
}

#[tokio::test]
async fn evidence_frame_filtered_keyboard_selection() {
    let mut app = EvidenceApp::skills(&["alpha"]).await;
    app.search("beta").await;
    app.key(KeyCode::Enter).await;
    assert_eq!(
        app.manager.detail_filter().map(String::as_str),
        Some("beta")
    );
    assert!(app.writes().is_empty());
    let rows = app.rows(120, 40, false).join("\n");
    assert!(!rows.contains("[x] alpha"));
    app.frame(
        "filtered-keyboard-selection",
        &["beta", "[x] beta", "clear filter", "Pending edits"],
    );
}

#[tokio::test]
async fn evidence_frame_filtered_mouse_selection() {
    let mut app = EvidenceApp::skills(&["alpha"]).await;
    app.search("beta").await;
    app.rows(120, 40, false);
    let area = app.manager.last_main_area;
    let click = MouseEvent {
        kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: area.x + 3,
        row: area.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    for _ in 0..2 {
        tokio::time::timeout(
            Duration::from_secs(2),
            app.manager
                .handle_mouse(click, Rect::new(0, 1, 120, 38), &mut app.term),
        )
        .await
        .expect("mouse dispatch completes")
        .unwrap();
    }
    assert_eq!(
        app.manager.detail_filter().map(String::as_str),
        Some("beta")
    );
    assert!(app.writes().is_empty());
    app.frame(
        "filtered-mouse-selection",
        &["[x] beta", "clear filter", "Pending edits"],
    );
}

#[tokio::test]
async fn evidence_frame_picker_help() {
    let mut app = EvidenceApp::skills(&[]).await;
    app.key(KeyCode::Enter).await;
    // The actual global resolver accepts '?' here. app::run owns opening the
    // overlay inline; this frame proves modal content, not live event delivery.
    let help_key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
    assert_eq!(
        crate::keymap::GlobalAction::from_chord(&help_key),
        Some(crate::keymap::GlobalAction::Help)
    );
    assert!(!app.manager.wants_text_input());
    app.frame_at(
        "picker-context-help",
        180,
        50,
        true,
        &[
            "Toggle selection",
            "Save array",
            "Mouse",
            "double-click",
            "Cancel",
        ],
    );
}

#[tokio::test]
async fn evidence_frame_saved_and_reopened() {
    let mut app = EvidenceApp::skills(&["gamma", "alpha"]).await;
    app.key(KeyCode::Down).await;
    app.key(KeyCode::Enter).await;
    app.save().await;
    assert_eq!(app.writes(), vec![json!(["gamma", "alpha", "beta"])]);
    app.key(KeyCode::Enter).await;
    assert_eq!(
        &app.manager.array_selection.as_ref().unwrap().entries,
        &["gamma", "alpha", "beta"]
    );
    app.frame(
        "saved-and-reopened",
        &["[x] alpha", "[x] beta", "[x] gamma", "No pending edits"],
    );
}

#[tokio::test]
async fn evidence_frame_empty_selection_saved_and_reopened() {
    let mut app = EvidenceApp::skills(&["alpha"]).await;
    app.key(KeyCode::Enter).await;
    app.frame(
        "empty-selection-before-save",
        &[
            "[ ] alpha",
            "No entries selected; saving clears this list",
            "Pending edits",
        ],
    );
    app.save().await;
    assert_eq!(app.writes(), vec![json!([])]);
    app.key(KeyCode::Enter).await;
    app.frame(
        "empty-selection-reopened",
        &[
            "[ ] alpha",
            "[ ] beta",
            "No entries selected; saving clears this list",
            "No pending edits",
        ],
    );
}

#[tokio::test]
async fn evidence_frame_unresolved_entry() {
    let mut app = EvidenceApp::skills(&["alpha", "retired-bundle"]).await;
    app.key(KeyCode::Down).await;
    app.key(KeyCode::Enter).await;
    app.frame(
        "unresolved-saved-entry",
        &[
            "[x] retired-bundle (not configured)",
            "[x] alpha",
            "[x] beta",
            "[ ] gamma",
        ],
    );
    assert!(app.writes().is_empty());
}

#[tokio::test]
async fn evidence_frame_discard_confirmation_and_stay() {
    let mut app = EvidenceApp::skills(&["alpha"]).await;
    app.key(KeyCode::Down).await;
    app.key(KeyCode::Enter).await;
    app.key(KeyCode::Esc).await;
    assert!(app.manager.pending_confirmation.is_some());
    app.frame(
        "discard-confirmation",
        &["Discard pending edits?", "Stay", "discard"],
    );
    app.key(KeyCode::Esc).await;
    assert!(app.manager.pending_confirmation.is_none());
    app.frame(
        "stay-keeps-staged-selection",
        &["[x] alpha", "[x] beta", "Pending edits"],
    );
    assert!(app.writes().is_empty());
}

#[tokio::test]
async fn evidence_frame_cancel_and_reopen() {
    let mut app = EvidenceApp::skills(&["alpha"]).await;
    app.key(KeyCode::Down).await;
    app.key(KeyCode::Enter).await;
    app.search("beta").await;
    app.key(KeyCode::Esc).await;
    assert!(app.manager.pending_confirmation.is_none());
    assert!(app.manager.detail_filter().is_none());
    app.key(KeyCode::Esc).await;
    assert!(app.manager.pending_confirmation.is_some());
    app.key(KeyCode::Enter).await;
    assert!(matches!(app.manager.screen, Screen::FieldList { .. }));
    app.key(KeyCode::Enter).await;
    assert!(app.writes().is_empty());
    app.frame(
        "cancel-and-reopen",
        &["[x] alpha", "[ ] beta", "No pending edits"],
    );
}

#[tokio::test]
async fn evidence_frame_empty_lookup_text_fallback() {
    let mut app = EvidenceApp::open(
        "agents.review-agent.skill_bundles",
        AliasSource::SkillBundles,
        &["alpha", "retired-bundle"],
        Some(vec![]),
    )
    .await;
    assert!(!app.manager.is_select_edit());
    app.frame(
        "empty-lookup-text-fallback",
        &[
            "string_array",
            "alpha",
            "retired-bundle",
            "None configured yet. Enter manually.",
        ],
    );
}

#[tokio::test]
async fn evidence_frame_failed_lookup_text_fallback() {
    let mut app = EvidenceApp::open(
        "agents.review-agent.skill_bundles",
        AliasSource::SkillBundles,
        &["alpha", "retired-bundle"],
        None,
    )
    .await;
    assert!(!app.manager.is_select_edit());
    app.frame(
        "failed-lookup-text-fallback",
        &[
            "string_array",
            "alpha",
            "retired-bundle",
            "Options fetch failed. Enter manually.",
        ],
    );
}

#[tokio::test]
async fn evidence_frame_other_reference_arrays() {
    for (path, source, slug) in [
        (
            "agents.review-agent.knowledge_bundles",
            AliasSource::KnowledgeBundles,
            "knowledge-bundles-selection",
        ),
        (
            "agents.review-agent.mcp_bundles",
            AliasSource::McpBundles,
            "mcp-bundles-selection",
        ),
        (
            "mcp_bundles.review-bundle.servers",
            AliasSource::McpServers,
            "mcp-servers-selection",
        ),
        (
            "mcp_bundles.review-bundle.exclude",
            AliasSource::McpServers,
            "mcp-exclude-selection",
        ),
    ] {
        let mut app = EvidenceApp::open(path, source, &[], Some(vec!["alpha", "beta"])).await;
        app.key(KeyCode::Enter).await;
        app.frame(
            slug,
            &[
                path.rsplit('.').next().unwrap(),
                "[x] alpha",
                "[ ] beta",
                "Toggle selection",
                "Pending edits",
            ],
        );
        assert!(app.writes().is_empty());
    }
}
