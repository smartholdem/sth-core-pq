//! Author: TechnoL0g
//! `sth-core init newnet` — generate a private / test network: network files (mainnet rules from block 1, tokens/PQ off),
//! a signed genesis block with N delegates, their passphrases and a ready node.yaml on separate ports.

use crate::config::{Network, MAINNET_GENESIS_GZ, MAINNET_MILESTONES_JSON, MAINNET_NETWORK_JSON};
use crate::crypto::{sign_schnorr_legacy, transaction_id, transaction_signing_hash, KeyPair};
use crate::delegate::block_builder::forge_block;
use crate::error::{Error, Result};
use crate::models::{Block, Transaction};
use crate::node_config::NodeConfig;
use serde_json::{json, Map, Value};
use std::path::Path;

pub struct NewNetOptions {
    /// `client.token` — its first letter picks the address prefix (`pubkey_hash`) and the burn address.
    pub ticker: String,
    /// Network name (`network.json.name`, `node.yaml network`, folder name in the wizard).
    pub title: String,
    /// `client.symbol` (empty = same as the token).
    pub symbol: String,
    /// `messagePrefix` (empty = "<title> message:\n").
    pub message_prefix: String,
    pub slip44: u32,
    pub wif: u8,
    /// Everything mainnet has plus newer rules from the first blocks: tokens + market, PQ v3 transactions,
    /// hard finality, service nodes (PQ hybrid blocks stay off: delegates must `pq-register` first).
    pub all_features: bool,
    pub delegates: u16,
    pub pubkey_hash: u8,
    pub p2p_port: u16,
    pub api_port: u16,
    pub metrics_port: u16,
    pub seed: String,
    /// Height at which native tokens activate (None = off).
    pub tokens_at: Option<u64>,
    /// Height at which Quantum Shield stage B (v3 transactions) activates (None = off).
    pub pq_at: Option<u64>,
    /// Height at which stage C (hybrid block signatures, `pq.blocks`) activates; grace = 21 blocks (None = off).
    pub pq_blocks_at: Option<u64>,
    /// SHIP-35 hard finality from height 1 (rollback below a certificate refused, equivocation slashing on). Default soft.
    pub finality_hard: bool,
    pub treasury_supply: u64,
    pub delegate_stake: u64,
    /// Smartoshi paid to the forger of every block from height 2 (0 = no block reward).
    pub block_reward: u64,
    /// Network epoch (`milestones.json` `epoch`, ISO-8601 UTC); None = the current time, rounded to the second.
    pub epoch: Option<String>,
}

/// Current UTC time as a milestone epoch, e.g. `2026-11-09T08:39:06.000Z`.
pub fn now_epoch() -> String {
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    crate::api::render::human_time(unix)
}

impl Default for NewNetOptions {
    fn default() -> Self {
        Self {
            ticker: "TSH".into(), title: "smartholdem-testnet".into(), symbol: String::new(), message_prefix: String::new(), slip44: 255, wif: 255, all_features: false,
            delegates: 21, pubkey_hash: 30, p2p_port: 4002, api_port: 4004, metrics_port: 4889, seed: String::new(),
            tokens_at: None, pq_at: None, pq_blocks_at: None, finality_hard: false, treasury_supply: 100_000_000 * 100_000_000, delegate_stake: 1_000 * 100_000_000, block_reward: 0, epoch: None,
        }
    }
}

