//! Author: TechnoL0g
//!
//! `mempool.antispam`: per-sender caps, per-peer budgets and bans, deny list, replace-by-fee eviction, metrics.

use serde_json::json;
use sth_core::antispam::{AntiSpamConfig, TxOrigin};
use sth_core::config::Network;
use sth_core::crypto::{sign_schnorr_legacy, transaction_id, transaction_signing_hash, KeyPair};
use sth_core::mempool::Mempool;
use sth_core::models::Transaction;
use sth_core::storage::Storage;
use std::sync::Arc;

fn transfer(keys: &KeyPair, nonce: u64, to: &str, fee: u64) -> Transaction {
    let mut t: Transaction = serde_json::from_value(json!({
        "version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": nonce.to_string(), "senderPublicKey": keys.public_key_hex(),
        "fee": fee.to_string(), "amount": "100000000", "recipientId": to, "expiration": 0
    }))
    .unwrap();
    let h = transaction_signing_hash(&t).unwrap();
    t.signature = Some(sign_schnorr_legacy(&h, keys.private_key()).unwrap());
    t.id = Some(transaction_id(&t).unwrap());
    t
}

const FEE: u64 = 10_000_000;

fn funded(storage: &Storage, name: &str) -> (KeyPair, String) {
    let k = KeyPair::from_passphrase(name).unwrap();
    let a = k.address(63).unwrap();
    storage.update_wallet_state(&a, 1_000_000_000_000, 0).unwrap();
    (k, a)
}

fn pool(storage: &Arc<Storage>, max_size: usize, cfg: AntiSpamConfig) -> Mempool {
    Mempool::new(storage.clone(), vec![], max_size).with_antispam(cfg)
}

#[tokio::test]
async fn sender_cap_peer_budget_ban_and_deny_list() {
    let network = Network::mainnet();
    let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let (alice, _) = funded(&storage, "alice");
    let (bob, _) = funded(&storage, "bob");
    let to = KeyPair::from_passphrase("carol").unwrap().address(63).unwrap();
    let cfg = AntiSpamConfig { max_per_sender: 3, peer_tx_per_min: 5, peer_invalid_per_min: 2, ban_secs: 60, deny_peers: vec!["9.9.9.9".into()], alert_drops_per_min: 0, ..AntiSpamConfig::default() };
    let p = pool(&storage, 100, cfg);

    // per-sender cap: the 4th pending transaction of alice is refused, a same-nonce replacement still passes
    let txs: Vec<Transaction> = (1..=4).map(|n| transfer(&alice, n, &to, FEE)).collect();
    let (r, e) = p.add_many(txs).await;
    assert_eq!(r.accept.len(), 3, "{e:?}");
    assert_eq!(e.values().next().unwrap().type_, "ERR_SPAM_SENDER");
    let (r, e) = p.add_many(vec![transfer(&alice, 2, &to, FEE * 2)]).await;
    assert_eq!(r.accept.len(), 1, "replacement is not a new pending slot: {e:?}");
    assert_eq!(p.antispam().counters().sender_limited, 1);

    // per-peer budget: 5 per minute from one legacy IP, the 6th and 7th are dropped silently
    let peer = TxOrigin::LegacyPeer("1.2.3.4".into());
    let txs: Vec<Transaction> = (1..=7).map(|n| transfer(&bob, n, &to, FEE)).collect();
    let (r, _) = p.add_many_from(txs, &peer).await;
    assert_eq!(r.accept.len(), 3, "bob's own cap of 3 applies to the 5 admitted");
    let c = p.antispam().counters();
    assert_eq!((c.rate_limited, c.dropped), (2, 1 + 2 + 2), "2 over budget; sender_limited also counts as dropped");

    // invalid budget: two bad signatures from one iroh peer → banned, later batches dropped, listed in metrics
    let bad = TxOrigin::IrohPeer("abcdef".into());
    let mut forged = transfer(&bob, 9, &to, FEE);
    forged.amount += 1;
    let mut forged2 = forged.clone();
    forged2.nonce = Some(10);
    forged2.id = None;
    let (r, e) = p.add_many_from(vec![forged, forged2], &bad).await;
    assert_eq!(r.invalid.len(), 2, "{e:?}");
    assert!(p.antispam().is_banned("abcdef"));
    let banned = p.antispam().banned();
    assert_eq!((banned.len(), banned[0].peer.as_str(), banned[0].invalid), (1, "abcdef", 2));
    assert!(banned[0].seconds_left > 50);
    let (r, _) = p.add_many_from(vec![transfer(&alice, 4, &to, FEE)], &bad).await;
    assert!(r.accept.is_empty() && r.invalid.is_empty(), "banned peer: batch dropped without validation");
    assert_eq!(p.antispam().counters().banned_dropped, 1);

    // deny list
    let (r, _) = p.add_many_from(vec![transfer(&alice, 4, &to, FEE)], &TxOrigin::LegacyPeer("9.9.9.9".into())).await;
    assert!(r.accept.is_empty());
    assert_eq!(p.antispam().counters().denied, 1);
    // the same transaction from the node's own client is fine
    let (r, e) = p.add_many(vec![transfer(&alice, 4, &to, FEE)]).await;
    assert_eq!(r.invalid.len(), 1, "alice is at her cap of 3: {e:?}");

    let top = p.top_senders(5).await;
    assert_eq!(top.len(), 2);
    assert_eq!(top[0]["pending"], 3);
}

