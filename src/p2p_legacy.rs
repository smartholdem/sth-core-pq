//! Author: TechnoL0g
//!
//! Legacy inter-node protocol client (CORE_P2P_PORT 4001): hapi-nes binary framing over
//! WebSocket + protobuf payloads (`p2p.peer.getStatus`, `p2p.peer.getPeers`,
//! `p2p.blocks.getBlocks`, `p2p.transactions.postTransactions`).
//!
//! Nes frame: `<version u8><type u8><id u32 BE><statusCode u16 BE><pathLen u8><socketLen u8>
//!             <heartbeat.interval u16 BE><heartbeat.timeout u16 BE><path><socket><payload>`
//! types: 0 hello, 1 ping, 2 update, 3 request.

use crate::crypto::deserialize_transaction;
use crate::error::{Error, Result};
use crate::models::Block;
use futures::{SinkExt, StreamExt};
use prost::Message;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub const DEFAULT_P2P_PORT: u16 = 4001;

/// Published peer list of the mainnet (maintained by the SmartHoldem team).
pub const PEERS_URL: &str = "https://raw.githubusercontent.com/smartholdem/data/main/mainnet/peers.json";

/// Fetch `peers.json` (`[{ "ip", "port" }]`) and return `ip` entries for `port`; falls back to `P2P_SEEDS`.
pub async fn fetch_peer_list(port: u16) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Entry {
        ip: String,
        port: u16,
    }
    let fetched = async {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
        client.get(PEERS_URL).send().await?.error_for_status()?.json::<Vec<Entry>>().await
    }
    .await;
    match fetched {
        Ok(list) if !list.is_empty() => {
            let mut ips: Vec<String> = list.into_iter().filter(|e| e.port == port).map(|e| e.ip).collect();
            for seed in P2P_SEEDS {
                if !ips.iter().any(|ip| ip == seed) {
                    ips.push(seed.to_string());
                }
            }
            tracing::info!(count = ips.len(), url = PEERS_URL, "peer list loaded");
            ips
        }
        Ok(_) => P2P_SEEDS.iter().map(|s| s.to_string()).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "cannot fetch peers.json, using built-in seeds");
            P2P_SEEDS.iter().map(|s| s.to_string()).collect()
        }
    }
}

/// Seed peers of the legacy network. The P2P port must be addressed by IP — the
/// `nodeN.smartholdem.io` hostnames sit behind a reverse proxy that answers 403 on 4001.
pub const P2P_SEEDS: &[&str] = &[
    "138.199.164.235",
    "138.199.149.214",
    "116.202.32.250",
    "188.245.166.98",
    "188.245.206.222",
    "136.243.144.114",
    "91.99.119.119",
    "159.69.188.60",
    "78.47.194.10",
    "157.180.114.125",
    "95.217.132.244",
];
/// Version advertised in `headers.version` (peers reject unknown majors).
pub const PEER_VERSION: &str = "3.8.2";
/// Server-side hard limit of `p2p.blocks.getBlocks`.
pub const MAX_BLOCKS_PER_REQUEST: u32 = 400;

const NES_VERSION: u8 = 2;
const TYPE_HELLO: u8 = 0;
const TYPE_PING: u8 = 1;
const TYPE_REQUEST: u8 = 3;
const HEADER_LEN: usize = 14;

// ------------------------------------------------------------ protobuf types

