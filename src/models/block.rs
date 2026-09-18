//! Author: TechnoL0g
//!
//! `Block` — 1:1 JSON mirror of core's `IBlockData` (header + embedded transactions).

use super::serde_utils::string_u64;
use super::Transaction;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub version: u32,
    pub timestamp: u32,
    pub previous_block: String,
    pub height: u64,
    pub number_of_transactions: u32,
    #[serde(with = "string_u64")]
    pub total_amount: u64,
    #[serde(with = "string_u64")]
    pub total_fee: u64,
    #[serde(with = "string_u64")]
    pub reward: u64,
    pub payload_length: u32,
    pub payload_hash: String,
    pub generator_public_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_signature: Option<String>,
    /// Stage C (block version 1): ML-DSA-44 signature of the forging delegate over `sha256(header || blockSignature)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pq_signature: Option<super::PqSignatureBlock>,
    #[serde(default)]
    pub transactions: Vec<Transaction>,
}

/// Block version carrying `pqSignature` (Quantum Shield stage C, milestone `pq.blocks`).
pub const BLOCK_VERSION_PQ: u32 = 1;

impl Block {
    /// Header copy without transactions (core's `getHeader()`).
    pub fn header(&self) -> Block {
        Block {
            transactions: Vec::new(),
            ..self.clone()
        }
    }
}
