//! Author: TechnoL0g
//!
//! Network configuration in the exact `crypto-networks` layout (`network.json`, `milestones.json`,
//! `exceptions.json`, `genesisBlock.json`). Mainnet is embedded from `network/mainnet/`; a directory
//! with the same files can be supplied (`network_dir` in node.yaml) to override or extend it — e.g.
//! a future milestone that switches block rewards or forging parameters on from height N.

use crate::error::{Error, Result};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 1 STH = 100_000_000 smartoshi.
pub const SMARTOSHI: u64 = 100_000_000;

pub const MAINNET_NETWORK_JSON: &str = include_str!("../network/mainnet/network.json");
pub const MAINNET_MILESTONES_JSON: &str = include_str!("../network/mainnet/milestones.json");
pub const MAINNET_EXCEPTIONS_JSON: &str = include_str!("../network/mainnet/exceptions.json");
pub const MAINNET_GENESIS_GZ: &[u8] = include_bytes!("../network/mainnet/genesisBlock.json.gz");

/// Milestone parameters active from a given height (later milestones deep-merge into earlier ones).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Milestone {
    pub height: u64,
    #[serde(deserialize_with = "de_u64_lenient")]
    pub reward: u64,
    pub active_delegates: u32,
    pub blocktime: u32,
    pub block: BlockParams,
    pub epoch: String,
    pub fees: Fees,
    pub vendor_field_length: u16,
    pub multi_payment_limit: u32,
    pub htlc_enabled: bool,
    pub block_burn_address: bool,
    /// SHIP-11: version-2 transaction wire format + Schnorr transaction signatures. JSON key `ship11`; the historical
    /// `aip11` (and `txV2`) are accepted as aliases.
    #[serde(default, rename = "ship11", alias = "aip11", alias = "txV2")]
    pub tx_v2: bool,
    /// SHIP-13: SmartObject (sObject, typeGroup 2 / type 6) transactions accepted from this height. JSON key `ship13`;
    /// historical `aip36` and `sobj` are aliases.
    #[serde(default, rename = "ship13", alias = "aip36", alias = "sobj")]
    pub sobj_active: bool,
    /// SHIP-11 (blocks): Schnorr block signatures. JSON key `ship11Blocks`; historical `aip37` and `schnorrBlocks` are aliases.
    #[serde(default, rename = "ship11Blocks", alias = "aip37", alias = "schnorrBlocks")]
    pub schnorr_blocks: bool,
    /// Native tokens (typeGroup 3) accepted from this height.
    #[serde(default)]
    pub tokens: bool,
    #[serde(default)]
    pub token_fees: TokenFees,
    #[serde(default = "default_token_recipients")]
    pub token_transfer_max_recipients: u16,
    /// sObject rules v2: `ntfryData` = any UTF-8 ≤ 255 bytes, type-5 names must be tickers. Off on mainnet until all
    /// delegates run this core (legacy nodes keep the base58 / free-name rules). `entityV2` is read as an alias.
    #[serde(default, alias = "entityV2")]
    pub sobj_v2: bool,
    /// Reject blocks whose sender cannot cover amount + fee (legacy `InsufficientBalanceError`). Off = log only
    /// (lets mainnet operators verify history before the flag is switched on together with tokens / sobjV2).
    #[serde(default)]
    pub strict_balance: bool,
    /// Lowest sth-core version that implements this milestone's rules (e.g. "0.14.0"); the Delegate Dashboard flags
    /// Rust delegates announcing an older version. Empty = no requirement.
    #[serde(default)]
    pub min_core_version: String,
    /// Quantum Shield stage B (v3 transactions with ML-DSA-44 second signatures). Off until every active delegate runs this core.
    #[serde(default)]
    pub pq: PqParams,
    /// SHIP-35 BFT finality gadget: `active` = hard mode (rollback below a certified block is refused). Off = soft mode.
    #[serde(default)]
    pub finality: FinalityParams,
}

