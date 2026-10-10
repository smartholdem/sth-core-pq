//! Author: TechnoL0g
//!
//! SHIP-44 Wallet Stream API: REST over iroh (`netfory/api/1`, signed pdata) and push subscriptions (`sth/events/1`).

use serde_json::json;
use sth_core::api::AppState;
use sth_core::config::Network;
use sth_core::crypto::{sign_schnorr_legacy, transaction_id, transaction_signing_hash, KeyPair};
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::mempool::Mempool;
use sth_core::models::Transaction;
use sth_core::node_config::WalletApiConfig;
use sth_core::p2p_iroh::wallet_api::{client, MeshPacket, ALPN_API_V0};
use sth_core::p2p_iroh::IrohNode;
use sth_core::storage::Storage;
use std::sync::Arc;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(20);

struct Node {
    network: Network,
    storage: Arc<Storage>,
    mempool: Arc<Mempool>,
    iroh: Arc<IrohNode>,
    addr: iroh::EndpointAddr,
}

async fn node(cfg: Option<WalletApiConfig>) -> Node {
    let network = Network::mainnet();
    let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let mempool = Arc::new(Mempool::new(storage.clone(), vec![], 100));
    let iroh = IrohNode::spawn(iroh::SecretKey::generate(), vec![], true, None, storage.clone(), mempool.clone(), false, None, None).await.unwrap();
    if let Some(cfg) = cfg {
        let state = Arc::new(AppState::new(storage.clone(), mempool.clone(), vec![]).with_iroh(iroh.clone()));
        iroh.wallet_api.attach(state, cfg);
    }
    let _ = tokio::time::timeout(Duration::from_secs(10), iroh.endpoint.online()).await;
    let addr = iroh.endpoint.addr();
    Node { network, storage, mempool, iroh, addr }
}

async fn wallet_endpoint() -> iroh::Endpoint {
    iroh::Endpoint::builder(iroh::endpoint::presets::N0DisableRelay).bind().await.unwrap()
}

fn transfer(keys: &KeyPair, nonce: u64, to: &str, amount: u64) -> Transaction {
    let mut t: Transaction = serde_json::from_value(json!({
        "version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": nonce.to_string(), "senderPublicKey": keys.public_key_hex(),
        "fee": "10000000", "amount": amount.to_string(), "recipientId": to, "expiration": 0
    }))
    .unwrap();
    let h = transaction_signing_hash(&t).unwrap();
    t.signature = Some(sign_schnorr_legacy(&h, keys.private_key()).unwrap());
    t.id = Some(transaction_id(&t).unwrap());
    t
}

