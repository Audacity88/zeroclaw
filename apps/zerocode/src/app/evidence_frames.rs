//! Review evidence from production Config input/RPC/render paths.
//! These are text frames, not screenshots of a terminal or a live daemon.

#[cfg(test)] // test-only module: lets the keymap guard skip review-evidence key input
use super::*;
use crate::client::method;
use crate::jsonrpc::RpcOutbound;
use crate::widgets::HelpContext;
use serde_json::{Value, json};
use tokio::sync::mpsc;

struct EvidenceApp {
    config: config_manager::App,
    chat: chat::Chat,
    code: acp::Acp,
    client: Arc<RpcClient>,
    help: Option<HelpOverlayState>,
    term: config_manager::Term,
    // Restore the process override before deleting its temporary directory.
    _config_dir_override: ConfigDirOverride,
    _config_dir: tempfile::TempDir,
}

/// All evidence tests hold `env_test_lock` until the app is dropped. Unlike
/// `i18n::init`, this override can be restored before sibling tests run.
struct ConfigDirOverride(Option<std::ffi::OsString>);

impl ConfigDirOverride {
    fn set(dir: &std::path::Path) -> Self {
        let previous = std::env::var_os("ZEROCLAW_CONFIG_DIR");
        // SAFETY: the evidence test owns the crate's shared environment lock.
        unsafe { std::env::set_var("ZEROCLAW_CONFIG_DIR", dir) };
        Self(previous)
    }
}

impl Drop for ConfigDirOverride {
    fn drop(&mut self) {
        // SAFETY: the app is dropped before the evidence test's environment lock.
        match &self.0 {
            Some(previous) => unsafe { std::env::set_var("ZEROCLAW_CONFIG_DIR", previous) },
            None => unsafe { std::env::remove_var("ZEROCLAW_CONFIG_DIR") },
        }
    }
}

async fn answer(
    rpc: &RpcOutbound,
    rx: &mut mpsc::Receiver<String>,
    method: &str,
    params: Value,
    result: Value,
) {
    let raw = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("evidence RPC request deadline")
        .expect("evidence RPC request");
    let request: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(request["method"], method);
    assert_eq!(request["params"], params);
    rpc.dispatch_response(request["id"].as_str().unwrap(), Some(result), None);
}

fn descriptor(path: &str, kind: &str, value: Option<&str>, description: &str) -> Value {
    json!({
        "path": path, "category": "evidence", "kind": kind, "type_hint": kind,
        "value": value, "populated": value.is_some(), "is_secret": false,
        "description": description
    })
}

