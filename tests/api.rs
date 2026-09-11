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
    assert_eq!(list["meta"]["self"], "/blocks?limit=1&page=1&transform=true");

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

    let (_, l) = get(&base, &format!("/api/transactions?recipientId={RECIPIENT}&page=1&limit=100&orderBy=timestamp:desc")).await;
    assert_eq!(l["meta"]["count"], 1);
    assert_eq!(l["meta"]["self"], format!("/transactions?recipientId={RECIPIENT}&page=1&limit=100&transform=true&orderBy=timestamp:desc"));
    assert_eq!(l["meta"]["totalCountIsEstimate"], false);
    let (_, l) = get(&base, "/api/transactions?type=3&typeGroup=1").await;
    assert_eq!(l["meta"]["totalCount"], 0);

    let (_, d) = get(&base, "/api/delegates").await;
    assert_eq!(d["meta"]["totalCount"], 0);
    let (_, f) = get(&base, "/api/transactions/fees").await;
    assert_eq!(f["data"]["1"]["transfer"], "100000000");
    let (_, f) = get(&base, "/api/node/fees?days=30").await;
    assert_eq!(f["meta"]["days"], 30);
    if let Some(t) = f["data"]["1"].get("transfer") {
        assert_eq!(t["avg"], "100000000");
    }
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

    // fund the wallet → accepted, visible as unconfirmed, duplicate rejected
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

#[tokio::test]
async fn ascending_order_is_honoured() {
    let (base, _s) = spawn_api().await;
    let (_, desc) = get(&base, "/api/wallets?limit=10").await;
    let (_, asc) = get(&base, "/api/wallets?limit=10&orderBy=balance:asc").await;
    let d: Vec<&str> = desc["data"].as_array().unwrap().iter().map(|w| w["address"].as_str().unwrap()).collect();
    let mut a: Vec<&str> = asc["data"].as_array().unwrap().iter().map(|w| w["address"].as_str().unwrap()).collect();
    assert!(d.len() >= 2);
    a.reverse();
    assert_eq!(a, d);

    let (_, b) = get(&base, "/api/blocks?orderBy=height:asc&height.from=11704043&limit=5").await;
    assert_eq!(b["data"][0]["height"], 11704043);
    assert_eq!(b["meta"]["totalCount"], 1);
    let (_, t) = get(&base, "/api/transactions?orderBy=timestamp:asc").await;
    assert_eq!(t["data"][0]["id"], TX_ID);
    assert!(t["meta"]["self"].as_str().unwrap().contains("orderBy=timestamp:asc"));
}

