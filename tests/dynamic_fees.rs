//! Author: TechnoL0g
//!
//! Per-node dynamic fee policy of the mempool (legacy `transactionPool.dynamicFees`): `(addon + bytes) × rate` minimum,
//! broadcast threshold, exact static fee when disabled, blocks never judged by it, node.yaml round trip.

use sth_core::config::Network;
use sth_core::crypto::{serialize_transaction, sign_schnorr_legacy, transaction_id, transaction_signing_hash, KeyPair, SerializeOptions};
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::genesis::mainnet_block;
use sth_core::mempool::Mempool;
use sth_core::models::Transaction;
use sth_core::node_config::{DynamicFeesConfig, NodeConfig};
use sth_core::storage::Storage;
use sth_core::sync::{apply_blocks, ChainTip};
use std::sync::Arc;

fn transfer(keys: &KeyPair, nonce: u64, to: &str, fee: u64) -> Transaction {
    let v = serde_json::json!({ "version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": nonce.to_string(), "senderPublicKey": keys.public_key_hex(),
        "amount": "100000000", "fee": fee.to_string(), "recipientId": to, "expiration": 0 });
    let mut t: Transaction = serde_json::from_value(v).unwrap();
    let h = transaction_signing_hash(&t).unwrap();
    t.signature = Some(sign_schnorr_legacy(&h, keys.private_key()).unwrap());
    t.id = Some(transaction_id(&t).unwrap());
    t
}

fn wire_bytes(t: &Transaction) -> usize {
    serialize_transaction(t, SerializeOptions::default(), Network::mainnet_ref()).unwrap().len()
}

fn funded_chain() -> (Arc<Storage>, KeyPair, String) {
    let network = Network::mainnet();
    let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let alice = KeyPair::from_passphrase("alice fees").unwrap();
    storage.update_wallet_state(&alice.address(63).unwrap(), 100_000_000_000, 0).unwrap();
    let bob = KeyPair::from_passphrase("bob fees").unwrap().address(63).unwrap();
    (storage, alice, bob)
}

fn first_error(errors: &std::collections::HashMap<String, sth_core::mempool::PoolError>) -> Option<&str> {
    errors.values().next().map(|e| e.type_.as_str())
}

#[test]
fn defaults_match_the_legacy_mainnet_policy() {
    let df = DynamicFeesConfig::default();
    assert!(df.enabled);
    assert_eq!((df.min_fee_pool, df.min_fee_broadcast), (3_000, 3_000));
    assert_eq!(df.addon_bytes["transfer"], 100);
    assert_eq!(df.addon_bytes["delegateRegistration"], 400_000);
    assert_eq!(df.addon_bytes["htlcClaim"], 0);
    assert_eq!(df.addon_bytes.len(), 11);
    // (100 + 153 bytes) × 3000 = 759 000 smartoshi ≈ 0.0076 STH — the fee a legacy node asks for a plain transfer
    assert_eq!(df.min_fee("transfer", 153, 3_000), 759_000);
    // unknown type = no addon; rate 0 behaves as 1 (legacy guard)
    assert_eq!(df.min_fee("nope", 10, 0), 10);
    // type 5 (Netfory pointer): legacy key `ipfs`, node.yaml may spell it `ntfry`
    assert_eq!(df.min_fee("ipfs", 100, 3_000), (250 + 100) * 3_000);
    let mut ntfry = df.clone();
    ntfry.addon_bytes.remove("ipfs");
    ntfry.addon_bytes.insert("ntfry".into(), 300);
    assert_eq!(ntfry.min_fee("ipfs", 100, 3_000), (300 + 100) * 3_000);
}

#[test]
fn node_yaml_round_trips_and_partial_override_keeps_addon_defaults() {
    let yaml = NodeConfig::default_yaml();
    assert!(yaml.contains("dynamic_fees:") && yaml.contains("min_fee_pool: 3000") && yaml.contains("delegateRegistration: 400000"));
    let back = NodeConfig::parse(&yaml).unwrap();
    assert_eq!(back.mempool.dynamic_fees, DynamicFeesConfig::default());
    let custom = NodeConfig::parse("mempool:\n  dynamic_fees:\n    enabled: false\n    min_fee_pool: 5000\n").unwrap();
    assert!(!custom.mempool.dynamic_fees.enabled);
    assert_eq!(custom.mempool.dynamic_fees.min_fee_pool, 5_000);
    assert_eq!(custom.mempool.dynamic_fees.addon_bytes["vote"], 100);
}

#[tokio::test]
async fn dynamic_minimum_is_addon_plus_bytes_times_rate() {
    let (storage, alice, bob) = funded_chain();
    let pool = Mempool::new(storage, vec![], 100);
    let probe = transfer(&alice, 1, &bob, 1);
    assert_eq!(wire_bytes(&probe), Mempool::TRANSFER_WIRE_BYTES, "plain v2 transfer size used by min_transfer_fee()");
    assert_eq!(pool.min_transfer_fee(), (100 + 156) * 3_000);
    let min = (100 + wire_bytes(&probe) as u64) * 3_000;

    let (resp, errors) = pool.add_many(vec![transfer(&alice, 1, &bob, min - 1)]).await;
    assert_eq!(first_error(&errors), Some("ERR_LOW_FEE"), "{errors:?}");
    assert!(resp.accept.is_empty());
    assert!(errors.values().next().unwrap().message.contains(&min.to_string()));

    let (resp, errors) = pool.add_many(vec![transfer(&alice, 1, &bob, min)]).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(resp.accept.len(), 1);
    assert_eq!(resp.broadcast.len(), 1);
    // the legacy static fee (1 STH) is of course still fine
    let (_, errors) = pool.add_many(vec![transfer(&alice, 2, &bob, 100_000_000)]).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(pool.len().await, 2);
}

