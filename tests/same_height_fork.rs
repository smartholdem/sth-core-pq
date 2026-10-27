//! Author: TechnoL0g
//!
//! Same-height fork at the tip: the block from the EARLIER slot replaces a later one (bounded: tip only, same parent,
//! not certified); a later block never displaces an earlier tip.

use std::sync::Arc;
use sth_core::config::Network;
use sth_core::crypto::KeyPair;
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::storage::Storage;

#[tokio::test]
async fn earlier_slot_wins_at_the_tip_only() {
    let network = Network::mainnet();
    let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let genesis = sth_core::genesis::mainnet_block().unwrap();
    let (a, b, c) = (KeyPair::from_passphrase("early").unwrap(), KeyPair::from_passphrase("late").unwrap(), KeyPair::from_passphrase("next").unwrap());
    let early = forge_block(&network, &a, &genesis, slot_start(1, 8), vec![]).unwrap();
    let late = forge_block(&network, &b, &genesis, slot_start(2, 8), vec![]).unwrap();

    // we applied the LATE block first (its forger had not seen the early one); the early one arrives → swap
    storage.apply_block(&late).unwrap();
    assert!(sth_core::intake::prefer_earlier_slot(&storage, &late, &early, "peer").await);
    assert_eq!(storage.get_last_block().unwrap().unwrap().id, early.id);
    // the late block arriving now does not displace the earlier tip
    assert!(!sth_core::intake::prefer_earlier_slot(&storage, &early, &late, "peer").await);
    assert_eq!(storage.get_last_block().unwrap().unwrap().id, early.id);

    // not at the tip any more (a child exists) → no swap even for an earlier slot
    let child = forge_block(&network, &c, &early, slot_start(3, 8), vec![]).unwrap();
    storage.apply_block(&child).unwrap();
    let other_early = forge_block(&network, &c, &genesis, slot_start(0, 8) + 8, vec![]).unwrap();
    assert!(!sth_core::intake::prefer_earlier_slot(&storage, &early, &other_early, "peer").await);
    assert_eq!(storage.get_last_height().unwrap(), 3);

    // a block with a broken signature is never preferred
    let mut forged = forge_block(&network, &a, &early, slot_start(2, 8), vec![]).unwrap();
    forged.block_signature = Some("00".repeat(64));
    assert!(!sth_core::intake::prefer_earlier_slot(&storage, &child, &forged, "peer").await);
}
