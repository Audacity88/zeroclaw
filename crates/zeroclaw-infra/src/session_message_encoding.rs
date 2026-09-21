//! Durable prompt-result provenance, separate from provider-facing roles.

use serde::{Deserialize, Serialize};
use zeroclaw_api::model_provider::ChatMessage;

pub const PROMPT_TOOL_RESULTS_ROLE: &str = "zeroclaw_prompt_tool_results";
pub(crate) const CURRENT_ENCODING_VERSION: u32 = 1;

/// Only unversioned storage may use this historical heuristic. Old literal
/// user text is inherently ambiguous; current writes must never pass here.
pub fn restore_legacy_prompt_result(message: &mut ChatMessage) {
    if message.role == "user" && message.content.starts_with("[Tool results]") {
        message.role = PROMPT_TOOL_RESULTS_ROLE.to_owned();
    }
}

pub(crate) fn decode_message(mut message: ChatMessage, version: u32) -> ChatMessage {
    if version == 0 {
        restore_legacy_prompt_result(&mut message);
    }
    message
}

#[derive(Deserialize)]
struct StoredMessage {
    #[serde(flatten)]
    message: ChatMessage,
    #[serde(default)]
    history_encoding_version: u32,
}

pub(crate) fn decode_json(line: &str) -> serde_json::Result<ChatMessage> {
    let stored: StoredMessage = serde_json::from_str(line)?;
    Ok(decode_message(
        stored.message,
        stored.history_encoding_version,
    ))
}

pub(crate) fn encode_message(message: &ChatMessage) -> impl Serialize + '_ {
    #[derive(Serialize)]
    struct StoredMessageRef<'a> {
        #[serde(flatten)]
        message: &'a ChatMessage,
        history_encoding_version: u32,
    }
    StoredMessageRef {
        message,
        history_encoding_version: CURRENT_ENCODING_VERSION,
    }
}

/// Add only metadata, leaving legacy content intact until a normal rewrite.
pub(crate) fn ensure_sqlite_encoding_column(
    conn: &rusqlite::Connection,
    table: &str,
) -> rusqlite::Result<()> {
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = 'history_encoding_version')",
        [table],
        |row| row.get(0),
    )?;
    if !present {
        // Table names are fixed internal call sites, never session/user input.
        conn.execute(
            &format!(
                "ALTER TABLE {table} ADD COLUMN history_encoding_version INTEGER NOT NULL DEFAULT 0"
            ),
            [],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp_session_store::AcpSessionStore;
    use crate::session_backend::SessionBackend;
    use crate::session_sqlite::SqliteSessionBackend;
    use crate::session_store::SessionStore;
    use std::io::Write;
    use zeroclaw_api::model_provider::ConversationMessage;

    const RESULT: &str = "[Tool results]\nlegacy output";

    fn assert_mixed_roles(messages: &[ChatMessage]) {
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, PROMPT_TOOL_RESULTS_ROLE);
        assert_eq!(messages[0].content, RESULT);
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[1].content, RESULT);
    }

    #[test]
    fn jsonl_mixed_encoding_survives_rewrite_and_sqlite_import() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let path = tmp.path().join("sessions/mixed.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::to_string(&ChatMessage::user(RESULT)).unwrap()
        )
        .unwrap();
        drop(file);
        store.append("mixed", &ChatMessage::user(RESULT)).unwrap();
        assert_mixed_roles(&store.try_load("mixed").unwrap());
        assert_mixed_roles(&store.load("mixed"));

        // Import the mixed versions directly, not only a rewritten v1 file.
        let sqlite = SqliteSessionBackend::new(tmp.path()).unwrap();
        sqlite.migrate_from_jsonl(tmp.path()).unwrap();
        assert_mixed_roles(&sqlite.load("mixed"));
        sqlite
            .rewrite_messages("mixed", &sqlite.load("mixed"))
            .unwrap();
        drop(sqlite);
        assert_mixed_roles(&SqliteSessionBackend::new(tmp.path()).unwrap().load("mixed"));

        let other = tempfile::tempdir().unwrap();
        let rewritten = SessionStore::new(other.path()).unwrap();
        let messages = vec![
            decode_message(ChatMessage::user(RESULT), 0),
            ChatMessage::user(RESULT),
        ];
        rewritten.rewrite_messages("mixed", &messages).unwrap();
        rewritten
            .update_last("mixed", &ChatMessage::user(RESULT))
            .unwrap();
        drop(rewritten);
        assert_mixed_roles(
            &SessionStore::new(other.path())
                .unwrap()
                .try_load("mixed")
                .unwrap(),
        );
    }

    #[test]
    fn sqlite_upgrade_and_append_keep_per_row_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        let db = tmp.path().join("sessions/sessions.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
            id INTEGER PRIMARY KEY AUTOINCREMENT, session_key TEXT NOT NULL,
            role TEXT NOT NULL, content TEXT NOT NULL, created_at TEXT NOT NULL
        );",
        )
        .unwrap();
        conn.execute("INSERT INTO sessions(session_key, role, content, created_at) VALUES ('mixed', 'user', ?1, '2026-01-01T00:00:00Z')", [RESULT]).unwrap();
        drop(conn);
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        store.append("mixed", &ChatMessage::user(RESULT)).unwrap();
        store
            .update_last("mixed", &ChatMessage::user(RESULT))
            .unwrap();
        assert_mixed_roles(&store.load("mixed"));
        let timestamped = store.load_with_timestamps("mixed");
        assert!(timestamped.iter().all(|row| row.created_at.is_some()));
        assert_mixed_roles(
            &timestamped
                .into_iter()
                .map(|row| row.message)
                .collect::<Vec<_>>(),
        );
        drop(store);
        assert_mixed_roles(&SqliteSessionBackend::new(tmp.path()).unwrap().load("mixed"));
    }

    #[test]
    fn acp_upgrade_append_and_replace_keep_per_row_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        let db = tmp.path().join("sessions/acp-sessions.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE acp_messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT, session_id INTEGER NOT NULL,
            role TEXT NOT NULL, content TEXT NOT NULL,
            reasoning_content TEXT, created_at TEXT NOT NULL
        );",
        )
        .unwrap();
        drop(conn);
        let store = AcpSessionStore::new(tmp.path()).unwrap();
        let id = store.create_session("mixed", "test", "/tmp/test").unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("INSERT INTO acp_messages(session_id, role, content, created_at) VALUES (?1, 'user', ?2, '2026-01-01T00:00:00Z')", rusqlite::params![id, RESULT]).unwrap();
        drop(conn);
        store
            .append_turn(
                "mixed",
                &[ConversationMessage::Chat(ChatMessage::user(RESULT))],
            )
            .unwrap();
        let messages = store.load_session("mixed").unwrap().unwrap().messages;
        let flatten = |messages: Vec<ConversationMessage>| {
            messages
                .into_iter()
                .map(|message| {
                    let ConversationMessage::Chat(chat) = message else {
                        panic!("expected chat")
                    };
                    chat
                })
                .collect::<Vec<_>>()
        };
        assert_mixed_roles(&flatten(messages.clone()));
        store
            .replace_messages_and_breadcrumb("mixed", &messages, false)
            .unwrap();
        drop(store);
        let store = AcpSessionStore::new(tmp.path()).unwrap();
        assert_mixed_roles(&flatten(
            store.load_session("mixed").unwrap().unwrap().messages,
        ));
    }
}
