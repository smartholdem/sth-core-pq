//! Author: TechnoL0g
//!
//! `sth/rpc/1` protocol: one JSON request per bidirectional stream, one JSON response back.
//! Server side = `RpcHandler` (iroh `ProtocolHandler`), client side = `fetch_status` / `fetch_blocks`.

use super::peers::IrohPeers;
use super::proto::{Request, Response, VERSION};
use crate::error::{Error, Result};
use crate::models::Block;
use crate::storage::Storage;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr};
use std::sync::Arc;
use std::time::Instant;

pub const ALPN: &[u8] = b"sth/rpc/1";
/// Hard cap of `GetBlocks` (same as the legacy port).
pub const MAX_BLOCKS: u32 = 400;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct RpcHandler {
    storage: Arc<Storage>,
    peers: Arc<IrohPeers>,
    addrbook: Arc<std::sync::OnceLock<Arc<super::addrbook::AddrBook>>>,
}

impl std::fmt::Debug for RpcHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RpcHandler")
    }
}

impl RpcHandler {
    pub fn new(storage: Arc<Storage>, peers: Arc<IrohPeers>, addrbook: Arc<std::sync::OnceLock<Arc<super::addrbook::AddrBook>>>) -> Self {
        Self { storage, peers, addrbook }
    }

    fn handle(&self, req: Request) -> Response {
        match req {
            Request::GetStatus => match self.storage.get_last_block() {
                Ok(last) => Response::Status {
                    version: VERSION,
                    core_version: env!("CARGO_PKG_VERSION").to_string(),
                    nethash: self.storage.network().nethash.clone(),
                    height: last.as_ref().map(|b| b.height).unwrap_or(0),
                    id: last.and_then(|b| b.id),
                },
                Err(e) => Response::Error { message: e.to_string() },
            },
            Request::GetBlocks { last_block_height, limit } => {
                match self.storage.get_blocks_from(last_block_height + 1, limit.clamp(1, MAX_BLOCKS) as usize) {
                    Ok(blocks) => Response::Blocks { blocks },
                    Err(e) => Response::Error { message: e.to_string() },
                }
            }
            Request::GetFinality => match self.storage.latest_finality_cert() {
                Ok(certificate) => Response::Finality { certificate },
                Err(e) => Response::Error { message: e.to_string() },
            },
            Request::GetAddrs { limit } => match self.addrbook.get() {
                Some(book) => Response::Addrs { records: book.fresh(limit as usize) },
                None => Response::Error { message: "address book disabled".into() },
            },
        }
    }
}

impl ProtocolHandler for RpcHandler {
    async fn accept(&self, connection: Connection) -> std::result::Result<(), AcceptError> {
        let remote = connection.remote_id();
        self.peers.seen(remote, None);
        // Serve requests until the client closes the connection.
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let raw = recv.read_to_end(MAX_REQUEST_BYTES).await.map_err(AcceptError::from_err)?;
            let response = match serde_json::from_slice::<Request>(&raw) {
                Ok(req) => self.handle(req),
                Err(e) => Response::Error { message: format!("invalid request: {e}") },
            };
            let body = serde_json::to_vec(&response).map_err(AcceptError::from_err)?;
            send.write_all(&body).await.map_err(AcceptError::from_err)?;
            send.finish().map_err(AcceptError::from_err)?;
        }
        Ok(())
    }
}

/// Outgoing RPC connections, one per peer, reused across calls (a QUIC stream per request). Before, every call opened
/// and closed a fresh connection: handshake + holepunching each time, the first packets always through the relay —
/// forger probes every slot and the 60 s refresh kept the relays busy ("Dropping received relay packet").
fn pool() -> &'static std::sync::Mutex<std::collections::HashMap<iroh::EndpointId, Connection>> {
    static POOL: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<iroh::EndpointId, Connection>>> = std::sync::OnceLock::new();
    POOL.get_or_init(Default::default)
}

/// Pooled RPC connections (memory line gauge); closed ones are dropped here.
pub fn pool_len() -> usize {
    let mut p = pool().lock().unwrap_or_else(|e| e.into_inner());
    p.retain(|_, c| c.close_reason().is_none());
    p.len()
}

async fn connection(endpoint: &Endpoint, peer: EndpointAddr) -> Result<Connection> {
    let id = peer.id;
    let cached = pool().lock().unwrap_or_else(|e| e.into_inner()).get(&id).filter(|c| c.close_reason().is_none()).cloned();
    if let Some(c) = cached {
        return Ok(c);
    }
    let conn = endpoint.connect(peer, ALPN).await.map_err(|e| Error::Sync(format!("iroh connect: {e}")))?;
    pool().lock().unwrap_or_else(|e| e.into_inner()).insert(id, conn.clone());
    Ok(conn)
}

