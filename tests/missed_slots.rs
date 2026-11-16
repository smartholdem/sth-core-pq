//! Author: TechnoL0g
//!
//! Missed-slot index (per delegate, undone on rollback) and the future-slot discard rule.

use std::sync::Arc;
use sth_core::config::Network;
use sth_core::crypto::KeyPair;
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::{forging_order, slot_start};
use sth_core::error::Error;
use sth_core::storage::Storage;
use sth_core::sync::ChainTip;

#[test]
fn gaps_are_attributed_by_round_order_and_undone_on_rollback() {
    let network = Network::mainnet();
    let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let genesis = sth_core::genesis::mainnet_block().unwrap();
    let order = forging_order(&storage, 1, 21).unwrap().expect("round 1 snapshot from the genesis ranking");
    let k = KeyPair::from_passphrase("forger").unwrap();

    let b1 = forge_block(&network, &k, &genesis, slot_start(1, 8), vec![]).unwrap();
    storage.apply_block(&b1).unwrap();
    assert!(storage.missed_recent(10).unwrap().is_empty(), "consecutive slots: nothing missed");

    // slot 1 → slot 4: slots 2 and 3 were skipped by their owners
    let b2 = forge_block(&network, &k, &b1, slot_start(4, 8), vec![]).unwrap();
    storage.apply_block(&b2).unwrap();
    let recent = storage.missed_recent(10).unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].slot, 3, "newest first");
    assert_eq!(recent[0].public_key, order[3].public_key);
    assert_eq!(recent[1].public_key, order[2].public_key);
    assert_eq!(recent[0].height, 3);
    assert_eq!(recent[0].timestamp, slot_start(3, 8));
    assert_eq!(storage.missed_count(&order[2].public_key).unwrap(), 1);
    assert_eq!(storage.missed_count(&order[3].public_key).unwrap(), 1);
    assert_eq!(storage.missed_since().unwrap(), Some(3));

    // a gap longer than a round is an outage, not attributed
    let b3 = forge_block(&network, &k, &b2, slot_start(4 + 30, 8), vec![]).unwrap();
    storage.apply_block(&b3).unwrap();
    assert_eq!(storage.missed_recent(100).unwrap().len(), 2);

    // rollback of the gap-closing block undoes the attribution
    storage.set_undo_enabled(true);
    storage.rollback_to(2).unwrap();
    assert_eq!(storage.missed_count(&order[2].public_key).unwrap(), 0);
    assert!(storage.missed_recent(10).unwrap().is_empty());
}

#[test]
fn blocks_for_a_future_slot_are_discarded() {
    let network = Network::mainnet();
    let storage = Storage::temporary(network.clone()).unwrap();
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let genesis = sth_core::genesis::mainnet_block().unwrap();
    let k = KeyPair::from_passphrase("fast clock").unwrap();
    let now = network.now_epoch();
    let tip = ChainTip { height: 1, id: genesis.id.clone() };

    // more than a slot ahead (the cyber case: forged ~7.4 s before its slot) → discarded, counted, not applied
    let future = forge_block(&network, &k, &genesis, slot_start(now as u64 / 8 + 2, 8), vec![]).unwrap();
    assert!(sth_core::intake::future_slot_ms(&future, &network).is_some_and(|ms| ms > 1_000));
    let err = sth_core::sync::apply_blocks(&storage, &network, &[future], tip.clone(), false).unwrap_err();
    assert!(matches!(err, Error::FutureBlock(_)), "{err}");
    assert!(err.to_string().contains("takes slot"), "{err}");
    assert_eq!(storage.get_last_height().unwrap(), 1);
    let snap = sth_core::intake::snapshot();
    assert!(snap["futureRejected"]["count"].as_u64().unwrap() >= 1);
    assert_eq!(snap["futureRejected"]["last"]["height"], 2);

    // the current slot (and anything in the past) is fine
    let current = forge_block(&network, &k, &genesis, slot_start(now as u64 / 8, 8), vec![]).unwrap();
    assert!(sth_core::intake::future_slot_ms(&current, &network).is_none());
    sth_core::sync::apply_blocks(&storage, &network, &[current], tip, false).unwrap();
    assert_eq!(storage.get_last_height().unwrap(), 2);
}

