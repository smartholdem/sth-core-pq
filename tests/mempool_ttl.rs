//! Author: TechnoL0g
//!
//! Mempool housekeeping: legacy `maxTransactionAge` TTL, `expiration`, periodic re-broadcast, same-nonce replacement
//! (RBF-lite), explanatory `ERR_BAD_NONCE`, `/api/transactions/unconfirmed?senderId=` and the loopback-only purge.

use serde_json::{json, Value};
use sth_core::api::{serve_listener, AppState};
use sth_core::config::Network;
use sth_core::crypto::{sign_schnorr_legacy, transaction_id, transaction_signing_hash, KeyPair};
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::mempool::Mempool;
use sth_core::models::{Block, Transaction};
use sth_core::storage::Storage;
use std::sync::Arc;

fn transfer(keys: &KeyPair, nonce: u64, to: &str, amount: u64, fee: u64, expiration: u32) -> Transaction {
    let mut t: Transaction = serde_json::from_value(json!({
        "version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": nonce.to_string(), "senderPublicKey": keys.public_key_hex(),
        "fee": fee.to_string(), "amount": amount.to_string(), "recipientId": to, "expiration": expiration
    }))
    .unwrap();
    let h = transaction_signing_hash(&t).unwrap();
    t.signature = Some(sign_schnorr_legacy(&h, keys.private_key()).unwrap());
    t.id = Some(transaction_id(&t).unwrap());
    t
}

const FEE: u64 = 10_000_000;

struct Chain {
    network: Network,
    storage: Arc<Storage>,
    tip: Block,
    slot: u64,
}

impl Chain {
    fn new() -> Self {
        let network = Network::mainnet();
        let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
        sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
        Self { network, storage, tip: sth_core::genesis::mainnet_block().unwrap(), slot: 1 }
    }

    fn empty_block(&mut self) {
        let forger = KeyPair::from_passphrase("forger").unwrap();
        let block = forge_block(&self.network, &forger, &self.tip, slot_start(self.slot, 8), vec![]).unwrap();
        self.storage.apply_block(&block).unwrap();
        self.tip = block;
        self.slot += 1;
    }
}

#[tokio::test]
async fn same_nonce_resend_replaces_and_nonce_error_explains_pending() {
    let c = Chain::new();
    let (alice, bob, carol) = (KeyPair::from_passphrase("alice").unwrap(), KeyPair::from_passphrase("bob").unwrap(), KeyPair::from_passphrase("carol").unwrap());
    let (a, b, cc) = (alice.address(63).unwrap(), bob.address(63).unwrap(), carol.address(63).unwrap());
    c.storage.update_wallet_state(&a, 100_000_000_000, 0).unwrap();
    let pool = Mempool::new(c.storage.clone(), vec![], 100);

    let tx1 = transfer(&alice, 1, &b, 1_000_000_000, FEE, 0);
    let tx2 = transfer(&alice, 2, &b, 1_000_000_000, FEE, 0);
    let (r, e) = pool.add_many(vec![tx1.clone(), tx2.clone()]).await;
    assert_eq!(r.accept.len(), 2, "{e:?}");

    // gap: chain nonce 0 + 2 pending → expected 3; the message names the pending range
    let (r, e) = pool.add_many(vec![transfer(&alice, 5, &b, 1, FEE, 0)]).await;
    assert_eq!(r.invalid.len(), 1);
    let msg = &e.values().next().unwrap().message;
    assert!(msg.contains("nonce 5, expected 3") && msg.contains("chain nonce 0 + 2 pending") && msg.contains("1–2") && msg.contains(&format!("senderId={a}")), "{msg}");

    // same nonce, fee not lower → replaces the stuck one (other pending stays)
    let tx1b = transfer(&alice, 1, &cc, 2_000_000_000, FEE, 0);
    let (r, e) = pool.add_many(vec![tx1b.clone()]).await;
    assert_eq!(r.accept, vec![tx1b.id.clone().unwrap()], "{e:?}");
    assert!(pool.get(tx1.id.as_ref().unwrap()).await.is_none());
    assert!(pool.get(tx1b.id.as_ref().unwrap()).await.is_some());
    assert!(pool.get(tx2.id.as_ref().unwrap()).await.is_some());
    assert_eq!(pool.len().await, 2);

    // a lower fee cannot replace (dynamic fees on so a lower static-fee mismatch is not what rejects it)
    let mut df = sth_core::node_config::DynamicFeesConfig::default();
    df.enabled = true;
    df.min_fee_pool = 1;
    df.min_fee_broadcast = 1;
    let pool = Mempool::new(c.storage.clone(), vec![], 100).with_dynamic_fees(df);
    let hi = transfer(&alice, 1, &b, 1, 5_000_000, 0);
    let lo = transfer(&alice, 1, &cc, 1, 4_000_000, 0);
    assert_eq!(pool.add_many(vec![hi.clone()]).await.0.accept.len(), 1);
    let (r, e) = pool.add_many(vec![lo]).await;
    assert_eq!(r.invalid.len(), 1);
    let err = e.values().next().unwrap();
    assert_eq!(err.type_, "ERR_BAD_NONCE");
    assert!(err.message.contains("already pending") && err.message.contains("at least 5000000"), "{}", err.message);
    assert!(pool.get(hi.id.as_ref().unwrap()).await.is_some());
}

