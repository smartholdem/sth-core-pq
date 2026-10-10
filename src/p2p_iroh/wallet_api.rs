//! Author: TechnoL0g
//!
//! SHIP-44 Wallet Stream API — wallets talk to the node over iroh instead of polling REST.
//! * `netfory/api/1` (+ legacy `netfory/api/0`, alias `sth/api/1`): one `MeshPacket` request per bi-stream, executed by the
//!   axum router in-process (no HTTP socket, no nginx) and answered in the netfory-provider `pdata` envelope signed by the
//!   node key (the EndpointId is the Ed25519 verification key). Every `/api/*` endpoint is reachable this way.
//! * `sth/events/1`: one long-lived bi-stream per wallet. The wallet subscribes to addresses / topics and the node pushes
//!   blocks, confirmed transactions, balances, mempool arrivals and SHIP-43 order updates the moment they happen.
//!
//! Frames on both protocols are `[u32 LE length][JSON]` (`netfory/api/0` is raw JSON + EOF).

use crate::api::{render, Shared};
use crate::models::{Block, Transaction};
use crate::node_config::WalletApiConfig;
use crate::storage::ChainEvent;
use axum::body::Body;
use axum::http::{header, HeaderName, HeaderValue, Method, Request};
use axum::Router;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use iroh::endpoint::{Connection, ReadExactError, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tower::ServiceExt;

pub const ALPN_API_V0: &[u8] = b"netfory/api/0";
pub const ALPN_API_V1: &[u8] = b"netfory/api/1";
pub const ALPN_API: &[u8] = b"sth/api/1";
pub const ALPN_EVENTS: &[u8] = b"sth/events/1";
/// Largest request frame (a v3 multipayment with 1024 recipients is ~250 KB of JSON).
pub const MAX_REQUEST: usize = 2 * 1024 * 1024;
/// Largest response frame (netfory-provider `MAX_FRAME`).
pub const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const MAX_COMMAND: usize = 64 * 1024;
/// Transactions replayed for `subscribe { sinceHeight }`; more → the wallet gets `resync` and pages over the API instead.
pub const MAX_REPLAY: usize = 200;
pub const TOPICS: &[&str] = &["blocks", "wallet", "mempool", "dex"];

/// netfory-provider request packet (`api://<EndpointId>/<provider>/<path>`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MeshPacket {
    #[serde(default)]
    pub target_provider: String,
    #[serde(default = "default_method")]
    pub method: String,
    pub path: String,
    #[serde(default, with = "b64")]
    pub body: Vec<u8>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_ttl")]
    pub ttl: u8,
    #[serde(default)]
    pub request_id: String,
}

fn default_method() -> String {
    "GET".into()
}

fn default_ttl() -> u8 {
    5
}

impl MeshPacket {
    pub fn new(method: &str, path: &str, body: Vec<u8>) -> Self {
        let mut id = [0u8; 12];
        let _ = getrandom::getrandom(&mut id);
        Self { target_provider: String::new(), method: method.to_string(), path: path.to_string(), body, headers: Default::default(), ttl: default_ttl(), request_id: hex::encode(id) }
    }
}

mod b64 {
    use super::{Engine, B64};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&B64.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        B64.decode(s.as_bytes()).map_err(serde::de::Error::custom)
    }
}

// ------------------------------------------------------------------ framing

pub async fn read_frame(recv: &mut RecvStream, max: usize) -> Result<Option<Vec<u8>>, String> {
    let mut len = [0u8; 4];
    match recv.read_exact(&mut len).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n == 0 || n > max {
        return Err(format!("frame length {n} out of range (max {max})"));
    }
    let mut buf = vec![0u8; n];
    recv.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    Ok(Some(buf))
}

pub async fn write_frame(send: &mut SendStream, body: &[u8]) -> Result<(), String> {
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
    send.write_all(&buf).await.map_err(|e| e.to_string())
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Payload codec of `sth/events/1` frames. The node advertises both in `hello.encodings`; a session starts in JSON and
/// switches when a command carries `"encoding": "msgpack"` (the reply to that command is already MessagePack).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    #[default]
    Json,
    MsgPack,
}

impl Encoding {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "json" => Some(Self::Json),
            "msgpack" | "messagepack" => Some(Self::MsgPack),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::MsgPack => "msgpack",
        }
    }
}

pub const ENCODINGS: &[&str] = &["json", "msgpack"];

pub fn encode_frame(v: &Value, enc: Encoding) -> Result<Vec<u8>, String> {
    match enc {
        Encoding::Json => serde_json::to_vec(v).map_err(|e| e.to_string()),
        Encoding::MsgPack => {
            let mv = rmpv::ext::to_value(v).map_err(|e| e.to_string())?;
            let mut buf = Vec::new();
            rmpv::encode::write_value(&mut buf, &mv).map_err(|e| e.to_string())?;
            Ok(buf)
        }
    }
}

