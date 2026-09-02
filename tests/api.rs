//! Author: TechnoL0g
//!
//! REST API tests: legacy JSON shapes, filters, pagination and mempool admission over a real socket.

use serde_json::Value;
use sth_core::api::{human_time, serve_listener, AppState};
use sth_core::config::Network;
use sth_core::crypto::{sign_schnorr_legacy, transaction_signing_hash, KeyPair};
use sth_core::mempool::Mempool;
use sth_core::models::{Block, Transaction};
use sth_core::storage::Storage;
use std::sync::Arc;

const BLOCK_11704043: &str = r#"{"id": "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d", "version": 0, "timestamp": 95101456, "previousBlock": "f7f523ce32716bc968383afff31c0def91acefa72df9ba71bb95ab506a0592a7", "height": 11704043, "numberOfTransactions": 1, "totalAmount": "314159265", "totalFee": "100000000", "reward": "0", "payloadLength": 32, "payloadHash": "fdee5b08437ad279fd6461bdfcb48fe8c36a5d394203566ba5bf33819fe2d2e2", "generatorPublicKey": "03d67017411e92a8e93d7b0318e2748f013c130d6e4e01b1248da0ee0959ee9df8", "blockSignature": "3045022100bfcfed36e8019c760490fd453cc28a2118241907d63c3ed0d3004687907107ff02200d300e64fdf5c5ca358e3266794b12b6b900004084b7fba838ceedcb1364e658",
"transactions": [{"version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": "10103", "senderPublicKey": "036560f20d578da7b8433248d0c82e68121163958d533dc74b0e7d8dbabc0606a0", "fee": "100000000", "amount": "314159265", "expiration": 0, "recipientId": "SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw", "signature": "1d311090b61358077d2f59972b0913ec3687ec82f8a7b752121df934b701a7bc07e3e0d7bf051bf939a5291790ae5ed43ed59d1b6feb8dda0f76f07f747d8601", "id": "596236ba37bc2d419f5825b2d771a74e151e7719923afefdef0b0baa9a8cfcb8", "blockId": "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d", "blockHeight": 11704043, "sequence": 0}]}"#;

const SENDER: &str = "SR1W4qS8DCPN65oV9Jd8JSLbfU5vhmEEky";
const RECIPIENT: &str = "SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw";
const TX_ID: &str = "596236ba37bc2d419f5825b2d771a74e151e7719923afefdef0b0baa9a8cfcb8";

