//! Author: TechnoL0g
//!
//! Wire messages of the iroh layer. JSON (same block / transaction schema as the REST API and the
//! legacy core) so browser and Node.js clients can speak it without a protobuf toolchain.

use crate::models::{Block, Transaction};
use serde::{Deserialize, Serialize};

/// Protocol revision; bump on incompatible changes.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum GossipMessage {
    /// A freshly forged / received block (full, with transactions).
    Block { block: Block },
    /// Transactions accepted into the sender's mempool.
    Transactions { transactions: Vec<Transaction> },
    /// Legacy peers the sender found healthy (reputation sharing for newcomers) and, for gateway nodes,
    /// the sender's own public legacy address (`ip:4001`).
    Peers {
        peers: Vec<PeerHint>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gateway: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerHint {
    pub ip: String,
    pub port: u16,
    pub height: u64,
    pub latency_ms: u64,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Request {
    GetStatus,
    /// Blocks with height in `(lastBlockHeight, lastBlockHeight + limit]`.
    GetBlocks {
        #[serde(rename = "lastBlockHeight")]
        last_block_height: u64,
        limit: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Response {
    Status {
        version: u32,
        #[serde(rename = "coreVersion")]
        core_version: String,
        nethash: String,
        height: u64,
        id: Option<String>,
    },
    Blocks { blocks: Vec<Block> },
    Error { message: String },
}

impl GossipMessage {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}
