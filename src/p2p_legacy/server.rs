//! Author: TechnoL0g
//!
//! Inbound legacy P2P server (nes/WebSocket + protobuf) so old nodes — and Rust nodes reached through a
//! netfory-provider `ws://` endpoint — can pull from this node: `getStatus`, `getPeers`, `getBlocks`,
//! `postBlock`, `postTransactions`. Enabled with `p2p.legacy_listen` (gateway nodes with a public IP).

use super::client::{decode_frame, encode_frame, TYPE_HELLO, TYPE_PING, TYPE_REQUEST};
use super::proto::*;
use super::{PeerTable, MAX_BLOCKS_PER_REQUEST, PEER_VERSION};
use crate::crypto::{deserialize_transaction, serialize_transaction, SerializeOptions};
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
    /// Advertised in `getStatus.config.plugins` (core 3 schema requires the map; peers list it as `ports`).
    pub api_port: u16,
    pub api_enabled: bool,
}

/// Plugin key legacy peers use for the public REST API port.
pub const CORE_API_PLUGIN: &str = "@arkecosystem/core-api";
/// Legacy `transactionPool.maxTransactionsPerRequest`.
const MAX_POST_TRANSACTIONS: usize = 40;
/// Marker plugin: lets PQ Nodes recognise each other in `getStatus` (legacy ignores unknown plugin keys).
pub const CORE_PQ_PLUGIN: &str = "@smartholdem/core-pq";

fn non_empty(v: &str, fallback: &str) -> String {
    if v.trim().is_empty() { fallback.to_string() } else { v.to_string() }
}

fn clip(v: &str, max: usize) -> String {
    let s: String = v.chars().take(max).collect();
    if s.is_empty() { "-".to_string() } else { s }
}

impl LegacyServer {
    pub fn new(storage: Arc<Storage>, mempool: Arc<Mempool>, table: Arc<PeerTable>, verify: bool) -> Arc<Self> {
        let (received, _) = tokio::sync::broadcast::channel(64);
        Arc::new(Self { storage, mempool, table, verify, received, api_port: 4003, api_enabled: true })
    }

