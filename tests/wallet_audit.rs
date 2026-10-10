//! Author: TechnoL0g
//!
//! `Storage::audit_wallet` / `repair_wallet`: stored nonce + balance vs. the transaction history (state drift detector).

use serde_json::json;
use sth_core::config::Network;
use sth_core::crypto::{sign_schnorr_legacy, transaction_id, transaction_signing_hash, KeyPair};
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::models::Transaction;
use sth_core::storage::Storage;
use sth_core::sync::ChainTip;

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

#[test]
fn audit_detects_and_repairs_state_drift() {
    let network = Network::mainnet();
    let storage = Storage::temporary(network.clone()).unwrap();
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let (alice, bob, carol, forger) = (KeyPair::from_passphrase("alice").unwrap(), KeyPair::from_passphrase("bob").unwrap(), KeyPair::from_passphrase("carol").unwrap(), KeyPair::from_passphrase("forger").unwrap());
    let (a, b, c) = (alice.address(63).unwrap(), bob.address(63).unwrap(), carol.address(63).unwrap());
    storage.update_wallet_state(&a, 100_000_000_000, 0).unwrap();

    let genesis = sth_core::genesis::mainnet_block().unwrap();
    let b2 = forge_block(&network, &forger, &genesis, slot_start(1, 8), vec![transfer(&alice, 1, &b, 5_000_000_000)]).unwrap();
    storage.apply_block(&b2).unwrap();
    let b3 = forge_block(&network, &forger, &b2, slot_start(2, 8), vec![transfer(&bob, 1, &c, 1_000_000_000), transfer(&alice, 2, &b, 500_000_000)]).unwrap();
    storage.apply_block(&b3).unwrap();

    // bob: received 50 + 5, sent 10 + fee 0.1 → consistent
    let audit = storage.audit_wallet(&b).unwrap();
    assert_eq!(audit.stored_nonce, Some(1));
    assert_eq!(audit.computed_nonce, 1);
    assert_eq!(audit.computed_balance, 5_000_000_000 + 500_000_000 - 1_000_000_000 - 10_000_000);
    assert_eq!(audit.stored_balance.map(i128::from), Some(audit.computed_balance));
    assert_eq!((audit.sent, audit.received), (1, 2));
    assert!(audit.nonce_ok && audit.balance_ok, "{audit:?}");

    // drift like the mainnet incident: nonce +5, balance −594 STH without any transaction behind it
    storage.update_wallet_state(&b, -594_000_000, 5).unwrap();
    let drift = storage.audit_wallet(&b).unwrap();
    assert_eq!(drift.stored_nonce, Some(6));
    assert!(!drift.nonce_ok && !drift.balance_ok, "{drift:?}");
    assert_eq!(drift.computed_nonce, 1);

    storage.repair_wallet(&drift).unwrap();
    let fixed = storage.audit_wallet(&b).unwrap();
    assert!(fixed.nonce_ok && fixed.balance_ok, "{fixed:?}");
    let w = storage.get_wallet(&b).unwrap().unwrap();
    assert_eq!((w.nonce, w.balance as i128), (1, audit.computed_balance));

    // an unknown wallet audits to zero
    let none = storage.audit_wallet(&KeyPair::from_passphrase("nobody").unwrap().address(63).unwrap()).unwrap();
    assert_eq!((none.stored_nonce, none.computed_nonce, none.computed_balance), (None, 0, 0));
    assert!(none.nonce_ok && none.balance_ok);

    // full-chain audit: forger fees are reconstructed too; the drifted wallet shows up, everything else is consistent
    storage.update_wallet_state(&b, -594_000_000, 5).unwrap();
    // (alice was funded by `update_wallet_state` without a transaction → she is the expected "funding artifact")
    let report = storage.audit_all(|_| {}).unwrap();
    assert_eq!(report.height, 3);
    assert_eq!(drifted(&report), sorted([a.clone(), b.clone()]), "{report:?}");
    assert_eq!(report.skipped, 0);
    let fw = storage.get_wallet(&forger.address(63).unwrap()).unwrap().unwrap();
    assert_eq!(fw.balance, 30_000_000, "3 fees of 0.1 STH forged");
    storage.repair_wallet(report.drifted.iter().find(|x| x.address == b).unwrap()).unwrap();
    assert_eq!(drifted(&storage.audit_all(|_| {}).unwrap()), vec![a.clone()]);
}

fn sorted(mut v: [String; 2]) -> Vec<String> {
    v.sort();
    v.to_vec()
}

fn drifted(report: &sth_core::storage::ChainAudit) -> Vec<String> {
    let mut v: Vec<String> = report.drifted.iter().map(|a| a.address.clone()).collect();
    v.sort();
    v
}

