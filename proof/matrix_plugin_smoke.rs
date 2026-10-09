//! Temporary runner-only integration test for the pinned production factory.
//! No nested Cargo, native Matrix instance, model invocation or real config.
//! Encryption lookup and send are non-atomic; no race-free encryption claim.
#![cfg(feature = "plugins-wasm-cranelift")]

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use zeroclaw_api::channel::{Channel, ChannelMessage, SendMessage};
use zeroclaw_config::live::LiveConfig;
use zeroclaw_config::multi_agent::{AgentAlias, PeerGroupConfig, PeerUsername};
use zeroclaw_config::providers::{ChannelRef, ModelProviderRef};
use zeroclaw_config::schema::{
    AliasedAgentConfig, AnthropicModelProviderConfig, Config, PluginChannelConfig,
    PluginEntryConfig, RiskProfileConfig,
};
use zeroclaw_plugins::PluginCapability;
use zeroclaw_plugins::host::PluginHost;
use zeroclaw_plugins::instance::PluginInstanceScope;

const BOT: &str = "@bot:proof.test";
const SENDER: &str = "@sender:proof.test";
const DENY: &str = "@deny:proof.test";
const ROOM: &str = "!script:proof.test";
const WAIT: Duration = Duration::from_secs(45);

#[derive(Deserialize)]
struct User {
    id: String,
    token: String,
}
#[derive(Deserialize)]
struct Fixture {
    homeserver: String,
    room: String,
    bot: User,
    sender: User,
    deny: User,
}

fn passed(case: &'static str) -> Result<()> {
    let path = PathBuf::from(std::env::var("MATRIX_PROOF_PUBLIC")?).join("cases.jsonl");
    let mut out = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(out, "{}", json!({"case": case, "status": "passed"}))?;
    Ok(())
}

// Mirrors tests/channel_egress_e2e.rs construction, including the canonical
// instance key. The production factory owns egress, secrets and authorization.
fn operator_config(url: &str, token: &str, room: &str, grant: bool) -> Result<Config> {
    let package = PathBuf::from(std::env::var("MATRIX_PROOF_PACKAGE")?);
    let private = PathBuf::from(std::env::var("MATRIX_PROOF_PRIVATE")?);
    let host = PluginHost::from_plugins_dir(&package)?;
    let manifest = host.manifest("matrix").context("package admission")?;
    ensure!(
        manifest.name == "matrix" && manifest.version == "0.3.0",
        "manifest identity"
    );
    let scope = PluginInstanceScope::from_manifest(
        manifest,
        PluginCapability::Channel,
        "operations",
        manifest.permissions.iter().copied(),
    )?;
    let mut config = Config::default();
    config.config_path = private.join("operator.toml");
    config.data_dir = private.join("data");
    config.plugins.enabled = true;
    config.plugins.auto_discover = false;
    config.plugins.max_active_instances = 1;
    config.plugins.plugins_dir = package.display().to_string();
    ensure!(
        config.plugins.limits.call_timeout_ms == 30_000,
        "default host call limit"
    );
    config
        .risk_profiles
        .insert("default".into(), RiskProfileConfig::default());
    config
        .providers
        .models
        .anthropic
        .insert("default".into(), AnthropicModelProviderConfig::default());
    config.channels.plugin.insert(
        "operations".into(),
        PluginChannelConfig {
            package: "matrix".into(),
            enabled: true,
        },
    );
    config.agents.insert(
        "operator".into(),
        AliasedAgentConfig {
            enabled: true,
            channels: vec![ChannelRef::new("plugin.operations")],
            model_provider: ModelProviderRef::new("anthropic.default"),
            risk_profile: "default".into(),
            ..AliasedAgentConfig::default()
        },
    );
    config.peer_groups.insert(
        "proof".into(),
        PeerGroupConfig {
            channel: ChannelRef::new("plugin.operations"),
            agents: vec![AgentAlias::new("operator")],
            // Denied account is also explicitly granted: ignore must win.
            external_peers: vec![
                PeerUsername::new(SENDER),
                PeerUsername::new(DENY),
                PeerUsername::new(BOT),
            ],
            ignore: vec![PeerUsername::new(DENY)],
            ..PeerGroupConfig::default()
        },
    );
    config.plugins.entries.push(PluginEntryConfig {
        name: scope.id().config_entry_key()?,
        config: HashMap::from([
            ("homeserver".into(), url.into()),
            ("access_token".into(), token.into()),
            ("user_id".into(), BOT.into()),
            ("allowed_rooms".into(), json!([room]).to_string()),
            ("mention_only".into(), "false".into()),
            ("reply_in_thread".into(), "true".into()),
        ]),
        egress_hosts: if grant {
            vec!["127.0.0.1".into()]
        } else {
            vec![]
        },
        egress_allow_private: if grant {
            vec!["127.0.0.1".into()]
        } else {
            vec![]
        },
        tls_profiles: vec![],
    });
    ensure!(config.channels.matrix.is_empty(), "native Matrix forbidden");
    config.validate().context("synthetic operator config")?;
    Ok(config)
}

