//! Author: TechnoL0g
//!
//! `Transaction` — 1:1 JSON mirror of the legacy core `ITransactionData` (SHIP-11 v2 wire format).

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
/// SmartObject group (typeGroup 2, SHIP-13); legacy business/bridgechain types 0–5 are never accepted.
pub const TYPE_GROUP_SOBJ: u32 = 2;

/// SmartObject (sObject) transaction (typeGroup 2).
/// Native tokens (typeGroup 3) — see docs/SPEC-TOKENS-NATIVE.md.
pub const TYPE_GROUP_TOKEN: u32 = 3;

pub mod token {
    pub const INIT: u16 = 0;
    pub const TRANSFER: u16 = 1;
    pub const MINT: u16 = 2;
    pub const BURN: u16 = 3;
    /// On-chain manifest (name, description, website, logo) — owner only, updatable.
    pub const META: u16 = 4;
    pub const FLAG_MINTABLE: u8 = 1;
    pub const FLAG_BURNABLE: u8 = 2;
    pub const FLAG_FROZEN_CAP: u8 = 4;
    /// sObject type that acts as the ticker registry.
    pub const TICKER_SOBJ_TYPE: u8 = 5;
    pub const MAX_DECIMALS: u8 = 8;
    pub const MAX_MEMO: usize = 64;
    pub const META_MAX_NAME: usize = 64;
    pub const META_MAX_DESCRIPTION: usize = 512;
    pub const META_MAX_WEBSITE: usize = 128;
    /// Logo bytes (SVG or PNG) stored in the chain.
    pub const META_MAX_LOGO: usize = 8192;
    pub const LOGO_NONE: u8 = 0;
    pub const LOGO_SVG: u8 = 1;
    pub const LOGO_PNG: u8 = 2;
}

/// `asset.token.meta` of a TokenMeta transaction: the token's manifest, stored in the chain (no external storage).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TokenMeta {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    /// `svg` | `png` (absent = no logo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_type: Option<String>,
    /// Logo bytes, base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
}

impl TokenMeta {
    pub fn logo_type_byte(&self) -> u8 {
        match self.logo_type.as_deref() {
            Some("svg") => token::LOGO_SVG,
            Some("png") => token::LOGO_PNG,
            _ => token::LOGO_NONE,
        }
    }
    pub fn logo_type_name(b: u8) -> Option<&'static str> {
        match b {
            token::LOGO_SVG => Some("svg"),
            token::LOGO_PNG => Some("png"),
            _ => None,
        }
    }
    pub fn logo_bytes(&self) -> Option<Vec<u8>> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(self.logo.as_deref()?).ok()
    }
    pub fn mime(&self) -> Option<&'static str> {
        match self.logo_type.as_deref() {
            Some("svg") => Some("image/svg+xml"),
            Some("png") => Some("image/png"),
            _ => None,
        }
    }
}

/// `asset.token` of a typeGroup-3 transaction (fields depend on the type, see SPEC-TOKENS-NATIVE §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TokenAsset {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_u64_str")]
    pub initial_supply: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_u64_str")]
    pub supply_cap: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfers: Option<Vec<TokenTransferItem>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_u64_str")]
    pub amount: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<TokenMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TokenTransferItem {
    pub recipient_id: String,
    #[serde(with = "u64_str")]
    pub amount: u64,
}

mod u64_str {
    pub fn serialize<S: serde::Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::String(s) => s.parse().map_err(serde::de::Error::custom),
            serde_json::Value::Number(n) => n.as_u64().ok_or_else(|| serde::de::Error::custom("not u64")),
            _ => Err(serde::de::Error::custom("expected u64 string")),
        }
    }
    use serde::Deserialize;
}

mod opt_u64_str {
    pub fn serialize<S: serde::Serializer>(v: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => s.serialize_str(&v.to_string()),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        Ok(Some(super::u64_str::deserialize(d)?))
    }
}

pub mod sobj {
    pub const TYPE: u16 = 6;
    pub const ACTION_REGISTER: u8 = 0;
    pub const ACTION_UPDATE: u8 = 1;
    pub const ACTION_RESIGN: u8 = 2;
    /// sobjV2: hand the sObject (and its token registry) to `asset.recipientId`.
    pub const ACTION_TRANSFER: u8 = 3;
    /// sobjV2: owner puts the sObject up for sale at `asset.price` (0 = cancel the order).
    pub const ACTION_SELL: u8 = 4;
    /// sobjV2: anyone pays the recorded price; coins go to the owner, the sObject (and token registry) to the buyer.
    pub const ACTION_BUY: u8 = 5;
    pub const TYPE_DELEGATE: u8 = 4;
    pub const FEE_REGISTER: u64 = 5_000_000_000;
    pub const FEE_UPDATE: u64 = 500_000_000;
    pub const FEE_RESIGN: u64 = 500_000_000;
    pub const FEE_TRANSFER: u64 = 500_000_000;
    pub const FEE_SELL: u64 = 100_000_000;
    pub const FEE_BUY: u64 = 100_000_000;

