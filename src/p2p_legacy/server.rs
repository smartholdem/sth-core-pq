//! Author: TechnoL0g
//!
//! Inbound legacy P2P server (nes/WebSocket + protobuf) so old nodes — and Rust nodes reached through a
//! netfory-provider `ws://` endpoint — can pull from this node: `getStatus`, `getPeers`, `getBlocks`,
//! `postBlock`, `postTransactions`. Enabled with `p2p.legacy_listen` (gateway nodes with a public IP).

use super::client::{decode_frame, encode_frame, TYPE_HELLO, TYPE_PING, TYPE_REQUEST};
use super::proto::*;
use super::{PeerTable, MAX_BLOCKS_PER_REQUEST, PEER_VERSION};
use crate::crypto::{deserialize_block_header, deserialize_transaction, serialize_transaction, SerializeOptions};
use crate::error::{Error, Result};
use crate::mempool::Mempool;
use crate::models::Block;
use crate::storage::Storage;
use crate::sync::{apply_blocks, ChainTip};
use futures::{SinkExt, StreamExt};
use prost::Message;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub struct LegacyServer {
    pub storage: Arc<Storage>,
    pub mempool: Arc<Mempool>,
    pub table: Arc<PeerTable>,
    pub verify: bool,
    /// Blocks accepted via `postBlock` (height) — the forger / gossip publisher can observe them.
    pub received: tokio::sync::broadcast::Sender<u64>,
}

impl LegacyServer {
    pub fn new(storage: Arc<Storage>, mempool: Arc<Mempool>, table: Arc<PeerTable>, verify: bool) -> Arc<Self> {
        let (received, _) = tokio::sync::broadcast::channel(64);
        Arc::new(Self { storage, mempool, table, verify, received })
    }