const FUNDING: i128 = 100_000_000_000;

/// The 2026-09 mainnet incident: two intake paths (iroh gossip + legacy pull / postBlock) applied the same block
/// concurrently → wallet deltas booked twice (nonce +N, balance −N×amount). The tip check inside the sled transaction
/// must let exactly one apply through and report `StaleTip` to the other.
#[test]
fn concurrent_apply_of_the_same_block_is_booked_once() {
    let network = Network::mainnet();
    let storage = std::sync::Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    storage.set_undo_enabled(true);
    let (alice, bob, forger) = (KeyPair::from_passphrase("alice").unwrap(), KeyPair::from_passphrase("bob").unwrap(), KeyPair::from_passphrase("forger").unwrap());
    let (a, b) = (alice.address(63).unwrap(), bob.address(63).unwrap());
    storage.update_wallet_state(&a, 100_000_000_000, 0).unwrap();
    let genesis = sth_core::genesis::mainnet_block().unwrap();
    let txs: Vec<_> = (1..=5).map(|n| transfer(&alice, n, &b, 1_000_000_000)).collect();
    let b2 = forge_block(&network, &forger, &genesis, slot_start(1, 8), txs).unwrap();

    for _round in 0..3 {
        let tip = ChainTip { height: 1, id: Some(network.genesis_block_id.clone()) };
        let results: Vec<_> = (0..4)
            .map(|_| {
                let (st, net, blk, tip) = (storage.clone(), network.clone(), b2.clone(), tip.clone());
                std::thread::spawn(move || sth_core::sync::apply_blocks(&st, &net, &[blk], tip, true))
            })
            .map(|h| h.join().unwrap())
            .collect();
        let ok = results.iter().filter(|r| r.is_ok()).count();
        let stale = results.iter().filter(|r| matches!(r, Err(sth_core::error::Error::StaleTip(_)))).count();
        assert_eq!((ok, stale), (if _round == 0 { 1 } else { 0 }, if _round == 0 { 3 } else { 4 }), "{results:?}");
    }
    let w = storage.get_wallet(&a).unwrap().unwrap();
    assert_eq!((w.nonce, w.balance), (5, 100_000_000_000 - 5 * 1_000_000_000 - 5 * 10_000_000));
    assert!(storage.audit_wallet(&a).unwrap().nonce_ok);
    let report = storage.audit_all(|_| {}).unwrap();
    assert_eq!(drifted(&report), vec![a.clone()], "only the funding artifact: {report:?}");
    assert_eq!(i128::from(report.drifted[0].stored_balance.unwrap()) - report.drifted[0].computed_balance, FUNDING);
    // the undo record belongs to the single successful apply → rollback restores the pre-block state exactly
    storage.rollback_last_block().unwrap();
    let w = storage.get_wallet(&a).unwrap().unwrap();
    assert_eq!((w.nonce, w.balance), (0, 100_000_000_000));
}