/// Auto-detects the codec: JSON starts with `{`, everything else is read as MessagePack.
pub fn decode_frame(bytes: &[u8]) -> Result<(Value, Encoding), String> {
    if bytes.first() == Some(&b'{') {
        return serde_json::from_slice(bytes).map(|v| (v, Encoding::Json)).map_err(|e| format!("invalid JSON: {e}"));
    }
    let mv = rmpv::decode::read_value(&mut &bytes[..]).map_err(|e| format!("invalid MessagePack: {e}"))?;
    rmpv::ext::from_value::<Value>(mv).map(|v| (v, Encoding::MsgPack)).map_err(|e| format!("MessagePack is not a JSON-like map: {e}"))
}

// ------------------------------------------------------------------ server

#[derive(Clone)]
struct Attached {
    state: Shared,
    router: Router,
    cfg: WalletApiConfig,
}

struct Bucket {
    tokens: f64,
    updated: Instant,
}

#[derive(Default)]
pub struct Stats {
    pub requests: AtomicU64,
    pub rate_limited: AtomicU64,
    pub events_sent: AtomicU64,
    pub connections: AtomicUsize,
    pub sessions: AtomicUsize,
    pub rejected_busy: AtomicU64,
    /// Incoming connections that announced our ALPN but never completed the QUIC handshake (client gave up, relay path broken).
    pub handshake_failed: AtomicU64,
    /// Last `RECENT_CLIENTS` incoming connections (who knocks on the wallet api), newest last.
    pub recent: Mutex<std::collections::VecDeque<ClientEvent>>,
}

pub const RECENT_CLIENTS: usize = 20;

/// One incoming wallet-api connection for the operator ("who connects to my node?").
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientEvent {
    pub at_unix: u64,
    /// `netfory/api/1` or `sth/events/1`.
    pub alpn: &'static str,
    /// Direct `ip:port` or `relay <url> <peer>` — known before the handshake completes.
    pub remote: String,
    /// EndpointId (short) once the handshake completed.
    pub peer: Option<String>,
    /// `ok` or the handshake error (`timed out`, …).
    pub outcome: String,
}

impl Stats {
    fn record(&self, ev: ClientEvent) {
        let mut q = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= RECENT_CLIENTS {
            q.pop_front();
        }
        q.push_back(ev);
    }
}

fn describe_remote(addr: &iroh::endpoint::IncomingAddr) -> String {
    match addr {
        iroh::endpoint::IncomingAddr::Ip(a) => a.to_string(),
        iroh::endpoint::IncomingAddr::Relay { url, endpoint_id } => format!("relay {} {}", url.to_string().trim_start_matches("https://").trim_end_matches('/'), endpoint_id.fmt_short()),
        other => format!("{other:?}"),
    }
}

/// Shared `on_accepting` of both protocols: remember who knocked, log handshake failures at INFO with the remote
/// address (iroh's own WARN for this is filtered out by the default log filter — it carries no remote).
async fn accept_with_log(stats: &Stats, alpn: &'static str, accepting: iroh::endpoint::Accepting) -> std::result::Result<Connection, AcceptError> {
    let remote_addr = accepting.remote_addr();
    let remote = describe_remote(&remote_addr);
    let at_unix = now_secs();
    match accepting.await {
        Ok(conn) => {
            stats.record(ClientEvent { at_unix, alpn, remote, peer: Some(conn.remote_id().fmt_short().to_string()), outcome: "ok".into() });
            Ok(conn)
        }
        Err(e) => {
            stats.handshake_failed.fetch_add(1, Ordering::Relaxed);
            super::relay_guard::note_handshake_failure(&remote_addr);
            tracing::info!(alpn, remote = %remote, error = %e, "wallet api: incoming connection did not complete the handshake (client gave up or its relay path is broken) — not a node fault");
            stats.record(ClientEvent { at_unix, alpn, remote, peer: None, outcome: e.to_string() });
            Err(AcceptError::from_err(e))
        }
    }
}

/// Shared server state of both protocols; handlers are registered at endpoint bind, the REST state is attached later
/// (`attach`) once the API layer exists. Until then every request is answered with 503.
pub struct WalletApi {
    secret: SecretKey,
    attached: OnceLock<Attached>,
    limiter: Mutex<HashMap<EndpointId, Bucket>>,
    pub stats: Stats,
}

impl std::fmt::Debug for WalletApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WalletApi")
    }
}

struct Slot<'a>(&'a AtomicUsize);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl WalletApi {
    pub fn new(secret: SecretKey) -> Arc<Self> {
        Arc::new(Self { secret, attached: OnceLock::new(), limiter: Mutex::new(HashMap::new()), stats: Stats::default() })
    }

    /// Enable the protocols with the REST state they execute against. A second call is ignored.
    pub fn attach(&self, state: Shared, cfg: WalletApiConfig) {
        let router = crate::api::router(state.clone());
        let _ = self.attached.set(Attached { state, router, cfg });
    }

    pub fn enabled(&self) -> bool {
        self.attached.get().is_some()
    }

    pub fn config(&self) -> Option<&WalletApiConfig> {
        self.attached.get().map(|a| &a.cfg)
    }

    pub fn node_id(&self) -> EndpointId {
        self.secret.public()
    }