#[derive(Clone, PartialEq, Message)]
pub struct Headers {
    #[prost(string, tag = "1")]
    pub version: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetStatusRequest {
    #[prost(message, optional, tag = "1")]
    pub headers: Option<Headers>,
}

#[derive(Clone, PartialEq, Message)]
pub struct StatusBlockHeader {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(uint32, tag = "7")]
    pub height: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct StatusState {
    #[prost(uint32, tag = "1")]
    pub height: u32,
    #[prost(bool, tag = "2")]
    pub forging_allowed: bool,
    #[prost(uint32, tag = "3")]
    pub current_slot: u32,
    #[prost(message, optional, tag = "4")]
    pub header: Option<StatusBlockHeader>,
}

#[derive(Clone, PartialEq, Message)]
pub struct StatusNetwork {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub nethash: String,
    #[prost(uint32, tag = "5")]
    pub version: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct StatusConfig {
    #[prost(string, tag = "1")]
    pub version: String,
    #[prost(message, optional, tag = "2")]
    pub network: Option<StatusNetwork>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetStatusResponse {
    #[prost(message, optional, tag = "1")]
    pub state: Option<StatusState>,
    #[prost(message, optional, tag = "2")]
    pub config: Option<StatusConfig>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetPeersRequest {
    #[prost(message, optional, tag = "1")]
    pub headers: Option<Headers>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PeerInfo {
    #[prost(string, tag = "1")]
    pub ip: String,
    #[prost(uint32, tag = "2")]
    pub port: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetPeersResponse {
    #[prost(message, repeated, tag = "1")]
    pub peers: Vec<PeerInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetBlocksRequest {
    #[prost(uint32, tag = "1")]
    pub last_block_height: u32,
    #[prost(uint32, tag = "2")]
    pub block_limit: u32,
    #[prost(bool, tag = "3")]
    pub headers_only: bool,
    #[prost(bool, tag = "4")]
    pub serialized: bool,
    #[prost(message, optional, tag = "5")]
    pub headers: Option<Headers>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BlockHeaderProto {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(uint32, tag = "3")]
    pub version: u32,
    #[prost(uint32, tag = "4")]
    pub timestamp: u32,
    #[prost(string, tag = "5")]
    pub previous_block: String,
    #[prost(uint32, tag = "7")]
    pub height: u32,
    #[prost(uint32, tag = "8")]
    pub number_of_transactions: u32,
    #[prost(string, tag = "9")]
    pub total_amount: String,
    #[prost(string, tag = "10")]
    pub total_fee: String,
    #[prost(string, tag = "11")]
    pub reward: String,
    #[prost(uint32, tag = "12")]
    pub payload_length: u32,
    #[prost(string, tag = "13")]
    pub payload_hash: String,
    #[prost(string, tag = "14")]
    pub generator_public_key: String,
    #[prost(string, tag = "15")]
    pub block_signature: String,
    #[prost(bytes = "vec", tag = "16")]
    pub transactions: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PostTransactionsRequest {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub transactions: Vec<Vec<u8>>,
    #[prost(message, optional, tag = "2")]
    pub headers: Option<Headers>,
}

fn headers() -> Option<Headers> {
    Some(Headers { version: PEER_VERSION.to_string() })
}

// ------------------------------------------------------------------ framing

#[derive(Debug)]
struct NesMessage {
    type_: u8,
    id: u32,
    status_code: u16,
    path: String,
    payload: Vec<u8>,
}

fn encode_frame(type_: u8, id: u32, path: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + path.len() + payload.len());
    out.push(NES_VERSION);
    out.push(type_);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&200u16.to_be_bytes());
    out.push(path.len() as u8);
    out.push(0); // socket length
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(path.as_bytes());
    out.extend_from_slice(payload);
    out
}

fn decode_frame(buf: &[u8]) -> Result<NesMessage> {
    if buf.len() < HEADER_LEN {
        return Err(Error::Sync(format!("nes frame too short ({} bytes)", buf.len())));
    }
    let type_ = buf[1];
    let id = u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]);
    let status_code = u16::from_be_bytes([buf[6], buf[7]]);
    let path_len = buf[8] as usize;
    let socket_len = buf[9] as usize;
    let path_end = HEADER_LEN + path_len;
    let payload_start = path_end + socket_len;
    if buf.len() < payload_start {
        return Err(Error::Sync("nes frame truncated".into()));
    }
    Ok(NesMessage {
        type_,
        id,
        status_code,
        path: String::from_utf8_lossy(&buf[HEADER_LEN..path_end]).into_owned(),
        payload: buf[payload_start..].to_vec(),
    })
}

// ------------------------------------------------------------------- client

pub struct LegacyPeer {
    pub host: String,
    pub port: u16,
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u32,
    timeout: Duration,
}

impl LegacyPeer {
    /// Open the WebSocket and perform the nes `hello` handshake.
    pub async fn connect(host: &str, port: u16, timeout: Duration) -> Result<Self> {
        let url = format!("ws://{host}:{port}/");
        let (ws, _) = tokio::time::timeout(timeout, tokio_tungstenite::connect_async(&url))
            .await
            .map_err(|_| Error::Sync(format!("{url}: connect timeout")))?
            .map_err(|e| Error::Sync(format!("{url}: {e}")))?;
        let mut peer = Self { host: host.to_string(), port, ws, next_id: 1, timeout };
        let reply = peer.roundtrip(TYPE_HELLO, "", &[]).await?;
        if reply.type_ != TYPE_HELLO {
            return Err(Error::Sync(format!("{host}: unexpected hello reply type {}", reply.type_)));
        }
        Ok(peer)
    }

    async fn roundtrip(&mut self, type_: u8, path: &str, payload: &[u8]) -> Result<NesMessage> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let frame = encode_frame(type_, id, path, payload);
        self.ws
            .send(WsMessage::Binary(frame))
            .await
            .map_err(|e| Error::Sync(format!("{}: send failed: {e}", self.host)))?;
        loop {
            let msg = tokio::time::timeout(self.timeout, self.ws.next())
                .await
                .map_err(|_| Error::Sync(format!("{}: response timeout for {path}", self.host)))?
                .ok_or_else(|| Error::Sync(format!("{}: connection closed", self.host)))?
                .map_err(|e| Error::Sync(format!("{}: {e}", self.host)))?;
            let data = match msg {
                WsMessage::Binary(b) => b,
                WsMessage::Text(t) => t.into_bytes(),
                WsMessage::Ping(p) => {
                    let _ = self.ws.send(WsMessage::Pong(p)).await;
                    continue;
                }
                WsMessage::Close(_) => return Err(Error::Sync(format!("{}: closed by peer", self.host))),
                _ => continue,
            };
            let reply = decode_frame(&data)?;
            if reply.type_ == TYPE_PING {
                let _ = self.ws.send(WsMessage::Binary(encode_frame(TYPE_PING, reply.id, "", &[]))).await;
                continue;
            }
            if reply.id != id {
                tracing::debug!(path = reply.path, id = reply.id, "ignoring unrelated nes message");
                continue;
            }
            return Ok(reply);
        }
    }

    async fn request<Req: Message, Resp: Message + Default>(&mut self, path: &str, req: &Req) -> Result<Resp> {
        let reply = self.roundtrip(TYPE_REQUEST, path, &req.encode_to_vec()).await?;
        if reply.status_code != 200 {
            return Err(Error::Sync(format!(
                "{}: {path} → status {} ({})",
                self.host,
                reply.status_code,
                String::from_utf8_lossy(&reply.payload[..reply.payload.len().min(200)])
            )));
        }
        Resp::decode(&reply.payload[..]).map_err(|e| Error::Sync(format!("{}: {path} decode: {e}", self.host)))
    }

    pub async fn get_status(&mut self) -> Result<GetStatusResponse> {
        self.request("p2p.peer.getStatus", &GetStatusRequest { headers: headers() }).await
    }

    pub async fn get_peers(&mut self) -> Result<Vec<PeerInfo>> {
        let r: GetPeersResponse = self.request("p2p.peer.getPeers", &GetPeersRequest { headers: headers() }).await?;
        Ok(r.peers)
    }

    /// Blocks strictly above `last_block_height` (≤ 400), fully decoded with transactions.
    pub async fn get_blocks(&mut self, last_block_height: u64, limit: u32) -> Result<Vec<Block>> {
        let req = GetBlocksRequest {
            last_block_height: last_block_height as u32,
            block_limit: limit.clamp(1, MAX_BLOCKS_PER_REQUEST),
            headers_only: false,
            serialized: true,
            headers: headers(),
        };
        // The legacy codec sends the custom `[u32 BE len][BlockHeader]*` buffer directly (no proto wrapper).
        let reply = self.roundtrip(TYPE_REQUEST, "p2p.blocks.getBlocks", &req.encode_to_vec()).await?;
        if reply.status_code != 200 {
            return Err(Error::Sync(format!(
                "{}: getBlocks → status {} ({})",
                self.host,
                reply.status_code,
                String::from_utf8_lossy(&reply.payload[..reply.payload.len().min(200)])
            )));
        }
        decode_blocks(&reply.payload)
    }

    pub async fn post_transactions(&mut self, serialized: Vec<Vec<u8>>) -> Result<()> {
        let req = PostTransactionsRequest { transactions: serialized, headers: headers() };
        let _reply = self.roundtrip(TYPE_REQUEST, "p2p.transactions.postTransactions", &req.encode_to_vec()).await?;
        Ok(())
    }
}

/// Iterate `[u32 BE len][bytes]*` frames (the custom framing used inside getBlocks responses).
fn be_frames(buf: &[u8]) -> Result<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 4 <= buf.len() {
        let len = u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]) as usize;
        let chunk = buf
            .get(off + 4..off + 4 + len)
            .ok_or_else(|| Error::Sync("truncated length-prefixed frame in getBlocks response".into()))?;
        out.push(chunk);
        off += 4 + len;
    }
    Ok(out)
}