#[tokio::test]
async fn replace_by_fee_evicts_cheapest_last_nonce_when_pool_is_full() {
    let network = Network::mainnet();
    let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let (alice, _) = funded(&storage, "alice");
    let (bob, _) = funded(&storage, "bob");
    let (carol, _) = funded(&storage, "carol");
    let to = KeyPair::from_passphrase("dave").unwrap().address(63).unwrap();

    // RBF off: full pool refuses
    let p = pool(&storage, 2, AntiSpamConfig { replace_by_fee: false, ..AntiSpamConfig::default() });
    let (r, _) = p.add_many(vec![transfer(&alice, 1, &to, FEE), transfer(&alice, 2, &to, FEE * 3)]).await;
    assert_eq!(r.accept.len(), 2);
    let (r, e) = p.add_many(vec![transfer(&bob, 1, &to, FEE * 10)]).await;
    assert_eq!(r.invalid.len(), 1);
    assert_eq!(e.values().next().unwrap().type_, "ERR_POOL_FULL");

    // RBF on: alice holds nonce 1 (cheap) and 2 (expensive); only her LAST nonce (2) is evictable, and only by a higher rate
    let p = pool(&storage, 2, AntiSpamConfig::default());
    let cheap = transfer(&alice, 1, &to, FEE);
    let last = transfer(&alice, 2, &to, FEE * 3);
    p.add_many(vec![cheap.clone(), last.clone()]).await;
    let (r, e) = p.add_many(vec![transfer(&bob, 1, &to, FEE * 2)]).await;
    assert_eq!(r.invalid.len(), 1, "fee ×2 does not beat alice's last nonce (×3): {e:?}");
    assert!(e.values().next().unwrap().message.contains("replace-by-fee"));
    let (r, e) = p.add_many(vec![transfer(&bob, 1, &to, FEE * 10)]).await;
    assert_eq!(r.accept.len(), 1, "{e:?}");
    assert!(p.get(last.id.as_deref().unwrap()).await.is_none(), "alice's last nonce evicted");
    assert!(p.get(cheap.id.as_deref().unwrap()).await.is_some(), "her middle/first nonce is never evicted");
    assert_eq!(p.len().await, 2);
    assert_eq!(p.antispam().counters().evicted, 1);
    // a newcomer cannot evict to make room for itself when every pending tx pays more
    let (r, e) = p.add_many(vec![transfer(&carol, 1, &to, FEE)]).await;
    assert_eq!(r.invalid.len(), 1, "{e:?}");
}
