//! Author: TechnoL0g
//!
//! Embedded genesis block: decodes, verifies and seeds an empty database.

use sth_core::config::Network;
use sth_core::genesis::{ensure_genesis, mainnet_block};
use sth_core::storage::Storage;

#[test]
fn embedded_genesis_verifies_and_seeds_empty_db() {
    let network = Network::mainnet();
    let block = mainnet_block().unwrap();
    assert_eq!(block.height, 1);
    assert_eq!(block.id.as_deref(), Some(network.genesis_block_id.as_str()));
    assert_eq!(block.transactions.len(), 1855);
    assert_eq!(block.total_amount, 24_977_000_000_000_000);

    let storage = Storage::temporary(network.clone()).unwrap();
    assert!(ensure_genesis(&storage, &network).unwrap());
    assert_eq!(storage.get_last_height().unwrap(), 1);
    assert!(!ensure_genesis(&storage, &network).unwrap());
    let w = storage.get_wallet("SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw").unwrap();
    assert!(w.is_some() || storage.get_transaction(block.transactions[0].id.as_deref().unwrap()).unwrap().is_some());
}