fn publish(live: &LiveConfig, edit: impl FnOnce(&mut Config)) -> Result<()> {
    let mut config = live.snapshot();
    edit(&mut config);
    live.publish(live.next_revision()?, config)?;
    Ok(())
}

async fn channels(config: &Config, live: &LiveConfig) -> Vec<Arc<dyn Channel>> {
    zeroclaw_runtime::plugin_runtime::configured_plugin_channels(
        Arc::new(config.clone()),
        Some(live.handle()),
    )
    .await
}

async fn activate(config: Config) -> Result<(LiveConfig, Arc<dyn Channel>)> {
    let live = LiveConfig::new(config.clone());
    let mut built = channels(&config, &live).await;
    ensure!(built.len() == 1, "one actual configured plugin required");
    let channel = built.remove(0);
    ensure!(channel.name() == "plugin", "host endpoint type");
    ensure!(
        channel.self_handle().as_deref() == Some(BOT),
        "authenticated self identity"
    );
    Ok((live, channel))
}

struct Listening {
    rx: mpsc::Receiver<ChannelMessage>,
    task: JoinHandle<Result<()>>,
}
impl Listening {
    fn start(channel: Arc<dyn Channel>, capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        let task = tokio::spawn(async move { channel.listen(tx).await });
        Self { rx, task }
    }
    async fn next(&mut self) -> Result<ChannelMessage> {
        timeout(WAIT, self.rx.recv())
            .await
            .context("inbound deadline")?
            .context("listener closed")
    }
    async fn stop(mut self) -> Result<()> {
        self.rx.close();
        match timeout(WAIT, &mut self.task).await {
            Ok(joined) => joined.context("listener task")??,
            Err(_) => {
                self.task.abort();
                let _ = self.task.await;
                anyhow::bail!("listener shutdown deadline");
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
enum Response {
    Json(u16, Value),
    Raw(u16, Vec<u8>),
    Close,
    StallHeaders,
    StallBody,
    TrickleBody,
}
struct Script {
    token: String,
    identity: String,
    hits: usize,
    whoami: usize,
    syncs: usize,
    incremental: usize,
    encryption_gets: usize,
    puts: usize,
    wrong_auth: usize,
    active_request: bool,
    batch: VecDeque<Value>,
    issued_batch: Option<(String, usize)>,
    sync_response: Option<Response>,
    encryption: Response,
    put_bodies: Vec<Value>,
}
impl Default for Script {
    fn default() -> Self {
        Self {
            token: "synthetic-current-token".into(),
            identity: BOT.into(),
            hits: 0,
            whoami: 0,
            syncs: 0,
            incremental: 0,
            encryption_gets: 0,
            puts: 0,
            wrong_auth: 0,
            active_request: false,
            batch: VecDeque::new(),
            issued_batch: None,
            sync_response: None,
            encryption: Response::Json(404, json!({"errcode": "M_NOT_FOUND"})),
            put_bodies: vec![],
        }
    }
}

// Cheap loopback HTTP/1.1 fixture. No header/auth logging. The optional relay
// observes completed real Synapse requests, so startup proof is not a sleep.
struct Server {
    url: String,
    state: Arc<Mutex<Script>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<()>>,
}
struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}
async fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut bytes = Vec::new();
    let split = loop {
        let mut buf = [0; 4096];
        let n = stream.read(&mut buf).await?;
        ensure!(
            n > 0 && bytes.len() + n <= 1024 * 1024,
            "fixture request bounds"
        );
        bytes.extend_from_slice(&buf[..n]);
        if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..split])?;
    let mut lines = headers.split("\r\n");
    let mut first = lines.next().context("request line")?.split_whitespace();
    let method = first.next().context("method")?.to_string();
    let path = first.next().context("path")?.to_string();
    let mut length = 0;
    let mut chunked = false;
    let mut authorization = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse::<usize>()?,
                "authorization" => authorization = Some(value.trim().to_string()),
                "transfer-encoding" => {
                    ensure!(
                        value.trim().eq_ignore_ascii_case("chunked"),
                        "fixture transfer encoding"
                    );
                    chunked = true;
                }
                _ => {}
            }
        }
    }
    ensure!(
        length <= 512 * 1024 && (!chunked || length == 0),
        "fixture body bounds"
    );
    if chunked {
        let mut wire = bytes[split..].to_vec();
        let mut body = Vec::new();
        loop {
            let line = loop {
                if let Some(n) = wire.windows(2).position(|w| w == b"\r\n") {
                    break n;
                }
                ensure!(wire.len() <= 1024, "chunk header bounds");
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).await?;
                ensure!(n > 0, "chunk header incomplete");
                wire.extend_from_slice(&buf[..n]);
            };
            let size = usize::from_str_radix(
                std::str::from_utf8(&wire[..line])?
                    .split(';')
                    .next()
                    .unwrap_or(""),
                16,
            )?;
            wire.drain(..line + 2);
            if size == 0 {
                break;
            }
            ensure!(body.len() + size <= 512 * 1024, "chunk body bounds");
            while wire.len() < size + 2 {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).await?;
                ensure!(n > 0, "chunk body incomplete");
                wire.extend_from_slice(&buf[..n]);
            }
            ensure!(&wire[size..size + 2] == b"\r\n", "chunk delimiter");
            body.extend_from_slice(&wire[..size]);
            wire.drain(..size + 2);
        }
        return Ok(Request {
            method,
            path,
            authorization,
            body,
        });
    }
    while bytes.len() - split < length {
        let mut buf = [0; 4096];
        let n = stream.read(&mut buf).await?;
        ensure!(n > 0, "fixture incomplete body");
        bytes.extend_from_slice(&buf[..n]);
    }
    Ok(Request {
        method,
        path,
        authorization,
        body: bytes[split..split + length].to_vec(),
    })
}

