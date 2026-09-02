//! Author: TechnoL0g
//!
//! `Transaction` 1:1 JSON mirror of core's `ITransactionData` (AIP-11 / v2 format).

use super::serde_utils::{opt_string_u64, string_u64};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Core transaction type ids (typeGroup 1).
pub mod tx_type {
    pub const TRANSFER: u16 = 0;
    pub const SECOND_SIGNATURE: u16 = 1;
    pub const DELEGATE_REGISTRATION: u16 = 2;
    pub const VOTE: u16 = 3;
    pub const MULTI_SIGNATURE: u16 = 4;
    pub const IPFS: u16 = 5;
    pub const MULTI_PAYMENT: u16 = 6;
    pub const DELEGATE_RESIGNATION: u16 = 7;
    pub const HTLC_LOCK: u16 = 8;
    pub const HTLC_CLAIM: u16 = 9;
    pub const HTLC_REFUND: u16 = 10;
}

pub const TYPE_GROUP_CORE: u32 = 1;

fn default_version() -> u8 {
    1
}

fn default_type_group() -> u32 {
    TYPE_GROUP_CORE
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    #[serde(default = "default_version")]
    pub version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<u8>,
    #[serde(default = "default_type_group")]
    pub type_group: u32,
    #[serde(rename = "type")]
    pub type_: u16,
    #[serde(default, with = "opt_string_u64", skip_serializing_if = "Option::is_none")]
    pub nonce: Option<u64>,
    pub sender_public_key: String,
    #[serde(with = "string_u64")]
    pub fee: u64,
    #[serde(default, with = "string_u64")]
    pub amount: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiration: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<TransactionAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signatures: Option<Vec<String>>,
    /// Legacy v1 timestamp (absent on v2 transactions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_height: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u32>,
}

impl Transaction {
    /// Whether this type carries a vendorField (memo) on the wire.
    pub fn has_vendor_field(&self) -> bool {
        self.type_group == TYPE_GROUP_CORE
            && matches!(
                self.type_,
                tx_type::TRANSFER | tx_type::MULTI_PAYMENT | tx_type::HTLC_LOCK
            )
    }

    /// Second signature under either legacy or current field name.
    pub fn second_signature_any(&self) -> Option<&String> {
        self.second_signature.as_ref().or(self.sign_signature.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TransactionAsset {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub votes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate: Option<DelegateAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<SecondSignatureAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_signature: Option<MultiSignatureAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payments: Option<Vec<Payment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipfs: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<HtlcLockAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<HtlcClaimAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refund: Option<HtlcRefundAsset>,
    /// Unknown asset keys (e.g. magistrate assets) are preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegateAsset {
    pub username: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecondSignatureAsset {
    pub public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiSignatureAsset {
    pub min: u8,
    pub public_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Payment {
    #[serde(with = "string_u64")]
    pub amount: u64,
    pub recipient_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HtlcExpiration {
    #[serde(rename = "type")]
    pub type_: u8,
    pub value: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HtlcLockAsset {
    pub secret_hash: String,
    pub expiration: HtlcExpiration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HtlcClaimAsset {
    pub lock_transaction_id: String,
    pub unlock_secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HtlcRefundAsset {
    pub lock_transaction_id: String,
}