    /// netfory-provider style address of this node: `api://<EndpointId>/<name>`; REST paths are appended (`…/api/blocks`).
    pub fn api_url(&self, name: &str) -> String {
        format!("api://{}/{}", self.node_id(), name.trim_matches('/'))
    }

    /// `irohWalletApi` block of `/api/node/status` and the metrics page.
    pub fn stats_json(&self) -> Value {
        let cfg = self.config();
        json!({
            "enabled": cfg.is_some(),
            "url": self.api_url("sth-core"),
            "connections": self.stats.connections.load(Ordering::Relaxed),
            "maxConnections": cfg.map(|c| c.max_connections),
            "sessions": self.stats.sessions.load(Ordering::Relaxed),
            "requests": self.stats.requests.load(Ordering::Relaxed),
            "rateLimited": self.stats.rate_limited.load(Ordering::Relaxed),
            "rejectedBusy": self.stats.rejected_busy.load(Ordering::Relaxed),
            "eventsSent": self.stats.events_sent.load(Ordering::Relaxed),
            "handshakeFailed": self.stats.handshake_failed.load(Ordering::Relaxed),
            "recentClients": self.stats.recent.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect::<Vec<_>>(),
        })
    }

    fn admit(&self) -> Option<Slot<'_>> {
        let max = self.config().map(|c| c.max_connections).unwrap_or(0);
        let now = self.stats.connections.fetch_add(1, Ordering::Relaxed) + 1;
        let slot = Slot(&self.stats.connections);
        if max > 0 && now > max {
            self.stats.rejected_busy.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(slot)
    }

    /// Token bucket per EndpointId: `rate_limit_rps` sustained, 2× burst.
    fn allow(&self, peer: EndpointId) -> bool {
        let rps = self.config().map(|c| c.rate_limit_rps).unwrap_or(0);
        if rps == 0 {
            return true;
        }
        let burst = f64::from(rps) * 2.0;
        let now = Instant::now();
        let mut map = self.limiter.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > 10_000 {
            map.retain(|_, b| now.duration_since(b.updated) < Duration::from_secs(60));
        }
        let b = map.entry(peer).or_insert(Bucket { tokens: burst, updated: now });
        b.tokens = (b.tokens + now.duration_since(b.updated).as_secs_f64() * f64::from(rps)).min(burst);
        b.updated = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            self.stats.rate_limited.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// netfory-provider envelope: `pdata { v, node_id, name, status, signed_at, relayed, alg, sig, body_b64 }` next to the
    /// node's own JSON; signature over `node_id|status|signed_at|` + raw body.
    fn finalize(&self, name: &str, status: u16, raw: Vec<u8>) -> Vec<u8> {
        let node_id = self.node_id().to_string();
        let signed_at = now_secs();
        let mut canon = format!("{node_id}|{status}|{signed_at}|").into_bytes();
        canon.extend_from_slice(&raw);
        let sig = hex::encode(self.secret.sign(&canon).to_bytes());
        let pdata = json!({
            "v": 1, "node_id": node_id, "name": if name.is_empty() { "sth-core" } else { name }, "status": status,
            "signed_at": signed_at, "relayed": false, "alg": "ed25519", "sig": sig, "body_b64": B64.encode(&raw),
        });
        match serde_json::from_slice::<Value>(&raw) {
            Ok(Value::Object(mut m)) => {
                m.insert("pdata".into(), pdata);
                serde_json::to_vec(&Value::Object(m)).unwrap_or(raw)
            }
            _ => serde_json::to_vec(&json!({ "pdata": pdata })).unwrap_or(raw),
        }
    }

    fn error_body(status: u16, error: &str, detail: impl std::fmt::Display) -> Vec<u8> {
        serde_json::to_vec(&json!({ "statusCode": status, "error": error, "message": detail.to_string() })).unwrap_or_default()
    }

    /// Run one packet through the axum router (same handlers, same JSON as HTTP). Paths without the `/api` prefix
    /// (`api://<id>/api/blocks` parsed by a provider client as name `api` + path `/blocks`) are mapped onto `/api/...`.
    async fn execute(att: &Attached, pkt: &MeshPacket) -> (u16, Vec<u8>) {
        let method = Method::from_bytes(pkt.method.to_ascii_uppercase().as_bytes()).unwrap_or(Method::GET);
        let mut path = if pkt.path.starts_with('/') { pkt.path.clone() } else { format!("/{}", pkt.path) };
        let root = path == "/" || path.starts_with("/?") || path == "/status" || path.starts_with("/status?");
        if !root && !(path.starts_with("/api/") || path == "/api" || path.starts_with("/api?")) {
            path = format!("/api{path}");
        }
        let mut req = Request::builder().method(method).uri(&path);
        let mut has_ct = false;
        for (k, v) in &pkt.headers {
            let lk = k.to_ascii_lowercase();
            if matches!(lk.as_str(), "host" | "connection" | "content-length" | "transfer-encoding" | "upgrade" | "origin" | "referer" | "user-agent") {
                continue;
            }
            has_ct |= lk == "content-type";
            if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
                req = req.header(n, v);
            }
        }
        if !pkt.body.is_empty() && !has_ct {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        let req = match req.body(Body::from(pkt.body.clone())) {
            Ok(r) => r,
            Err(e) => return (400, Self::error_body(400, "Bad Request", e)),
        };
        match att.router.clone().oneshot(req).await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let bytes = axum::body::to_bytes(resp.into_body(), MAX_RESPONSE).await.map(|b| b.to_vec()).unwrap_or_default();
                (status, bytes)
            }
            Err(never) => match never {},
        }
    }

    async fn serve_request(&self, peer: EndpointId, mut send: SendStream, mut recv: RecvStream, v1: bool) -> Result<(), String> {
        let raw = if v1 {
            read_frame(&mut recv, MAX_REQUEST).await?.ok_or_else(|| "empty stream".to_string())?
        } else {
            recv.read_to_end(MAX_REQUEST).await.map_err(|e| e.to_string())?
        };
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        let pkt: MeshPacket = match serde_json::from_slice(&raw) {
            Ok(p) => p,
            Err(e) => {
                let body = self.finalize("", 400, Self::error_body(400, "Bad Request", format!("invalid MeshPacket: {e}")));
                return finish(&mut send, &body).await;
            }
        };
        let body = match self.attached.get() {
            None => self.finalize(&pkt.target_provider, 503, Self::error_body(503, "Service Unavailable", "wallet api is disabled on this node")),
            Some(_) if !self.allow(peer) => self.finalize(&pkt.target_provider, 429, Self::error_body(429, "Too Many Requests", "rate limit exceeded for this EndpointId")),
            Some(_) if pkt.method.eq_ignore_ascii_case("WS") => self.finalize(&pkt.target_provider, 400, Self::error_body(400, "Bad Request", "WS tunnels are not served here — use ALPN sth/events/1")),
            Some(att) => {
                let (status, raw) = Self::execute(att, &pkt).await;
                self.finalize(&pkt.target_provider, status, raw)
            }
        };
        finish(&mut send, &body).await
    }
}

