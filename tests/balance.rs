//! Author: TechnoL0g
//! Milestone `strictBalance`: a block whose sender cannot cover amount + fee is rejected (legacy InsufficientBalanceError),
//! with legacy ordering — coins received earlier in the block count, the forger's reward of the same block does not.

use sth_core::config::{Network, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ, MAINNET_MILESTONES_JSON, MAINNET_NETWORK_JSON};
use sth_core::crypto::{sign_schnorr_legacy, transaction_id, transaction_signing_hash, KeyPair};
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::models::{Block, Transaction};
#[allow(unused_imports)]
use sth_core::models::tx_type;
use sth_core::storage::Storage;
use sth_core::sync::{apply_blocks, ChainTip};
use std::sync::Arc;

fn net(strict: bool) -> Network {
    let ms = MAINNET_MILESTONES_JSON.replace("\"ship11\": true", &format!("\"ship11\": true, \"aip36\": true, \"sobjV2\": true, \"strictBalance\": {strict}"));
    Network::from_json(MAINNET_NETWORK_JSON, &ms, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap()
}

fn transfer(keys: &KeyPair, nonce: u64, to: &str, amount: u64) -> Transaction {
    let mut t: Transaction = serde_json::from_value(serde_json::json!({
        "version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": nonce.to_string(), "senderPublicKey": keys.public_key_hex(),
        "fee": "10000000", "amount": amount.to_string(), "recipientId": to, "expiration": 0
    }))
    .unwrap();
    let h = transaction_signing_hash(&t).unwrap();
    t.signature = Some(sign_schnorr_legacy(&h, keys.private_key()).unwrap());
    t.id = Some(transaction_id(&t).unwrap());
    t
}

struct Chain {
    storage: Arc<Storage>,
    forger: KeyPair,
    tip_block: Block,
    slot: u64,
}
impl Chain {
    fn new(network: Network) -> Self {
        let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
        let forger = KeyPair::from_passphrase("forger").unwrap();
        sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
        let genesis: Block = sth_core::genesis::mainnet_block().unwrap();
        Self { storage, forger, tip_block: genesis, slot: 1 }
    }
    fn try_block(&mut self, txs: Vec<Transaction>) -> Result<(), String> {
        let net = self.storage.network().clone();
        let block = forge_block(&net, &self.forger, &self.tip_block, slot_start(self.slot, 8), txs).map_err(|e| e.to_string())?;
        let tip = ChainTip { height: self.tip_block.height, id: self.tip_block.id.clone() };
        apply_blocks(&self.storage, &net, &[block.clone()], tip, true).map_err(|e| e.to_string())?;
        self.tip_block = block;
        self.slot += 1;
        Ok(())
    }
    fn bal(&self, w: &str) -> i64 {
        self.storage.get_wallet(w).unwrap().map(|w| w.balance).unwrap_or(0)
    }
}

#[test]
fn strict_balance_rejects_overspending_like_legacy() {
    let (alice, bob, carol) = (KeyPair::from_passphrase("alice").unwrap(), KeyPair::from_passphrase("bob").unwrap(), KeyPair::from_passphrase("carol").unwrap());
    let (a, b, c_addr) = (alice.address(63).unwrap(), bob.address(63).unwrap(), carol.address(63).unwrap());
    let mut c = Chain::new(net(true));
    c.storage.update_wallet_state(&a, 10_000_000_000, 0).unwrap(); // 100 coins

    // exactly amount + fee passes; one satoshi more fails
    c.try_block(vec![transfer(&alice, 1, &b, 5_000_000_000)]).unwrap(); // left: 49.9
    let e = c.try_block(vec![transfer(&alice, 2, &b, 4_980_000_001)]).unwrap_err();
    assert!(e.contains("InsufficientBalanceError"), "{e}");
    c.try_block(vec![transfer(&alice, 2, &b, 4_980_000_000)]).unwrap();
    assert_eq!(c.bal(&a), 0);

    // coins received earlier in the same block count (legacy applies txs in order)
    let e = c.try_block(vec![transfer(&carol, 1, &a, 1)]).unwrap_err();
    assert!(e.contains("InsufficientBalanceError"), "{e}");
    c.try_block(vec![transfer(&bob, 1, &c_addr, 1_000_000_000), transfer(&carol, 1, &a, 500_000_000)]).unwrap();
    assert_eq!(c.bal(&c_addr), 1_000_000_000 - 500_000_000 - 10_000_000);
    // …but not coins that arrive later in the block
    let e = c.try_block(vec![transfer(&carol, 2, &a, 490_000_000), transfer(&bob, 2, &c_addr, 1_000_000_000)]).unwrap_err();
    assert!(e.contains("InsufficientBalanceError"), "{e}");

    // the forger cannot spend the reward / fees of the block it is forging (credited after the transactions)
    let f = c.forger.address(63).unwrap();
    assert!(c.bal(&f) > 0, "forger collected fees");
    let fee_income = c.bal(&f) as u64;
    let e = c.try_block(vec![transfer(&c.forger.clone(), 1, &b, fee_income)]).unwrap_err();
    assert!(e.contains("InsufficientBalanceError"), "{e}");
    c.try_block(vec![transfer(&c.forger.clone(), 1, &b, fee_income - 10_000_000)]).unwrap();

    // flag off: the same overspend is only logged, block applies (mainnet before the milestone)
    let mut lenient = Chain::new(net(false));
    lenient.storage.update_wallet_state(&a, 1_000_000_000, 0).unwrap();
    lenient.try_block(vec![transfer(&alice, 1, &b, 5_000_000_000)]).unwrap();
    assert!(lenient.bal(&a) < 0);
}
