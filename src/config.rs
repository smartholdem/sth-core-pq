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
    pub aip11: bool,
    pub aip37: bool,
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
            aip11: true,
            aip37: false,
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
            deep_merge(&mut merged, e);
            let m: Milestone = serde_json::from_value(merged.clone()).map_err(|e| Error::Config(format!("milestones.json: {e}")))?;
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
    fn mainnet_milestones_merge() {
        let n = Network::mainnet();
        assert_eq!(n.pubkey_hash, 63);
        assert_eq!(n.epoch_unix, 1_693_267_200);
        assert_eq!(n.genesis_block_id, "ea60ebb15e3e8abe9e47a7ef18145d65c570c3dfe2b0ac678c665787860bae32");
        assert_eq!(n.milestone(1).static_fee("delegateRegistration"), 100_000_000_000);
        assert_eq!(n.milestone(151_200).static_fee("delegateRegistration"), 1_000_000_000_000);
        assert_eq!(n.milestone(151_200).static_fee("transfer"), 100_000_000);
        assert!(!n.milestone(563_999).aip37);
        assert!(n.milestone(564_000).aip37);
        assert_eq!(n.milestone(10_000_000).blocktime, 8);
        assert!(n.milestone(10_000_000).block_burn_address);
    }

    #[test]
    fn epoch_parsing() {
        assert_eq!(parse_epoch("1970-01-01T00:00:00.000Z").unwrap(), 0);
        assert_eq!(parse_epoch("2023-08-29T00:00:00.000Z").unwrap(), 1_693_267_200);
    }
}