async fn finish(send: &mut SendStream, body: &[u8]) -> Result<(), String> {
    send.write_all(body).await.map_err(|e| e.to_string())?;
    send.finish().map_err(|e| e.to_string())
}

/// `netfory/api/*` + `sth/api/1`: request/response streams.
#[derive(Clone, Debug)]
pub struct ApiHandler {
    api: Arc<WalletApi>,
    v1: bool,
}

impl ApiHandler {
    pub fn new(api: Arc<WalletApi>, v1: bool) -> Self {
        Self { api, v1 }
    }
}

impl ProtocolHandler for ApiHandler {
    async fn on_accepting(&self, accepting: iroh::endpoint::Accepting) -> std::result::Result<Connection, AcceptError> {
        accept_with_log(&self.api.stats, "netfory/api/1", accepting).await
    }

    async fn accept(&self, connection: Connection) -> std::result::Result<(), AcceptError> {
        let Some(_slot) = self.api.admit() else {
            connection.close(1u32.into(), b"too many connections");
            return Ok(());
        };
        let peer = connection.remote_id();
        while let Ok((send, recv)) = connection.accept_bi().await {
            let api = self.api.clone();
            let v1 = self.v1;
            tokio::spawn(async move {
                if let Err(e) = api.serve_request(peer, send, recv, v1).await {
                    tracing::debug!(peer = %peer.fmt_short(), error = %e, "wallet api stream");
                }
            });
        }
        Ok(())
    }
}

/// `sth/events/1`: push subscriptions.
#[derive(Clone, Debug)]
pub struct EventsHandler {
    api: Arc<WalletApi>,
}

impl EventsHandler {
    pub fn new(api: Arc<WalletApi>) -> Self {
        Self { api }
    }
}

impl ProtocolHandler for EventsHandler {
    async fn on_accepting(&self, accepting: iroh::endpoint::Accepting) -> std::result::Result<Connection, AcceptError> {
        accept_with_log(&self.api.stats, "sth/events/1", accepting).await
    }