#[tokio::test]
async fn rest_over_iroh_is_signed_and_rate_limited() {
    let n = node(None).await;
    let wallet = wallet_endpoint().await;

    // not attached yet → 503 in the legacy error envelope, still signed
    let r = tokio::time::timeout(WAIT, client::call(&wallet, n.addr.clone(), "GET", "/api/blockchain", b"")).await.unwrap().unwrap();
    assert_eq!(r.status, 503, "{:?}", r.body);
    assert_eq!(r.body["statusCode"], 503);
    assert_eq!(r.pdata["node_id"], n.iroh.id().to_string());

    let cfg = WalletApiConfig { rate_limit_rps: 2, ..WalletApiConfig::default() };
    let state = Arc::new(AppState::new(n.storage.clone(), n.mempool.clone(), vec![]).with_iroh(n.iroh.clone()));
    n.iroh.wallet_api.attach(state, cfg);

    // v1: same JSON as HTTP, pdata verified by the client helper
    let r = client::call(&wallet, n.addr.clone(), "GET", "/api/blockchain", b"").await.unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body["data"]["block"]["height"], 1);
    assert_eq!(r.body["data"]["block"]["id"], n.network.genesis_block_id);
    assert_eq!(r.pdata["alg"], "ed25519");
    assert_eq!(r.pdata["name"], "sth-core");
    assert!(client::verify_pdata(&n.iroh.id(), &r.pdata));
    let mut forged = r.pdata.clone();
    forged["status"] = json!(201);
    assert!(!client::verify_pdata(&n.iroh.id(), &forged));

    // unknown route → legacy 404 envelope; POST reaches the mempool handler
    let r = client::call(&wallet, n.addr.clone(), "GET", "/api/nothing", b"").await.unwrap();
    assert_eq!(r.status, 404);
    assert_eq!(r.body["error"], "Not Found");
    // `api://<id>/api/blocks` parsed by a provider client as name "api" + path "/blocks" → mapped onto /api/blocks
    let r = client::call(&wallet, n.addr.clone(), "GET", "/blocks/last", b"").await.unwrap();
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["data"]["height"], 1, "{:?}", r.body);
    assert_eq!(n.iroh.wallet_api.stats_json()["url"], n.iroh.wallet_api.api_url("sth-core"));
    let r = client::call(&wallet, n.addr.clone(), "POST", "/api/transactions", br#"{"transactions":[]}"#).await.unwrap();
    assert!(r.status < 500, "{} {:?}", r.status, r.body);

    // v0 (raw JSON + EOF) as older api:// clients send it (fresh EndpointId: the burst above is spent)
    let legacy = wallet_endpoint().await;
    let conn = legacy.connect(n.addr.clone(), ALPN_API_V0).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let mut pkt = MeshPacket::new("GET", "/api/node/status", vec![]);
    pkt.target_provider = "node-sth-r1".into();
    send.write_all(&serde_json::to_vec(&pkt).unwrap()).await.unwrap();
    send.finish().unwrap();
    let raw = recv.read_to_end(8 * 1024 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(v["data"]["now"], 1);
    assert_eq!(v["data"]["irohWalletApi"]["enabled"], true);
    assert_eq!(v["pdata"]["name"], "node-sth-r1");
    assert!(client::verify_pdata(&n.iroh.id(), &v["pdata"]));
    conn.close(0u32.into(), b"done");

    // 2 rps → burst of 4 on one connection, then 429 (fresh EndpointId: the bucket above is already spent)
    let burst = wallet_endpoint().await;
    let conn = burst.connect(n.addr.clone(), sth_core::p2p_iroh::wallet_api::ALPN_API_V1).await.unwrap();
    let mut statuses = Vec::new();
    for _ in 0..8 {
        statuses.push(client::call_on(&conn, "GET", "/api/blockchain", b"").await.unwrap().status);
    }
    assert!(statuses.contains(&429), "{statuses:?}");
    assert!(statuses.iter().filter(|s| **s == 200).count() >= 3, "{statuses:?}");
    conn.close(0u32.into(), b"done");
    let stats = n.iroh.wallet_api.stats_json();
    assert!(stats["requests"].as_u64().unwrap() >= 12);
    assert!(stats["rateLimited"].as_u64().unwrap() >= 1);
    // who knocked: every completed connection is remembered with the wallet's EndpointId and the transport it came over
    assert_eq!(stats["handshakeFailed"], 0);
    let recent = stats["recentClients"].as_array().unwrap();
    assert!(recent.len() >= 2 && recent.len() <= sth_core::p2p_iroh::wallet_api::RECENT_CLIENTS, "{recent:?}");
    let wallet_short = wallet.id().fmt_short().to_string();
    assert!(recent.iter().any(|c| c["peer"] == wallet_short && c["alpn"] == "netfory/api/1" && c["outcome"] == "ok"), "{recent:?}");
    assert!(recent.iter().all(|c| !c["remote"].as_str().unwrap().is_empty() && c["atUnix"].as_u64().unwrap() > 1_700_000_000), "{recent:?}");
    n.iroh.shutdown().await;
}

#[tokio::test]
async fn events_push_replay_and_rollback() {
    let n = node(Some(WalletApiConfig::default())).await;
    let wallet = wallet_endpoint().await;
    let (alice, bob, forger) = (KeyPair::from_passphrase("alice").unwrap(), KeyPair::from_passphrase("bob").unwrap(), KeyPair::from_passphrase("forger").unwrap());
    let (a, b) = (alice.address(63).unwrap(), bob.address(63).unwrap());
    n.storage.update_wallet_state(&a, 10_000_000_000, 0).unwrap();
    n.storage.set_undo_enabled(true);

    let mut ev = tokio::time::timeout(WAIT, client::EventStream::connect(&wallet, n.addr.clone())).await.unwrap().unwrap();
    assert_eq!(ev.hello["type"], "hello");
    assert_eq!(ev.hello["height"], 1);
    assert_eq!(ev.hello["nodeId"], n.iroh.id().to_string());

    // validation errors are frames, not disconnects
    ev.subscribe(&["not-an-address"], &["wallet"], None).await.unwrap();
    let f = ev.wait_for("error", WAIT).await.unwrap();
    assert_eq!(f.last().unwrap()["code"], "bad_address");
    ev.command(&json!({ "type": "subscribe", "addresses": [b], "topics": ["nope"] })).await.unwrap();
    assert_eq!(ev.wait_for("error", WAIT).await.unwrap().last().unwrap()["code"], "bad_topic");

    ev.subscribe(&[&b], &["blocks", "wallet", "mempool"], None).await.unwrap();
    let frames = ev.wait_for("subscribed", WAIT).await.unwrap();
    assert_eq!(frames[0]["type"], "balance");
    assert_eq!(frames[0]["wallet"]["balance"], "0");
    assert_eq!(frames.last().unwrap()["height"], 1);
    assert_eq!(frames.last().unwrap()["replayed"], 0);

    // switch to MessagePack: the acknowledgement and everything after arrive as msgpack
    assert_eq!(ev.hello["encodings"], json!(["json", "msgpack"]));
    ev.set_encoding(sth_core::p2p_iroh::wallet_api::Encoding::MsgPack).await.unwrap();
    let ack = ev.wait_for("encoding", WAIT).await.unwrap();
    assert_eq!(ack.last().unwrap()["encoding"], "msgpack");
    assert_eq!(ev.last_encoding, sth_core::p2p_iroh::wallet_api::Encoding::MsgPack);
    ev.ping().await.unwrap(); // command itself sent as msgpack
    assert_eq!(ev.wait_for("pong", WAIT).await.unwrap().last().unwrap()["height"], 1);

    // block 2 with alice → bob: block header, the transaction, bob's new balance
    let genesis = sth_core::genesis::mainnet_block().unwrap();
    let block2 = forge_block(&n.network, &forger, &genesis, slot_start(1, 8), vec![transfer(&alice, 1, &b, 5_000_000_000)]).unwrap();
    n.storage.apply_block(&block2).unwrap();
    let frames = ev.wait_for("balance", WAIT).await.unwrap();
    let kinds: Vec<&str> = frames.iter().map(|f| f["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["block", "tx", "balance"], "{frames:?}");
    assert_eq!(frames[0]["block"]["height"], 2);
    assert_eq!(frames[0]["block"]["transactions"], 1);
    assert_eq!(frames[1]["addresses"], json!([b]));
    assert_eq!(frames[1]["tx"]["recipient"], b);
    assert_eq!(frames[1]["tx"]["amount"], "5000000000");
    assert_eq!(frames[1]["tx"]["confirmations"], 1);
    assert_eq!(frames[2]["address"], b);
    assert_eq!(frames[2]["wallet"]["balance"], "5000000000");
    assert_eq!(ev.last_encoding, sth_core::p2p_iroh::wallet_api::Encoding::MsgPack);

    // REST filters travel with the path over iroh: `?recipientId=…&limit=2` → same result as HTTP
    let r = client::call(&wallet, n.addr.clone(), "GET", &format!("/api/transactions?recipientId={b}&limit=2"), b"").await.unwrap();
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["data"].as_array().unwrap().len(), 1, "{:?}", r.body);
    assert_eq!(r.body["data"][0]["recipient"], b);
    assert_eq!(r.body["data"][0]["amount"], "5000000000");
    assert_eq!(r.body["meta"]["totalCount"], 1);
    let r = client::call(&wallet, n.addr.clone(), "GET", &format!("/api/transactions?senderId={a}&type=0&orderBy=timestamp:desc&limit=2"), b"").await.unwrap();
    assert_eq!(r.body["data"][0]["sender"], a, "{:?}", r.body);

    // mempool arrival for bob
    let pending = transfer(&alice, 2, &b, 1_000_000_000);
    let (resp, errors) = n.mempool.add_many(vec![pending.clone()]).await;
    assert_eq!(resp.accept.len(), 1, "{errors:?}");
    let frames = ev.wait_for("mempoolTx", WAIT).await.unwrap();
    assert_eq!(frames.last().unwrap()["tx"]["id"], pending.id.clone().unwrap());
    assert_eq!(frames.last().unwrap()["tx"]["confirmations"], 0);

    // a second wallet reconnecting after height 1 gets the missed transaction replayed
    let mut late = client::EventStream::connect(&wallet, n.addr.clone()).await.unwrap();
    late.subscribe(&[&b, &a], &["wallet"], Some(1)).await.unwrap();
    let frames = late.wait_for("subscribed", WAIT).await.unwrap();
    let kinds: Vec<&str> = frames.iter().map(|f| f["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["tx", "balance", "balance", "subscribed"], "{frames:?}");
    assert_eq!(frames[0]["replay"], true);
    assert_eq!(frames[0]["height"], 2);
    let mut addrs = frames[0]["addresses"].as_array().unwrap().clone();
    addrs.sort_by_key(|v| v.as_str().unwrap().to_string());
    let mut want = vec![json!(a), json!(b)];
    want.sort_by_key(|v| v.as_str().unwrap().to_string());
    assert_eq!(addrs, want);
    assert_eq!(frames.last().unwrap()["replayed"], 1);
    assert_eq!(frames.last().unwrap()["height"], 2);

    // ping / pong keeps the session; rollback is announced to both
    ev.ping().await.unwrap();
    assert_eq!(ev.wait_for("pong", WAIT).await.unwrap().last().unwrap()["height"], 2);
    n.storage.rollback_last_block().unwrap();
    assert_eq!(ev.wait_for("rollback", WAIT).await.unwrap().last().unwrap()["height"], 1);
    assert_eq!(late.wait_for("rollback", WAIT).await.unwrap().last().unwrap()["height"], 1);
    assert_eq!(n.iroh.wallet_api.stats_json()["sessions"], 2);
    n.iroh.shutdown().await;
}

#[tokio::test]
async fn events_limits_addresses_and_connections() {
    let n = node(Some(WalletApiConfig { max_addresses: 2, max_connections: 1, ..WalletApiConfig::default() })).await;
    let wallet = wallet_endpoint().await;
    let addrs: Vec<String> = ["a", "b", "c"].iter().map(|p| KeyPair::from_passphrase(p).unwrap().address(63).unwrap()).collect();
    let mut ev = tokio::time::timeout(WAIT, client::EventStream::connect(&wallet, n.addr.clone())).await.unwrap().unwrap();
    let refs: Vec<&str> = addrs.iter().map(String::as_str).collect();
    ev.subscribe(&refs, &["wallet"], None).await.unwrap();
    assert_eq!(ev.wait_for("error", WAIT).await.unwrap().last().unwrap()["code"], "too_many_addresses");
    ev.subscribe(&refs[..2], &["wallet"], None).await.unwrap();
    assert_eq!(ev.wait_for("subscribed", WAIT).await.unwrap().last().unwrap()["addresses"].as_array().unwrap().len(), 2);

    // the single slot is taken → the second connection is refused
    let second = tokio::time::timeout(WAIT, client::EventStream::connect(&wallet, n.addr.clone())).await.unwrap();
    assert!(second.is_err());
    assert_eq!(n.iroh.wallet_api.stats_json()["rejectedBusy"], 1);
    n.iroh.shutdown().await;
}

/// Browser path: WebSocket bridge on the node itself → `sth/events/1` and `netfory/api/1` over the node's own iroh endpoint.
#[tokio::test]
async fn ws_bridge_relays_events_and_api() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let n = node(Some(WalletApiConfig::default())).await;
    let (addr, fut) = sth_core::p2p_iroh::ws_bridge::bind(n.iroh.clone(), "127.0.0.1:0".parse().unwrap(), None).await.unwrap();
    tokio::spawn(fut);
    let bob = KeyPair::from_passphrase("bob").unwrap().address(63).unwrap();

    let info: serde_json::Value = reqwest::get(format!("http://{addr}/")).await.unwrap().json().await.unwrap();
    assert_eq!(info["data"]["node"], n.iroh.id().to_string());

    // events: one WS text message = one frame; hello first, then balance + subscribed
    let (mut ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(format!("ws://{addr}/ws/events"))).await.unwrap().unwrap();
    ws.send(Message::Text(json!({ "type": "subscribe", "addresses": [bob], "topics": ["wallet"] }).to_string())).await.unwrap();
    let mut kinds = Vec::new();
    while kinds.len() < 3 {
        let msg = tokio::time::timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        kinds.push(v["type"].as_str().unwrap().to_string());
    }
    assert_eq!(kinds, ["hello", "balance", "subscribed"]);
    ws.close(None).await.unwrap();

    // api: one WS message = one MeshPacket, reply = signed response
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/api/{}", n.iroh.id())).await.unwrap();
    ws.send(Message::Text(serde_json::to_string(&MeshPacket::new("GET", "/api/blockchain", vec![])).unwrap())).await.unwrap();
    let msg = tokio::time::timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap();
    let v: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(v["data"]["block"]["height"], 1);
    assert!(client::verify_pdata(&n.iroh.id(), &v["pdata"]));
    ws.close(None).await.unwrap();
    n.iroh.shutdown().await;
}