    /// Exact static fee for an action (`StaticFeeMismatchError` otherwise).
    pub fn static_fee(action: u8) -> Option<u64> {
        match action {
            ACTION_REGISTER => Some(FEE_REGISTER),
            ACTION_UPDATE => Some(FEE_UPDATE),
            ACTION_RESIGN => Some(FEE_RESIGN),
            ACTION_TRANSFER => Some(FEE_TRANSFER),
            ACTION_SELL => Some(FEE_SELL),
            ACTION_BUY => Some(FEE_BUY),
            _ => None,
        }
    }
}

fn default_version() -> u8 {
    1
}

/// Quantum Shield stage B: transaction version whose second-signature section is a list of `alg || len || sig` blocks.
pub const VERSION_PQ: u8 = 3;

/// One block of the v3 second-signature section (`SPEC-PQ-V3.md` §4.3): `algorithm` 0 = secp256k1 Schnorr, 1 = ML-DSA-44.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PqSignatureBlock {
    pub algorithm: u8,
    pub signature: String,
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
    /// v3 only: second-signature blocks in ascending `algorithm` order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_signatures: Option<Vec<PqSignatureBlock>>,
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

    pub fn is_pq(&self) -> bool {
        self.version == VERSION_PQ
    }

    pub fn pq_blocks(&self) -> &[PqSignatureBlock] {
        self.second_signatures.as_deref().unwrap_or_default()
    }

    /// Extra fee a v3 transaction owes for its second-signature blocks: `feePerByte × Σ(3 + sig bytes)`.
    pub fn pq_surcharge(&self, fee_per_byte: u64) -> u64 {
        if !self.is_pq() {
            return 0;
        }
        let bytes: u64 = self.pq_blocks().iter().map(|b| 3 + (b.signature.len() / 2) as u64).sum();
        fee_per_byte.saturating_mul(bytes)
    }

    pub fn is_sobj(&self) -> bool {
        self.type_group == TYPE_GROUP_SOBJ && self.type_ == sobj::TYPE
    }

    pub fn is_token(&self) -> bool {
        self.type_group == TYPE_GROUP_TOKEN && self.type_ <= token::META
    }

    /// `asset.token` of a typeGroup-3 transaction.
    pub fn token_asset(&self) -> Option<TokenAsset> {
        if !self.is_token() {
            return None;
        }
        serde_json::from_value(self.asset.as_ref()?.extra.get("token")?.clone()).ok()
    }

    /// sObject asset (`typeGroup 2 / type 6`), parsed from the verbatim `asset` map.
    pub fn sobj_asset(&self) -> Option<SmartObjectAsset> {
        if !self.is_sobj() {
            return None;
        }
        let a = self.asset.as_ref()?;
        serde_json::from_value(Value::Object(a.extra.clone())).ok()
    }
}

/// SmartObject asset: `{ type, subType, action, registrationId?, data: { name?, ntfryData? } }`.
/// `ntfryData` is the legacy `ipfsData` slot (same wire bytes): an opaque pointer string stored in the chain —
/// the node never resolves it anywhere. The old JSON key is still accepted on input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SmartObjectAsset {
    #[serde(rename = "type")]
    pub type_: u8,
    pub sub_type: u8,
    pub action: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_id: Option<String>,
    /// New owner (action 3 only). On the wire it travels in the `ntfryData` slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_id: Option<String>,
    /// Sale price in smartoshi (action 4 only; 0 cancels). On the wire it travels in the `ntfryData` slot.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_u64_str")]
    pub price: Option<u64>,
    #[serde(default)]
    pub data: SmartObjectData,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SmartObjectData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "ntfryData", alias = "ipfsData")]
    pub ntfry_data: Option<String>,
}

impl SmartObjectAsset {
    pub fn into_map(self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        }
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
    /// Unknown asset keys are preserved verbatim.
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
    /// v3: PQ algorithm id of `public_key` (1 = ML-DSA-44); absent in v2 (secp256k1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<u8>,
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