/// forge → the network orphans our block → fork rollback → the network's branch is applied: nonce / balance of every
/// wallet must match the chain history afterwards (regression for the drift found on mainnet).
#[test]
fn forge_orphan_rollback_keeps_wallets_consistent() {
    let network = Network::mainnet();
    let storage = std::sync::Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    storage.set_undo_enabled(true);
    let (alice, bob, us, them) = (KeyPair::from_passphrase("alice").unwrap(), KeyPair::from_passphrase("bob").unwrap(), KeyPair::from_passphrase("us").unwrap(), KeyPair::from_passphrase("them").unwrap());
    let (a, b) = (alice.address(63).unwrap(), bob.address(63).unwrap());
    storage.update_wallet_state(&a, 100_000_000_000, 0).unwrap();
    let genesis = sth_core::genesis::mainnet_block().unwrap();
    let apply = |blocks: &[sth_core::models::Block]| {
        let tip = storage.get_last_block().unwrap().unwrap();
        sth_core::sync::apply_blocks(&storage, &network, blocks, ChainTip { height: tip.height, id: tip.id }, true)
    };

    // network block 2, then OUR block 3 with alice's tx (the one the network later rejects)
    let n2 = forge_block(&network, &them, &genesis, slot_start(1, 8), vec![transfer(&alice, 1, &b, 2_000_000_000)]).unwrap();
    apply(&[n2.clone()]).unwrap();
    let ours3 = forge_block(&network, &us, &n2, slot_start(2, 8), vec![transfer(&alice, 2, &b, 3_000_000_000)]).unwrap();
    apply(&[ours3.clone()]).unwrap();
    assert_eq!(storage.get_wallet(&a).unwrap().unwrap().nonce, 2);

    // a stale second delivery of our own block (gossip echo) must be a no-op
    assert!(matches!(sth_core::sync::apply_blocks(&storage, &network, &[ours3.clone()], ChainTip { height: 2, id: n2.id.clone() }, true), Err(sth_core::error::Error::StaleTip(_))));

    // the network continued with ANOTHER block 3 (ours orphaned) → follow loop detects the fork and rolls back
    let n3 = forge_block(&network, &them, &n2, slot_start(2, 8), vec![]).unwrap();
    let n4 = forge_block(&network, &them, &n3, slot_start(3, 8), vec![transfer(&alice, 2, &b, 500_000_000)]).unwrap();
    assert!(apply(&[n3.clone(), n4.clone()]).is_err(), "previousBlock mismatch = fork");
    storage.rollback_to(2).unwrap();
    assert_eq!(storage.get_wallet(&a).unwrap().unwrap().nonce, 1);
    apply(&[n3, n4]).unwrap();

    let audit_a = storage.audit_wallet(&a).unwrap();
    assert!(audit_a.nonce_ok, "{audit_a:?}");
    assert_eq!((audit_a.computed_nonce, audit_a.stored_nonce), (2, Some(2)));
    assert_eq!(audit_a.computed_balance, -2_000_000_000 - 500_000_000 - 2 * 10_000_000);
    assert_eq!(i128::from(audit_a.stored_balance.unwrap()) - audit_a.computed_balance, FUNDING, "only the funding artifact");
    let audit_b = storage.audit_wallet(&b).unwrap();
    assert!(audit_b.nonce_ok && audit_b.balance_ok, "{audit_b:?}");
    assert_eq!(audit_b.computed_balance, 2_500_000_000);
    let report = storage.audit_all(|_| {}).unwrap();
    assert_eq!(drifted(&report), vec![a.clone()], "only the funding artifact: {report:?}");
    assert!(report.drifted[0].nonce_ok);
    assert_eq!(i128::from(report.drifted[0].stored_balance.unwrap()) - report.drifted[0].computed_balance, FUNDING);
    assert!(storage.get_wallet(&us.address(63).unwrap()).unwrap().map_or(true, |w| w.balance == 0), "orphaned forger keeps nothing");
}

/// `audit_all_live` (running node): the scan runs unlocked, the tail is booked under the apply lock, `fix` repairs the
/// drift and rebuilds the vote index in the same critical section — same verdict as the offline `audit_all`.
#[test]
fn live_audit_matches_offline_and_repairs_under_the_apply_lock() {
    let network = Network::mainnet();
    let storage = Storage::temporary(network.clone()).unwrap();
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let (alice, bob, forger) = (KeyPair::from_passphrase("alice").unwrap(), KeyPair::from_passphrase("bob").unwrap(), KeyPair::from_passphrase("forger").unwrap());
    let (a, b) = (alice.address(63).unwrap(), bob.address(63).unwrap());
    storage.update_wallet_state(&a, 100_000_000_000, 0).unwrap();
    let genesis = sth_core::genesis::mainnet_block().unwrap();
    let b2 = forge_block(&network, &forger, &genesis, slot_start(1, 8), vec![transfer(&alice, 1, &b, 5_000_000_000)]).unwrap();
    storage.apply_block(&b2).unwrap();

    let offline = storage.audit_all(|_| {}).unwrap();
    let (live, repaired) = storage.audit_all_live(false, |_| {}).unwrap();
    assert_eq!((live.height, live.wallets, live.checked, drifted(&live)), (offline.height, offline.wallets, offline.checked, drifted(&offline)));
    assert_eq!(repaired, 0);
    assert_eq!(drifted(&live), vec![a.clone()], "only the funding artifact");

    storage.update_wallet_state(&b, -594_000_000, 5).unwrap();
    let (report, repaired) = storage.audit_all_live(true, |_| {}).unwrap();
    assert_eq!(drifted(&report), sorted([a.clone(), b.clone()]));
    assert_eq!(repaired, 2);
    let w = storage.get_wallet(&b).unwrap().unwrap();
    assert_eq!((w.nonce, w.balance), (0, 5_000_000_000), "bob never sent → nonce 0");
    // repaired → a second pass is clean (alice is repaired to her history too)
    let (again, repaired) = storage.audit_all_live(true, |_| {}).unwrap();
    assert!(again.drifted.is_empty(), "{again:?}");
    assert_eq!(repaired, 0);
    // the storage is still usable for block application after the locked section
    let b3 = forge_block(&network, &forger, &b2, slot_start(2, 8), vec![]).unwrap();
    storage.apply_block(&b3).unwrap();
    assert_eq!(storage.audit_all_live(false, |_| {}).unwrap().0.height, 3);
}
