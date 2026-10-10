//! Author: TechnoL0g
//!
//! SHIP-44 browser bridge: WebSocket ⇄ iroh. Browsers cannot open QUIC, so any node (typically a Service Node) can expose
//! `ws://host:port/ws/events[/<EndpointId>]` and `ws://host:port/ws/api[/<EndpointId>]`. One WebSocket message = one frame:
//! the bridge adds / strips the `[u32 LE len]` prefix and otherwise forwards bytes untouched, so the browser speaks exactly
//! the `sth/events/1` / `netfory/api/1` payloads (JSON or MessagePack). Without `<EndpointId>` the bridge targets the node it
//! runs in (over a private client endpoint — iroh refuses self-connections) or `ws_bridge.default_node`.

use super::wallet_api::{read_frame, write_frame, ALPN_API_V1, ALPN_EVENTS, MAX_REQUEST, MAX_RESPONSE};
use crate::error::{Error, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use iroh::endpoint::{presets, Connection};
use iroh::{Endpoint, EndpointAddr, EndpointId};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Clone)]
struct Bridge {
    /// Private client endpoint of the bridge (ephemeral key): dials the target node like a wallet would.
    client: Endpoint,
    node: Arc<super::IrohNode>,
    default_node: Option<EndpointId>,
}

impl Bridge {
    fn target(&self, id: Option<String>) -> Result<EndpointAddr> {
        match id.map(|s| super::parse_endpoint_id(&s)).transpose()?.or(self.default_node) {
            Some(id) if id != self.node.id() => Ok(EndpointAddr::from(id)),
            // ourselves: iroh refuses self-connections, dial the node's own socket addresses from the client endpoint
            _ => Ok(self.node.endpoint.addr()),
        }
    }

    async fn connect(&self, id: Option<String>, alpn: &'static [u8]) -> Result<Connection> {
        let target = self.target(id)?;
        // another node: the main endpoint already knows the sthpq address book, the Mainline DHT and the n1 relays —
        // no separate discovery path (and no n0) is needed; ourselves: iroh refuses self-connections, so the private
        // client endpoint dials our own socket addresses (pinned in its MemoryLookup, direct, no relay)
        let endpoint = if target.id == self.node.id() { &self.client } else { &self.node.endpoint };
        endpoint.connect(target, alpn).await.map_err(|e| Error::Sync(format!("bridge iroh connect: {e}")))
    }
}

/// Bind the bridge listener; `default_node = None` → this node.
pub async fn serve(node: Arc<super::IrohNode>, listen: SocketAddr, default_node: Option<EndpointId>) -> Result<()> {
    let (_, fut) = bind(node, listen, default_node).await?;
    fut.await
}