#[tokio::test]
async fn below_broadcast_threshold_is_accepted_but_kept_local() {
    let (storage, alice, bob) = funded_chain();
    let cfg = DynamicFeesConfig { min_fee_pool: 1_000, min_fee_broadcast: 3_000, ..Default::default() };
    let pool = Mempool::new(storage, vec![], 100).with_dynamic_fees(cfg);
    let probe = transfer(&alice, 1, &bob, 1);
    let bytes = 100 + wire_bytes(&probe) as u64;

    let (resp, errors) = pool.add_many(vec![transfer(&alice, 1, &bob, bytes * 1_000)]).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(resp.accept.len(), 1);
    assert!(resp.broadcast.is_empty(), "fee between pool and broadcast thresholds must not be relayed");

    let (resp, errors) = pool.add_many(vec![transfer(&alice, 2, &bob, bytes * 3_000)]).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(resp.broadcast.len(), 1);
}

#[tokio::test]
async fn disabled_policy_demands_the_exact_static_fee() {
    let (storage, alice, bob) = funded_chain();
    let cfg = DynamicFeesConfig { enabled: false, ..Default::default() };
    let pool = Mempool::new(storage, vec![], 100).with_dynamic_fees(cfg);

    let (_, errors) = pool.add_many(vec![transfer(&alice, 1, &bob, 10_000_000)]).await;
    assert_eq!(first_error(&errors), Some("ERR_LOW_FEE"), "{errors:?}");
    let (_, errors) = pool.add_many(vec![transfer(&alice, 1, &bob, 200_000_000)]).await;
    assert_eq!(first_error(&errors), Some("ERR_LOW_FEE"), "overpaying is not allowed either: {errors:?}");
    let (resp, errors) = pool.add_many(vec![transfer(&alice, 1, &bob, 100_000_000)]).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(resp.broadcast.len(), 1);
}

#[test]
fn blocks_with_low_fee_transactions_are_still_applied() {
    // the fee policy is a pool matter: a block forged by another node with a 1-smartoshi transfer is valid
    let (storage, alice, bob) = funded_chain();
    let network = Network::mainnet();
    let genesis = mainnet_block().unwrap();
    let forger = KeyPair::from_passphrase("forger").unwrap();
    let block = forge_block(&network, &forger, &genesis, slot_start(2, 8), vec![transfer(&alice, 1, &bob, 1)]).unwrap();
    let tip = apply_blocks(&storage, &network, &[block], ChainTip { height: 1, id: genesis.id.clone() }, true).unwrap();
    assert_eq!(tip.height, 2);
    assert_eq!(storage.get_wallet(&bob).unwrap().unwrap().balance, 100_000_000);
}

#[test]
fn ship41_wire_sizes_used_by_wallets() {
    // SHIP-41 table: vote with one +vote = 158 bytes, delegate registration = 124 + len(username)
    let alice = KeyPair::from_passphrase("alice fees").unwrap();
    let vote = serde_json::json!({ "version": 2, "network": 63, "typeGroup": 1, "type": 3, "nonce": "1", "senderPublicKey": alice.public_key_hex(), "amount": "0", "fee": "1", "expiration": 0,
        "asset": { "votes": [format!("+{}", alice.public_key_hex())] } });
    let mut t: Transaction = serde_json::from_value(vote).unwrap();
    t.signature = Some(sign_schnorr_legacy(&transaction_signing_hash(&t).unwrap(), alice.private_key()).unwrap());
    assert_eq!(wire_bytes(&t), 158);
    let reg = serde_json::json!({ "version": 2, "network": 63, "typeGroup": 1, "type": 2, "nonce": "1", "senderPublicKey": alice.public_key_hex(), "amount": "0", "fee": "1", "expiration": 0,
        "asset": { "delegate": { "username": "alice" } } });
    let mut t: Transaction = serde_json::from_value(reg).unwrap();
    t.signature = Some(sign_schnorr_legacy(&transaction_signing_hash(&t).unwrap(), alice.private_key()).unwrap());
    assert_eq!(wire_bytes(&t), 124 + 5);
}

#[test]
fn sth_cli_rounds_the_ship41_minimum_up_to_a_thousandth_of_sth() {
    let df = DynamicFeesConfig::default();
    // transfer: (100 + 156) × 3000 = 768 000 → 0.008 STH; with an 18-byte memo: (100 + 174) × 3000 = 822 000 → 0.009 STH
    assert_eq!(sth_core::cli::dynamic_fee(&df, "transfer", 156), 800_000);
    assert_eq!(sth_core::cli::dynamic_fee(&df, "transfer", 174), 900_000);
    // exact multiples stay as they are; the higher of the two rates is used
    let df2 = DynamicFeesConfig { min_fee_pool: 1_000, min_fee_broadcast: 5_000, ..Default::default() };
    assert_eq!(sth_core::cli::dynamic_fee(&df2, "transfer", 100), 1_000_000);
    assert_eq!(sth_core::cli::dynamic_fee(&df2, "vote", 158), 1_300_000, "(100 + 158) × 5000 = 1 290 000 → 0.013 STH");
}