    /// REST API port / state advertised to legacy peers.
    pub fn with_api(mut self: Arc<Self>, port: u16, enabled: bool) -> Arc<Self> {
        if let Some(s) = Arc::get_mut(&mut self) {
            s.api_port = port;
            s.api_enabled = enabled;
        }
        self
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
        // legacy core-p2p `maxPayload` = 20 MiB; tungstenite's default 64 MiB would let one peer pin that much memory
        let ws_cfg = tokio_tungstenite::tungstenite::protocol::WebSocketConfig { max_message_size: Some(20 << 20), max_frame_size: Some(20 << 20), ..Default::default() };
        let mut ws = tokio_tungstenite::accept_async_with_config(stream, Some(ws_cfg)).await.map_err(|e| Error::Sync(format!("ws accept: {e}")))?;
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
            "p2p.peer.getCommonBlocks" => self.get_common_blocks(payload),
            "p2p.blocks.getBlocks" => self.get_blocks(payload),
            "p2p.blocks.postBlock" => self.post_block(payload, remote).await,
            "p2p.transactions.postTransactions" => self.post_transactions(payload, remote).await,
            // sth-core extension (PQ nodes only): sthpq address book records as JSON, so two PQ nodes that already see
            // each other on port 4001 can exchange iroh dial info without n0 DNS or a relay
            "p2p.pq.getAddrs" => match crate::p2p_iroh::addrbook::global() {
                Some(book) => serde_json::to_vec(&book.fresh(crate::p2p_iroh::addrbook::MAX_RECORDS)).map_err(Error::Json),
                None => Err(Error::Sync("address book disabled".into())),
            },
            _ => Err(Error::Sync(format!("unknown route {path}"))),
        };
        match result {
            Ok(bytes) => (200, bytes),
            Err(e) => {
                if path == "p2p.blocks.postBlock" {
                    // the forging peer sees this text in its "postBlock REJECTED" line; our operator must see it too
                    tracing::warn!(peer = %remote, error = %e, "postBlock rejected (drifted wallet state? run `sth-core audit-all --fix`)");
                } else {
                    tracing::debug!(peer = %remote, path, error = %e, "legacy request failed");
                }
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
                header: last.as_ref().map(status_header),
            }),
            config: Some(StatusConfig {
                version: PEER_VERSION.to_string(),
                network: Some(StatusNetwork {
                    name: net.name.clone(),
                    nethash: net.nethash.clone(),
                    // core 3 peer verifier validates the reply against a JSON schema: explorer + token{name,symbol} are
                    // required and protobuf drops empty strings → never send an empty value
                    explorer: non_empty(&net.explorer, "-"),
                    token: Some(StatusToken { name: clip(&non_empty(&net.token, &net.symbol), 8), symbol: clip(&non_empty(&net.symbol, &net.token), 4) }),
                    version: net.pubkey_hash as u32,
                }),
                plugins: std::collections::HashMap::from([
                    (CORE_API_PLUGIN.to_string(), StatusPlugin { port: u32::from(self.api_port), enabled: self.api_enabled, estimate_total_count: false }),
                    (CORE_PQ_PLUGIN.to_string(), StatusPlugin { port: u32::from(self.api_port), enabled: true, estimate_total_count: false }),
                ]),
            }),
        };
        Ok(resp.encode_to_vec())
    }

    /// `p2p.peer.getCommonBlocks`: the highest of the asked block ids that is in our chain (peer verifier's
    /// 8-ary search for the fork point); `common = null` when none is.
    fn get_common_blocks(&self, payload: &[u8]) -> Result<Vec<u8>> {
        let req = GetCommonBlocksRequest::decode(payload).map_err(|e| Error::Sync(format!("getCommonBlocks request: {e}")))?;
        let mut common: Option<CommonBlock> = None;
        for id in req.ids.iter().take(100) {
            if let Some(b) = self.storage.get_block_by_id(id)? {
                if common.as_ref().map_or(true, |c| (b.height as u32) > c.height) {
                    common = Some(CommonBlock { height: b.height as u32, id: id.clone() });
                }
            }
        }
        Ok(GetCommonBlocksResponse { common }.encode_to_vec())
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
        let (mut block, header_len) = crate::crypto::deserialize_block_header_prefix(bytes, net)?;
        let n = block.number_of_transactions as usize;
        // header claims come from the wire: bound them before any allocation (4 length bytes per tx at least)
        let max_tx = net.milestone(block.height).max_transactions() as usize;
        if n > max_tx || n > bytes.len().saturating_sub(header_len) / 4 {
            return Err(Error::Sync(format!("postBlock: implausible transaction count {n} (max {max_tx}, {} payload bytes)", bytes.len())));
        }
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
            let ours = self.storage.get_block_by_height(height)?;
            let known = ours.as_ref().and_then(|b| b.id.as_deref()) == block.id.as_deref();
            if let (false, Some(ours)) = (known, ours.as_ref()) {
                if height + 1 >= tip.height {
                    crate::delegate::guard::on_competing(crate::intake::Source::PushLegacy, &remote.ip().to_string(), ours, &block);
                    if crate::intake::prefer_earlier_slot(&self.storage, ours, &block, &remote.ip().to_string()).await {
                        return Ok(PostBlockResponse { status: true, height: height as u32 }.encode_to_vec());
                    }
                }
            }
            return Ok(PostBlockResponse { status: known, height: tip.height as u32 }.encode_to_vec());
        }
        if height > tip.height + 1 && height - tip.height <= 8 {
            // the sender has the block(s) we are missing (a poorly connected forger's block did not reach us): pull
            // them from the sender right now instead of answering "not chained" and waiting for the follow loop
            let ip = remote.ip().to_string();
            match super::follow::pull_gap_from(&self.storage, &self.table, &ip, height - 1, self.verify).await {
                Ok(t) => tracing::debug!(peer = ip, height = t.height, pushed = height, "postBlock gap filled from the sender"),
                Err(e) => tracing::debug!(peer = ip, error = %e, pushed = height, "postBlock gap fill from the sender failed"),
            }
        }
        let tip = match self.storage.get_last_block()? {
            Some(b) => ChainTip { height: b.height, id: b.id },
            None => ChainTip { height: 0, id: None },
        };
        if height <= tip.height {
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
        let block_id = block.id.clone();
        let header = Block { transactions: Vec::new(), ..block.clone() };
        let applied = tokio::task::spawn_blocking(move || apply_blocks(&st, &net, &[block], tip, verify))
            .await
            .map_err(|e| Error::Sync(format!("apply task failed: {e}")))?;
        if let Err(Error::StaleTip(msg)) = &applied {
            // gossip / pull applied it first: legacy answers status=true for a block it already has
            tracing::debug!(peer = %remote, %msg, "postBlock raced with another intake path");
            let known = self.storage.get_block_by_height(height)?.and_then(|b| b.id) == block_id;
            return Ok(PostBlockResponse { status: known, height: self.storage.get_last_height()? as u32 }.encode_to_vec());
        }
        applied?;
        crate::intake::record(crate::intake::Source::PushLegacy, remote.ip().to_string(), height, block_ts, &generator, self.storage.network());
        crate::delegate::guard::on_accepted(crate::intake::Source::PushLegacy, &remote.ip().to_string(), &header, self.storage.network());
        tracing::info!("Received new block at height {} with {} transactions from {}", crate::delegate::forger::group(height), txs, remote.ip());
        let _ = self.received.send(height);
        self.mempool.prune_confirmed().await;
        Ok(PostBlockResponse { status: true, height: height as u32 }.encode_to_vec())
    }

    async fn post_transactions(&self, payload: &[u8], remote: SocketAddr) -> Result<Vec<u8>> {
        let req = PostTransactionsRequest::decode(payload).map_err(|e| Error::Sync(format!("postTransactions request: {e}")))?;
        // legacy core caps a request at 40 transactions; more is a flood (ML-DSA verification costs CPU)
        if req.transactions.len() > MAX_POST_TRANSACTIONS {
            return Err(Error::Sync(format!("postTransactions: {} transactions in one request (max {MAX_POST_TRANSACTIONS})", req.transactions.len())));
        }
        let mut txs = Vec::with_capacity(req.transactions.len());
        for raw in &req.transactions {
            txs.push(deserialize_transaction(raw)?);
        }
        let (resp, errors) = self.mempool.add_many_from(txs, &crate::antispam::TxOrigin::LegacyPeer(remote.ip().to_string())).await;
        let duplicates = errors.values().filter(|e| e.type_ == "ERR_DUPLICATE").count();
        if resp.accept.is_empty() && duplicates == resp.invalid.len() {
            tracing::debug!(peer = %remote.ip(), duplicates, "transactions received over legacy P2P: all already in the pool");
        } else {
            tracing::info!(peer = %remote.ip(), accepted = resp.accept.len(), invalid = resp.invalid.len() - duplicates, duplicates, "transactions received over legacy P2P");
        }
        Ok(serde_json::to_vec(&resp.accept)?)
    }
}

/// Legacy `block.getHeader()`: every header field; `*Hex` is the id itself for full-sha256 ids and the
/// zero-padded 16-hex form for historical numeric ids.
pub fn status_header(b: &Block) -> StatusBlockHeader {
    let hex = |id: &str| if id.len() == 64 { id.to_string() } else { id.parse::<u64>().map(|n| format!("{n:016x}")).unwrap_or_default() };
    let id = b.id.clone().unwrap_or_default();
    StatusBlockHeader {
        id_hex: hex(&id),
        id,
        version: b.version,
        timestamp: b.timestamp,
        previous_block_hex: hex(&b.previous_block),
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
    }
}
