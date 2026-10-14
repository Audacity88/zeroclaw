//! The sidebar's open-session list, saved so the next launch reopens it.
//!
//! The daemon already keeps every transcript; what a zerocode restart loses
//! is which sessions this client had open, in what order, and which one was
//! focused. That list is written here whenever it changes and handed to the
//! panes' resume path at startup, the same path a reconnect rebuild uses.
//!
//! One file per connection target under `<config_dir>/open-sessions/`, so a
//! client that talks to two daemons keeps two lists. Only ids, aliases, order
//! and focus are stored; queued input and drafts are not.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chat::{PaneKind, ResumeEntry, SidebarSessionSummary};
use crate::client::{RpcClient, SessionEntry, Transport};

const DIR_NAME: &str = "open-sessions";
const FORMAT_VERSION: u32 = 1;
/// Matches the panes' own cap; a hand-edited file cannot exceed it.
const MAX_PER_PANE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SavedPane {
    Code,
    Chat,
}

impl From<PaneKind> for SavedPane {
    fn from(kind: PaneKind) -> Self {
        match kind {
            PaneKind::Acp => Self::Code,
            PaneKind::Chat => Self::Chat,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedSession {
    pub pane: SavedPane,
    pub session_id: String,
    pub agent_alias: String,
    pub focused: bool,
}

/// What one launch saves: the sidebar rows in order, plus the pane that was
/// on screen so the next launch opens there.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_pane: Option<SavedPane>,
    pub sessions: Vec<SavedSession>,
}

impl Snapshot {
    pub(crate) fn from_rows(rows: &[SidebarSessionSummary], active_pane: Option<PaneKind>) -> Self {
        Self {
            active_pane: active_pane.map(SavedPane::from),
            sessions: rows
                .iter()
                .map(|row| SavedSession {
                    pane: row.pane_kind.into(),
                    session_id: row.session_id.clone(),
                    agent_alias: row.agent_alias.clone(),
                    focused: row.focused,
                })
                .collect(),
        }
    }

    /// Resume entries for the Chat and Code panes, in saved order.
    pub(crate) fn resume_entries(&self) -> (Vec<ResumeEntry>, Vec<ResumeEntry>) {
        let mut chat = Vec::new();
        let mut code = Vec::new();
        // Sidebar labels number each agent's sessions per pane; renumber
        // from 1 in saved order, as a fresh launch would.
        let mut ordinals = std::collections::HashMap::<(SavedPane, &str), u64>::new();
        for saved in &self.sessions {
            let list = match saved.pane {
                SavedPane::Chat => &mut chat,
                SavedPane::Code => &mut code,
            };
            if list.len() < MAX_PER_PANE {
                let ordinal = ordinals
                    .entry((saved.pane, saved.agent_alias.as_str()))
                    .or_insert(0);
                *ordinal += 1;
                list.push(ResumeEntry::saved(
                    saved.session_id.clone(),
                    saved.agent_alias.clone(),
                    saved.focused,
                    *ordinal,
                ));
            }
        }
        (chat, code)
    }

    /// The pane to open on: the saved one if it has sessions, else Code, else
    /// Chat, else none (stay on the dashboard).
    pub(crate) fn start_pane(&self) -> Option<PaneKind> {
        let has = |pane| self.sessions.iter().any(|s| s.pane == pane);
        match self.active_pane {
            Some(SavedPane::Code) if has(SavedPane::Code) => Some(PaneKind::Acp),
            Some(SavedPane::Chat) if has(SavedPane::Chat) => Some(PaneKind::Chat),
            _ if has(SavedPane::Code) => Some(PaneKind::Acp),
            _ if has(SavedPane::Chat) => Some(PaneKind::Chat),
            _ => None,
        }
    }
}

impl Snapshot {
    /// Drop saved sessions the daemon no longer has. `None` for a pane means
    /// its list could not be fetched: keep that pane's entries and let the
    /// resume path retry them. Returns how many were dropped.
    pub(crate) fn retain_known(
        &mut self,
        code: Option<&[SessionEntry]>,
        chat: Option<&[SessionEntry]>,
    ) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|saved| {
            let known = match saved.pane {
                SavedPane::Code => code,
                SavedPane::Chat => chat,
            };
            known.is_none_or(|list| {
                list.iter().any(|entry| {
                    entry.session_id == saved.session_id
                        && entry
                            .agent_alias
                            .as_deref()
                            .is_none_or(|alias| alias == saved.agent_alias)
                })
            })
        });
        before - self.sessions.len()
    }
}