impl Server {
    async fn start(forward: Option<String>) -> Result<Self> {
        let socket = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", socket.local_addr()?);
        let state = Arc::new(Mutex::new(Script::default()));
        let shared = state.clone();
        let (shutdown, mut stopped) = oneshot::channel();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()?;
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = tokio::select! {
                    _ = &mut stopped => return Ok(()),
                    accepted = socket.accept() => accepted?,
                };
                shared.lock().unwrap().active_request = true;
                let served = async {
                    let request =
                        timeout(Duration::from_secs(5), read_request(&mut stream)).await??;
                    shared.lock().unwrap().hits += 1;
                    let response = if let Some(base) = &forward {
                        if request.method == "PUT" && request.path.contains("/send/m.room.message/")
                        {
                            shared.lock().unwrap().puts += 1;
                        }
                        if request.path.contains("/state/m.room.encryption/") {
                            shared.lock().unwrap().encryption_gets += 1;
                        }
                        let method = reqwest::Method::from_bytes(request.method.as_bytes())?;
                        let mut builder = client
                            .request(method, format!("{base}{}", request.path))
                            .header("Content-Type", "application/json")
                            .body(request.body.clone());
                        if let Some(auth) = &request.authorization {
                            builder = builder.header("Authorization", auth);
                        }
                        let response = builder.send().await.context("relay request")?;
                        let status = response.status().as_u16();
                        let body = response.bytes().await.context("relay response")?.to_vec();
                        if status == 200 && request.path.contains("/sync?") {
                            let mut state = shared.lock().unwrap();
                            state.syncs += 1;
                            if request.path.contains("since=") {
                                state.incremental += 1;
                            }
                        }
                        Response::Raw(status, body)
                    } else {
                        let mut state = shared.lock().unwrap();
                        let auth = request.authorization.as_deref()
                            == Some(format!("Bearer {}", state.token).as_str());
                        if !auth {
                            state.wrong_auth += 1;
                            Response::Json(401, json!({"errcode": "M_UNKNOWN_TOKEN"}))
                        } else if request.path.ends_with("/account/whoami") {
                            state.whoami += 1;
                            Response::Json(200, json!({"user_id": state.identity}))
                        } else if request.path.contains("/sync?") {
                            state.syncs += 1;
                            if request.path.contains("since=") {
                                state.incremental += 1;
                            }
                            state.sync_response.clone().unwrap_or_else(|| {
                                let value = if let Some(value) = state.batch.pop_front() {
                                    state.issued_batch = Some((value["next_batch"].as_str().unwrap_or("").into(), state.whoami));
                                    value
                                } else {
                                    json!({"next_batch": format!("cursor-{}", state.syncs), "rooms": {"join": {}}})
                                };
                                Response::Json(200, value)
                            })
                        } else if request.path.contains("/state/m.room.encryption/") {
                            state.encryption_gets += 1;
                            state.encryption.clone()
                        } else if request.method == "PUT"
                            && request.path.contains("/send/m.room.message/")
                        {
                            state.puts += 1;
                            state
                                .put_bodies
                                .push(serde_json::from_slice(&request.body)?);
                            Response::Json(200, json!({"event_id": format!("$put-{}", state.puts)}))
                        } else {
                            Response::Json(404, json!({"errcode": "M_UNRECOGNIZED"}))
                        }
                    };
                    let (status, body) = match response {
                        Response::Json(status, value) => (status, value.to_string().into_bytes()),
                        Response::Raw(status, bytes) => (status, bytes),
                        Response::Close => return Ok::<(), anyhow::Error>(()),
                        Response::StallHeaders => {
                            tokio::time::sleep(Duration::from_secs(27)).await;
                            return Ok(());
                        }
                        Response::StallBody => {
                            stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{").await?;
                            tokio::time::sleep(Duration::from_secs(27)).await;
                            return Ok(());
                        }
                        Response::TrickleBody => {
                            stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{").await?;
                            for _ in 0..54 {
                                tokio::time::sleep(Duration::from_millis(500)).await;
                                stream.write_all(b" ").await?;
                            }
                            return Ok(());
                        }
                    };
                    stream.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await?;
                    stream.write_all(&body).await?;
                    stream.shutdown().await?;
                    Ok(())
                };
                // Cancels an owned in-flight stall on shutdown. No detached
                // connection tasks survive the server's completed JoinHandle.
                tokio::select! {
                    _ = &mut stopped => return Ok(()),
                    result = served => {
                        shared.lock().unwrap().active_request = false;
                        let _ = result;
                    }
                }
            }
        });
        Ok(Self {
            url,
            state,
            shutdown: Some(shutdown),
            task,
        })
    }
    fn edit(&self, edit: impl FnOnce(&mut Script)) {
        edit(&mut self.state.lock().unwrap());
    }
    fn read<T>(&self, read: impl FnOnce(&Script) -> T) -> T {
        read(&self.state.lock().unwrap())
    }
    async fn wait(&self, predicate: impl Fn(&Script) -> bool) -> Result<()> {
        let deadline = Instant::now() + WAIT;
        loop {
            if self.read(&predicate) {
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "HTTP observation deadline");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    async fn stop(mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        match timeout(WAIT, &mut self.task).await {
            Ok(joined) => joined.context("server task")??,
            Err(_) => {
                self.task.abort();
                let _ = self.task.await;
                anyhow::bail!("server shutdown deadline");
            }
        }
        Ok(())
    }
}

fn endpoint(message: &ChannelMessage, room: &str) -> Result<()> {
    ensure!(
        message.channel == "plugin" && message.channel_alias.as_deref() == Some("operations"),
        "host-stamped endpoint"
    );
    ensure!(
        message.sender == SENDER && message.reply_target == room,
        "sender and room policy"
    );
    Ok(())
}
fn room_url(base: &str, room: &str, suffix: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(base)?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("base URL"))?
        .extend(["_matrix", "client", "v3", "rooms", room]);
    for part in suffix.split('/') {
        url.path_segments_mut().unwrap().push(part);
    }
    Ok(url.to_string())
}
async fn api(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    user: &User,
    body: Value,
) -> Result<Value> {
    let response = client
        .request(method, url)
        .bearer_auth(&user.token)
        .json(&body)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("synthetic API transport"))?;
    ensure!(response.status().is_success(), "synthetic API status");
    response
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("synthetic API JSON"))
}
async fn event(
    client: &reqwest::Client,
    fixture: &Fixture,
    user: &User,
    tag: &str,
    root: Option<&str>,
) -> Result<String> {
    let mut content = json!({"msgtype": "m.text", "body": tag});
    if let Some(root) = root {
        content["m.relates_to"] = json!({"rel_type": "m.thread", "event_id": root,
            "is_falling_back": true, "m.in_reply_to": {"event_id": root}});
    }
    let value = api(
        client,
        reqwest::Method::PUT,
        room_url(
            &fixture.homeserver,
            &fixture.room,
            &format!("send/m.room.message/{tag}"),
        )?,
        user,
        content,
    )
    .await?;
    Ok(value["event_id"]
        .as_str()
        .context("synthetic event ID")?
        .to_string())
}
async fn replied(
    client: &reqwest::Client,
    fixture: &Fixture,
    body: &str,
    root: &str,
) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        let response = client
            .get(room_url(&fixture.homeserver, &fixture.room, "messages")?)
            .query(&[("dir", "b"), ("limit", "100")])
            .bearer_auth(&fixture.sender.token)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("reply observation transport"))?;
        ensure!(response.status().is_success(), "reply observation status");
        let value: Value = response
            .json()
            .await
            .map_err(|_| anyhow::anyhow!("reply observation JSON"))?;
        if let Some(reply) = value["chunk"]
            .as_array()
            .context("reply events")?
            .iter()
            .find(|v| v["sender"] == BOT && v["content"]["body"] == body)
        {
            ensure!(
                reply["content"]["m.relates_to"]["rel_type"] == "m.thread",
                "outbound thread relation"
            );
            ensure!(
                reply["content"]["m.relates_to"]["event_id"] == root,
                "outbound root relation"
            );
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "reply event deadline");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn primary(server: &Server, fixture: &Fixture) -> Result<()> {
    ensure!(
        fixture.bot.id == BOT && fixture.sender.id == SENDER && fixture.deny.id == DENY,
        "synthetic users only"
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    let backlog = event(&client, fixture, &fixture.sender, "old-backlog", None)
        .await
        .context("proof-stage-startup-backlog")?;
    let config = operator_config(&server.url, &fixture.bot.token, &fixture.room, true)
        .context("proof-stage-operator-config")?;
    let (_live, channel) = activate(config.clone())
        .await
        .context("proof-stage-production-activation")?;
    passed("actual-package-ABI-config-authenticated-identity")?;
    let mut listening = Listening::start(channel.clone(), 8);
    let result = async {
        server.wait(|s| s.syncs >= 1 && s.incremental >= 1).await?;
        let root_a = event(&client, fixture, &fixture.sender, "top-level-a", None).await?;
        let inbound = listening.next().await?;
        ensure!(
            inbound.id == root_a && inbound.id != backlog,
            "startup backlog suppression"
        );
        endpoint(&inbound, &fixture.room)?;
        ensure!(
            inbound.interruption_scope_id.is_none()
                && inbound.thread_ts.as_deref() == Some(root_a.as_str()),
            "top-level root metadata"
        );
        channel
            .send(&SendMessage::reply_to(&inbound, "reply-top-level-a"))
            .await?;
        replied(&client, fixture, "reply-top-level-a", &root_a).await?;
        passed("real-unencrypted-roundtrip-top-level-root-backlog-suppression")?;
        let root_b = event(&client, fixture, &fixture.sender, "top-level-b", None).await?;
        let inbound = listening.next().await?;
        ensure!(inbound.id == root_b, "second root inbound");
        for (tag, root) in [("thread-a", &root_a), ("thread-b", &root_b)] {
            let id = event(&client, fixture, &fixture.sender, tag, Some(root)).await?;
            let inbound = listening.next().await?;
            endpoint(&inbound, &fixture.room)?;
            ensure!(
                inbound.id == id && inbound.thread_ts.as_deref() == Some(root.as_str()),
                "genuine thread root"
            );
            ensure!(
                inbound.interruption_scope_id.as_deref() == Some(root.as_str()),
                "thread interruption isolation"
            );
            let reply = format!("reply-{tag}");
            channel
                .send(&SendMessage::reply_to(&inbound, &reply))
                .await?;
            replied(&client, fixture, &reply, root).await?;
        }
        ensure!(root_a != root_b, "two distinct thread roots");
        passed("two-real-threads-interruption-metadata-outbound-root")?;
        let deny = event(&client, fixture, &fixture.deny, "deny-body", None).await?;
        let echo = event(&client, fixture, &fixture.bot, "self-echo-body", None).await?;
        let sentinel = event(&client, fixture, &fixture.sender, "policy-sentinel", None).await?;
        let inbound = listening.next().await?;
        ensure!(
            inbound.id == sentinel && inbound.id != deny && inbound.id != echo,
            "deny/self never invoke responder"
        );
        endpoint(&inbound, &fixture.room)?;
        passed("real-explicit-sender-deny-and-self-echo-suppression")?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let stopped = listening.stop().await;
    result?;
    stopped?;
    drop(channel);
    let (_live, channel) = activate(config).await?;
    let syncs = server.read(|s| s.syncs);
    let mut listening = Listening::start(channel.clone(), 8);
    let result = async {
        server.wait(|s| s.syncs >= syncs + 2).await?;
        let id = event(&client, fixture, &fixture.sender, "restart-text", None).await?;
        let inbound = listening.next().await?;
        ensure!(inbound.id == id, "restart resync inbound");
        channel
            .send(&SendMessage::reply_to(&inbound, "reply-restart"))
            .await?;
        replied(&client, fixture, "reply-restart", &id).await?;
        passed("instance-recreation-resync-new-roundtrip")?;
        api(
            &client,
            reqwest::Method::PUT,
            room_url(
                &fixture.homeserver,
                &fixture.room,
                "state/m.room.encryption/",
            )?,
            &fixture.bot,
            json!({"algorithm": "m.megolm.v1.aes-sha2"}),
        )
        .await?;
        let preflight = server.read(|s| (s.puts, s.encryption_gets));
        ensure!(
            channel
                .send(&SendMessage::new("must-not-send-encrypted", &fixture.room))
                .await
                .is_err(),
            "real encryption refusal"
        );
        ensure!(
            server.read(|s| (s.puts, s.encryption_gets)) == (preflight.0, preflight.1 + 1),
            "real encryption preflight reached, zero PUTs"
        );
        let response = client
            .get(room_url(&fixture.homeserver, &fixture.room, "messages")?)
            .query(&[("dir", "b"), ("limit", "100")])
            .bearer_auth(&fixture.sender.token)
            .send()
            .await?;
        let value: Value = response.json().await?;
        ensure!(
            !value["chunk"]
                .as_array()
                .context("encryption events")?
                .iter()
                .any(|v| v["content"]["body"] == "must-not-send-encrypted"),
            "encrypted plaintext absent"
        );
        passed("real-enable-encryption-refuses-plaintext")?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let stopped = listening.stop().await;
    result?;
    stopped?;
    passed("owned-listeners-finished")
}

fn batch(tag: &str, events: Vec<Value>) -> Value {
    json!({"next_batch": tag, "rooms": {"join": {(ROOM): {
        "summary": {"m.joined_member_count": 3},
        "timeline": {"events": events, "limited": false}
    }}}})
}
fn scripted_event(id: &str, body: &str) -> Value {
    json!({"event_id": id, "type": "m.room.message", "sender": SENDER,
           "origin_server_ts": 1, "content": {"msgtype": "m.text", "body": body}})
}

async fn scripted(server: &Server) -> Result<()> {
    let config = operator_config(&server.url, "synthetic-current-token", ROOM, true)?;
    let mut bad = config.clone();
    bad.plugins.entries[0]
        .config
        .insert("access_token".into(), "invalid-synthetic-token".into());
    let live = LiveConfig::new(bad.clone());
    let wrong_auth = server.read(|s| s.wrong_auth);
    ensure!(
        channels(&bad, &live).await.is_empty(),
        "bad token must construct zero channels"
    );
    ensure!(
        server.read(|s| s.wrong_auth) > wrong_auth,
        "bad token reached authentication endpoint"
    );
    passed("bad-token-configure-zero-channels")?;
    let mut unreadable = config.clone();
    unreadable.plugins.entries[0].config.remove("homeserver");
    let live = LiveConfig::new(unreadable.clone());
    ensure!(
        channels(&unreadable, &live).await.is_empty(),
        "unreadable config must construct zero channels"
    );
    passed("invalid-config-configure-zero-channels")?;
    let denied = operator_config(&server.url, "synthetic-current-token", ROOM, false)?;
    let live = LiveConfig::new(denied.clone());
    let hits = server.read(|s| s.hits);
    ensure!(
        channels(&denied, &live).await.is_empty(),
        "denied egress must construct zero channels"
    );
    ensure!(
        server.read(|s| s.hits) == hits,
        "denied egress zero network hits"
    );
    passed("denied-egress-configure-zero-channels-zero-hits")?;
    let (live, channel) = activate(config.clone()).await?;
    channel
        .send(&SendMessage::new("baseline-send", ROOM))
        .await?;
    ensure!(
        server.read(|s| s
            .put_bodies
            .last()
            .is_some_and(|v| v["body"] == "baseline-send")),
        "actual outbound plaintext payload"
    );
    passed("authenticated-not-found-is-the-only-plaintext-permission")?;
    server.edit(|s| s.token = "synthetic-rotated-token".into());
    publish(&live, |c| {
        c.plugins.entries[0]
            .config
            .insert("access_token".into(), "synthetic-rotated-token".into());
    })?;
    let before = server.read(|s| (s.puts, s.wrong_auth));
    channel
        .send(&SendMessage::new("rotated-send", ROOM))
        .await?;
    ensure!(
        server.read(|s| (s.puts, s.wrong_auth)) == (before.0 + 1, before.1),
        "current scoped secret on every request"
    );
    ensure!(
        server.read(|s| s
            .put_bodies
            .last()
            .is_some_and(|v| v["body"] == "rotated-send")),
        "rotated outbound payload"
    );
    passed("same-account-live-token-rotation-current-secret")?;
    let refusals = [
        (
            "encryption-present",
            Response::Json(200, json!({"algorithm": "m.megolm.v1.aes-sha2"})),
        ),
        (
            "generic-404",
            Response::Json(404, json!({"errcode": "M_UNRECOGNIZED"})),
        ),
        ("malformed-state", Response::Raw(404, b"not-json".to_vec())),
        (
            "auth-failure",
            Response::Json(401, json!({"errcode": "M_UNKNOWN_TOKEN"})),
        ),
        (
            "permission-failure",
            Response::Json(403, json!({"errcode": "M_FORBIDDEN"})),
        ),
        (
            "server-failure",
            Response::Json(500, json!({"errcode": "M_UNKNOWN"})),
        ),
        ("network-close", Response::Close),
        ("response-stall", Response::StallHeaders),
        ("body-stall", Response::StallBody),
        ("aggregate-deadline-trickle", Response::TrickleBody),
    ];
    for (case, response) in refusals {
        server.edit(|s| s.encryption = response);
        let puts = server.read(|s| s.puts);
        let preflight = server.read(|s| s.encryption_gets);
        let refusal = timeout(
            Duration::from_secs(26),
            channel.send(&SendMessage::new("refusal-body", ROOM)),
        )
        .await;
        let error = refusal
            .context("guest aggregate deadline before host cap")?
            .err()
            .context("bounded encryption uncertainty refusal")?;
        if case == "aggregate-deadline-trickle" {
            ensure!(
                error.to_string().contains("HTTP deadline exceeded"),
                "guest deadline classification"
            );
        }
        ensure!(
            server.read(|s| s.puts) == puts,
            "uncertain encryption zero PUTs"
        );
        ensure!(
            server.read(|s| s.encryption_gets) > preflight,
            "uncertainty case must reach encryption endpoint"
        );
        // A timed-out guest request can outlive the call in this intentionally
        // serial fixture. Observe fixture completion before changing its mode.
        server.wait(|s| !s.active_request).await?;
        passed(case)?;
    }
    server.edit(|s| s.encryption = Response::Json(404, json!({"errcode": "M_NOT_FOUND"})));
    channel
        .send(&SendMessage::new("post-refusal-positive-control", ROOM))
        .await?;
    passed("post-refusal-positive-send-control")?;
    let hits = server.read(|s| s.hits);
    publish(&live, |c| {
        c.plugins.entries[0].egress_hosts.clear();
        c.plugins.entries[0].egress_allow_private.clear();
    })?;
    ensure!(
        channel
            .send(&SendMessage::new("live-denied-egress", ROOM))
            .await
            .is_err(),
        "live egress refusal"
    );
    ensure!(
        server.read(|s| s.hits) == hits,
        "live denied egress zero hits"
    );
    passed("live-egress-revocation-zero-hits")?;
    publish(&live, |c| {
        c.plugins.entries[0].egress_hosts = vec!["127.0.0.1".into()];
        c.plugins.entries[0].egress_allow_private = vec!["127.0.0.1".into()];
    })?;
    server.edit(|s| s.identity = "@other:proof.test".into());
    let puts = server.read(|s| s.puts);
    ensure!(
        channel
            .send(&SendMessage::new("changed-account", ROOM))
            .await
            .is_err(),
        "changed authenticated identity refusal"
    );
    server.edit(|s| s.identity = BOT.into());
    ensure!(
        channel
            .send(&SendMessage::new("restored-account", ROOM))
            .await
            .is_err(),
        "account invalidation is sticky"
    );
    ensure!(
        server.read(|s| s.puts) == puts,
        "account invalidation zero PUTs"
    );
    passed("account-binding-invalidates-until-reconfigure")?;
    drop(channel);
    let mut config = live.snapshot();
    let (live, channel) = activate(config.clone()).await?;
    publish(&live, |c| {
        c.plugins.entries[0]
            .config
            .insert("homeserver".into(), format!("{}/changed", server.url));
    })?;
    let hits = server.read(|s| s.hits);
    ensure!(
        channel
            .send(&SendMessage::new("changed-homeserver", ROOM))
            .await
            .is_err(),
        "homeserver binding refusal"
    );
    publish(&live, |c| {
        c.plugins.entries[0]
            .config
            .insert("homeserver".into(), server.url.clone());
    })?;
    ensure!(
        channel
            .send(&SendMessage::new("restored-homeserver", ROOM))
            .await
            .is_err(),
        "homeserver invalidation is sticky"
    );
    ensure!(
        server.read(|s| s.hits) == hits,
        "changed binding makes no network request"
    );
    passed("homeserver-binding-invalidates-until-reconfigure")?;
    drop(channel);
    config.plugins.entries[0]
        .config
        .insert("access_token".into(), "synthetic-rotated-token".into());
    let (_live, channel) = activate(config.clone()).await?;
    let listening = Listening::start(channel.clone(), 8);
    let result = async {
        let syncs = server.read(|s| s.syncs);
        server
            .wait(|s| s.syncs > syncs && s.incremental > 0)
            .await?;
        for response in [
            Response::Json(500, json!({"errcode": "M_UNKNOWN"})),
            Response::Raw(200, b"not-json".to_vec()),
            Response::Json(200, json!({"next_batch": "broken", "rooms": []})),
        ] {
            let syncs = server.read(|s| s.syncs);
            server.edit(|s| s.sync_response = Some(response));
            server.wait(|s| s.syncs > syncs).await?;
            let deadline = Instant::now() + WAIT;
            while channel.health_check().await {
                ensure!(Instant::now() < deadline, "sync failure health observation");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let whoami = server.read(|s| s.whoami);
            ensure!(
                !channel.health_check().await,
                "successful whoami cannot clear sync failure"
            );
            ensure!(
                server.read(|s| s.whoami) > whoami,
                "identity-only probe actually succeeded"
            );
            let syncs = server.read(|s| s.syncs);
            server.edit(|s| s.sync_response = None);
            server.wait(|s| s.syncs > syncs).await?;
            let deadline = Instant::now() + WAIT;
            while !channel.health_check().await {
                ensure!(
                    Instant::now() < deadline,
                    "sync recovery health observation"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        passed("sync-failure-malformed-health-sticky-identity-only-recovery")
    }
    .await;
    let stopped = listening.stop().await;
    result?;
    stopped?;
    drop(channel);
    queued_policy(server, config).await
}

async fn queued_policy(server: &Server, config: Config) -> Result<()> {
    let (live, channel) = activate(config).await?;
    let mut listening = Listening::start(channel.clone(), 1);
    let result = async {
        let syncs = server.read(|s| s.syncs);
        server.wait(|s| s.syncs > syncs).await?;
        // Two events can have crossed poll_message before publication (one in
        // mpsc and one blocked on tx.send). Subsequent events remain guest-owned.
        for (case, key, value) in [
            ("queued-live-mention-policy", "mention_only", "true".to_string()),
            ("queued-live-room-policy", "allowed_rooms", json!(["!other:proof.test"]).to_string()),
        ] {
            let before_one = format!("${case}-before-1");
            let before_two = format!("${case}-before-2");
            let queued_three = format!("${case}-queued-3");
            let queued_four = format!("${case}-queued-4");
            let sentinel_id = format!("${case}-sentinel");
            publish(&live, |c| {
                c.plugins.entries[0].config.insert("mention_only".into(), "false".into());
                c.plugins.entries[0].config.insert("allowed_rooms".into(), json!([ROOM]).to_string());
            })?;
            server.edit(|s| s.batch.push_back(batch(case, vec![
                scripted_event(&before_one, "before-one"), scripted_event(&before_two, "before-two"),
                scripted_event(&queued_three, "must-be-filtered"), scripted_event(&queued_four, "must-be-filtered"),
            ])));
            // The observed batch response plus the next whoami proves the
            // first export completed and the second poll began. Capacity one
            // prevents a third export until the receiver is drained.
            server.wait(|s| s.issued_batch.as_ref().is_some_and(|(tag, auth)| tag == case && s.whoami > *auth)).await?;
            publish(&live, |c| { c.plugins.entries[0].config.insert(key.into(), value.clone()); })?;
            let one = listening.next().await?;
            ensure!(one.id == before_one, "queued first event");
            // A policy change may win before the second export. Do not require
            // it; use a permitted marker in the next sync as the drain barrier.
            let mut sentinel = scripted_event(&sentinel_id, &format!("marker {BOT}"));
            sentinel["content"]["m.mentions"] = json!({"user_ids": [BOT]});
            let mut marker = batch("queue-drained", vec![sentinel]);
            if key == "allowed_rooms" {
                marker["rooms"]["join"] = json!({"!other:proof.test": {
                    "summary": {"m.joined_member_count": 3}, "timeline": {"events": [scripted_event(&sentinel_id, "marker")], "limited": false}
                }});
            }
            server.edit(|s| s.batch.push_back(marker));
            loop {
                let next = listening.next().await?;
                ensure!(next.id != queued_three && next.id != queued_four, "queued event escaped live policy");
                if next.id == sentinel_id { break; }
                ensure!(next.id == before_two, "unexpected queue event");
            }
            passed(case)?;
        }
        // Make config unreadable during an already configured instance.
        let puts = server.read(|s| s.puts);
        publish(&live, |c| { c.plugins.entries[0].config.remove("homeserver"); })?;
        ensure!(channel.send(&SendMessage::new("invalid-live-config", ROOM)).await.is_err(), "live config fail closed");
        ensure!(server.read(|s| s.puts) == puts, "invalid config zero PUTs");
        passed("live-unreadable-config-fails-closed")
    }.await;
    let stopped = listening.stop().await;
    result?;
    stopped
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn matrix_plugin_smoke() -> Result<()> {
    let capture_installed = zeroclaw_log::try_install_line_sink_for_tests(|line| eprint!("{line}"));
    eprintln!("proof-runtime-entered capture-installed={capture_installed}");
    ensure!(
        !cfg!(feature = "channel-matrix"),
        "native Matrix build forbidden"
    );
    let config_path = PathBuf::from(std::env::var("MATRIX_PROOF_CONFIG")?);
    let private = PathBuf::from(std::env::var("MATRIX_PROOF_PRIVATE")?).canonicalize()?;
    ensure!(
        config_path.canonicalize()?.starts_with(&private),
        "private fixture path"
    );
    ensure!(
        std::fs::metadata(&config_path)?.permissions().mode() & 0o777 == 0o600,
        "fixture mode 0600"
    );
    let fixture: Fixture = serde_json::from_slice(&std::fs::read(&config_path)?)?;
    let url = reqwest::Url::parse(&fixture.homeserver)?;
    ensure!(
        url.scheme() == "http"
            && url.port() == Some(8008)
            && url
                .host_str()
                .and_then(|host| host.parse::<std::net::Ipv4Addr>().ok())
                .is_some_and(|address| address.is_private())
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "test-owned internal bridge fixture only"
    );
    let relay = Server::start(Some(fixture.homeserver.clone()))
        .await
        .context("proof-stage-relay-start")?;
    let result = primary(&relay, &fixture).await;
    let stopped = relay.stop().await;
    result.context("proof-stage-primary")?;
    stopped.context("proof-stage-relay-stop")?;
    let server = Server::start(None)
        .await
        .context("proof-stage-scripted-start")?;
    let result = scripted(&server).await;
    let stopped = server.stop().await;
    result.context("proof-stage-scripted")?;
    stopped.context("proof-stage-scripted-stop")?;
    passed("all-owned-HTTP-fixtures-finished")?;
    Ok(())
}
