//! Author: TechnoL0g
//!
//! Legacy inter-node protocol client: hapi-nes binary framing over WebSocket + protobuf payloads
//! (`p2p.peer.getStatus`, `p2p.peer.getPeers`, `p2p.blocks.getBlocks`, `p2p.transactions.postTransactions`).
//!
//! Nes frame: `<version u8><type u8><id u32 BE><statusCode u16 BE><pathLen u8><socketLen u8>
//!             <heartbeat.interval u16 BE><heartbeat.timeout u16 BE><path><socket><payload>`
//! types: 0 hello, 1 ping, 2 update, 3 request.

use super::proto::*;
use super::MAX_BLOCKS_PER_REQUEST;
use crate::crypto::deserialize_transaction;
use crate::error::{Error, Result};
use crate::models::Block;
use futures::{SinkExt, StreamExt};
use prost::Message;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const NES_VERSION: u8 = 2;
pub(super) const TYPE_HELLO: u8 = 0;
pub(super) const TYPE_PING: u8 = 1;
pub(super) const TYPE_REQUEST: u8 = 3;
const HEADER_LEN: usize = 14;

// ------------------------------------------------------------------ framing

#[derive(Debug)]
pub(super) struct NesMessage {
    pub(super) type_: u8,
    pub(super) id: u32,
    pub(super) status_code: u16,
    pub(super) path: String,
    pub(super) payload: Vec<u8>,
}

pub(super) fn encode_frame(type_: u8, id: u32, path: &str, payload: &[u8]) -> Vec<u8> {
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

pub(super) fn decode_frame(buf: &[u8]) -> Result<NesMessage> {
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
    /// `host` is an IP / hostname, or a full `ws://` / `wss://` URL (e.g. a netfory-provider endpoint
    /// `ws://<proxy>/<nodeId>/<name>` reached through a local Web 4.0 proxy) — then `port` is ignored.
    pub async fn connect(host: &str, port: u16, timeout: Duration) -> Result<Self> {
        let url = if host.starts_with("ws://") || host.starts_with("wss://") { host.to_string() } else { format!("ws://{host}:{port}/") };
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

    /// `p2p.blocks.postBlock` with a fully serialised block (header + transactions).
    pub async fn post_block(&mut self, serialized_block: Vec<u8>) -> Result<PostBlockResponse> {
        let req = PostBlockRequest { block: serialized_block, headers: headers() };
        let reply = self.roundtrip(TYPE_REQUEST, "p2p.blocks.postBlock", &req.encode_to_vec()).await?;
        Ok(PostBlockResponse::decode(reply.payload.as_slice()).unwrap_or_default())
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
            pq_signature: None,
            transactions,
        });
    }
    blocks.sort_by_key(|b| b.height);
    Ok(blocks)
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