fn risk_fields() -> Vec<Value> {
    vec![
        descriptor(
            "risk_profiles.review.allowed_commands",
            "string_array",
            Some(
                r#"["git status", "git diff --stat", "cargo test --locked -p zerocode", "python3 scripts/review_evidence.py", "REVIEW-VALUE-END"]"#,
            ),
            "Commands allowed by this risk profile. Each entry is a command pattern.",
        ),
        descriptor(
            "risk_profiles.review.forbidden_paths",
            "string_array",
            Some(r#"["/private/review-only"]"#),
            "Paths that this risk profile forbids.",
        ),
        descriptor(
            "risk_profiles.review.require_approval_for_medium_risk",
            "bool",
            Some("true"),
            "Require approval before medium risk commands run.",
        ),
    ]
}

impl EvidenceApp {
    /// Load the section, alias, and descriptors through the typed RPC client.
    /// No screen, cursor, field list, or HelpNode is manufactured by the test.
    async fn open(section: &str, fields: Vec<Value>) -> Self {
        let (tx, mut rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(rpc.clone()));
        // Isolate local ZeroCode preferences from the operator's config.
        let config_dir = tempfile::tempdir().unwrap();
        // Do not call i18n::init here: its OnceLock pins the config directory
        // for the whole binary and defeats later Todo-settings tests' overrides.
        let config_dir_override = ConfigDirOverride::set(config_dir.path());
        assert_eq!(crate::i18n::config_dir(), config_dir.path());
        let mut config = config_manager::App::new(client.clone(), config_dir.path());
        let replies = async {
            answer(
                &rpc,
                &mut rx,
                method::CONFIG_SECTIONS,
                json!({}),
                json!({"sections": [{
                    "key": section, "label": section, "help": "Review fixture section",
                    "completed": true, "shape": "one_tier_alias_map"
                }]}),
            )
            .await;
            answer(
                &rpc,
                &mut rx,
                method::CONFIG_TEMPLATES,
                json!({}),
                json!({"templates": []}),
            )
            .await;
            answer(
                &rpc,
                &mut rx,
                method::CONFIG_MAP_KEYS,
                json!({"path": section}),
                json!({"keys": ["review"]}),
            )
            .await;
            answer(
                &rpc,
                &mut rx,
                method::CONFIG_LIST,
                json!({"prefix": format!("{section}.review.enabled")}),
                json!({"entries": []}),
            )
            .await;
        };
        let (initialized, ()) = tokio::join!(config.init(), replies);
        initialized.unwrap();
        // The handlers require the concrete terminal type. A fixed viewport
        // avoids a TTY query; these navigation paths never draw to this handle.
        let term = ratatui::Terminal::with_options(
            crate::terminal_backend::WideCellCleanupBackend::new(std::io::stdout()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 100, 30)),
            },
        )
        .unwrap();
        let mut app = Self {
            config,
            chat: chat::Chat::new(client.clone(), chat::PaneKind::Chat),
            code: acp::Acp::new(client.clone()),
            client,
            help: None,
            term,
            _config_dir_override: config_dir_override,
            _config_dir: config_dir,
        };
        app.key(KeyCode::Enter, KeyModifiers::NONE).await;
        let reply = answer(
            &rpc,
            &mut rx,
            method::CONFIG_LIST,
            json!({"prefix": format!("{section}.review")}),
            json!({"entries": fields}),
        );
        let ((), ()) = tokio::join!(app.key(KeyCode::Enter, KeyModifiers::NONE), reply);
        assert!(
            rx.try_recv().is_err(),
            "opening evidence must only read Config"
        );
        app
    }

    /// Follow app::run's global Help dispatch, then the actual pane handler.
    async fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let key = KeyEvent::new(code, modifiers);
        let global = GlobalAction::from_chord(&key);
        if let Some(state) = self.help.as_mut() {
            if global == Some(GlobalAction::Help) || state.handle_key(&key) {
                self.help = None;
            }
        } else if global == Some(GlobalAction::Help)
            && (!self.config.wants_text_input() || crate::keymap::help_bypasses_text_input(&key))
        {
            self.help = Some(HelpOverlayState::default());
        } else {
            let quit = tokio::time::timeout(
                Duration::from_secs(2),
                self.config.handle_key(key, &mut self.term),
            )
            .await
            .expect("evidence key deadline")
            .unwrap();
            assert!(!quit);
        }
    }

    async fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.key(KeyCode::Char(c), KeyModifiers::NONE).await;
        }
    }

    fn rows(&mut self, width: u16, height: u16) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                draw_app_frame(
                    frame,
                    Mode::Config,
                    &mut self.chat,
                    &mut self.code,
                    |frame, chunks, _, _| {
                        draw_mode_bar(frame, chunks[0], Mode::Config, None);
                        self.config.draw_into(frame, chunks[1]);
                        draw_status_bar(
                            frame,
                            chunks[2],
                            &self.client.connection_state(),
                            self.client.tui_id(),
                            CtxBar::new(None, None, None, None),
                            false,
                            false,
                        );
                    },
                );
                if let Some(state) = self.help.as_mut() {
                    let node = help_node_for_pane(Mode::Config, self.config.help_context());
                    draw_help_modal(frame, frame.area(), &node, state);
                }
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

    #[allow(clippy::print_stdout)] // The review evidence protocol requires stdout.
    fn capture(&mut self, slug: &str, width: u16, height: u16, visible: &[&str], absent: &[&str]) {
        let rows = self.rows(width, height);
        let text = rows.join("\n");
        for expected in visible {
            assert!(text.contains(expected), "{slug}: missing {expected:?}");
        }
        for unexpected in absent {
            assert!(
                !text.contains(unexpected),
                "{slug}: unexpected {unexpected:?}"
            );
        }
        println!("=== EVIDENCE BEGIN {slug} ({width}x{height}) ===");
        for row in rows {
            println!("{row}");
        }
        println!("=== EVIDENCE END {slug} ===");
    }
}

// Serialize environment readers with the crate's existing global lock. Each
// test uses a current-thread runtime so the synchronous guard stays on its thread.
#[test]
fn evidence_frame_long_value_row() {
    let _guard = crate::test_support::env_test_lock();
    runtime().block_on(async {
        let mut app = EvidenceApp::open("risk_profiles", risk_fields()).await;
        app.capture(
            "long-value-row",
            100,
            30,
            &["Allowed Commands = [", "…"],
            &["REVIEW-VALUE-END"],
        );
    });
}