fn parse_amount(s: &str, what: &str) -> Result<u64> {
    s.parse::<u64>().map_err(|_| Error::Sync(format!("bad {what} '{s}' in peer block")))
}

/// Decode a `GetBlocksResponse.blocks` payload into core blocks (ids/txs verified later by the sync layer).
pub fn decode_blocks(payload: &[u8]) -> Result<Vec<Block>> {
    let mut blocks = Vec::new();
    for raw in be_frames(payload)? {
        let h = BlockHeaderProto::decode(raw).map_err(|e| Error::Sync(format!("block header decode: {e}")))?;
        let mut transactions = Vec::with_capacity(h.number_of_transactions as usize);
        for (seq, tx_bytes) in be_frames(&h.transactions)?.into_iter().enumerate() {
            let mut tx = deserialize_transaction(tx_bytes)?;
            tx.block_id = Some(h.id.clone());
            tx.block_height = Some(h.height as u64);
            tx.sequence = Some(seq as u32);
            transactions.push(tx);
        }
        blocks.push(Block {
            id: Some(h.id),
            version: h.version,
            timestamp: h.timestamp,
            previous_block: h.previous_block,
            height: h.height as u64,
            number_of_transactions: h.number_of_transactions,
            total_amount: parse_amount(&h.total_amount, "totalAmount")?,
            total_fee: parse_amount(&h.total_fee, "totalFee")?,
            reward: parse_amount(&h.reward, "reward")?,
            payload_length: h.payload_length,
            payload_hash: h.payload_hash,
            generator_public_key: h.generator_public_key,
            block_signature: if h.block_signature.is_empty() { None } else { Some(h.block_signature) },
            transactions,
        });
    }
    blocks.sort_by_key(|b| b.height);
    Ok(blocks)
}