/// Check a loaded list against the daemon before resuming it, so a session
/// deleted or pruned since the last launch is skipped instead of showing as a
/// failed row that cannot be closed.
///
/// Local connections only. Over WSS the daemon lists just the sessions this
/// client instance owns right now, which after a restart is none, so a check
/// there would drop the whole list. Remote entries go straight to the resume
/// path, which keeps any that fail as retryable rows.
pub(crate) async fn prune_missing(rpc: &RpcClient, mut saved: Snapshot) -> (Snapshot, usize) {
    if rpc.transport() != Transport::Local {
        return (saved, 0);
    }
    let wants = |pane| saved.sessions.iter().any(|s| s.pane == pane);
    let code = if wants(SavedPane::Code) {
        rpc.acp_session_list().await.ok().map(|list| list.sessions)
    } else {
        None
    };
    let chat = if wants(SavedPane::Chat) {
        rpc.session_list(None).await.ok().map(|list| list.sessions)
    } else {
        None
    };
    let dropped = saved.retain_known(code.as_deref(), chat.as_deref());
    (saved, dropped)
}

#[derive(Debug, Serialize, Deserialize)]
struct FileBody {
    version: u32,
    /// The connection label, for a human reading the file; the file name is
    /// its hash.
    target: String,
    #[serde(flatten)]
    snapshot: Snapshot,
}

/// Where one connection target's list lives, and the last list written there.
#[derive(Debug)]
pub(crate) struct Store {
    path: PathBuf,
    target: String,
    last_saved: Option<Snapshot>,
}