#[test]
fn evidence_frame_long_value_help() {
    let _guard = crate::test_support::env_test_lock();
    runtime().block_on(async {
        let mut app = EvidenceApp::open("risk_profiles", risk_fields()).await;
        app.key(KeyCode::Char('?'), KeyModifiers::NONE).await;
        assert!(app.help.is_some());
        app.capture(
            "long-value-help",
            100,
            30,
            &[
                "Path: risk_profiles.review.allowed_commands",
                "Each entry is a command pattern.",
                "Current value:",
                "REVIEW-VALUE-END",
            ],
            &[],
        );
    });
}

async fn filtered_app() -> EvidenceApp {
    let mut app = EvidenceApp::open("risk_profiles", risk_fields()).await;
    app.key(KeyCode::Char('/'), KeyModifiers::NONE).await;
    app.type_text("for").await;
    app.key(KeyCode::Down, KeyModifiers::NONE).await;
    app
}

#[test]
fn evidence_frame_path_filter() {
    let _guard = crate::test_support::env_test_lock();
    runtime().block_on(async {
        let mut app = filtered_app().await;
        app.capture(
            "path-filter",
            100,
            30,
            &[
                "for",
                "Forbidden Paths =",
                "› Require Approval For Medium Risk =",
            ],
            &["Allowed Commands ="],
        );
    });
}

#[test]
fn evidence_frame_filtered_help() {
    let _guard = crate::test_support::env_test_lock();
    runtime().block_on(async {
        let mut app = filtered_app().await;
        app.key(KeyCode::Char('g'), KeyModifiers::CONTROL).await;
        assert!(app.help.is_some(), "Ctrl+G bypasses the focused filter");
        app.capture(
            "filtered-help",
            100,
            30,
            &[
                "Path: risk_profiles.review.require_approval_for_medium_risk",
                "Require approval before medium risk commands run.",
                "true",
            ],
            &["Path: risk_profiles.review.allowed_commands"],
        );
    });
}

async fn secret_app(populated: bool) -> EvidenceApp {
    // Match the daemon contract: secret values are omitted, even when populated.
    let mut secret = descriptor(
        "mcp.servers.review.env.REVIEW_TOKEN",
        "string",
        None,
        "Environment credential supplied to this MCP server.",
    );
    secret.as_object_mut().unwrap().remove("value");
    secret["is_secret"] = json!(true);
    secret["populated"] = json!(populated);
    EvidenceApp::open("mcp.servers", vec![secret]).await
}

#[test]
fn evidence_frame_secret_set_row_and_help() {
    let _guard = crate::test_support::env_test_lock();
    runtime().block_on(async {
        let mut app = secret_app(true).await;
        app.capture("secret-set-row", 100, 30, &["Set (hidden)"], &["<unset>"]);
        app.key(KeyCode::Char('?'), KeyModifiers::NONE).await;
        assert!(app.help.is_some());
        app.capture(
            "secret-set-help",
            100,
            30,
            &[
                "Path: mcp.servers.review.env.REVIEW_TOKEN",
                "Current value:",
                "Set (hidden)",
            ],
            &["<unset>"],
        );
    });
}

#[test]
fn evidence_frame_secret_unset_row_and_help() {
    let _guard = crate::test_support::env_test_lock();
    runtime().block_on(async {
        let mut app = secret_app(false).await;
        app.capture("secret-unset-row", 100, 30, &["<unset>"], &["Set (hidden)"]);
        app.key(KeyCode::Char('?'), KeyModifiers::NONE).await;
        assert!(app.help.is_some());
        app.capture(
            "secret-unset-help",
            100,
            30,
            &[
                "Path: mcp.servers.review.env.REVIEW_TOKEN",
                "Current value:",
                "<unset>",
            ],
            &["Set (hidden)"],
        );
    });
}

#[test]
fn evidence_frame_narrow_setup_markers() {
    let _guard = crate::test_support::env_test_lock();
    runtime().block_on(async {
        let mut missing = descriptor(
            "model_routes.review.model_provider",
            "alias_ref",
            None,
            "Model provider used by this route.",
        );
        missing["setup"] = json!({"missing": true});
        let mut required = descriptor(
            "model_routes.review.model",
            "string",
            Some("review-model-with-a-long-name-that-needs-a-bounded-preview"),
            "Model used by this route.",
        );
        required["setup"] = json!({"missing": false});
        let mut app = EvidenceApp::open("model_routes", vec![missing, required]).await;
        app.capture(
            "narrow-setup-markers",
            80,
            24,
            &[
                "Model Provider [missing] = <unset>",
                "Model [required] =",
                "…",
            ],
            &[],
        );
    });
}