/// `milestones[].finality` — see docs/SHIPs/SHIP-35.md. Legacy nodes ignore votes, so soft mode is always safe.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FinalityParams {
    /// Hard mode: a node never rolls back below the highest finality certificate it holds.
    pub active: bool,
    /// Votes needed for a certificate; 0 = ⌊2·activeDelegates/3⌋ + 1 (15 of 21).
    pub quorum: u32,
    /// Slashing: a delegate with a proven double vote (two ids at one height) leaves the active set for `slashingRounds`.
    /// Consensus-affecting (round snapshots differ) — Rust-only network. Off = proofs are only recorded and shown.
    pub slashing: bool,
    /// Rounds of exclusion after a proven equivocation (default 30 ≈ 84 min at 21 × 8 s).
    pub slashing_rounds: u32,
}

impl FinalityParams {
    pub fn slashing_rounds(&self) -> u64 {
        if self.slashing_rounds == 0 { 30 } else { self.slashing_rounds as u64 }
    }
}

impl Milestone {
    /// Finality quorum of this milestone (> 2/3 of the active set unless overridden).
    pub fn finality_quorum(&self) -> u32 {
        if self.finality.quorum > 0 { self.finality.quorum } else { self.active_delegates * 2 / 3 + 1 }
    }
}

fn default_token_recipients() -> u16 {
    64
}

/// `milestones[].pq` — see docs/SPEC-PQ-V3.md §6/§8.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PqParams {
    /// v3 transactions accepted from this milestone.
    #[serde(default)]
    pub active: bool,
    /// Surcharge per byte of the second-signature blocks (smartoshi), default 10 000 → +0.2423 STH per ML-DSA-44 block.
    #[serde(default = "default_pq_fee_per_byte")]
    pub fee_per_byte: u64,
    /// Blocks after activation during which a wallet with a stage-A commitment may only register the committed key.
    #[serde(default = "default_pq_commitment_grace")]
    pub commitment_grace: u64,
    /// Stage C: blocks must carry a hybrid secp256k1 + ML-DSA-44 signature (version 1). Rust-only network.
    #[serde(default)]
    pub blocks: bool,
    /// Blocks after `blocks` activation during which version-0 blocks (delegates without a PQ key) are still accepted.
    #[serde(default = "default_pq_blocks_grace")]
    pub blocks_grace: u64,
}

fn default_pq_blocks_grace() -> u64 {
    43_200
}
fn default_pq_fee_per_byte() -> u64 {
    10_000
}
fn default_pq_commitment_grace() -> u64 {
    86_400
}
impl Default for PqParams {
    fn default() -> Self {
        Self { active: false, fee_per_byte: default_pq_fee_per_byte(), commitment_grace: default_pq_commitment_grace(), blocks: false, blocks_grace: default_pq_blocks_grace() }
    }
}

/// Static fees of typeGroup 3 (smartoshi). `transfer_per_recipient` is added for every recipient after the first.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenFees {
    /// Every field has a default so a later milestone may patch a single key (e.g. `{ "tokenFees": { "initBurnPercent": 0 } }`).
    #[serde(default = "default_init_fee")]
    pub init: u64,
    #[serde(default = "default_transfer_fee")]
    pub transfer: u64,
    #[serde(default = "default_transfer_per_recipient_fee")]
    pub transfer_per_recipient: u64,
    #[serde(default = "default_mint_fee")]
    pub mint: u64,
    #[serde(default = "default_burn_fee")]
    pub burn: u64,
    /// TokenMeta (on-chain manifest, default 1 STH).
    #[serde(default = "default_meta_fee")]
    pub meta: u64,
    /// Share of the TokenInit fee sent to `burnAddress` (0 = no burn, 100 = whole fee). Milestone-switchable without a code fork.
    #[serde(default = "default_init_burn_percent")]
    pub init_burn_percent: u8,
}

fn default_init_fee() -> u64 {
    50_000_000_000
}
fn default_transfer_fee() -> u64 {
    10_000_000
}
fn default_transfer_per_recipient_fee() -> u64 {
    1_000_000
}
fn default_mint_fee() -> u64 {
    100_000_000
}
fn default_burn_fee() -> u64 {
    10_000_000
}

fn default_meta_fee() -> u64 {
    100_000_000
}

fn default_init_burn_percent() -> u8 {
    50
}

impl TokenFees {
    /// Smartoshi burned per TokenInit under this milestone.
    pub fn init_burn(&self) -> u64 {
        self.init * self.init_burn_percent.min(100) as u64 / 100
    }
}