impl Store {
    pub(crate) fn new(config_dir: &Path, target_label: &str) -> Self {
        let digest = Sha256::digest(target_label.as_bytes());
        let name: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        Self {
            path: config_dir.join(DIR_NAME).join(format!("{name}.json")),
            target: target_label.to_string(),
            last_saved: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The saved list, or `None` when there is none or it cannot be read. A
    /// damaged or newer-format file is ignored, not an error: the launch goes
    /// on with an empty sidebar and the next save replaces the file.
    pub(crate) fn load(&mut self) -> Option<Snapshot> {
        let bytes = std::fs::read(&self.path).ok()?;
        let body: FileBody = serde_json::from_slice(&bytes).ok()?;
        if body.version != FORMAT_VERSION {
            return None;
        }
        self.last_saved = Some(body.snapshot.clone());
        Some(body.snapshot)
    }

    /// Write `snapshot` if it differs from the last one written. Returns
    /// whether a write happened. A failed write is not retried until the list
    /// changes again, so a read-only directory costs one error, not one per
    /// frame.
    pub(crate) fn save_if_changed(&mut self, snapshot: &Snapshot) -> Result<bool> {
        if self.last_saved.as_ref() == Some(snapshot) {
            return Ok(false);
        }
        self.last_saved = Some(snapshot.clone());
        let body = FileBody {
            version: FORMAT_VERSION,
            target: self.target.clone(),
            snapshot: snapshot.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&body).context("serializing open sessions")?;
        crate::secure_file::write_private_atomic(&self.path, &bytes)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::SidebarStatus;

    fn row(pane: PaneKind, id: &str, alias: &str, focused: bool) -> SidebarSessionSummary {
        SidebarSessionSummary {
            session_id: id.to_string(),
            agent_alias: alias.to_string(),
            message_count: 3,
            status: SidebarStatus::Ready,
            pane_kind: pane,
            focused,
            display_ordinal: 1,
            last_activity: None,
        }
    }

    #[test]
    fn saved_list_round_trips_in_order_with_focus() {
        let dir = tempfile::tempdir().unwrap();
        let rows = [
            row(PaneKind::Acp, "code-1", "fable", false),
            row(PaneKind::Acp, "code-2", "ox", true),
            row(PaneKind::Chat, "chat-1", "fable", true),
        ];
        let snapshot = Snapshot::from_rows(&rows, Some(PaneKind::Acp));
        let mut store = Store::new(dir.path(), "local:/run/a.sock");
        assert!(store.save_if_changed(&snapshot).unwrap());

        let mut reopened = Store::new(dir.path(), "local:/run/a.sock");
        let loaded = reopened.load().expect("saved list loads");
        assert_eq!(loaded, snapshot);

        let (chat, code) = loaded.resume_entries();
        let ids = |entries: &[ResumeEntry]| {
            entries
                .iter()
                .map(|e| (e.session_id.clone(), e.agent_alias.clone(), e.was_focused))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&code),
            vec![
                ("code-1".into(), "fable".into(), false),
                ("code-2".into(), "ox".into(), true),
            ]
        );
        assert_eq!(ids(&chat), vec![("chat-1".into(), "fable".into(), true)]);
        assert_eq!(loaded.start_pane(), Some(PaneKind::Acp));
    }

    #[test]
    fn each_connection_target_keeps_its_own_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut main = Store::new(dir.path(), "local:/run/main.sock");
        let mut other = Store::new(dir.path(), "local:/run/experiment.sock");
        assert_ne!(main.path(), other.path());
        main.save_if_changed(&Snapshot::from_rows(
            &[row(PaneKind::Acp, "main-1", "fable", true)],
            None,
        ))
        .unwrap();
        assert!(
            other.load().is_none(),
            "another daemon's list must not load"
        );
        assert_eq!(main.load().unwrap().sessions[0].session_id, "main-1");
    }

    #[test]
    fn unchanged_list_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::new(dir.path(), "local:/run/a.sock");
        let snapshot = Snapshot::from_rows(&[row(PaneKind::Chat, "c", "a", true)], None);
        assert!(store.save_if_changed(&snapshot).unwrap());
        assert!(!store.save_if_changed(&snapshot).unwrap());
        let moved = Snapshot::from_rows(&[row(PaneKind::Chat, "c", "a", false)], None);
        assert!(
            store.save_if_changed(&moved).unwrap(),
            "a focus change is saved"
        );
    }

