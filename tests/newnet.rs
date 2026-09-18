//! Author: TechnoL0g
//! `sth-core init newnet`: generated network files load from any cwd, the genesis applies to an empty DB,
//! the generated delegates forge and a treasury transfer goes through with the new network byte.

use sth_core::config::Network;
use sth_core::crypto::{sign_schnorr_legacy, transaction_id, transaction_signing_hash, KeyPair};
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::models::Transaction;
use sth_core::newnet::{generate, NewNetOptions};
use sth_core::node_config::NodeConfig;
use sth_core::storage::Storage;
use sth_core::sync::{apply_blocks, ChainTip};
use std::sync::Arc;

fn opts(seed: &str) -> NewNetOptions {
    NewNetOptions { ticker: "TST".into(), title: "TestNet".into(), delegates: 3, pubkey_hash: 30, p2p_port: 4102, api_port: 4104, metrics_port: 4989, seed: seed.into(), tokens_at: None, pq_at: None, pq_blocks_at: None, finality_hard: false, treasury_supply: 1_000_000 * 100_000_000, delegate_stake: 1000 * 100_000_000 }
}

#[test]
fn generated_network_is_deterministic_and_loads_from_config_path() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let na = generate(&opts("seed-1"), a.path()).unwrap();
    let nb = generate(&opts("seed-1"), b.path()).unwrap();
    assert_eq!(na.nethash, nb.nethash, "same seed → same network");
    assert_eq!(na.genesis_id, nb.genesis_id);
    assert_ne!(generate(&opts("seed-2"), tempfile::tempdir().unwrap().path()).unwrap().nethash, na.nethash);

    // node.yaml points at `.`; loading it from another cwd must still resolve the network dir
    let cfg = NodeConfig::load(&a.path().join("node.yaml")).unwrap();
    assert_eq!(cfg.network, "TestNet");
    assert!(cfg.sync.rest_nodes.is_empty(), "no mainnet REST nodes in a private network");
    assert!(cfg.p2p.legacy_peers.is_empty());
    assert_eq!(cfg.delegate.secrets.len(), 3);
    assert_eq!(cfg.delegate.quorum_share, 0.0);
    let net = cfg.load_network().unwrap();
    assert_eq!(net.name, "TestNet");
    assert_eq!(net.pubkey_hash, 30);
    assert_eq!(net.nethash, na.nethash);
    assert_eq!(net.genesis_block_id, na.genesis_id);
    assert_eq!(net.milestone(1).active_delegates, 3);
    assert!(!net.milestone(1).tokens);

    // a missing directory is an error, never a silent fallback to mainnet
    assert!(Network::from_dir(&a.path().join("nope")).is_err());
}

#[test]
fn genesis_applies_delegates_forge_and_treasury_pays() {
    let dir = tempfile::tempdir().unwrap();
    let n = generate(&opts("seed-forge"), dir.path()).unwrap();
    let cfg = NodeConfig::load(&dir.path().join("node.yaml")).unwrap();
    let net = cfg.load_network().unwrap();
    let storage = Arc::new(Storage::temporary(net.clone()).unwrap());

    assert!(sth_core::genesis::ensure_genesis(&storage, &net).unwrap());
    assert!(!sth_core::genesis::ensure_genesis(&storage, &net).unwrap(), "idempotent");
    let genesis = storage.get_last_block().unwrap().unwrap();
    assert_eq!(genesis.height, 1);
    assert_eq!(genesis.id.as_deref(), Some(n.genesis_id.as_str()));
    let treasury = storage.get_wallet(&n.treasury_address).unwrap().unwrap();
    assert_eq!(treasury.balance, 1_000_000 * 100_000_000);
    assert_eq!(storage.active_delegates(net.milestone(1).active_delegates as usize).unwrap().len(), 3);

    // the generated delegate keys forge valid blocks under the new network's rules
    let delegates: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("delegates.json")).unwrap()).unwrap();
    let forger = KeyPair::from_passphrase(delegates["delegates"][0]["passphrase"].as_str().unwrap()).unwrap();
    let treasury_keys = KeyPair::from_passphrase(delegates["treasury"]["passphrase"].as_str().unwrap()).unwrap();
    let bob = KeyPair::from_passphrase("bob").unwrap().address(30).unwrap();
    assert!(bob.starts_with('D'), "{bob}");
    let mut t: Transaction = serde_json::from_value(serde_json::json!({
        "version": 2, "network": 30, "typeGroup": 1, "type": 0, "nonce": "1", "senderPublicKey": treasury_keys.public_key_hex(),
        "fee": "100000000", "amount": "500000000000", "recipientId": bob, "expiration": 0
    }))
    .unwrap();
    let h = transaction_signing_hash(&t).unwrap();
    t.signature = Some(sign_schnorr_legacy(&h, treasury_keys.private_key()).unwrap());
    t.id = Some(transaction_id(&t).unwrap());

    let block = forge_block(&net, &forger, &genesis, slot_start(2, 8), vec![t]).unwrap();
    let tip = apply_blocks(&storage, &net, &[block.clone()], ChainTip { height: 1, id: genesis.id.clone() }, true).unwrap();
    assert_eq!(tip.height, 2);
    assert_eq!(storage.get_wallet(&bob).unwrap().unwrap().balance, 500_000_000_000);
    assert_eq!(storage.get_wallet(&n.treasury_address).unwrap().unwrap().balance, 1_000_000 * 100_000_000 - 500_000_000_000 - 100_000_000);
}