/// Live follow over the legacy P2P port: poll `getStatus`, pull up to 400 blocks per call,
/// verify + apply. Rotates through `hosts` on errors. Runs until cancelled by the caller.
pub async fn follow(
    storage: std::sync::Arc<crate::storage::Storage>,
    hosts: Vec<String>,
    port: u16,
    verify: bool,
) -> Result<()> {
    use crate::sync::{apply_blocks, ChainTip};
    let network = storage.network().clone();
    let timeout = Duration::from_secs(20);
    let mut hosts: Vec<String> = if hosts.is_empty() { P2P_SEEDS.iter().map(|s| s.to_string()).collect() } else { hosts };
    let mut idx = 0usize;
    loop {
        let host = hosts[idx % hosts.len()].clone();
        idx += 1;
        let mut peer = match LegacyPeer::connect(&host, port, timeout).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(host, error = %e, "legacy peer unreachable");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        tracing::info!(host, port, "connected to legacy peer");
        // Peer discovery: learn new addresses from the peer table (IPs only, same port).
        if let Ok(peers) = peer.get_peers().await {
            let before = hosts.len();
            for p in peers.iter().filter(|p| p.port as u16 == port) {
                if !hosts.contains(&p.ip) {
                    hosts.push(p.ip.clone());
                }
            }
            if hosts.len() > before {
                tracing::info!(discovered = hosts.len() - before, total = hosts.len(), "peer table updated");
            }
        }
        loop {
            let tip = match storage.get_last_block() {
                Ok(Some(b)) => ChainTip { height: b.height, id: b.id },
                Ok(None) => ChainTip { height: 0, id: None },
                Err(e) => return Err(e),
            };
            let status = match peer.get_status().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(host, error = %e, "getStatus failed, switching peer");
                    break;
                }
            };
            let peer_height = status.state.as_ref().map(|s| s.height as u64).unwrap_or(0);
            if peer_height <= tip.height {
                let blocktime = network.milestone(tip.height.max(1)).blocktime as u64;
                tokio::time::sleep(Duration::from_secs(blocktime)).await;
                continue;
            }
            let blocks = match peer.get_blocks(tip.height, MAX_BLOCKS_PER_REQUEST).await {
                Ok(b) => b,
                Err(e) => {
                    // Legacy nodes often reset the socket right after a large getBlocks reply; just reconnect.
                    tracing::debug!(host, error = %e, "getBlocks connection dropped, reconnecting");
                    break;
                }
            };
            if blocks.is_empty() {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            let st = storage.clone();
            let net = network.clone();
            let from = blocks[0].height;
            let to = blocks[blocks.len() - 1].height;
            match tokio::task::spawn_blocking(move || apply_blocks(&st, &net, &blocks, tip, verify)).await {
                Ok(Ok(new_tip)) => tracing::info!(host, from, to, height = new_tip.height, "blocks applied from legacy peer"),
                Ok(Err(e)) => {
                    tracing::error!(host, error = %e, "peer blocks rejected, switching peer");
                    break;
                }
                Err(e) => return Err(Error::Sync(format!("apply task failed: {e}"))),
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nes_frame_roundtrip() {
        let f = encode_frame(TYPE_REQUEST, 7, "p2p.peer.getStatus", b"abc");
        assert_eq!(f.len(), HEADER_LEN + "p2p.peer.getStatus".len() + 3);
        assert_eq!(f[0], 2);
        let m = decode_frame(&f).unwrap();
        assert_eq!((m.type_, m.id, m.status_code), (TYPE_REQUEST, 7, 200));
        assert_eq!(m.path, "p2p.peer.getStatus");
        assert_eq!(m.payload, b"abc");
        assert!(decode_frame(&f[..5]).is_err());
    }

    #[test]
    fn get_blocks_payload_decodes_block_with_transaction() {
        let tx_json = r#"{"version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": "10103", "senderPublicKey": "036560f20d578da7b8433248d0c82e68121163958d533dc74b0e7d8dbabc0606a0", "fee": "100000000", "amount": "314159265", "expiration": 0, "recipientId": "SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw", "signature": "1d311090b61358077d2f59972b0913ec3687ec82f8a7b752121df934b701a7bc07e3e0d7bf051bf939a5291790ae5ed43ed59d1b6feb8dda0f76f07f747d8601"}"#;
        let tx_obj: crate::models::Transaction = serde_json::from_str(tx_json).unwrap();
        let tx = crate::crypto::serialize_transaction(&tx_obj, crate::crypto::SerializeOptions::default(), &crate::config::Network::mainnet()).unwrap();
        let mut txs = (tx.len() as u32).to_be_bytes().to_vec();
        txs.extend_from_slice(&tx);
        let header = BlockHeaderProto {
            id: "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d".into(),
            version: 0,
            timestamp: 95101456,
            previous_block: "f7f523ce32716bc968383afff31c0def91acefa72df9ba71bb95ab506a0592a7".into(),
            height: 11704043,
            number_of_transactions: 1,
            total_amount: "314159265".into(),
            total_fee: "100000000".into(),
            reward: "0".into(),
            payload_length: 32,
            payload_hash: "fdee5b08437ad279fd6461bdfcb48fe8c36a5d394203566ba5bf33819fe2d2e2".into(),
            generator_public_key: "03d67017411e92a8e93d7b0318e2748f013c130d6e4e01b1248da0ee0959ee9df8".into(),
            block_signature: "3045022100bfcfed36e8019c760490fd453cc28a2118241907d63c3ed0d3004687907107ff02200d300e64fdf5c5ca358e3266794b12b6b900004084b7fba838ceedcb1364e658".into(),
            transactions: txs,
        };
        let enc = header.encode_to_vec();
        let mut payload = (enc.len() as u32).to_be_bytes().to_vec();
        payload.extend_from_slice(&enc);
        let blocks = decode_blocks(&payload).unwrap();
        assert_eq!(blocks.len(), 1);
        let b = &blocks[0];
        assert_eq!(b.height, 11704043);
        assert_eq!(b.total_amount, 314159265);
        assert_eq!(b.transactions.len(), 1);
        assert_eq!(b.transactions[0].id.as_deref(), Some("596236ba37bc2d419f5825b2d771a74e151e7719923afefdef0b0baa9a8cfcb8"));
        assert_eq!(b.transactions[0].sequence, Some(0));
        assert_eq!(crate::crypto::block_id(b).unwrap(), b.id.clone().unwrap());
    }
}