    #[test]
    fn a_loaded_list_is_not_written_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = Snapshot::from_rows(&[row(PaneKind::Acp, "s", "a", true)], None);
        Store::new(dir.path(), "t")
            .save_if_changed(&snapshot)
            .unwrap();
        let mut store = Store::new(dir.path(), "t");
        let loaded = store.load().unwrap();
        assert!(!store.save_if_changed(&loaded).unwrap());
    }

    #[test]
    fn damaged_or_newer_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::new(dir.path(), "t");
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        std::fs::write(store.path(), b"{not json").unwrap();
        assert!(store.load().is_none());
        std::fs::write(
            store.path(),
            br#"{"version":99,"target":"t","sessions":[]}"#,
        )
        .unwrap();
        assert!(store.load().is_none());
    }

    #[test]
    fn a_hand_edited_file_cannot_exceed_the_pane_cap() {
        let rows: Vec<_> = (0..12)
            .map(|i| row(PaneKind::Acp, &format!("s{i}"), "a", i == 0))
            .collect();
        let (_, code) = Snapshot::from_rows(&rows, None).resume_entries();
        assert_eq!(code.len(), MAX_PER_PANE);
        assert_eq!(code[0].session_id, "s0");
    }

    #[test]
    fn start_pane_falls_back_to_a_pane_that_has_sessions() {
        let only_chat =
            Snapshot::from_rows(&[row(PaneKind::Chat, "c", "a", true)], Some(PaneKind::Acp));
        assert_eq!(only_chat.start_pane(), Some(PaneKind::Chat));
        assert_eq!(Snapshot::default().start_pane(), None);
    }

    fn listed(id: &str, alias: Option<&str>) -> SessionEntry {
        SessionEntry {
            session_id: id.to_string(),
            session_key: id.to_string(),
            created_at: String::new(),
            last_activity: String::new(),
            message_count: 0,
            agent_alias: alias.map(str::to_string),
            channel_id: None,
            name: None,
        }
    }

    #[test]
    fn sessions_the_daemon_no_longer_has_are_dropped() {
        let mut snapshot = Snapshot::from_rows(
            &[
                row(PaneKind::Acp, "kept", "fable", false),
                row(PaneKind::Acp, "pruned", "fable", true),
                row(PaneKind::Acp, "reassigned", "fable", false),
                row(PaneKind::Chat, "chat-kept", "ox", true),
            ],
            None,
        );
        let code = [
            listed("kept", Some("fable")),
            listed("reassigned", Some("ox")),
        ];
        let chat = [listed("chat-kept", None)];
        assert_eq!(snapshot.retain_known(Some(&code), Some(&chat)), 2);
        let ids: Vec<_> = snapshot
            .sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();
        assert_eq!(ids, ["kept", "chat-kept"]);
    }

    #[test]
    fn an_unreadable_daemon_list_keeps_that_panes_sessions() {
        let mut snapshot = Snapshot::from_rows(
            &[
                row(PaneKind::Acp, "code", "a", true),
                row(PaneKind::Chat, "chat", "a", true),
            ],
            None,
        );
        assert_eq!(snapshot.retain_known(None, Some(&[])), 1);
        assert_eq!(snapshot.sessions[0].session_id, "code");
    }

    fn rpc_on(
        transport: Transport,
    ) -> (
        std::sync::Arc<crate::jsonrpc::RpcOutbound>,
        RpcClient,
        tokio::sync::mpsc::Receiver<String>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(16);
        let out = std::sync::Arc::new(crate::jsonrpc::RpcOutbound::new(tx));
        let client = RpcClient::with_rpc_transport(std::sync::Arc::clone(&out), transport);
        (out, client, rx)
    }

    #[tokio::test]
    async fn local_restore_asks_the_daemon_and_drops_what_it_lacks() {
        let (out, client, mut rx) = rpc_on(Transport::Local);
        let saved = Snapshot::from_rows(
            &[
                row(PaneKind::Acp, "alive", "fable", true),
                row(PaneKind::Acp, "gone", "fable", false),
            ],
            Some(PaneKind::Acp),
        );
        let task = tokio::spawn(async move { prune_missing(&client, saved).await });
        let line = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("restore should list Code sessions")
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], crate::client::method::SESSION_LIST_ACP);
        out.dispatch_response(
            request["id"].as_str().unwrap(),
            Some(serde_json::json!({ "sessions": [{
                "session_id": "alive", "session_key": "alive",
                "created_at": "", "last_activity": "", "message_count": 2,
                "agent_alias": "fable"
            }]})),
            None,
        );
        let (pruned, dropped) = task.await.unwrap();
        assert_eq!(dropped, 1);
        assert_eq!(pruned.sessions.len(), 1);
        assert_eq!(pruned.sessions[0].session_id, "alive");
        assert!(
            rx.try_recv().is_err(),
            "no Chat list is fetched without Chat rows"
        );
    }

    #[tokio::test]
    async fn remote_restore_keeps_the_list_without_asking() {
        let (_out, client, mut rx) = rpc_on(Transport::Wss);
        let saved = Snapshot::from_rows(&[row(PaneKind::Acp, "s", "a", true)], None);
        let (kept, dropped) = prune_missing(&client, saved.clone()).await;
        assert_eq!((kept, dropped), (saved, 0));
        assert!(
            rx.try_recv().is_err(),
            "a remote restore must not list sessions"
        );
    }

    #[cfg(unix)]
    #[test]
    fn saved_list_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::new(dir.path(), "t");
        store
            .save_if_changed(&Snapshot::from_rows(
                &[row(PaneKind::Acp, "s", "a", true)],
                None,
            ))
            .unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
