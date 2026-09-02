//! Author: TechnoL0g
//!
//! `Block` - 1:1 JSON mirror of core's `IBlockData` (header + embedded transactions).

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
    #[serde(default)]
    pub transactions: Vec<Transaction>,
}

impl Block {
    /// Header copy without transactions (core's `getHeader()`).
    pub fn header(&self) -> Block {
        Block {
            transactions: Vec::new(),
            ..self.clone()
        }
    }
}