#[test]
fn evidence_frame_secret_map_add_row() {
    let _guard = crate::test_support::env_test_lock();
    runtime().block_on(async {
        let mut map = descriptor(
            "mcp.servers.review.env",
            "secret_map",
            None,
            "Environment variables supplied to this MCP server.",
        );
        map["is_secret"] = json!(true);
        let mut app = EvidenceApp::open("mcp.servers", vec![map]).await;
        let add = crate::i18n::t("zc-config-secret-map-add");
        app.capture("secret-map-add-row", 100, 30, &["Env =", &add], &[]);
    });
}

#[test]
fn evidence_frame_help_search_field_details() {
    let _guard = crate::test_support::env_test_lock();
    runtime().block_on(async {
        let mut app = EvidenceApp::open("risk_profiles", risk_fields()).await;
        app.key(KeyCode::Char('?'), KeyModifiers::NONE).await;
        assert!(app.help.is_some());
        app.type_text("allowed_commands").await;
        let unrelated = crate::i18n::t("zc-app-help-reload");
        app.capture(
            "help-search-field-details",
            100,
            30,
            &[
                "Path: risk_profiles.review.allowed_commands",
                "REVIEW-VALUE-END",
            ],
            &[&unrelated],
        );
    });
}

#[test]
fn evidence_frame_narrow_help_scroll() {
    let _guard = crate::test_support::env_test_lock();
    runtime().block_on(async {
        let mut fields = risk_fields();
        // Public, synthetic long-token data; no operator config or secrets.
        fields[0]["value"] = json!(format!(
            "[\"/{} REVIEW-LONG-TOKEN-END\"]",
            "界config_path_".repeat(100)
        ));
        let mut app = EvidenceApp::open("risk_profiles", fields).await;
        app.key(KeyCode::Char('?'), KeyModifiers::NONE).await;
        assert!(app.help.is_some());
        app.capture(
            "narrow-help-top",
            60,
            24,
            &["Path: risk_profiles.review.allowed_commands"],
            &["REVIEW-LONG-TOKEN-END"],
        );
        // Scroll with the same keys as the live Help overlay until its final
        // value token is visible. Never assign the overlay's scroll offset.
        for _ in 0..100 {
            if app
                .rows(60, 24)
                .join("\n")
                .contains("REVIEW-LONG-TOKEN-END")
            {
                app.capture(
                    "narrow-help-scrolled",
                    60,
                    24,
                    &["REVIEW-LONG-TOKEN-END"],
                    &[],
                );
                return;
            }
            app.key(KeyCode::Down, KeyModifiers::NONE).await;
        }
        panic!("the complete long value must be reachable through Help scrolling");
    });
}

#[test]
fn evidence_frame_mouse_filtered_help() {
    let _guard = crate::test_support::env_test_lock();
    runtime().block_on(async {
        let mut app = filtered_app().await;
        let rows = app.rows(100, 30);
        let (row, line) = rows
            .iter()
            .enumerate()
            .find(|(_, line)| line.contains("Forbidden Paths ="))
            .expect("real rendered filtered row");
        let label = line.find("Forbidden Paths =").unwrap();
        let column = crate::display_width::display_width(&line[..label]) as u16;
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row: row as u16,
            modifiers: KeyModifiers::NONE,
        };
        tokio::time::timeout(
            Duration::from_secs(2),
            app.config
                .handle_mouse(event, Rect::new(0, 0, 100, 30), &mut app.term),
        )
        .await
        .expect("evidence mouse deadline")
        .unwrap();
        app.key(KeyCode::Char('g'), KeyModifiers::CONTROL).await;
        assert!(app.help.is_some());
        app.capture(
            "filtered-mouse-help",
            100,
            30,
            &[
                "Path: risk_profiles.review.forbidden_paths",
                "Paths that this risk profile forbids.",
                "/private/review-only",
            ],
            &["Path: risk_profiles.review.require_approval_for_medium_risk"],
        );
    });
}

#[test]
fn evidence_frame_environment_override() {
    let _guard = crate::test_support::env_test_lock();
    runtime().block_on(async {
        let mut fields = risk_fields();
        fields[0]["is_env_overridden"] = json!(true);
        let mut app = EvidenceApp::open("risk_profiles", fields).await;
        app.capture(
            "environment-override-row",
            100,
            30,
            &["Allowed Commands =", "[env]"],
            &[],
        );
        app.key(KeyCode::Char('?'), KeyModifiers::NONE).await;
        assert!(app.help.is_some());
        app.capture(
            "environment-override-help",
            100,
            30,
            &[
                "Path: risk_profiles.review.allowed_commands",
                "Environment override is active.",
                "REVIEW-VALUE-END",
            ],
            &[],
        );
    });
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