    pub async fn serve(self: Arc<Self>, addr: SocketAddr) -> Result<()> {
        let listener = TcpListener::bind(addr).await.map_err(|e| Error::Sync(format!("legacy listen {addr}: {e}")))?;
        tracing::info!(%addr, "legacy P2P server listening (nes/WebSocket)");
        loop {
            let (stream, remote) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    tracing::debug!(error = %e, "legacy accept failed");
                    continue;
                }
            };
            let srv = self.clone();
            tokio::spawn(async move {
                if let Err(e) = srv.handle_connection(stream, remote).await {
                    tracing::debug!(peer = %remote, error = %e, "legacy inbound connection closed");
                }
            });
        }
    }

    async fn handle_connection(&self, stream: tokio::net::TcpStream, remote: SocketAddr) -> Result<()> {
        let mut ws = tokio_tungstenite::accept_async(stream).await.map_err(|e| Error::Sync(format!("ws accept: {e}")))?;
        tracing::debug!(peer = %remote, "legacy inbound connection");
        while let Some(msg) = ws.next().await {
            let data = match msg.map_err(|e| Error::Sync(format!("ws: {e}")))? {
                WsMessage::Binary(b) => b,
                WsMessage::Close(_) => break,
                WsMessage::Ping(p) => {
                    ws.send(WsMessage::Pong(p)).await.ok();
                    continue;
                }
                _ => continue,
            };
            let frame = decode_frame(&data)?;
            let reply = match frame.type_ {
                TYPE_HELLO => encode_frame(TYPE_HELLO, frame.id, "", &[]),
                TYPE_PING => encode_frame(TYPE_PING, frame.id, "", &[]),
                TYPE_REQUEST => {
                    let (status, payload) = self.dispatch(&frame.path, &frame.payload, remote).await;
                    let mut f = encode_frame(TYPE_REQUEST, frame.id, "", &payload);
                    f[6..8].copy_from_slice(&status.to_be_bytes());
                    f
                }
                _ => continue,
            };
            ws.send(WsMessage::Binary(reply.into())).await.map_err(|e| Error::Sync(format!("ws send: {e}")))?;
        }
        Ok(())
    }

    async fn dispatch(&self, path: &str, payload: &[u8], remote: SocketAddr) -> (u16, Vec<u8>) {
        // a node that talks the block protocol to us is a peer candidate (verified by the next status probe);
        // this is how a private network's first node learns about nodes that dial in
        if path.starts_with("p2p.blocks.") && self.table.add(&remote.ip().to_string()) {
            tracing::info!(peer = %remote.ip(), "new legacy peer discovered from inbound connection");
        }
        let result = match path {
            "p2p.peer.getStatus" => self.get_status(),
            "p2p.peer.getPeers" => self.get_peers(),
            "p2p.blocks.getBlocks" => self.get_blocks(payload),
            "p2p.blocks.postBlock" => self.post_block(payload, remote).await,
            "p2p.transactions.postTransactions" => self.post_transactions(payload, remote).await,
            _ => Err(Error::Sync(format!("unknown route {path}"))),
        };
        match result {
            Ok(bytes) => (200, bytes),
            Err(e) => {
                tracing::debug!(peer = %remote, path, error = %e, "legacy request failed");
                (if path.starts_with("p2p.") { 400 } else { 404 }, e.to_string().into_bytes())
            }
        }
    }

    fn get_status(&self) -> Result<Vec<u8>> {
        let net = self.storage.network();
        let last = self.storage.get_last_block()?;
        let height = last.as_ref().map(|b| b.height).unwrap_or(0);
        let blocktime = net.milestone(height.max(1)).blocktime;
        let resp = GetStatusResponse {
            state: Some(StatusState {
                height: height as u32,
                forging_allowed: true,
                current_slot: net.now_epoch() / blocktime,
                header: last.as_ref().map(|b| StatusBlockHeader { id: b.id.clone().unwrap_or_default(), height: b.height as u32 }),
            }),
            config: Some(StatusConfig {
                version: PEER_VERSION.to_string(),
                network: Some(StatusNetwork { name: net.name.clone(), nethash: net.nethash.clone(), version: net.pubkey_hash as u32 }),
            }),
        };
        Ok(resp.encode_to_vec())
    }

    fn get_peers(&self) -> Result<Vec<u8>> {
        let port = self.table.port() as u32;
        let peers = self.table.snapshot().into_iter().filter(|p| p.successes > 0).map(|p| PeerInfo { ip: p.ip, port }).collect();
        Ok(GetPeersResponse { peers }.encode_to_vec())
    }

    /// `[u32 BE len][BlockHeaderProto]*`, transactions as `[u32 BE len][tx bytes]*` inside each header.
    fn get_blocks(&self, payload: &[u8]) -> Result<Vec<u8>> {
        let req = GetBlocksRequest::decode(payload).map_err(|e| Error::Sync(format!("getBlocks request: {e}")))?;
        let limit = req.block_limit.clamp(1, MAX_BLOCKS_PER_REQUEST) as usize;
        let net = self.storage.network();
        let mut out = Vec::new();
        for b in self.storage.get_blocks_from(req.last_block_height as u64 + 1, limit)? {
            let mut txs = Vec::new();
            if !req.headers_only {
                for tx in &b.transactions {
                    let bytes = serialize_transaction(tx, SerializeOptions::default(), net)?;
                    txs.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                    txs.extend_from_slice(&bytes);
                }
            }
            let header = BlockHeaderProto {
                id: b.id.clone().unwrap_or_default(),
                version: b.version,
                timestamp: b.timestamp,
                previous_block: b.previous_block.clone(),
                height: b.height as u32,
                number_of_transactions: b.number_of_transactions,
                total_amount: b.total_amount.to_string(),
                total_fee: b.total_fee.to_string(),
                reward: b.reward.to_string(),
                payload_length: b.payload_length,
                payload_hash: b.payload_hash.clone(),
                generator_public_key: b.generator_public_key.clone(),
                block_signature: b.block_signature.clone().unwrap_or_default(),
                transactions: txs,
            }
            .encode_to_vec();
            out.extend_from_slice(&(header.len() as u32).to_be_bytes());
            out.extend_from_slice(&header);
        }
        Ok(out)
    }

    /// Decode `serializeWithTransactions` (header, u32 LE lengths, tx bytes).
    fn decode_full_block(&self, bytes: &[u8]) -> Result<Block> {
        let net = self.storage.network();
        let mut block = deserialize_block_header(bytes, net)?;
        let n = block.number_of_transactions as usize;
        let header_len = crate::crypto::serialize_block(&block, true)?.len();
        let mut off = header_len;
        let mut lens = Vec::with_capacity(n);
        for _ in 0..n {
            let l = bytes.get(off..off + 4).ok_or_else(|| Error::Sync("postBlock: truncated lengths".into()))?;
            lens.push(u32::from_le_bytes([l[0], l[1], l[2], l[3]]) as usize);
            off += 4;
        }
        for (seq, len) in lens.into_iter().enumerate() {
            let raw = bytes.get(off..off + len).ok_or_else(|| Error::Sync("postBlock: truncated transaction".into()))?;
            let mut tx = deserialize_transaction(raw)?;
            tx.block_id = block.id.clone();
            tx.block_height = Some(block.height);
            tx.sequence = Some(seq as u32);
            block.transactions.push(tx);
            off += len;
        }
        Ok(block)
    }

    async fn post_block(&self, payload: &[u8], remote: SocketAddr) -> Result<Vec<u8>> {
        let req = PostBlockRequest::decode(payload).map_err(|e| Error::Sync(format!("postBlock request: {e}")))?;
        let block = self.decode_full_block(&req.block)?;
        let tip = match self.storage.get_last_block()? {
            Some(b) => ChainTip { height: b.height, id: b.id },
            None => ChainTip { height: 0, id: None },
        };
        let height = block.height;
        if height <= tip.height {
            // already known (or stale): legacy answers status=true for pinged blocks
            let known = self.storage.get_block_by_height(height)?.and_then(|b| b.id) == block.id;
            return Ok(PostBlockResponse { status: known, height: tip.height as u32 }.encode_to_vec());
        }
        if height != tip.height + 1 || tip.id.as_deref().is_some_and(|id| block.previous_block != id) {
            return Err(Error::Sync(format!("block {height} is not chained to our tip {}", tip.height)));
        }
        let st = self.storage.clone();
        let net = self.storage.network().clone();
        let verify = self.verify;
        let txs = block.transactions.len();
        let block_ts = block.timestamp;
        let generator = block.generator_public_key.clone();
        tokio::task::spawn_blocking(move || apply_blocks(&st, &net, &[block], tip, verify))
            .await
            .map_err(|e| Error::Sync(format!("apply task failed: {e}")))??;
        crate::intake::record(crate::intake::Source::PushLegacy, remote.ip().to_string(), height, block_ts, &generator, self.storage.network());
        tracing::info!("Received new block at height {} with {} transactions from {}", crate::delegate::forger::group(height), txs, remote.ip());
        let _ = self.received.send(height);
        self.mempool.prune_confirmed().await;
        Ok(PostBlockResponse { status: true, height: height as u32 }.encode_to_vec())
    }

    async fn post_transactions(&self, payload: &[u8], remote: SocketAddr) -> Result<Vec<u8>> {
        let req = PostTransactionsRequest::decode(payload).map_err(|e| Error::Sync(format!("postTransactions request: {e}")))?;
        let mut txs = Vec::with_capacity(req.transactions.len());
        for raw in &req.transactions {
            txs.push(deserialize_transaction(raw)?);
        }
        let (resp, _) = self.mempool.add_many(txs).await;
        tracing::info!(peer = %remote.ip(), accepted = resp.accept.len(), invalid = resp.invalid.len(), "transactions received over legacy P2P");
        Ok(serde_json::to_vec(&resp.accept)?)
    }
}