#[tokio::test]
async fn pending_transactions_expire_and_are_rebroadcast() {
    let mut c = Chain::new();
    let (alice, bob) = (KeyPair::from_passphrase("alice").unwrap(), KeyPair::from_passphrase("bob").unwrap());
    let (a, b) = (alice.address(63).unwrap(), bob.address(63).unwrap());
    c.storage.update_wallet_state(&a, 100_000_000_000, 0).unwrap();
    let pool = Mempool::new(c.storage.clone(), vec![], 100).with_max_age_blocks(3).with_rebroadcast_blocks(1);
    let mut rebroadcasts = pool.subscribe_rebroadcast();

    let aged = transfer(&alice, 1, &b, 1_000_000_000, FEE, 0);
    let expiring = transfer(&alice, 2, &b, 1_000_000_000, FEE, 3); // expiration = height 3
    let (r, e) = pool.add_many(vec![aged.clone(), expiring.clone()]).await;
    assert_eq!(r.accept.len(), 2, "{e:?}");
    assert_eq!(pool.pending_meta(aged.id.as_ref().unwrap()), Some((1, 1)));

    // height 2: nothing expired yet, both re-relayed once (rebroadcast every block)
    c.empty_block();
    pool.prune_confirmed().await;
    assert_eq!(pool.len().await, 2);
    let batch = rebroadcasts.try_recv().unwrap();
    assert_eq!(batch.len(), 2);
    assert_eq!(pool.pending_meta(aged.id.as_ref().unwrap()), Some((1, 2)));
    pool.prune_confirmed().await; // same height → no second relay
    assert!(rebroadcasts.try_recv().is_err());

    // height 3: `expiration <= tip` drops the second, the first is 2 blocks old and stays
    c.empty_block();
    pool.prune_confirmed().await;
    assert!(pool.get(expiring.id.as_ref().unwrap()).await.is_none());
    assert!(pool.get(aged.id.as_ref().unwrap()).await.is_some());
    assert_eq!(pool.expired(), 1);

    // height 4: age 3 ≥ max_age_blocks → dropped, the sender's nonce is free again
    c.empty_block();
    pool.prune_confirmed().await;
    assert_eq!(pool.len().await, 0);
    assert_eq!(pool.expired(), 2);
    assert_eq!(pool.bytes(), 0);
    let (r, e) = pool.add_many(vec![transfer(&alice, 1, &b, 1, FEE, 0)]).await;
    assert_eq!(r.accept.len(), 1, "{e:?}");
}

#[tokio::test]
async fn unconfirmed_api_filters_by_sender_and_purge_is_loopback_only() {
    let c = Chain::new();
    let (alice, bob, carol) = (KeyPair::from_passphrase("alice").unwrap(), KeyPair::from_passphrase("bob").unwrap(), KeyPair::from_passphrase("carol").unwrap());
    let (a, b, cc) = (alice.address(63).unwrap(), bob.address(63).unwrap(), carol.address(63).unwrap());
    c.storage.update_wallet_state(&a, 100_000_000_000, 0).unwrap();
    c.storage.update_wallet_state(&cc, 100_000_000_000, 0).unwrap();
    let pool = Arc::new(Mempool::new(c.storage.clone(), vec![], 100));
    let (r, e) = pool.add_many(vec![transfer(&alice, 1, &b, 1, FEE, 0), transfer(&alice, 2, &b, 1, FEE, 0), transfer(&carol, 1, &b, 1, FEE, 0)]).await;
    assert_eq!(r.accept.len(), 3, "{e:?}");

    let state = Arc::new(AppState::new(c.storage.clone(), pool.clone(), vec![]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { serve_listener(state, listener).await.unwrap() });
    let http = reqwest::Client::new();

    let v: Value = http.get(format!("{base}/api/transactions/unconfirmed?senderId={a}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["meta"]["totalCount"], 2);
    assert_eq!(v["data"][0]["nonce"], "1");
    assert_eq!(v["data"][1]["nonce"], "2");
    assert_eq!(v["data"][0]["pool"]["enteredHeight"], 1);
    assert_eq!(v["data"][0]["pool"]["ageBlocks"], 0);
    let v: Value = http.get(format!("{base}/api/transactions/unconfirmed")).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["meta"]["totalCount"], 3);

    // behind a reverse proxy (forwarded headers) → refused even from loopback
    let r = http.delete(format!("{base}/api/transactions/unconfirmed")).header("X-Forwarded-For", "203.0.113.7").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 403);
    assert_eq!(pool.len().await, 3);
    // direct loopback: one sender, then everything
    let v: Value = http.delete(format!("{base}/api/transactions/unconfirmed?senderId={cc}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"]["removed"], 1);
    assert_eq!(pool.len().await, 2);
    let v: Value = http.delete(format!("{base}/api/transactions/unconfirmed")).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["data"]["removed"], 2);
    assert_eq!(pool.len().await, 0);
    assert_eq!(pool.bytes(), 0);
}