#[tokio::test]
async fn legacy_compat_endpoints() {
    let (base, _s) = spawn_api().await;
    let sender_pk = "03ee4b14a4aa3d7b41ebcbcc2f3c6a4f3f2dd0cb6a4a9c1f1f3c7c1c0b2e5a1d9f";
    let (code, types) = get(&base, "/api/transactions/types").await;
    assert_eq!(code, 200);
    assert_eq!(types["data"]["1"]["HtlcRefund"], 10);
    assert_eq!(types["data"]["2"]["BridgechainUpdate"], 5);
    let (_, schemas) = get(&base, "/api/transactions/schemas").await;
    assert!(schemas["data"]["1"]["0"].is_object());
    let (_, crypto) = get(&base, "/api/node/configuration/crypto").await;
    assert_eq!(crypto["data"]["network"]["pubKeyHash"], 63);
    assert_eq!(crypto["data"]["milestones"][0]["height"], 1);
    assert_eq!(crypto["data"]["genesisBlock"]["height"], 1);
    assert_eq!(crypto["data"]["genesisBlock"]["transactions"].as_array().unwrap().len(), 1855);
    let (_, cfg) = get(&base, "/api/node/configuration").await;
    assert_eq!(cfg["data"]["constants"]["fees"]["staticFees"]["delegateRegistration"], 1_000_000_000_000u64);
    assert_eq!(cfg["data"]["constants"]["blockBurnAddress"], true);
    let (_, fees) = get(&base, "/api/transactions/fees").await;
    assert_eq!(fees["data"]["1"]["transfer"], "100000000");

    let (_, votes) = get(&base, "/api/votes").await;
    assert_eq!(votes["meta"]["totalCount"], 0);
    let (code, _) = get(&base, &format!("/api/votes/{TX_ID}")).await;
    assert_eq!(code, 404);
    let (_, top) = get(&base, "/api/wallets/top?limit=1").await;
    assert_eq!(top["meta"]["count"], 1);
    assert!(top["meta"]["self"].as_str().unwrap().starts_with("/wallets/top?"));
    let (_, wv) = get(&base, &format!("/api/wallets/{SENDER}/votes")).await;
    assert_eq!(wv["meta"]["totalCount"], 0);
    let (_, wl) = get(&base, &format!("/api/wallets/{SENDER}/locks")).await;
    assert_eq!(wl["meta"]["totalCount"], 0);
    let (_, locks) = get(&base, "/api/locks").await;
    assert_eq!(locks["meta"]["totalCount"], 0);
    let (code, _) = get(&base, "/api/locks/deadbeef").await;
    assert_eq!(code, 404);
    let (_, ent) = get(&base, "/api/entities").await;
    assert_eq!(ent["data"].as_array().unwrap().len(), 0);
    let (code, _) = get(&base, "/api/entities/x").await;
    assert_eq!(code, 404);
    let (code, _) = get(&base, "/api/peers/10.0.0.1").await;
    assert_eq!(code, 404);
    let (_, dash) = get(&base, "/api/node/peers").await;
    assert_eq!(dash["meta"]["totalCount"], 0);

    // filters
    let (_, f) = get(&base, &format!("/api/transactions?senderPublicKey={sender_pk}")).await;
    assert_eq!(f["meta"]["totalCount"], 0);
    let (_, f) = get(&base, "/api/transactions?amount.from=314159265&amount.to=314159265").await;
    assert_eq!(f["meta"]["totalCount"], 1);
    let (_, f) = get(&base, "/api/transactions?fee.to=1").await;
    assert_eq!(f["meta"]["totalCount"], 0);
    let (_, f) = get(&base, "/api/transactions?timestamp.from=95101456&timestamp.to=95101456").await;
    assert_eq!(f["meta"]["totalCount"], 1);
    let (_, f) = get(&base, "/api/transactions?version=1").await;
    assert_eq!(f["meta"]["totalCount"], 0);
    let (_, b) = get(&base, "/api/blocks?generatorPublicKey=03d67017411e92a8e93d7b0318e2748f013c130d6e4e01b1248da0ee0959ee9df8").await;
    assert_eq!(b["meta"]["totalCount"], 1);
    let (_, b) = get(&base, "/api/blocks?timestamp.from=95101456").await;
    assert_eq!(b["data"][0]["height"], 11704043);
    let (_, b) = get(&base, "/api/blocks?timestamp.to=95101455").await;
    assert_eq!(b["meta"]["totalCount"], 0);
    let (_, w) = get(&base, &format!("/api/wallets?address={SENDER}")).await;
    assert_eq!(w["meta"]["totalCount"], 1);
    let (_, w) = get(&base, "/api/wallets?balance.from=99999999999999999").await;
    assert_eq!(w["meta"]["totalCount"], 0);
    let (_, d) = get(&base, "/api/delegates?isResigned=true").await;
    assert_eq!(d["meta"]["totalCount"], 0);
    let (_, r) = get(&base, "/api/rounds/557336/delegates").await;
    assert!(r["data"].is_array());
    let (code, _) = get(&base, "/api/rounds/1/delegates").await;
    assert_eq!(code, 404);
}

#[tokio::test]
async fn status_page_is_served() {
    let (base, _s) = spawn_api().await;
    let body = reqwest::get(format!("{base}/status")).await.unwrap().text().await.unwrap();
    assert!(body.contains("<title>sth-core status</title>"));
    assert!(body.contains("/api/node/forging"));
}
