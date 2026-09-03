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
}

impl std::fmt::Debug for RpcHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RpcHandler")
    }
}

impl RpcHandler {
    pub fn new(storage: Arc<Storage>, peers: Arc<IrohPeers>) -> Self {
        Self { storage, peers }
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

async fn call(endpoint: &Endpoint, peer: impl Into<EndpointAddr>, req: &Request) -> Result<Response> {
    let conn = endpoint.connect(peer, ALPN).await.map_err(|e| Error::Sync(format!("iroh connect: {e}")))?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| Error::Sync(format!("iroh open_bi: {e}")))?;
    send.write_all(&serde_json::to_vec(req)?).await.map_err(|e| Error::Sync(format!("iroh write: {e}")))?;
    send.finish().map_err(|e| Error::Sync(format!("iroh finish: {e}")))?;
    let raw = recv.read_to_end(MAX_RESPONSE_BYTES).await.map_err(|e| Error::Sync(format!("iroh read: {e}")))?;
    conn.close(0u32.into(), b"done");
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