async fn spawn_api() -> (String, Arc<Storage>) {
    let storage = Arc::new(Storage::temporary(Network::mainnet()).unwrap());
    let block: Block = serde_json::from_str(BLOCK_11704043).unwrap();
    storage.update_wallet_state(SENDER, 1_000_000_000, 10_102).unwrap();
    storage.apply_block(&block).unwrap();
    let mempool = Arc::new(Mempool::new(storage.clone(), vec![], 100));
    let state = Arc::new(AppState::new(storage.clone(), mempool, vec!["https://node0.smartholdem.io".into()]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { serve_listener(state, listener).await.unwrap() });
    (format!("http://{addr}"), storage)
}

async fn get(base: &str, path: &str) -> (u16, Value) {
    let r = reqwest::get(format!("{base}{path}")).await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
}

#[test]
fn human_time_matches_legacy_format() {
    assert_eq!(human_time(1788383912), "2026-09-02T21:18:32.000Z");
    assert_eq!(human_time(1693267200), "2023-08-29T00:00:00.000Z");
}

#[tokio::test]
async fn blockchain_blocks_and_transactions_have_legacy_shape() {
    let (base, _s) = spawn_api().await;

    let (st, v) = get(&base, "/api/blockchain").await;
    assert_eq!(st, 200);
    assert_eq!(v["data"]["block"]["height"], 11704043);
    assert_eq!(v["data"]["supply"], "24977000000000000");

    let (_, v) = get(&base, "/api/blocks/11704043").await;
    let b = &v["data"];
    assert_eq!(b["id"], "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d");
    assert_eq!(b["forged"]["fee"], "100000000");
    assert_eq!(b["forged"]["total"], "100000000");
    assert_eq!(b["generator"]["address"], "SXsQxVCEux4kkRFNrQJiQg6JT9K6SKLgS7");
    assert_eq!(b["confirmations"], 0);
    assert_eq!(b["transactions"], 1);
    assert_eq!(b["timestamp"]["unix"], 1788368656);
    assert_eq!(b["timestamp"]["human"], "2026-09-02T17:04:16.000Z");

    let (_, raw) = get(&base, "/api/blocks/53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d?transform=false").await;
    assert_eq!(raw["data"]["generatorPublicKey"], "03d67017411e92a8e93d7b0318e2748f013c130d6e4e01b1248da0ee0959ee9df8");
    assert_eq!(raw["data"]["totalAmount"], "314159265");

    let (_, list) = get(&base, "/api/blocks?limit=1").await;
    assert_eq!(list["meta"]["totalCount"], 11704043);
    assert_eq!(list["data"][0]["height"], 11704043);
    assert_eq!(list["meta"]["self"], "/blocks?limit=1&page=1");

    let (_, t) = get(&base, &format!("/api/transactions/{TX_ID}")).await;
    let t = &t["data"];
    assert_eq!(t["sender"], SENDER);
    assert_eq!(t["recipient"], RECIPIENT);
    assert_eq!(t["amount"], "314159265");
    assert_eq!(t["nonce"], "10103");
    assert_eq!(t["confirmations"], 1);
    assert_eq!(t["blockId"], "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d");
    assert_eq!(t["timestamp"]["epoch"], 95101456);

    let (st, e) = get(&base, "/api/blocks/00").await;
    assert_eq!(st, 404);
    assert_eq!(e["statusCode"], 404);
}

#[tokio::test]
async fn wallets_filters_and_delegates() {
    let (base, _s) = spawn_api().await;

    let (_, w) = get(&base, &format!("/api/wallets/{RECIPIENT}")).await;
    assert_eq!(w["data"]["balance"], "314159265");
    assert_eq!(w["data"]["nonce"], "0");
    assert!(w["data"]["attributes"].is_object());

    // lookup by public key
    let (_, w) = get(&base, "/api/wallets/036560f20d578da7b8433248d0c82e68121163958d533dc74b0e7d8dbabc0606a0").await;
    assert_eq!(w["data"]["address"], SENDER);
    assert_eq!(w["data"]["balance"], (1_000_000_000i64 - 314_159_265 - 100_000_000).to_string());

    let (_, l) = get(&base, &format!("/api/wallets/{SENDER}/transactions?limit=10")).await;
    assert_eq!(l["meta"]["totalCount"], 1);
    let (_, l) = get(&base, &format!("/api/wallets/{SENDER}/transactions/received")).await;
    assert_eq!(l["meta"]["totalCount"], 0);
    let (_, l) = get(&base, &format!("/api/wallets/{RECIPIENT}/transactions/received")).await;
    assert_eq!(l["data"][0]["id"], TX_ID);

    let (_, l) = get(&base, &format!("/api/transactions?recipientId={RECIPIENT}&limit=100")).await;
    assert_eq!(l["meta"]["count"], 1);
    let (_, l) = get(&base, "/api/transactions?type=3&typeGroup=1").await;
    assert_eq!(l["meta"]["totalCount"], 0);

    let (_, d) = get(&base, "/api/delegates").await;
    assert_eq!(d["meta"]["totalCount"], 0);
    let (_, f) = get(&base, "/api/transactions/fees").await;
    assert_eq!(f["data"]["1"]["transfer"], "100000000");
    let (_, f) = get(&base, "/api/node/fees").await;
    assert_eq!(f["data"]["1"]["vote"]["avg"], "100000000");
    let (_, s) = get(&base, "/api/node/status").await;
    assert_eq!(s["data"]["synced"], true);
    let (_, p) = get(&base, "/api/peers").await;
    assert_eq!(p["data"][0]["ip"], "node0.smartholdem.io");
}

#[tokio::test]
async fn mempool_admission_rules() {
    let (base, storage) = spawn_api().await;
    let client = reqwest::Client::new();
    let post = |body: Value| {
        let c = client.clone();
        let url = format!("{base}/api/transactions");
        async move { c.post(url).json(&body).send().await.unwrap().json::<Value>().await.unwrap() }
    };

    // already forged
    let forged: Transaction = serde_json::from_str(BLOCK_11704043).map(|b: Block| b.transactions[0].clone()).unwrap();
    let r = post(serde_json::json!({ "transactions": [forged] })).await;
    assert_eq!(r["data"]["invalid"][0], TX_ID);
    assert_eq!(r["errors"][TX_ID]["type"], "ERR_FORGED");

    // fresh sender without funds → nonce ok, balance too low
    let kp = KeyPair::from_passphrase("api test sender").unwrap();
    let mut tx: Transaction = serde_json::from_str(BLOCK_11704043).map(|b: Block| b.transactions[0].clone()).unwrap();
    tx.sender_public_key = kp.public_key_hex();
    tx.nonce = Some(1);
    tx.amount = 1_000;
    tx.id = None;
    tx.signature = None;
    tx.block_id = None;
    tx.block_height = None;
    tx.sequence = None;
    let hash = transaction_signing_hash(&tx).unwrap();
    tx.signature = Some(sign_schnorr_legacy(&hash, kp.private_key()).unwrap());
    let r = post(serde_json::json!({ "transactions": [tx.clone()] })).await;
    let id = r["data"]["invalid"][0].as_str().unwrap().to_string();
    assert_eq!(r["errors"][&id]["type"], "ERR_LOW_BALANCE");

    // fund the wallet > accepted, visible as unconfirmed, duplicate rejected
    storage.update_wallet_state(&kp.address(63).unwrap(), 10_000_000_000, 0).unwrap();
    let r = post(serde_json::json!({ "transactions": [tx.clone()] })).await;
    assert_eq!(r["data"]["accept"][0], id);
    assert!(r["errors"].is_null());
    let (_, u) = get(&base, "/api/transactions/unconfirmed").await;
    assert_eq!(u["data"][0]["id"], id);
    let (st, u) = get(&base, &format!("/api/transactions/{id}")).await;
    assert_eq!((st, u["data"]["confirmations"].as_u64()), (200, Some(0)));
    let r = post(serde_json::json!({ "transactions": [tx.clone()] })).await;
    assert_eq!(r["errors"][&id]["type"], "ERR_DUPLICATE");

    // wrong nonce and bad signature
    let mut bad = tx.clone();
    bad.nonce = Some(5);
    bad.id = None;
    let r = post(serde_json::json!({ "transactions": [bad] })).await;
    assert_eq!(r["data"]["invalid"].as_array().unwrap().len(), 1);
    let mut tampered = tx.clone();
    tampered.amount = 2_000;
    tampered.id = None;
    let r = post(serde_json::json!({ "transactions": [tampered] })).await;
    let bad_id = r["data"]["invalid"][0].as_str().unwrap();
    assert_eq!(r["errors"][bad_id]["type"], "ERR_BAD_DATA");
}
