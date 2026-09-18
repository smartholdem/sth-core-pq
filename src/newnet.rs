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
    pub ticker: String,
    pub title: String,
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
}

pub struct NewNet {
    pub nethash: String,
    pub genesis_id: String,
    pub treasury_address: String,
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
    base.insert("sobjV2".into(), json!(true));
    base.insert("strictBalance".into(), json!(true));
    base.insert("minCoreVersion".into(), json!(env!("CARGO_PKG_VERSION")));
    let mut list = vec![Value::Object(base)];
    if let Some(h) = opts.tokens_at {
        list.push(json!({ "height": h.max(2), "tokens": true }));
    }
    if let Some(h) = opts.pq_at {
        list.push(json!({ "height": h.max(2), "pq": { "active": true, "feePerByte": 10000, "commitmentGrace": 86400 } }));
    }
    if let Some(h) = opts.pq_blocks_at {
        let h = h.max(opts.pq_at.unwrap_or(2).max(2));
        list.push(json!({ "height": h, "pq": { "active": true, "blocks": true, "blocksGrace": 21 } }));
    }
    if opts.finality_hard {
        list[0]["finality"] = json!({ "active": true, "slashing": true });
    }
    Ok(serde_json::to_string_pretty(&list)?)
}

fn network_json(opts: &NewNetOptions, nethash: &str, burn: &str) -> Result<String> {
    let mut n: Map<String, Value> = serde_json::from_str(MAINNET_NETWORK_JSON)?;
    n.insert("name".into(), json!(opts.title));
    n.insert("messagePrefix".into(), json!(format!("{} message:\n", opts.title)));
    n.insert("pubKeyHash".into(), json!(opts.pubkey_hash));
    n.insert("nethash".into(), json!(nethash));
    n.insert("burnAddress".into(), json!(burn));
    n.insert("client".into(), json!({ "token": opts.ticker, "symbol": opts.ticker, "explorer": "" }));
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
    let burn = KeyPair::from_passphrase(&format!("{} burn", opts.seed))?.address(net)?;
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
    cfg.p2p.iroh.enabled = false;
    cfg.p2p.iroh.relay = false;
    cfg.p2p.iroh.relay_n0 = false;
    cfg.delegate.enabled = true;
    cfg.delegate.secrets = passphrases.clone();
    cfg.delegate.min_quorum_peers = 1;
    cfg.delegate.quorum_share = 0.0;
    let yaml = serde_yaml::to_string(&cfg).map_err(|e| Error::Config(format!("yaml: {e}")))?;
    write(
        "node.yaml",
        &format!("# {} — private network generated by `sth-core init newnet` (seed: {}).\n# All {} delegates forge on this node; quorum is disabled (min_quorum_peers 1, quorum_share 0) so a single node runs the chain.\n# Extra nodes: copy this directory, remove `delegate.secrets`, add this node's ip:{} to p2p.legacy_peers.\n{yaml}", opts.title, opts.seed, opts.delegates, opts.p2p_port),
    )?;
    Ok(NewNet { nethash, genesis_id, treasury_address: treasury.address(net)?, delegate_passphrases: passphrases })
}