/// Bind and return the bound address plus the serving future (tests bind port 0).
pub async fn bind(node: Arc<super::IrohNode>, listen: SocketAddr, default_node: Option<EndpointId>) -> Result<(SocketAddr, impl std::future::Future<Output = Result<()>>)> {
    // the private client endpoint only ever dials THIS node's own sockets → Minimal preset: no n0 DNS, no relays at all
    let lookup = iroh::address_lookup::MemoryLookup::new();
    lookup.add_endpoint_info(node.endpoint.addr());
    let client = Endpoint::builder(presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .address_lookup(lookup)
        .bind()
        .await
        .map_err(|e| Error::Sync(format!("bridge endpoint bind: {e}")))?;
    let st = Bridge { client, node, default_node };
    let app = Router::new()
        .route("/", get(info))
        .route("/ws/events", get(|ws: WebSocketUpgrade, State(b): State<Bridge>| async { ws.on_upgrade(move |s| events(b, None, s)) }))
        .route("/ws/events/:node", get(|ws: WebSocketUpgrade, Path(n): Path<String>, State(b): State<Bridge>| async { ws.on_upgrade(move |s| events(b, Some(n), s)) }))
        .route("/ws/api", get(|ws: WebSocketUpgrade, State(b): State<Bridge>| async { ws.on_upgrade(move |s| api(b, None, s)) }))
        .route("/ws/api/:node", get(|ws: WebSocketUpgrade, Path(n): Path<String>, State(b): State<Bridge>| async { ws.on_upgrade(move |s| api(b, Some(n), s)) }))
        .with_state(st);
    let listener = tokio::net::TcpListener::bind(listen).await.map_err(|e| Error::Config(format!("ws bridge bind {listen}: {e}")))?;
    let bound = listener.local_addr().map_err(|e| Error::Config(format!("ws bridge addr: {e}")))?;
    tracing::info!(listen = %bound, "SHIP-44 WebSocket bridge: ws://{bound}/ws/events[/<EndpointId>], ws://{bound}/ws/api[/<EndpointId>]");
    Ok((bound, async move { axum::serve(listener, app).await.map_err(|e| Error::Sync(format!("ws bridge: {e}"))) }))
}

async fn info(State(b): State<Bridge>) -> impl IntoResponse {
    Json(json!({
        "data": {
            "service": "sth-core ws-bridge", "version": env!("CARGO_PKG_VERSION"), "node": b.node.id().to_string(),
            "defaultNode": b.default_node.map(|n| n.to_string()).unwrap_or_else(|| b.node.id().to_string()),
            "events": "/ws/events[/<EndpointId>]  — one WS message = one sth/events/1 frame (JSON text or MessagePack binary)",
            "api": "/ws/api[/<EndpointId>]  — one WS message = one MeshPacket JSON, one reply message = the signed response",
        }
    }))
}

fn payload(msg: Message) -> Option<Vec<u8>> {
    match msg {
        Message::Text(t) => Some(t.into_bytes()),
        Message::Binary(b) => Some(b),
        _ => None,
    }
}

fn to_message(bytes: Vec<u8>) -> Message {
    match std::str::from_utf8(&bytes) {
        Ok(_) if bytes.first() == Some(&b'{') => Message::Text(String::from_utf8(bytes).unwrap_or_default()),
        _ => Message::Binary(bytes),
    }
}

async fn close(mut socket: WebSocket, reason: String) {
    tracing::debug!(%reason, "ws bridge session closed");
    let _ = socket.send(Message::Text(json!({ "type": "error", "code": "bridge", "message": reason }).to_string())).await;
    let _ = socket.close().await;
}

/// `sth/events/1`: one bi-stream for the whole WebSocket session, frames pumped both ways.
async fn events(b: Bridge, node: Option<String>, socket: WebSocket) {
    let conn = match b.connect(node, ALPN_EVENTS).await {
        Ok(c) => c,
        Err(e) => return close(socket, e.to_string()).await,
    };
    let (mut send, mut recv) = match conn.open_bi().await {
        Ok(s) => s,
        Err(e) => return close(socket, format!("open_bi: {e}")).await,
    };
    let (mut ws_tx, mut ws_rx) = socket.split();
    let node_to_ws = tokio::spawn(async move {
        while let Ok(Some(bytes)) = read_frame(&mut recv, MAX_RESPONSE).await {
            if ws_tx.send(to_message(bytes)).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });
    while let Some(Ok(msg)) = ws_rx.next().await {
        let Some(bytes) = payload(msg) else { continue };
        if bytes.len() > MAX_REQUEST || write_frame(&mut send, &bytes).await.is_err() {
            break;
        }
    }
    let _ = send.finish();
    conn.close(0u32.into(), b"ws closed");
    node_to_ws.abort();
}

/// `netfory/api/1`: every WebSocket message is one MeshPacket; the reply message is the node's signed response.
async fn api(b: Bridge, node: Option<String>, mut socket: WebSocket) {
    let conn = match b.connect(node, ALPN_API_V1).await {
        Ok(c) => c,
        Err(e) => return close(socket, e.to_string()).await,
    };
    while let Some(Ok(msg)) = socket.recv().await {
        let Some(bytes) = payload(msg) else { continue };
        let reply = async {
            let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
            write_frame(&mut send, &bytes).await?;
            send.finish().map_err(|e| e.to_string())?;
            recv.read_to_end(MAX_RESPONSE).await.map_err(|e| e.to_string())
        }
        .await;
        let out = match reply {
            Ok(raw) => to_message(raw),
            Err(e) => Message::Text(json!({ "statusCode": 502, "error": "Bad Gateway", "message": format!("bridge: {e}") }).to_string()),
        };
        if socket.send(out).await.is_err() {
            break;
        }
    }
    conn.close(0u32.into(), b"ws closed");
}
