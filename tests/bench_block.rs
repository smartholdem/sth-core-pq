//! Author: TechnoL0g
//!
//! Throughput probe: how long a block with N transfers takes to sign, serialize, verify and apply.
//! `cargo test --test bench_block -- --ignored --nocapture`  (BLOCK_TXS=10000 to change N)

use sth_core::config::Network;
use sth_core::crypto::{
    serialize_block_with_transactions, sign_schnorr_legacy, transaction_id, transaction_signing_hash, verify_block, KeyPair,
};
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::genesis::mainnet_block;
use sth_core::models::Transaction;
use sth_core::storage::Storage;
use std::time::Instant;

fn transfer(keys: &KeyPair, nonce: u64, recipient: &str) -> Transaction {
    let mut tx: Transaction = serde_json::from_value(serde_json::json!({
        "version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": nonce.to_string(),
        "senderPublicKey": keys.public_key_hex(), "fee": "10000000", "amount": "100000000",
        "recipientId": recipient, "expiration": 0
    }))
    .unwrap();
    let hash = transaction_signing_hash(&tx).unwrap();
    tx.signature = Some(sign_schnorr_legacy(&hash, keys.private_key()).unwrap());
    tx.id = Some(transaction_id(&tx).unwrap());
    tx
}

#[test]
#[ignore]
fn block_throughput() {
    let n: usize = std::env::var("BLOCK_TXS").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let network = {
        // lift the 500-tx block limit for the probe (consensus parameter, milestones.json)
        let ms = sth_core::config::MAINNET_MILESTONES_JSON.replace("\"maxTransactions\": 500", &format!("\"maxTransactions\": {}", n.max(500)));
        Network::from_json(sth_core::config::MAINNET_NETWORK_JSON, &ms, sth_core::config::MAINNET_EXCEPTIONS_JSON, sth_core::config::MAINNET_GENESIS_GZ).unwrap()
    };
    let storage = Storage::temporary(network.clone()).unwrap();
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let genesis = mainnet_block().unwrap();
    let forger = KeyPair::from_passphrase("this is a top secret passphrase").unwrap();
    let senders: Vec<KeyPair> = (0..64).map(|i| KeyPair::from_passphrase(&format!("sender {i}")).unwrap()).collect();
    let recipient = KeyPair::from_passphrase("recipient").unwrap().address(63).unwrap();

    let t = Instant::now();
    let txs: Vec<Transaction> = (0..n).map(|i| transfer(&senders[i % 64], (i / 64) as u64 + 1, &recipient)).collect();
    let sign_ms = t.elapsed().as_millis();

    let t = Instant::now();
    let block = forge_block(&network, &forger, &genesis, slot_start(1, 8), txs).unwrap();
    let forge_ms = t.elapsed().as_millis();

    let t = Instant::now();
    let bytes = serialize_block_with_transactions(&block, &network).unwrap();
    let ser_ms = t.elapsed().as_millis();

    let t = Instant::now();
    let v = verify_block(&block, &network);
    let verify_ms = t.elapsed().as_millis();
    assert!(v.verified, "{:?}", v.errors);

    let t = Instant::now();
    storage.apply_block(&block).unwrap();
    let apply_ms = t.elapsed().as_millis();
    assert_eq!(storage.get_last_height().unwrap(), 2);

    let json_len = serde_json::to_vec(&block).unwrap().len();
    println!(
        "\n{n} transfers | sign {sign_ms} ms | forge {forge_ms} ms | serialize {ser_ms} ms → {} KB binary ({} B/tx), {} KB json | verify {verify_ms} ms | apply {apply_ms} ms | total verify+apply {} ms",
        bytes.len() / 1024,
        bytes.len() / n.max(1),
        json_len / 1024,
        verify_ms + apply_ms
    );
}