/// "direct" / "relay" for the path a connection currently uses (metrics `peers[].path`).
pub fn path_kind(conn: &Connection) -> Option<&'static str> {
    let paths = conn.paths();
    let selected = paths.iter().find(|p| p.is_selected()).or_else(|| paths.iter().next())?;
    Some(if selected.is_relay() { "relay" } else { "direct" })
}

async fn call(endpoint: &Endpoint, peer: impl Into<EndpointAddr>, req: &Request) -> Result<Response> {
    let peer: EndpointAddr = peer.into();
    let id = peer.id;
    let mut conn = connection(endpoint, peer.clone()).await?;
    let streams = match conn.open_bi().await {
        Ok(s) => s,
        Err(_) => {
            // stale pooled connection (idle timeout, peer restarted) → one fresh attempt
            pool().lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            conn = connection(endpoint, peer).await?;
            conn.open_bi().await.map_err(|e| Error::Sync(format!("iroh open_bi: {e}")))?
        }
    };
    let (mut send, mut recv) = streams;
    send.write_all(&serde_json::to_vec(req)?).await.map_err(|e| Error::Sync(format!("iroh write: {e}")))?;
    send.finish().map_err(|e| Error::Sync(format!("iroh finish: {e}")))?;
    let raw = recv.read_to_end(MAX_RESPONSE_BYTES).await.map_err(|e| Error::Sync(format!("iroh read: {e}")))?;
    let resp: Response = serde_json::from_slice(&raw)?;
    if let Response::Error { message } = &resp {
        return Err(Error::Sync(format!("iroh peer error: {message}")));
    }
    Ok(resp)
}

/// `GetStatus` → (height, block id); records latency in the peer table.
pub async fn fetch_status(endpoint: &Endpoint, peers: &IrohPeers, peer: impl Into<EndpointAddr>) -> Result<(u64, Option<String>)> {
    let peer: EndpointAddr = peer.into();
    let started = Instant::now();
    let id = peer.id;
    match call(endpoint, peer, &Request::GetStatus).await {
        Ok(Response::Status { height, id: block_id, .. }) => {
            peers.record_rpc(id, Some(started.elapsed().as_millis() as u64), Some(height));
            if let Some(kind) = pool().lock().unwrap_or_else(|e| e.into_inner()).get(&id).and_then(path_kind) {
                peers.set_path(id, kind);
            }
            Ok((height, block_id))
        }
        Ok(_) => Err(Error::Sync("unexpected iroh response".into())),
        Err(e) => {
            peers.record_rpc(id, None, None);
            Err(e)
        }
    }
}

/// `GetBlocks` → up to `limit` blocks after `last_block_height`.
pub async fn fetch_blocks(endpoint: &Endpoint, peers: &IrohPeers, peer: impl Into<EndpointAddr>, last_block_height: u64, limit: u32) -> Result<Vec<Block>> {
    let peer: EndpointAddr = peer.into();
    let id = peer.id;
    let started = Instant::now();
    match call(endpoint, peer, &Request::GetBlocks { last_block_height, limit }).await {
        Ok(Response::Blocks { blocks }) => {
            peers.record_rpc(id, Some(started.elapsed().as_millis() as u64), blocks.last().map(|b| b.height));
            Ok(blocks)
        }
        Ok(_) => Err(Error::Sync("unexpected iroh response".into())),
        Err(e) => {
            peers.record_rpc(id, None, None);
            Err(e)
        }
    }
}

/// `GetFinality` → the peer's highest SHIP-35 certificate (unverified — pass it to `FinalityTracker::adopt_certificate`).
pub async fn fetch_finality(endpoint: &Endpoint, peers: &IrohPeers, peer: impl Into<EndpointAddr>) -> Result<Option<crate::storage::FinalityCert>> {
    let peer: EndpointAddr = peer.into();
    let id = peer.id;
    let started = Instant::now();
    match call(endpoint, peer, &Request::GetFinality).await {
        Ok(Response::Finality { certificate }) => {
            peers.record_rpc(id, Some(started.elapsed().as_millis() as u64), None);
            Ok(certificate)
        }
        Ok(_) => Err(Error::Sync("unexpected iroh response".into())),
        Err(e) => {
            peers.record_rpc(id, None, None);
            Err(e)
        }
    }
}