    async fn accept(&self, connection: Connection) -> std::result::Result<(), AcceptError> {
        let Some(_slot) = self.api.admit() else {
            connection.close(1u32.into(), b"too many connections");
            return Ok(());
        };
        let Some(att) = self.api.attached.get().cloned() else {
            connection.close(2u32.into(), b"wallet api disabled");
            return Ok(());
        };
        let peer = connection.remote_id();
        let (send, recv) = connection.accept_bi().await.map_err(AcceptError::from_err)?;
        self.api.stats.sessions.fetch_add(1, Ordering::Relaxed);
        let _session_slot = Slot(&self.api.stats.sessions);
        let session = Session { api: self.api.clone(), att, peer, send, addresses: HashSet::new(), topics: Topics::default(), tokens: HashSet::new(), encoding: Encoding::Json };
        if let Err(e) = session.run(recv).await {
            tracing::debug!(peer = %peer.fmt_short(), error = %e, "wallet events session ended");
        }
        connection.close(0u32.into(), b"bye");
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct Topics {
    blocks: bool,
    wallet: bool,
    mempool: bool,
    dex: bool,
}

impl Default for Topics {
    fn default() -> Self {
        Self { blocks: false, wallet: true, mempool: true, dex: false }
    }
}

impl Topics {
    fn parse(v: &Value) -> Result<Self, String> {
        let Some(list) = v.as_array() else { return Ok(Self::default()) };
        let mut t = Self { blocks: false, wallet: false, mempool: false, dex: false };
        for item in list {
            match item.as_str() {
                Some("blocks") => t.blocks = true,
                Some("wallet") => t.wallet = true,
                Some("mempool") => t.mempool = true,
                Some("dex") => t.dex = true,
                other => return Err(format!("unknown topic {other:?} (known: {})", TOPICS.join(", "))),
            }
        }
        Ok(t)
    }

    fn names(&self) -> Vec<&'static str> {
        [(self.blocks, "blocks"), (self.wallet, "wallet"), (self.mempool, "mempool"), (self.dex, "dex")].into_iter().filter(|(on, _)| *on).map(|(_, n)| n).collect()
    }
}

struct Session {
    api: Arc<WalletApi>,
    att: Attached,
    peer: EndpointId,
    send: SendStream,
    addresses: HashSet<String>,
    topics: Topics,
    tokens: HashSet<String>,
    encoding: Encoding,
}

impl Session {
    async fn emit(&mut self, frame: Value) -> Result<(), String> {
        let body = encode_frame(&frame, self.encoding)?;
        write_frame(&mut self.send, &body).await?;
        self.api.stats.events_sent.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn run(mut self, mut recv: RecvStream) -> Result<(), String> {
        let st = self.att.state.clone();
        let mut chain = st.storage.subscribe_chain();
        let mut pool = st.mempool.subscribe();
        let height = st.storage.get_last_height().map_err(|e| e.to_string())?;
        let cfg = self.att.cfg.clone();
        tracing::debug!(peer = %self.peer.fmt_short(), "wallet events session opened");
        self.emit(json!({
            "type": "hello", "nodeId": self.api.node_id().to_string(), "nethash": st.network.nethash, "height": height,
            "coreVersion": env!("CARGO_PKG_VERSION"), "topics": TOPICS, "encodings": ENCODINGS, "encoding": self.encoding.name(),
            "limits": { "maxAddresses": cfg.max_addresses, "idleTimeoutSecs": cfg.idle_timeout_secs, "maxReplay": MAX_REPLAY },
        }))
        .await?;
        let idle = Duration::from_secs(cfg.idle_timeout_secs.max(30));
        let mut last_command = Instant::now();
        loop {
            tokio::select! {
                frame = read_frame(&mut recv, MAX_COMMAND) => match frame? {
                    Some(bytes) => {
                        last_command = Instant::now();
                        self.handle_command(&bytes).await?;
                    }
                    None => return Ok(()),
                },
                ev = chain.recv() => match ev {
                    Ok(ChainEvent::Applied(block)) => self.on_block(&block).await?,
                    Ok(ChainEvent::RolledBack { height }) => self.emit(json!({ "type": "rollback", "height": height })).await?,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => self.emit(json!({ "type": "resync", "reason": format!("{n} chain events dropped — refresh over the API") })).await?,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                },
                ev = pool.recv() => match ev {
                    Ok(txs) => self.on_mempool(&txs).await?,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                },
                _ = tokio::time::sleep_until((last_command + idle).into()) => {
                    let _ = self.emit(json!({ "type": "error", "code": "idle", "message": format!("no command for {} s — send ping to keep the session", idle.as_secs()) })).await;
                    return Ok(());
                }
            }
        }
    }

    async fn handle_command(&mut self, bytes: &[u8]) -> Result<(), String> {
        let cmd: Value = match decode_frame(bytes) {
            Ok((v, _)) => v,
            Err(e) => return self.emit(json!({ "type": "error", "code": "bad_frame", "message": e })).await,
        };
        // any command may switch the frame codec; the reply is already in the new one
        if let Some(enc) = cmd["encoding"].as_str() {
            match Encoding::parse(enc) {
                Some(e) => self.encoding = e,
                None => return self.emit(json!({ "type": "error", "code": "bad_encoding", "message": format!("unknown encoding {enc:?} (known: {})", ENCODINGS.join(", ")) })).await,
            }
        }
        let tip = self.att.state.storage.get_last_height().map_err(|e| e.to_string())?;
        match cmd["type"].as_str() {
            Some("ping") => self.emit(json!({ "type": "pong", "height": tip, "time": now_secs() })).await,
            Some("encoding") => self.emit(json!({ "type": "encoding", "encoding": self.encoding.name() })).await,
            Some("unsubscribe") => {
                self.addresses.clear();
                self.tokens.clear();
                self.topics = Topics { blocks: false, wallet: false, mempool: false, dex: false };
                self.emit(json!({ "type": "subscribed", "addresses": [], "topics": [], "height": tip, "replayed": 0 })).await
            }
            Some("subscribe") => self.subscribe(&cmd, tip).await,
            other => self.emit(json!({ "type": "error", "code": "unknown_command", "message": format!("unknown command {other:?} (subscribe | unsubscribe | ping | encoding)") })).await,
        }
    }

    /// `{ addresses, topics, tokens, sinceHeight }` → replaces the subscription; replays missed confirmed transactions.
    async fn subscribe(&mut self, cmd: &Value, tip: u64) -> Result<(), String> {
        let st = self.att.state.clone();
        let net = st.network.pubkey_hash;
        let addresses: Vec<String> = cmd["addresses"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
        if addresses.len() > self.att.cfg.max_addresses {
            return self.emit(json!({ "type": "error", "code": "too_many_addresses", "message": format!("{} addresses requested, this node allows {}", addresses.len(), self.att.cfg.max_addresses) })).await;
        }
        if let Some(bad) = addresses.iter().find(|a| !crate::crypto::validate_address(a, net)) {
            return self.emit(json!({ "type": "error", "code": "bad_address", "message": format!("invalid address {bad}") })).await;
        }
        let topics = match Topics::parse(&cmd["topics"]) {
            Ok(t) => t,
            Err(m) => return self.emit(json!({ "type": "error", "code": "bad_topic", "message": m })).await,
        };
        self.addresses = addresses.iter().cloned().collect();
        self.tokens = cmd["tokens"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
        self.topics = topics;
        let mut replayed = 0usize;
        let mut resync = false;
        if let Some(since) = cmd["sinceHeight"].as_u64().filter(|s| *s < tip && topics.wallet) {
            let mut seen: HashSet<String> = HashSet::new();
            let mut entries: Vec<crate::storage::TxIndexEntry> = Vec::new();
            'outer: for addr in &addresses {
                for entry in st.storage.transaction_index(Some(addr), since + 1..=tip, true) {
                    let entry = entry.map_err(|e| e.to_string())?;
                    if !seen.insert(entry.id.clone()) {
                        continue;
                    }
                    if entries.len() >= MAX_REPLAY {
                        resync = true;
                        break 'outer;
                    }
                    entries.push(entry);
                }
            }
            entries.sort_by_key(|e| (e.height, e.seq));
            for entry in entries {
                if let Some(tx) = st.storage.get_transaction(&entry.id).map_err(|e| e.to_string())? {
                    let ts = st.storage.get_block_by_height(entry.height).ok().flatten().map(|b| b.timestamp);
                    let hit: Vec<String> = crate::storage::tx_addresses(&tx, net).unwrap_or_default().into_iter().filter(|a| self.addresses.contains(a)).collect();
                    let tx_json = render::tx_json(&st, &tx, ts, false, tip).map_err(|e| e.1)?;
                    self.emit(json!({ "type": "tx", "addresses": hit, "height": entry.height, "replay": true, "tx": tx_json })).await?;
                    replayed += 1;
                }
            }
        }
        if topics.wallet {
            for addr in &addresses {
                self.emit_balance(addr).await?;
            }
        }
        if resync {
            self.emit(json!({ "type": "resync", "reason": format!("more than {MAX_REPLAY} transactions since the given height — page /api/wallets/<address>/transactions over the API") })).await?;
        }
        self.emit(json!({ "type": "subscribed", "addresses": addresses, "topics": topics.names(), "tokens": self.tokens, "height": tip, "replayed": replayed })).await
    }

    async fn emit_balance(&mut self, address: &str) -> Result<(), String> {
        let st = self.att.state.clone();
        let wallet = match st.storage.get_wallet(address).map_err(|e| e.to_string())? {
            Some(w) => render::wallet_json(&st, &w, None, None),
            None => json!({ "address": address, "publicKey": Value::Null, "balance": "0", "nonce": "0", "attributes": {} }),
        };
        self.emit(json!({ "type": "balance", "address": address, "wallet": wallet })).await
    }

    async fn on_block(&mut self, block: &Block) -> Result<(), String> {
        let st = self.att.state.clone();
        let tip = block.height;
        if self.topics.blocks {
            let header = render::block_json(&st, block, false, tip).map_err(|e| e.1)?;
            self.emit(json!({ "type": "block", "block": header })).await?;
        }
        if self.addresses.is_empty() && !self.topics.dex {
            return Ok(());
        }
        let net = st.network.pubkey_hash;
        let mut touched: Vec<String> = Vec::new();
        for tx in &block.transactions {
            let hit: Vec<String> = crate::storage::tx_addresses(tx, net).unwrap_or_default().into_iter().filter(|a| self.addresses.contains(a)).collect();
            if !hit.is_empty() && self.topics.wallet {
                let mut confirmed = tx.clone();
                confirmed.block_height = Some(block.height);
                if confirmed.block_id.is_none() {
                    confirmed.block_id = block.id.clone();
                }
                let tx_json = render::tx_json(&st, &confirmed, Some(block.timestamp), false, tip).map_err(|e| e.1)?;
                self.emit(json!({ "type": "tx", "addresses": hit, "height": block.height, "tx": tx_json })).await?;
                touched.extend(hit);
            }
            if self.topics.dex {
                self.on_dex_tx(tx).await?;
            }
        }
        if self.topics.wallet {
            if let Ok(generator) = crate::crypto::address_from_public_key(&block.generator_public_key, net) {
                if self.addresses.contains(&generator) && (block.total_fee > 0 || block.reward > 0) {
                    touched.push(generator);
                }
            }
            touched.sort();
            touched.dedup();
            for addr in touched {
                self.emit_balance(&addr).await?;
            }
        }
        Ok(())
    }

    /// SHIP-43: `order` after every TokenSell / TokenBuy / TokenCancel (plus `trade` for a buy), filtered by `tokens`.
    async fn on_dex_tx(&mut self, tx: &Transaction) -> Result<(), String> {
        use crate::models::token::{BUY, CANCEL, SELL};
        let Some(a) = tx.token_asset() else { return Ok(()) };
        if !matches!(tx.type_, SELL | BUY | CANCEL) || !(self.tokens.is_empty() || self.tokens.contains(&a.id)) {
            return Ok(());
        }
        let st = self.att.state.clone();
        let order_id = if tx.type_ == SELL { tx.id.clone() } else { a.order_id.clone() };
        let token = st.storage.token_state(&a.id).ok().flatten();
        if let Some(order) = order_id.and_then(|id| st.storage.token_order(&id).ok().flatten()) {
            let action = match tx.type_ { SELL => "sell", BUY => "buy", _ => "cancel" };
            self.emit(json!({ "type": "order", "tokenId": a.id, "symbol": token.as_ref().map(|t| t.symbol.clone()), "action": action, "txId": tx.id, "order": crate::api::tokens::order_json(&order, token.as_ref()) })).await?;
        }
        if tx.type_ == BUY {
            if let Some(tr) = st.storage.token_trades(&a.id).filter_map(|r| r.ok()).take(64).find(|t| Some(&t.tx_id) == tx.id.as_ref()) {
                self.emit(json!({
                    "type": "trade", "tokenId": a.id, "symbol": token.as_ref().map(|t| t.symbol.clone()),
                    "trade": { "txId": tr.tx_id, "orderId": tr.order_id, "buyer": tr.buyer, "seller": tr.seller, "amount": tr.amount.to_string(), "price": tr.price.to_string(), "total": tr.total.to_string(), "height": tr.height, "timestamp": tr.timestamp },
                }))
                .await?;
            }
        }
        Ok(())
    }

    async fn on_mempool(&mut self, txs: &[Transaction]) -> Result<(), String> {
        if !self.topics.mempool || self.addresses.is_empty() {
            return Ok(());
        }
        let st = self.att.state.clone();
        let net = st.network.pubkey_hash;
        let tip = st.storage.get_last_height().map_err(|e| e.to_string())?;
        for tx in txs {
            let hit: Vec<String> = crate::storage::tx_addresses(tx, net).unwrap_or_default().into_iter().filter(|a| self.addresses.contains(a)).collect();
            if hit.is_empty() {
                continue;
            }
            let tx_json = render::tx_json(&st, tx, None, false, tip).map_err(|e| e.1)?;
            let mut frame = json!({ "type": "mempoolTx", "addresses": hit, "tx": tx_json });
            if let Some(h) = st.mempool.fee_hint(tx) {
                frame["feeHint"] = h;
            }
            self.emit(frame).await?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ client (wallets, tests)

pub mod client {
    //! Reference client: what a Tauri / CLI wallet does. `call` = one signed REST call over iroh, `EventStream` = the push
    //! session. Both verify they talk to the EndpointId they dialled (QUIC/TLS) and `call` also checks the `pdata` signature.

    use super::*;
    use crate::error::{Error, Result};
    use iroh::{Endpoint, EndpointAddr, Signature};

    pub struct ApiResponse {
        pub status: u16,
        /// Node JSON with `pdata` removed.
        pub body: Value,
        pub pdata: Value,
    }

    /// `pdata.sig` is Ed25519 by `node_id` over `node_id|status|signed_at|` + `body_b64` bytes.
    pub fn verify_pdata(node_id: &EndpointId, pdata: &Value) -> bool {
        let (Some(id), Some(status), Some(signed_at), Some(sig), Some(body_b64)) =
            (pdata["node_id"].as_str(), pdata["status"].as_u64(), pdata["signed_at"].as_u64(), pdata["sig"].as_str(), pdata["body_b64"].as_str())
        else {
            return false;
        };
        if id != node_id.to_string() {
            return false;
        }
        let Ok(raw) = B64.decode(body_b64) else { return false };
        let mut canon = format!("{id}|{status}|{signed_at}|").into_bytes();
        canon.extend_from_slice(&raw);
        let Ok(sig_bytes) = hex::decode(sig) else { return false };
        let Ok(arr) = <[u8; 64]>::try_from(sig_bytes.as_slice()) else { return false };
        node_id.verify(&canon, &Signature::from_bytes(&arr)).is_ok()
    }

    /// One request on a fresh connection (`netfory/api/1`). Wallets keep the connection and open a bi-stream per request.
    pub async fn call(endpoint: &Endpoint, node: impl Into<EndpointAddr>, method: &str, path: &str, body: &[u8]) -> Result<ApiResponse> {
        let conn = endpoint.connect(node, ALPN_API_V1).await.map_err(|e| Error::Sync(format!("iroh connect: {e}")))?;
        let resp = call_on(&conn, method, path, body).await;
        conn.close(0u32.into(), b"done");
        resp
    }

    /// One request on an existing `netfory/api/1` connection.
    pub async fn call_on(conn: &Connection, method: &str, path: &str, body: &[u8]) -> Result<ApiResponse> {
        let pkt = MeshPacket::new(method, path, body.to_vec());
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| Error::Sync(format!("iroh open_bi: {e}")))?;
        write_frame(&mut send, &serde_json::to_vec(&pkt)?).await.map_err(Error::Sync)?;
        send.finish().map_err(|e| Error::Sync(format!("iroh finish: {e}")))?;
        let raw = recv.read_to_end(MAX_RESPONSE).await.map_err(|e| Error::Sync(format!("iroh read: {e}")))?;
        let mut v: Value = serde_json::from_slice(&raw)?;
        let pdata = v.as_object_mut().and_then(|m| m.remove("pdata")).ok_or_else(|| Error::Sync("response without pdata".into()))?;
        if !verify_pdata(&conn.remote_id(), &pdata) {
            return Err(Error::Sync("pdata signature does not verify against the node EndpointId".into()));
        }
        let status = pdata["status"].as_u64().unwrap_or(0) as u16;
        let body = pdata["body_b64"].as_str().and_then(|b| B64.decode(b).ok()).and_then(|raw| serde_json::from_slice(&raw).ok()).unwrap_or(v);
        Ok(ApiResponse { status, body, pdata })
    }

    /// `sth/events/1` session.
    pub struct EventStream {
        pub conn: Connection,
        send: SendStream,
        recv: RecvStream,
        pub hello: Value,
        /// Codec used for outgoing commands (the node auto-detects) — set by `set_encoding`.
        pub encoding: Encoding,
        /// Codec of the last frame received.
        pub last_encoding: Encoding,
    }

    impl EventStream {
        pub async fn connect(endpoint: &Endpoint, node: impl Into<EndpointAddr>) -> Result<Self> {
            let conn = endpoint.connect(node, ALPN_EVENTS).await.map_err(|e| Error::Sync(format!("iroh connect: {e}")))?;
            let (send, recv) = conn.open_bi().await.map_err(|e| Error::Sync(format!("iroh open_bi: {e}")))?;
            let mut s = Self { conn, send, recv, hello: Value::Null, encoding: Encoding::Json, last_encoding: Encoding::Json };
            // the server only learns about the stream once bytes arrive
            s.command(&json!({ "type": "ping" })).await?;
            s.hello = s.next().await?.ok_or_else(|| Error::Sync("closed before hello".into()))?;
            let _pong = s.next().await?;
            Ok(s)
        }

        pub async fn command(&mut self, cmd: &Value) -> Result<()> {
            write_frame(&mut self.send, &encode_frame(cmd, self.encoding).map_err(Error::Sync)?).await.map_err(Error::Sync)
        }

        /// Switch both directions to `enc` (mobile wallets: MessagePack is ~30 % smaller). The node answers `{ type: "encoding" }`.
        pub async fn set_encoding(&mut self, enc: Encoding) -> Result<()> {
            self.command(&json!({ "type": "encoding", "encoding": enc.name() })).await?;
            self.encoding = enc;
            Ok(())
        }

        pub async fn subscribe(&mut self, addresses: &[&str], topics: &[&str], since_height: Option<u64>) -> Result<()> {
            let mut cmd = json!({ "type": "subscribe", "addresses": addresses, "topics": topics });
            if let Some(h) = since_height {
                cmd["sinceHeight"] = json!(h);
            }
            self.command(&cmd).await
        }

        pub async fn ping(&mut self) -> Result<()> {
            self.command(&json!({ "type": "ping" })).await
        }

        /// Next frame, `None` when the node closed the stream.
        pub async fn next(&mut self) -> Result<Option<Value>> {
            match read_frame(&mut self.recv, MAX_RESPONSE).await.map_err(Error::Sync)? {
                Some(bytes) => {
                    let (v, enc) = decode_frame(&bytes).map_err(Error::Sync)?;
                    self.last_encoding = enc;
                    Ok(Some(v))
                }
                None => Ok(None),
            }
        }

        /// Frames until one of `type` arrives (others are returned in order before it).
        pub async fn wait_for(&mut self, type_: &str, timeout: Duration) -> Result<Vec<Value>> {
            let mut out = Vec::new();
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let frame = tokio::time::timeout_at(deadline, self.next()).await.map_err(|_| Error::Sync(format!("timeout waiting for {type_}")))??;
                match frame {
                    Some(f) => {
                        let done = f["type"].as_str() == Some(type_);
                        out.push(f);
                        if done {
                            return Ok(out);
                        }
                    }
                    None => return Err(Error::Sync("stream closed".into())),
                }
            }
        }
    }
}
