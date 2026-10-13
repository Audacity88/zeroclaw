//! Authority-checked owner delivery for live session turn notifications.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zeroclaw_api::grants::{Resource, Verb};
use zeroclaw_api::jsonrpc::RpcOutbound;

use super::auth::ConnectionAuth;
use super::context::RpcContext;
use super::session::RpcSession;
use super::tui_identity::TuiEpoch;

struct OwnerRecipient {
    session_id: String,
    generation: u64,
    owner_tui_id: String,
    registry_epoch: TuiEpoch,
    auth: ConnectionAuth,
    outbound: Arc<RpcOutbound>,
    registration_cancel: CancellationToken,
}

/// Resolve a live owner recipient and return a forwarding outbound for it.
///
/// Admission uses the canonical session, authority, and TUI registry sources.
/// The forwarding task repeats those checks for every raw `session/update`
/// frame, so revocation, session replacement, and TUI reconnects fail closed
/// without retaining a grants cache.
pub(super) async fn owner_turn_outbound(
    ctx: Arc<RpcContext>,
    session_id: &str,
    issuer: &Arc<RpcOutbound>,
) -> Option<Arc<RpcOutbound>> {
    let recipient = ctx
        .sessions
        .with_live_session(session_id, |session| {
            let owner_tui_id = session.owner_tui_id.clone()?;
            let authority = ctx.auth.hold_authority();
            ctx.tui_registry
                .with_live_registration(&owner_tui_id, |epoch, entry, cancel| {
                    let auth = entry.auth.clone()?;
                    let outbound = entry.outbound.clone()?;
                    let grants = authority.current_grants(&auth).ok()?;
                    if !grants.permits(Resource::Sessions, Verb::Read)
                        || !grants.may_use_agent(&session.agent_alias)
                        || !owner_authorized(session, &auth, &grants)
                    {
                        return None;
                    }
                    Some(OwnerRecipient {
                        session_id: session_id.to_owned(),
                        generation: session.generation,
                        owner_tui_id: owner_tui_id.clone(),
                        registry_epoch: epoch,
                        auth,
                        outbound,
                        registration_cancel: cancel,
                    })
                })?
        })
        .await
        .flatten()?;

    if Arc::ptr_eq(&recipient.outbound, issuer) {
        return None;
    }

    let (tx, mut rx) = mpsc::channel::<String>(64);
    let (wrapper_outbound, wrapper_shutdown) = RpcOutbound::new_with_drop_signal(tx);
    let wrapper = Arc::new(wrapper_outbound);
    let ctx_for_forwarder = Arc::clone(&ctx);
    zeroclaw_spawn::spawn!(async move {
        while let Some(frame) = rx.recv().await {
            if !is_session_update_frame(&frame) {
                continue;
            }

            // Reservation deliberately happens before the authority check. A
            // full old writer must be interruptible by either registration
            // supersession or the upstream fanout dropping this wrapper.
            let permit = tokio::select! {
                biased;
                _ = recipient.registration_cancel.cancelled() => break,
                permit = recipient.outbound.reserve_raw() => match permit {
                    Some(permit) => permit,
                    None => break,
                },
                _ = wrapper_shutdown.cancelled() => break,
            };

            if !send_owner_frame(&ctx_for_forwarder, &recipient, permit, frame).await {
                break;
            }
        }
    });
    Some(wrapper)
}

