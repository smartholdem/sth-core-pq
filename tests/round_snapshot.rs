//! Author: TechnoL0g
//!
//! Round snapshots (legacy `applyRound`) are taken by the storage itself, atomically with the boundary block — never by
//! a timer that may miss the boundary and later "guess" the forging order from mid-round balances (mainnet 2026-09-20:
//! slot 12079774 forged by a PQ Node as nicholasflamel while the network's block came from titan → private fork).

use sth_core::config::Network;
use sth_core::crypto::KeyPair;
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::{forging_order, round_info, slot_start};
use sth_core::models::Block;
use sth_core::storage::Storage;

fn chain(network: &Network, forger: &KeyPair, from: &Block, count: usize) -> Vec<Block> {
    let mut out = Vec::new();
    let mut prev = from.clone();
    for i in 0..count {
        let b = forge_block(network, forger, &prev, slot_start(prev.height + 1 + i as u64, 8), vec![]).unwrap();
        prev = b.clone();
        out.push(b);
    }
    out
}

#[test]
fn round_snapshot_is_written_with_the_boundary_block_and_dropped_on_rollback() {
    let network = Network::mainnet();
    let n = network.milestone(1).active_delegates as u64;
    assert_eq!(n, 21);
    let storage = Storage::temporary(network.clone()).unwrap();
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let forger = KeyPair::from_passphrase("forger").unwrap();
    let genesis = sth_core::genesis::mainnet_block().unwrap();
    let blocks = chain(&network, &forger, &genesis, 30);

    // bulk mode (undo off, one batch crossing the boundary at 21): the batch is split at the boundary and rd:2 written
    storage.set_undo_enabled(false);
    assert!(forging_order(&storage, 2, 21).unwrap().is_none(), "no snapshot before the boundary");
    storage.apply_blocks(&blocks[..25]).unwrap();
    assert_eq!(storage.get_last_height().unwrap(), 26);
    assert_eq!(round_info(22, n).round, 2);
    let snap = storage.get_round(2).unwrap().expect("snapshot for round 2 taken at height 21");
    assert!(!snap.is_empty());
    assert!(forging_order(&storage, 2, 21).unwrap().is_some());
    assert!(forging_order(&storage, 3, 21).unwrap().is_none(), "round 3 starts at 43, not reached");

    // single-block path (follow loop / gossip / forger): boundary at 42 → rd:3
    storage.set_undo_enabled(true);
    for b in &blocks[25..] {
        storage.apply_blocks(std::slice::from_ref(b)).unwrap();
    }
    assert_eq!(storage.get_last_height().unwrap(), 31);
    let more = chain(&network, &forger, &blocks[29], 12);
    for b in &more {
        storage.apply_blocks(std::slice::from_ref(b)).unwrap();
    }
    assert_eq!(storage.get_last_height().unwrap(), 43);
    assert!(storage.get_round(3).unwrap().is_some(), "rd:3 written when block 42 was applied");

    // rolling the boundary block back drops the snapshot of the branch that produced it
    storage.rollback_to(41).unwrap();
    assert!(storage.get_round(3).unwrap().is_none(), "rd:3 belongs to the rolled-back branch");
    assert!(storage.get_round(2).unwrap().is_some(), "rd:2 (boundary 21) untouched");
    // re-applying the boundary recreates it
    storage.apply_blocks(std::slice::from_ref(&more[10])).unwrap();
    assert!(storage.get_round(3).unwrap().is_some());
}

#[test]
fn single_blocks_always_get_undo_records_even_in_bulk_mode() {
    let network = Network::mainnet();
    let storage = Storage::temporary(network.clone()).unwrap();
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let forger = KeyPair::from_passphrase("forger").unwrap();
    let genesis = sth_core::genesis::mainnet_block().unwrap();
    let blocks = chain(&network, &forger, &genesis, 3);
    storage.set_undo_enabled(false);
    // a gossip / postBlock delivery during catch-up: one block at a time → must remain rollback-able
    storage.apply_blocks(std::slice::from_ref(&blocks[0])).unwrap();
    storage.apply_blocks(std::slice::from_ref(&blocks[1])).unwrap();
    assert_eq!(storage.rollback_last_block().unwrap(), 2);
    assert_eq!(storage.rollback_last_block().unwrap(), 1);
    // a real bulk batch still skips undo
    storage.apply_blocks(&blocks[..2]).unwrap();
    assert!(storage.rollback_last_block().is_err(), "bulk batches have no undo records");
}