#[test]
fn minority_rule_needs_a_quorum_on_our_tip_and_fewer_than_a_third_ahead() {
    use sth_core::delegate::forger::minority_rule;
    // 8 responders: 2 ahead (< 1/3), 6 on our tip, quorum 4 → forge
    assert!(minority_rule(2, 6, 8, 4));
    // 3 of 8 ahead is not a minority under a third
    assert!(!minority_rule(3, 5, 8, 4));
    // nobody ahead → the normal path handles it, the rule does not apply
    assert!(!minority_rule(0, 8, 8, 4));
    // quorum on our tip missing (silent peers do not relax the bar)
    assert!(!minority_rule(1, 3, 8, 4));
    // tiny networks: 1 of 3 ahead is a third → wait
    assert!(!minority_rule(1, 2, 3, 2));
    assert!(minority_rule(1, 3, 4, 3));
}

#[test]
fn a_block_less_than_a_slot_early_is_held_and_applied_at_its_slot_start() {
    let network = Network::mainnet();
    let storage = Storage::temporary(network.clone()).unwrap();
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let genesis = sth_core::genesis::mainnet_block().unwrap();
    let k = KeyPair::from_passphrase("slightly fast clock").unwrap();
    // 2..5 s before the next slot (the cyber case of 16.11.2026: 2.5 s early)
    let unix_ms = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let slot_ms = |s: u64| network.epoch_to_unix(slot_start(s, 8)) * 1000;
    let next = loop {
        let next = network.now_epoch() as u64 / 8 + 1;
        let left = slot_ms(next) - unix_ms();
        if (2_000..=5_000).contains(&left) {
            break next;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    let tip = ChainTip { height: 1, id: genesis.id.clone() };
    let early = forge_block(&network, &k, &genesis, slot_start(next, 8), vec![]).unwrap();
    let err = sth_core::sync::apply_blocks(&storage, &network, std::slice::from_ref(&early), tip.clone(), false).unwrap_err();
    assert!(matches!(err, Error::FutureBlock(_)) && err.to_string().contains("held"), "{err}");
    assert_eq!(storage.get_last_height().unwrap(), 1, "not applied before its slot");
    assert_eq!(sth_core::intake::apply_due_parked(&storage), 0, "slot not started yet");
    assert_eq!(sth_core::intake::snapshot()["futureHeld"]["last"]["height"], 2);

    // a second block of the same generator is not held (one per delegate)
    let twin = forge_block(&network, &KeyPair::from_passphrase("slightly fast clock").unwrap(), &genesis, slot_start(next, 8) + 1, vec![]).unwrap();
    let err = sth_core::sync::apply_blocks(&storage, &network, &[twin], tip, false).unwrap_err();
    assert!(!err.to_string().contains("held"), "{err}");

    std::thread::sleep(std::time::Duration::from_millis((slot_ms(next) - unix_ms()).max(0) as u64 + 50));
    assert_eq!(sth_core::intake::apply_due_parked(&storage), 1);
    assert_eq!(storage.get_last_block().unwrap().unwrap().id, early.id);
    assert_eq!(sth_core::intake::apply_due_parked(&storage), 0);

    // a held block is dropped when the tip moved meanwhile (same test: the parked buffer is process-global)
    let storage = Storage::temporary(network.clone()).unwrap();
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let genesis = sth_core::genesis::mainnet_block().unwrap();
    let now = network.now_epoch() as u64;
    let honest = forge_block(&network, &KeyPair::from_passphrase("honest").unwrap(), &genesis, slot_start(now / 8, 8), vec![]).unwrap();
    let early = forge_block(&network, &KeyPair::from_passphrase("stale and fast").unwrap(), &genesis, slot_start(now / 8 + 1, 8), vec![]).unwrap();
    // parked directly: independent of where in the slot the test runs
    assert!(sth_core::intake::park_future(&early, &network, 0, false));
    storage.apply_block(&honest).unwrap();
    assert_eq!(sth_core::intake::apply_due_parked(&storage), 0);
    assert_eq!(storage.get_last_block().unwrap().unwrap().id, honest.id, "the honest block stays");
}