async fn send_owner_frame(
    ctx: &Arc<RpcContext>,
    recipient: &OwnerRecipient,
    permit: zeroclaw_api::jsonrpc::RpcRawPermit,
    frame: String,
) -> bool {
    ctx.sessions
        .with_live_session(&recipient.session_id, |session| {
            if session.generation != recipient.generation
                || session.owner_tui_id.as_deref() != Some(recipient.owner_tui_id.as_str())
            {
                return false;
            }

            let authority = ctx.auth.hold_authority();
            ctx.tui_registry
                .with_registration(
                    &recipient.owner_tui_id,
                    recipient.registry_epoch,
                    |entry, _cancel| {
                        if !entry.auth.is_some()
                            || !entry
                                .outbound
                                .as_ref()
                                .is_some_and(|outbound| Arc::ptr_eq(outbound, &recipient.outbound))
                        {
                            return false;
                        }
                        let Ok(grants) = authority.current_grants(&recipient.auth) else {
                            return false;
                        };
                        if !grants.permits(Resource::Sessions, Verb::Read)
                            || !grants.may_use_agent(&session.agent_alias)
                            || !owner_authorized(session, &recipient.auth, &grants)
                        {
                            return false;
                        }
                        permit.send(frame);
                        true
                    },
                )
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
}

fn owner_authorized(
    session: &RpcSession,
    auth: &ConnectionAuth,
    grants: &zeroclaw_api::grants::ResolvedGrants,
) -> bool {
    grants.admin
        || !auth.principal.is_authenticated()
        || session.owner_principal_id.as_deref() == Some(auth.principal.id.as_str())
}

fn is_session_update_frame(frame: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(frame) else {
        return false;
    };
    value.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && value.get("method").and_then(Value::as_str) == Some("session/update")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::sync::mpsc;
    use zeroclaw_api::attribution::{Attributable, ModelProviderKind, ProviderKind, Role};
    use zeroclaw_api::grants::{Resource, Verb};
    use zeroclaw_api::jsonrpc::RpcOutbound;
    use zeroclaw_api::model_provider::ModelProvider;
    use zeroclaw_api::principal::{AuthMethod, AuthenticatedIdentity, IdentitySubject};
    use zeroclaw_config::schema::{Config, PermissionProfileConfig, UserConfig};
    use zeroclaw_infra::session_queue::SessionActorQueue;

    use super::{is_session_update_frame, owner_turn_outbound};
    use crate::rpc::context::RpcContext;
    use crate::rpc::session::{RpcSession, SessionStore};
    use crate::rpc::types::ChatMode;

    struct DummyModelProvider;

    #[async_trait]
    impl ModelProvider for DummyModelProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    impl Attributable for DummyModelProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }

        fn alias(&self) -> &str {
            "owner-turn-test"
        }
    }

    fn test_config() -> Config {
        let mut config = Config::default();
        config.permission_profiles.insert(
            "session-reader".to_string(),
            PermissionProfileConfig {
                allowed_agents: vec!["*".to_string()],
                grants: std::collections::HashMap::from([(Resource::Sessions, vec![Verb::Read])]),
                ..PermissionProfileConfig::default()
            },
        );
        for (name, uid) in [("alice", 1001), ("bob", 1002)] {
            config.users.insert(
                name.to_string(),
                UserConfig {
                    principal_id: None,
                    uid: Some(uid),
                    permission_profiles: vec!["session-reader".to_string()],
                },
            );
        }
        config
    }

    fn test_agent() -> crate::agent::agent::Agent {
        crate::agent::agent::Agent::builder()
            .model_provider(Box::new(DummyModelProvider))
            .tools(crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(
                vec![],
            ))
            .memory(Arc::new(zeroclaw_memory::NoneMemory::new("none")))
            .observer(Arc::new(crate::observability::noop::NoopObserver))
            .tool_dispatcher(Box::new(crate::agent::dispatcher::NativeToolDispatcher))
            .workspace_dir(std::env::temp_dir())
            .build()
            .expect("test agent should build")
    }

    fn auth_for(
        ctx: &RpcContext,
        principal_id: &str,
        uid: u32,
    ) -> crate::rpc::auth::ConnectionAuth {
        let identity = AuthenticatedIdentity::new(
            IdentitySubject::Roster {
                principal_id: principal_id.to_string(),
            },
            AuthMethod::Peercred,
        );
        let resolved = ctx.auth.resolve(&identity).expect("test identity resolves");
        crate::rpc::auth::ConnectionAuth {
            identity,
            principal: resolved.principal,
            grants: resolved.grants,
            generation: resolved.generation,
            native_token_hash: None,
            local_evidence: crate::rpc::auth::LocalCredentialEvidence::Peercred { uid },
        }
    }

    async fn test_context() -> Arc<RpcContext> {
        let sessions = Arc::new(SessionStore::new(
            4,
            Arc::new(SessionActorQueue::new(4, 1, 60)),
        ));
        let ctx = RpcContext::minimal(test_config(), Arc::clone(&sessions));
        sessions
            .insert(
                "owner-turn".to_string(),
                RpcSession::new(
                    test_agent(),
                    "test-agent",
                    std::env::temp_dir().to_string_lossy().as_ref(),
                    ChatMode::Chat,
                )
                .with_owner(Some("owner-tui".to_string()))
                .with_owner_principal(Some("user:alice".to_string())),
            )
            .await
            .expect("session should insert");
        ctx
    }

    #[test]
    fn owner_forwarder_accepts_only_session_updates() {
        assert!(is_session_update_frame(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#
        ));
        assert!(!is_session_update_frame(
            r#"{"jsonrpc":"2.0","method":"session/prompt","params":{}}"#
        ));
        assert!(!is_session_update_frame("not json"));
    }

    #[tokio::test]
    async fn foreign_owner_principal_is_withheld() {
        let ctx = test_context().await;
        let (owner_tx, _owner_rx) = mpsc::channel(4);
        let owner_rpc = Arc::new(RpcOutbound::new(owner_tx));
        let foreign_auth = auth_for(&ctx, "bob", 1002);
        ctx.tui_registry
            .register(crate::rpc::tui_identity::TuiEntry {
                tui_id: "owner-tui".to_string(),
                connected_at: chrono::Utc::now(),
                peer_label: "test".to_string(),
                transport: "unix".to_string(),
                env: std::collections::HashMap::new(),
                outbound: Some(owner_rpc),
                auth: Some(foreign_auth),
            });

        let (issuer_tx, _issuer_rx) = mpsc::channel(4);
        let issuer = Arc::new(RpcOutbound::new(issuer_tx));
        assert!(
            owner_turn_outbound(ctx, "owner-turn", &issuer)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn revocation_terminates_a_blocked_owner_recipient() {
        let ctx = test_context().await;
        let (owner_tx, mut owner_rx) = mpsc::channel(1);
        owner_tx.send("occupied".to_string()).await.unwrap();
        let owner_rpc = Arc::new(RpcOutbound::new(owner_tx));
        let owner_auth = auth_for(&ctx, "alice", 1001);
        ctx.tui_registry
            .register(crate::rpc::tui_identity::TuiEntry {
                tui_id: "owner-tui".to_string(),
                connected_at: chrono::Utc::now(),
                peer_label: "test".to_string(),
                transport: "unix".to_string(),
                env: std::collections::HashMap::new(),
                outbound: Some(owner_rpc),
                auth: Some(owner_auth),
            });
        let (issuer_tx, _issuer_rx) = mpsc::channel(4);
        let issuer = Arc::new(RpcOutbound::new(issuer_tx));
        let recipient = owner_turn_outbound(Arc::clone(&ctx), "owner-turn", &issuer)
            .await
            .expect("owner recipient should be admitted");
        recipient
            .send_raw(r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#.to_string())
            .await;
        tokio::task::yield_now().await;

        let mut revoked = test_config();
        revoked
            .permission_profiles
            .get_mut("session-reader")
            .expect("profile")
            .grants
            .clear();
        revoked
            .permission_profiles
            .get_mut("session-reader")
            .expect("profile")
            .allowed_agents
            .clear();
        ctx.auth
            .refresh_from_config(&revoked)
            .expect("revoke policy");
        owner_rx.recv().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), owner_rx.recv())
                .await
                .is_err(),
            "revoked owner must not receive the queued update"
        );
        assert!(!recipient.send_raw("later".to_string()).await);
    }

    #[tokio::test]
    async fn superseded_owner_registration_terminates_blocked_recipient() {
        let ctx = test_context().await;
        let (old_tx, mut old_rx) = mpsc::channel(1);
        old_tx.send("occupied".to_string()).await.unwrap();
        let old_rpc = Arc::new(RpcOutbound::new(old_tx));
        let owner_auth = auth_for(&ctx, "alice", 1001);
        ctx.tui_registry
            .register(crate::rpc::tui_identity::TuiEntry {
                tui_id: "owner-tui".to_string(),
                connected_at: chrono::Utc::now(),
                peer_label: "old".to_string(),
                transport: "unix".to_string(),
                env: std::collections::HashMap::new(),
                outbound: Some(old_rpc),
                auth: Some(owner_auth.clone()),
            });
        let (issuer_tx, _issuer_rx) = mpsc::channel(4);
        let issuer = Arc::new(RpcOutbound::new(issuer_tx));
        let recipient = owner_turn_outbound(Arc::clone(&ctx), "owner-turn", &issuer)
            .await
            .expect("owner recipient should be admitted");
        recipient
            .send_raw(r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#.to_string())
            .await;
        tokio::task::yield_now().await;

        let (new_tx, _new_rx) = mpsc::channel(4);
        ctx.tui_registry
            .register(crate::rpc::tui_identity::TuiEntry {
                tui_id: "owner-tui".to_string(),
                connected_at: chrono::Utc::now(),
                peer_label: "new".to_string(),
                transport: "unix".to_string(),
                env: std::collections::HashMap::new(),
                outbound: Some(Arc::new(RpcOutbound::new(new_tx))),
                auth: Some(owner_auth),
            });
        old_rx.recv().await;
        let queued = tokio::time::timeout(Duration::from_millis(100), old_rx.recv()).await;
        assert!(
            matches!(queued, Err(_) | Ok(None)),
            "superseded owner must not receive the queued update"
        );
        assert!(!recipient.send_raw("later".to_string()).await);
    }

    #[tokio::test]
    async fn dropped_wrapper_drains_buffered_terminal_update_when_owner_is_ready() {
        let ctx = test_context().await;
        let (owner_tx, mut owner_rx) = mpsc::channel(1);
        let owner_rpc = Arc::new(RpcOutbound::new(owner_tx));
        let owner_auth = auth_for(&ctx, "alice", 1001);
        ctx.tui_registry
            .register(crate::rpc::tui_identity::TuiEntry {
                tui_id: "owner-tui".to_string(),
                connected_at: chrono::Utc::now(),
                peer_label: "test".to_string(),
                transport: "unix".to_string(),
                env: std::collections::HashMap::new(),
                outbound: Some(owner_rpc),
                auth: Some(owner_auth),
            });

        let (issuer_tx, _issuer_rx) = mpsc::channel(4);
        let issuer = Arc::new(RpcOutbound::new(issuer_tx));
        let recipient = owner_turn_outbound(Arc::clone(&ctx), "owner-turn", &issuer)
            .await
            .expect("owner recipient should be admitted");
        let terminal =
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"type":"turn_complete"}}"#
                .to_string();
        assert!(recipient.send_raw(terminal.clone()).await);
        drop(recipient);

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), owner_rx.recv())
                .await
                .expect("terminal update should be delivered before wrapper shutdown")
                .expect("owner writer should remain open"),
            terminal
        );
    }
}