/// "2.5" → 250_000_000 smartoshi (up to 8 decimals, `_` / `,` as thousands separators).
pub fn parse_coins(v: &str) -> Option<u64> {
    let v = v.replace(['_', ','], "");
    let (int, frac) = v.split_once('.').unwrap_or((&v, ""));
    if int.is_empty() && frac.is_empty() || frac.len() > 8 || !int.chars().chain(frac.chars()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let int: u64 = if int.is_empty() { 0 } else { int.parse().ok()? };
    let frac: u64 = if frac.is_empty() { 0 } else { format!("{frac:0<8}").parse().ok()? };
    int.checked_mul(100_000_000)?.checked_add(frac)
}

/// Smartoshi as coins without trailing zeros: 250_000_000 → "2.5".
pub fn coins(v: u64) -> String {
    let s = format!("{}.{:08}", v / 100_000_000, v % 100_000_000);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

const BASE58: &str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// Address version byte whose every address starts with `c` (63 for 'S' like mainnet); None if no byte gives `c`.
pub fn pubkey_hash_for(c: char) -> Option<u8> {
    let starts = |v: u8| {
        let enc = |fill: u8| bs58::encode([&[v][..], &[fill; 24][..]].concat()).into_string();
        let (lo, hi) = (enc(0), enc(0xff));
        lo.len() == 34 && hi.len() == 34 && lo.starts_with(c) && hi.starts_with(c)
    };
    if c == 'S' && starts(63) {
        return Some(63);
    }
    (0..=255u8).find(|&v| starts(v))
}

/// Characters an address can start with (each has a version byte where every address begins with it).
pub fn address_start_symbols() -> String {
    BASE58.chars().filter(|c| pubkey_hash_for(*c).is_some()).collect()
}

/// Unspendable burn address with the network prefix: a readable "<TOKEN>burnAddress…" vanity string with a valid
/// checksum (no private key exists for it, like mainnet `STHsmartHoLdemBurnAddrHere…`); falls back to a hash payload.
pub fn burn_address_for(token: &str, version: u8, name: &str) -> String {
    let tok: String = token.chars().filter(|c| BASE58.contains(*c)).collect();
    let c = bs58::encode([&[version][..], &[0u8; 24][..]].concat()).into_string().chars().next().unwrap_or('S');
    for prefix in [format!("{tok}burnAddress"), format!("{tok}burnAddr"), format!("{tok}burn"), format!("{tok}Burn"), format!("{c}{tok}burnAddress"), format!("{c}burnAddress"), format!("{c}burnAddr")] {
        let mut s: String = prefix.chars().take(28).collect();
        while s.len() < 34 {
            s.push('X');
        }
        if let Ok(raw) = bs58::decode(&s).into_vec() {
            if raw.len() == 25 && raw[0] == version {
                let addr = bs58::encode(&raw[..21]).with_check().into_string();
                if addr.starts_with(&prefix[..prefix.len().min(4)]) {
                    return addr;
                }
            }
        }
    }
    use ripemd::Digest as _;
    let h = ripemd::Ripemd160::digest(sha2::Sha256::digest(format!("{name} burn address").as_bytes()));
    bs58::encode([&[version][..], &h[..]].concat()).with_check().into_string()
}

pub struct NewNet {
    pub nethash: String,
    pub genesis_id: String,
    pub treasury_address: String,
    pub burn_address: String,
    /// EndpointId of the generated node (iroh.key), for `p2p.iroh.bootstrap` of further nodes.
    pub iroh_id: String,
    pub delegate_passphrases: Vec<String>,
}

fn deep_merge(into: &mut Map<String, Value>, from: &Map<String, Value>) {
    for (k, v) in from {
        match (into.get_mut(k), v) {
            (Some(Value::Object(a)), Value::Object(b)) => deep_merge(a, b),
            _ => {
                into.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Mainnet milestones collapsed into one entry at height 1 (latest rules everywhere) + newnet switches.
fn milestones(opts: &NewNetOptions) -> Result<String> {
    let all: Vec<Map<String, Value>> = serde_json::from_str(MAINNET_MILESTONES_JSON)?;
    let mut base = Map::new();
    for m in &all {
        deep_merge(&mut base, m);
    }
    base.insert("height".into(), json!(1));
    base.insert("activeDelegates".into(), json!(opts.delegates));
    base.insert("ship11".into(), json!(true));
    base.insert("ship13".into(), json!(true));
    base.insert("tokens".into(), json!(false));
    if opts.all_features {
        base.insert("tokenMarket".into(), json!({ "enabled": true, "maxOpenOrders": 64 }));
        base.insert("masternodes".into(), json!({ "enabled": true }));
    }
    base.insert("sobjV2".into(), json!(true));
    base.insert("strictBalance".into(), json!(true));
    base.insert("minCoreVersion".into(), json!(env!("CARGO_PKG_VERSION")));
    base.insert("reward".into(), json!("0"));
    // the chain starts now: slot 0 = the moment the network is generated
    base.insert("epoch".into(), json!(opts.epoch.clone().unwrap_or_else(now_epoch)));
    let mut list = vec![Value::Object(base)];
    let tokens_at = opts.tokens_at.or(opts.all_features.then_some(2));
    let pq_at = opts.pq_at.or(opts.all_features.then_some(2));
    if let Some(h) = tokens_at {
        list.push(json!({ "height": h.max(2), "tokens": true }));
    }
    if let Some(h) = pq_at {
        list.push(json!({ "height": h.max(2), "pq": { "active": true, "feePerByte": 10000, "commitmentGrace": 86400 } }));
    }
    if let Some(h) = opts.pq_blocks_at {
        let h = h.max(pq_at.unwrap_or(2).max(2));
        list.push(json!({ "height": h, "pq": { "active": true, "blocks": true, "blocksGrace": 21 } }));
    }
    if opts.block_reward > 0 {
        list.push(json!({ "height": 2, "reward": opts.block_reward.to_string() }));
    }
    if opts.finality_hard || opts.all_features {
        list[0]["finality"] = json!({ "active": true, "slashing": true });
    }
    // switches landing on one height become one entry
    let mut merged: Vec<Value> = Vec::new();
    for m in list {
        match merged.last_mut() {
            Some(Value::Object(prev)) if prev.get("height") == m.get("height") => deep_merge(prev, m.as_object().unwrap()),
            _ => merged.push(m),
        }
    }
    Ok(serde_json::to_string_pretty(&merged)?)
}

fn network_json(opts: &NewNetOptions, nethash: &str, burn: &str) -> Result<String> {
    let mut n: Map<String, Value> = serde_json::from_str(MAINNET_NETWORK_JSON)?;
    n.insert("name".into(), json!(opts.title));
    n.insert("messagePrefix".into(), json!(if opts.message_prefix.is_empty() { format!("{} message:\n", opts.title) } else { opts.message_prefix.clone() }));
    n.insert("pubKeyHash".into(), json!(opts.pubkey_hash));
    n.insert("slip44".into(), json!(opts.slip44));
    n.insert("wif".into(), json!(opts.wif));
    n.insert("nethash".into(), json!(nethash));
    n.insert("burnAddress".into(), json!(burn));
    n.insert("client".into(), json!({ "token": opts.ticker, "symbol": if opts.symbol.is_empty() { &opts.ticker } else { &opts.symbol }, "explorer": "" }));
    Ok(serde_json::to_string_pretty(&n)?)
}

fn tx(keys: &KeyPair, net: u8, nonce: u64, type_: u16, amount: u64, extra: Value) -> Result<Transaction> {
    let mut v = json!({ "version": 2, "network": net, "typeGroup": 1, "type": type_, "nonce": nonce.to_string(), "senderPublicKey": keys.public_key_hex(), "fee": "0", "amount": amount.to_string(), "timestamp": 0, "expiration": 0 });
    v.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
    let mut t: Transaction = serde_json::from_value(v)?;
    let h = transaction_signing_hash(&t)?;
    t.signature = Some(sign_schnorr_legacy(&h, keys.private_key())?);
    t.id = Some(transaction_id(&t)?);
    Ok(t)
}

/// Writes `network.json`, `milestones.json`, `exceptions.json`, `genesisBlock.json`, `delegates.json`, `node.yaml` into `out`.
pub fn generate(opts: &NewNetOptions, out: &Path) -> Result<NewNet> {
    std::fs::create_dir_all(out).map_err(|e| Error::Config(format!("cannot create {}: {e}", out.display())))?;
    let net = opts.pubkey_hash;
    let genesis_keys = KeyPair::from_passphrase(&format!("{} genesis", opts.seed))?;
    let treasury = KeyPair::from_passphrase(&format!("{} treasury", opts.seed))?;
    let burn = burn_address_for(&opts.ticker, net, &opts.title);
    let passphrases: Vec<String> = (1..=opts.delegates).map(|i| format!("{} delegate {i}", opts.seed)).collect();
    let delegates: Vec<KeyPair> = passphrases.iter().map(|p| KeyPair::from_passphrase(p)).collect::<Result<_>>()?;

    // network with placeholder nethash/genesis (only rules are needed to forge)
    let ms = milestones(opts)?;
    let exceptions = "{}";
    let placeholder = Network::from_json(&network_json(opts, &"0".repeat(64), &burn)?, &ms, exceptions, MAINNET_GENESIS_GZ)?;

    let mut txs = Vec::new();
    let mut nonce = 1;
    txs.push(tx(&genesis_keys, net, nonce, 0, opts.treasury_supply, json!({ "recipientId": treasury.address(net)? }))?);
    for d in &delegates {
        nonce += 1;
        txs.push(tx(&genesis_keys, net, nonce, 0, opts.delegate_stake, json!({ "recipientId": d.address(net)? }))?);
    }
    for (i, d) in delegates.iter().enumerate() {
        txs.push(tx(d, net, 1, 2, 0, json!({ "asset": { "delegate": { "username": format!("genesis_{}", i + 1) } } }))?);
    }
    for d in &delegates {
        txs.push(tx(d, net, 2, 3, 0, json!({ "asset": { "votes": [format!("+{}", d.public_key_hex())] } }))?);
    }
    let previous = Block { id: Some("0".repeat(64)), height: 0, ..serde_json::from_value(json!({ "id": "0".repeat(64), "version": 0, "timestamp": 0, "previousBlock": "", "height": 0, "numberOfTransactions": 0, "totalAmount": "0", "totalFee": "0", "reward": "0", "payloadLength": 0, "payloadHash": "", "generatorPublicKey": genesis_keys.public_key_hex(), "transactions": [] }))? };
    let mut genesis = forge_block(&placeholder, &genesis_keys, &previous, 0, txs)?;
    genesis.previous_block = "0".repeat(64);
    let nethash = genesis.payload_hash.clone();
    let genesis_id = genesis.id.clone().unwrap_or_default();

    let write = |name: &str, body: &str| std::fs::write(out.join(name), body).map_err(|e| Error::Config(format!("cannot write {name}: {e}")));
    write("network.json", &network_json(opts, &nethash, &burn)?)?;
    write("milestones.json", &ms)?;
    write("exceptions.json", exceptions)?;
    write("genesisBlock.json", &serde_json::to_string_pretty(&genesis)?)?;
    let delegates_json: Vec<Value> = delegates.iter().zip(&passphrases).enumerate().map(|(i, (d, p))| json!({ "username": format!("genesis_{}", i + 1), "publicKey": d.public_key_hex(), "address": d.address(net).unwrap_or_default(), "passphrase": p })).collect();
    write("delegates.json", &serde_json::to_string_pretty(&json!({ "network": opts.title, "nethash": nethash, "treasury": { "address": treasury.address(net)?, "passphrase": format!("{} treasury", opts.seed) }, "delegates": delegates_json }))?)?;

    let mut cfg = NodeConfig::default();
    cfg.network = opts.title.clone();
    cfg.network_dir = ".".into();
    cfg.db_path = "./data".into();
    cfg.api.host = "0.0.0.0".into();
    cfg.api.port = opts.api_port;
    cfg.api.page_metrics = true;
    cfg.api.metrics_listen = format!("0.0.0.0:{}", opts.metrics_port);
    cfg.sync.bootstrap_snapshot = String::new();
    cfg.sync.rest_nodes = Vec::new();
    cfg.sync.fast_import = false;
    cfg.p2p.legacy_port = opts.p2p_port;
    cfg.p2p.legacy_peers = Vec::new();
    cfg.p2p.use_peer_list = false;
    cfg.p2p.legacy_listen = format!("0.0.0.0:{}", opts.p2p_port);
    // iroh on (finality votes, wallet stream), isolated from mainnet: no mainnet bootstrap, no relays, no n0 DNS;
    // gossip topics and address records are bound to this nethash, GetStatus of a foreign-nethash peer is refused
    cfg.p2p.iroh.enabled = true;
    cfg.p2p.iroh.bootstrap = Vec::new();
    cfg.p2p.iroh.relay = false;
    cfg.p2p.iroh.relay_n0 = false;
    cfg.p2p.iroh.discovery.n0_dns = false;
    cfg.p2p.iroh.discovery.listen_port = opts.p2p_port;
    cfg.delegate.enabled = true;
    cfg.delegate.secrets = passphrases.clone();
    cfg.delegate.min_quorum_peers = 1;
    cfg.delegate.quorum_share = 0.0;
    let iroh_secret: [u8; 32] = crate::crypto::sha256(format!("{} iroh", opts.seed).as_bytes());
    write("iroh.key", &hex::encode(iroh_secret))?;
    let iroh_id = iroh::SecretKey::from_bytes(&iroh_secret).public().to_string();
    let yaml = serde_yaml::to_string(&cfg).map_err(|e| Error::Config(format!("yaml: {e}")))?;
    write(
        "node.yaml",
        &format!("# {} — private network generated by `sth-core init newnet` (seed: {}).\n# All {} delegates forge on this node; quorum is disabled (min_quorum_peers 1, quorum_share 0) so a single node runs the chain.\n# iroh is on and isolated from mainnet (no mainnet bootstrap / relays, topics bound to this nethash). This node: {iroh_id}\n# Extra nodes: copy this directory, remove `delegate.secrets` and iroh.key, set p2p.iroh.bootstrap: [\"{iroh_id}@<this ip>:{}\"]\n# (UDP) and/or add <this ip> to p2p.legacy_peers (TCP {}).\n{yaml}", opts.title, opts.seed, opts.delegates, opts.p2p_port, opts.p2p_port),
    )?;
    Ok(NewNet { nethash, genesis_id, treasury_address: treasury.address(net)?, burn_address: burn, iroh_id, delegate_passphrases: passphrases })
}