impl Default for TokenFees {
    fn default() -> Self {
        Self { init: default_init_fee(), transfer: default_transfer_fee(), transfer_per_recipient: default_transfer_per_recipient_fee(), mint: default_mint_fee(), burn: default_burn_fee(), meta: default_meta_fee(), init_burn_percent: default_init_burn_percent() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BlockParams {
    pub version: u32,
    pub id_full_sha256: bool,
    pub max_transactions: u32,
    pub max_payload: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Fees {
    pub static_fees: BTreeMap<String, u64>,
}

impl Default for BlockParams {
    fn default() -> Self {
        Self { version: 0, id_full_sha256: true, max_transactions: 500, max_payload: 84_000_000 }
    }
}

impl Default for Milestone {
    fn default() -> Self {
        Self {
            height: 1,
            reward: 0,
            active_delegates: 21,
            blocktime: 8,
            block: BlockParams::default(),
            epoch: "2023-08-29T00:00:00.000Z".into(),
            fees: Fees::default(),
            vendor_field_length: 255,
            multi_payment_limit: 256,
            htlc_enabled: true,
            block_burn_address: true,
            tx_v2: true,
            sobj_active: false,
            schnorr_blocks: false,
            tokens: false,
            token_fees: TokenFees::default(),
            token_transfer_max_recipients: 64,
            finality: FinalityParams::default(),
            sobj_v2: false,
            strict_balance: false,
            min_core_version: String::new(),
            pq: PqParams::default(),
        }
    }
}

impl Milestone {
    pub fn block_version(&self) -> u32 {
        self.block.version
    }
    pub fn id_full_sha256(&self) -> bool {
        self.block.id_full_sha256
    }
    pub fn max_transactions(&self) -> u32 {
        self.block.max_transactions
    }
    pub fn max_payload(&self) -> u64 {
        self.block.max_payload
    }
    /// Static fee of a core transaction type by its camelCase name (`transfer`, `vote`, ...).
    pub fn static_fee(&self, name: &str) -> u64 {
        self.fees.static_fees.get(name).copied().unwrap_or(0)
    }
    /// Static fee of a core transaction type id (0 for unknown types).
    pub fn static_fee_for_type(&self, type_: u16) -> u64 {
        FEE_NAMES.iter().find(|(_, t)| *t == type_).map(|(name, _)| self.static_fee(name)).unwrap_or(0)
    }
}

fn de_u64_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
    let v = Value::deserialize(d)?;
    match v {
        Value::Number(n) => n.as_u64().ok_or_else(|| serde::de::Error::custom("negative")),
        Value::String(s) => s.parse().map_err(serde::de::Error::custom),
        _ => Err(serde::de::Error::custom("expected number or string")),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NetworkJson {
    name: String,
    message_prefix: String,
    pub_key_hash: u8,
    nethash: String,
    wif: u8,
    #[serde(default)]
    slip44: u32,
    #[serde(default)]
    burn_address: String,
    client: ClientJson,
}

#[derive(Debug, Clone, Deserialize)]
struct ClientJson {
    token: String,
    symbol: String,
    explorer: String,
}

/// Static network configuration.
#[derive(Debug, Clone)]
pub struct Network {
    pub name: String,
    pub message_prefix: String,
    pub pubkey_hash: u8,
    pub wif: u8,
    pub slip44: u32,
    pub nethash: String,
    /// Id of block 1 (core patches the computed genesis id with this value).
    pub genesis_block_id: String,
    pub token: String,
    pub symbol: String,
    pub explorer: String,
    pub burn_address: String,
    /// Unix timestamp (seconds) of the chain epoch (block timestamp 0).
    pub epoch_unix: i64,
    milestones: Arc<Vec<Milestone>>,
    genesis_gz: Arc<[u8]>,
    /// Raw JSON as served by `/api/node/configuration/crypto`.
    pub raw_network: Arc<Value>,
    pub raw_milestones: Arc<Value>,
    pub raw_exceptions: Arc<Value>,
}

/// Historical / alias flag names (`aip11`, `txV2`, `aip36`, `sobj`, `aip37`, `schnorrBlocks`) → SHIP keys, so a file mixing
/// old and new spellings across milestones merges into one field instead of tripping serde's duplicate-field check.
pub const MILESTONE_KEY_ALIASES: [(&str, &str); 6] = [("aip11", "ship11"), ("txV2", "ship11"), ("aip36", "ship13"), ("sobj", "ship13"), ("aip37", "ship11Blocks"), ("schnorrBlocks", "ship11Blocks")];

fn canonical_milestone_keys(entry: &Value) -> Value {
    let Some(obj) = entry.as_object() else { return entry.clone() };
    let mut out = Map::new();
    for (k, v) in obj {
        let key = MILESTONE_KEY_ALIASES.iter().find(|(old, _)| old == k).map(|(_, new)| (*new).to_string()).unwrap_or_else(|| k.clone());
        out.insert(key, v.clone());
    }
    Value::Object(out)
}

fn deep_merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                match b.get_mut(k) {
                    Some(slot) if slot.is_object() && v.is_object() => deep_merge(slot, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

/// `2023-08-29T00:00:00.000Z` → unix seconds (UTC, no leap seconds — same as `Date.parse`).
fn parse_epoch(iso: &str) -> Result<i64> {
    let bad = || Error::Config(format!("invalid epoch {iso}"));
    let (date, time) = iso.split_once('T').ok_or_else(bad)?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().map_err(|_| bad()));
    let (y, m, day) = (d.next().ok_or_else(bad)??, d.next().ok_or_else(bad)??, d.next().ok_or_else(bad)??);
    let time = time.trim_end_matches('Z');
    let time = time.split('.').next().unwrap_or(time);
    let mut t = time.split(':').map(|p| p.parse::<i64>().map_err(|_| bad()));
    let (hh, mm, ss) = (t.next().ok_or_else(bad)??, t.next().ok_or_else(bad)??, t.next().ok_or_else(bad)??);
    // days_from_civil (Howard Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Ok(days * 86_400 + hh * 3_600 + mm * 60 + ss)
}

impl Network {
    /// SmartHoldem mainnet (network byte 63 / 0x3f) from the embedded `network/mainnet` files.
    /// Cheap: the parsed configuration is shared behind `Arc`s.
    pub fn mainnet() -> Self {
        Self::mainnet_ref().clone()
    }

    pub fn mainnet_ref() -> &'static Network {
        static MAINNET: OnceLock<Network> = OnceLock::new();
        MAINNET.get_or_init(|| {
            Self::from_json(MAINNET_NETWORK_JSON, MAINNET_MILESTONES_JSON, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ)
                .expect("embedded mainnet configuration is valid")
        })
    }

    /// Load `network.json`, `milestones.json`, `exceptions.json` (+ optional `genesisBlock.json[.gz]`) from `dir`;
    /// files that are missing fall back to the embedded mainnet ones.
    pub fn from_dir(dir: &Path) -> Result<Self> {
        if !dir.is_dir() {
            return Err(Error::Config(format!("network_dir {} does not exist", dir.display())));
        }
        let read = |name: &str, fallback: &str| -> Result<String> {
            let p = dir.join(name);
            if p.exists() {
                std::fs::read_to_string(&p).map_err(|e| Error::Config(format!("cannot read {}: {e}", p.display())))
            } else {
                Ok(fallback.to_string())
            }
        };
        let genesis_gz = dir.join("genesisBlock.json.gz");
        let genesis_plain = dir.join("genesisBlock.json");
        let genesis: Vec<u8> = if genesis_gz.exists() {
            std::fs::read(&genesis_gz).map_err(|e| Error::Config(format!("cannot read {}: {e}", genesis_gz.display())))?
        } else if genesis_plain.exists() {
            let plain = std::fs::read(&genesis_plain).map_err(|e| Error::Config(format!("cannot read {}: {e}", genesis_plain.display())))?;
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            std::io::Write::write_all(&mut enc, &plain).map_err(|e| Error::Config(e.to_string()))?;
            enc.finish().map_err(|e| Error::Config(e.to_string()))?
        } else {
            MAINNET_GENESIS_GZ.to_vec()
        };
        let net = Self::from_json(
            &read("network.json", MAINNET_NETWORK_JSON)?,
            &read("milestones.json", MAINNET_MILESTONES_JSON)?,
            &read("exceptions.json", MAINNET_EXCEPTIONS_JSON)?,
            &genesis,
        )?;
        tracing::info!(dir = %dir.display(), milestones = net.milestones.len(), "network configuration loaded");
        Ok(net)
    }

    pub fn from_json(network: &str, milestones: &str, exceptions: &str, genesis_gz: &[u8]) -> Result<Self> {
        let raw_network: Value = serde_json::from_str(network).map_err(|e| Error::Config(format!("network.json: {e}")))?;
        let raw_milestones: Value = serde_json::from_str(milestones).map_err(|e| Error::Config(format!("milestones.json: {e}")))?;
        let raw_exceptions: Value = serde_json::from_str(exceptions).map_err(|e| Error::Config(format!("exceptions.json: {e}")))?;
        let n: NetworkJson = serde_json::from_value(raw_network.clone()).map_err(|e| Error::Config(format!("network.json: {e}")))?;
        let entries = raw_milestones.as_array().filter(|a| !a.is_empty()).ok_or_else(|| Error::Config("milestones.json must be a non-empty array".into()))?;
        let mut merged = Value::Object(Map::new());
        let mut milestones = Vec::with_capacity(entries.len());
        for e in entries {
            deep_merge(&mut merged, &canonical_milestone_keys(e));
            let m: Milestone = serde_json::from_value(merged.clone()).map_err(|e| Error::Config(format!("milestones.json: {e}")))?;
            if m.token_fees.init_burn_percent > 100 {
                return Err(Error::Config(format!("milestones.json: tokenFees.initBurnPercent must be 0..=100 (height {})", m.height)));
            }
            milestones.push(m);
        }
        milestones.sort_by_key(|m| m.height);
        let epoch_unix = parse_epoch(&milestones[0].epoch)?;
        let genesis_block_id = genesis_id(genesis_gz)?;
        Ok(Self {
            name: n.name,
            message_prefix: n.message_prefix,
            pubkey_hash: n.pub_key_hash,
            wif: n.wif,
            slip44: n.slip44,
            nethash: n.nethash,
            genesis_block_id,
            token: n.client.token,
            symbol: n.client.symbol,
            explorer: n.client.explorer,
            burn_address: n.burn_address,
            epoch_unix,
            milestones: Arc::new(milestones),
            genesis_gz: Arc::from(genesis_gz),
            raw_network: Arc::new(raw_network),
            raw_milestones: Arc::new(raw_milestones),
            raw_exceptions: Arc::new(raw_exceptions),
        })
    }

    pub fn milestones(&self) -> &[Milestone] {
        &self.milestones
    }

    /// Height of the first milestone with `pq.active` (Quantum Shield stage B), if any.
    pub fn pq_activation_height(&self) -> Option<u64> {
        self.milestones.iter().find(|m| m.pq.active).map(|m| m.height)
    }

    /// First height with `pq.blocks` (hybrid block signatures), if any milestone enables it.
    pub fn pq_blocks_activation_height(&self) -> Option<u64> {
        self.milestones.iter().find(|m| m.pq.blocks).map(|m| m.height)
    }

    /// Block versions accepted at `height`: (v1 allowed, v0 allowed). v0 stays legal during `pq.blocksGrace`.
    pub fn block_versions_allowed(&self, height: u64) -> (bool, bool) {
        let m = self.milestone(height);
        if !m.pq.blocks {
            return (false, true);
        }
        let grace_end = self.pq_blocks_activation_height().unwrap_or(height).saturating_add(m.pq.blocks_grace);
        (true, height < grace_end)
    }

    /// gzip-compressed `genesisBlock.json` of this network.
    pub fn genesis_gz(&self) -> &[u8] {
        &self.genesis_gz
    }

    /// Milestone in effect at `height` (last milestone with `height <= h`).
    pub fn milestone(&self, height: u64) -> &Milestone {
        let h = height.max(1);
        self.milestones.iter().rev().find(|m| m.height <= h).unwrap_or(&self.milestones[0])
    }

    /// Convert a chain (epoch) timestamp to unix seconds.
    pub fn epoch_to_unix(&self, epoch: u32) -> i64 {
        self.epoch_unix + epoch as i64
    }

    /// Convert unix seconds to a chain (epoch) timestamp.
    pub fn unix_to_epoch(&self, unix: i64) -> u32 {
        (unix - self.epoch_unix).max(0) as u32
    }

    /// Current chain timestamp (`Slots.getTime()` in core).
    pub fn now_epoch(&self) -> u32 {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        self.unix_to_epoch(now)
    }

    /// Static fees of the milestone at `height` as `(name, type, fee)` in core type order.
    pub fn static_fees(&self, height: u64) -> Vec<(&'static str, u16, u64)> {
        let m = self.milestone(height);
        FEE_NAMES.iter().map(|(name, t)| (*name, *t, m.static_fee(name))).collect()
    }
}

impl Default for Network {
    fn default() -> Self {
        Self::mainnet()
    }
}

/// Only the block id is needed here; the full genesis block lives in `crate::genesis`.
fn genesis_id(gz: &[u8]) -> Result<String> {
    #[derive(Deserialize)]
    struct Head {
        id: String,
    }
    let mut json = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(gz), &mut json).map_err(|e| Error::Config(format!("genesisBlock gunzip: {e}")))?;
    let h: Head = serde_json::from_slice(&json).map_err(|e| Error::Config(format!("genesisBlock.json: {e}")))?;
    Ok(h.id)
}

/// Core transaction type names in type order (`/api/transactions/fees`, `/api/node/fees`).
pub const FEE_NAMES: [(&str, u16); 11] = [
    ("transfer", 0),
    ("secondSignature", 1),
    ("delegateRegistration", 2),
    ("vote", 3),
    ("multiSignature", 4),
    // type 5 = Netfory content pointer; `ipfs` is the legacy JSON key of staticFees / addonBytes that wallets rely on
    ("ipfs", 5),
    ("multiPayment", 6),
    ("delegateResignation", 7),
    ("htlcLock", 8),
    ("htlcClaim", 9),
    ("htlcRefund", 10),
];

/// Total supply of the chain (smartoshi), reported by `/api/blockchain`.
pub const TOTAL_SUPPLY: u64 = 24_977_000_000_000_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ship_keys_and_historical_aliases_merge_into_one_field() {
        // historical aipNN spellings, wallet-lib aliases and SHIP keys may be mixed across milestones
        let ms = MAINNET_MILESTONES_JSON
            .replace("\"ship11\": true", "\"aip11\": true")
            .replace("\"ship11Blocks\": true", "\"schnorrBlocks\": true")
            .replace("\"ship13\": true", "\"aip36\": true, \"sobj\": true");
        let n = Network::from_json(MAINNET_NETWORK_JSON, &ms, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap();
        assert!(n.milestone(1).tx_v2);
        assert!(n.milestone(564_000).schnorr_blocks && !n.milestone(563_999).schnorr_blocks);
        assert!(n.milestone(11_800_000).sobj_active && !n.milestone(11_799_999).sobj_active);
        // the embedded file itself uses the SHIP names
        assert!(MAINNET_MILESTONES_JSON.contains("\"ship11\"") && MAINNET_MILESTONES_JSON.contains("\"ship13\"") && MAINNET_MILESTONES_JSON.contains("\"ship11Blocks\""));
        assert!(!MAINNET_MILESTONES_JSON.contains("aip"));
    }

    #[test]
    fn mainnet_milestones_merge() {
        let n = Network::mainnet();
        assert_eq!(n.pubkey_hash, 63);
        assert_eq!(n.epoch_unix, 1_693_267_200);
        assert_eq!(n.genesis_block_id, "ea60ebb15e3e8abe9e47a7ef18145d65c570c3dfe2b0ac678c665787860bae32");
        assert_eq!(n.milestone(1).static_fee("delegateRegistration"), 100_000_000_000);
        assert_eq!(n.milestone(151_200).static_fee("delegateRegistration"), 1_000_000_000_000);
        assert_eq!(n.milestone(151_200).static_fee("transfer"), 100_000_000);
        assert!(!n.milestone(563_999).schnorr_blocks);
        assert!(n.milestone(564_000).schnorr_blocks);
        assert_eq!(n.milestone(10_000_000).blocktime, 8);
        assert!(n.milestone(10_000_000).block_burn_address);
    }

    #[test]
    fn epoch_parsing() {
        assert_eq!(parse_epoch("1970-01-01T00:00:00.000Z").unwrap(), 0);
        assert_eq!(parse_epoch("2023-08-29T00:00:00.000Z").unwrap(), 1_693_267_200);
    }
}
