//! Author: TechnoL0g
//!
//! Forging: a block built by the delegate module passes the same verification as network blocks.

use sth_core::config::Network;
use sth_core::crypto::{verify_block, KeyPair};
use sth_core::delegate::block_builder::{forge_block, select_transactions};
use sth_core::delegate::round::{round_info, slot_start};
use sth_core::genesis::mainnet_block;
use sth_core::storage::Storage;

#[test]
fn forged_block_verifies_and_applies() {
    let network = Network::mainnet();
    let storage = Storage::temporary(network.clone()).unwrap();
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let genesis = mainnet_block().unwrap();
    let keys = KeyPair::from_passphrase("this is a top secret passphrase").unwrap();
    let txs = select_transactions(&storage, &network, vec![], 2).unwrap();
    assert!(txs.is_empty());
    let block = forge_block(&network, &keys, &genesis, slot_start(1, 8), txs).unwrap();
    assert_eq!(block.height, 2);
    assert_eq!(block.previous_block, network.genesis_block_id);
    assert_eq!(block.generator_public_key, keys.public_key_hex());
    assert_eq!(block.payload_hash, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    let v = verify_block(&block, &network);
    assert!(v.verified, "{:?}", v.errors);
    storage.apply_block(&block).unwrap();
    assert_eq!(storage.get_last_height().unwrap(), 2);
    assert_eq!(round_info(2, 21).round, 1);
    let serialized = sth_core::crypto::serialize_block_with_transactions(&block, &network).unwrap();
    assert_eq!(serialized.len(), sth_core::crypto::serialize_block(&block, true).unwrap().len());
}
