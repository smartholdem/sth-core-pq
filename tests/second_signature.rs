//! Author: TechnoL0g
//!
//! Second-signature enforcement (legacy `throwIfCannotBeApplied` semantics) on block apply and in the mempool.

use sth_core::config::Network;
use sth_core::crypto::{
    serialize_transaction, sha256, sign_schnorr_legacy, transaction_id, transaction_signing_hash, KeyPair, SerializeOptions,
};
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::genesis::mainnet_block;
use sth_core::mempool::Mempool;
use sth_core::models::{Block, Transaction};
use sth_core::storage::Storage;
use sth_core::sync::{apply_blocks, ChainTip};
use std::sync::Arc;

fn tx(keys: &KeyPair, nonce: u64, body: serde_json::Value) -> Transaction {
    let mut v = serde_json::json!({
        "version": 2, "network": 63, "typeGroup": 1, "nonce": nonce.to_string(),
        "senderPublicKey": keys.public_key_hex(), "amount": "0", "expiration": 0
    });
    v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
    serde_json::from_value(v).unwrap()
}

fn transfer(keys: &KeyPair, nonce: u64, to: &str) -> Transaction {
    transfer_amount(keys, nonce, to, 100_000_000)
}

fn transfer_amount(keys: &KeyPair, nonce: u64, to: &str, amount: u64) -> Transaction {
    tx(keys, nonce, serde_json::json!({ "type": 0, "fee": "10000000", "amount": amount.to_string(), "recipientId": to }))
}

fn registration(keys: &KeyPair, nonce: u64, second: &KeyPair) -> Transaction {
    tx(keys, nonce, serde_json::json!({ "type": 1, "fee": "500000000", "asset": { "signature": { "publicKey": second.public_key_hex() } } }))
}

fn sign(mut t: Transaction, keys: &KeyPair, second: Option<&KeyPair>) -> Transaction {
    let h = transaction_signing_hash(&t).unwrap();
    t.signature = Some(sign_schnorr_legacy(&h, keys.private_key()).unwrap());
    if let Some(s) = second {
        let opts = SerializeOptions { exclude_signature: false, exclude_second_signature: true, exclude_multi_signature: false };
        let h2 = sha256(&serialize_transaction(&t, opts, Network::mainnet_ref()).unwrap());
        t.second_signature = Some(sign_schnorr_legacy(&h2, s.private_key()).unwrap());
    }
    t.id = Some(transaction_id(&t).unwrap());
    t
}

struct Chain {
    network: Network,
    storage: Arc<Storage>,
    forger: KeyPair,
    tip_block: Block,
    tip: ChainTip,
    slot: u64,
}

impl Chain {
    fn new() -> Self {
        let network = Network::mainnet();
        let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
        sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
        let genesis = mainnet_block().unwrap();
        let tip = ChainTip { height: 1, id: genesis.id.clone() };
        Self { network, storage, forger: KeyPair::from_passphrase("forger").unwrap(), tip_block: genesis, tip, slot: 1 }
    }

    fn block(&mut self, txs: Vec<Transaction>) -> Block {
        self.slot += 1;
        forge_block(&self.network, &self.forger, &self.tip_block, slot_start(self.slot, 8), txs).unwrap()
    }

    fn apply(&mut self, blocks: &[Block]) -> Result<(), String> {
        let tip = apply_blocks(&self.storage, &self.network, blocks, self.tip.clone(), true).map_err(|e| e.to_string())?;
        self.tip = tip;
        self.tip_block = blocks.last().unwrap().clone();
        Ok(())
    }
}

#[test]
fn second_signature_rules_on_block_apply() {
    let mut c = Chain::new();
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let alice2 = KeyPair::from_passphrase("alice second").unwrap();
    let carol = KeyPair::from_passphrase("carol").unwrap();
    let bob = KeyPair::from_passphrase("bob").unwrap().address(63).unwrap();

    // registration must not itself be second-signed
    let b = c.block(vec![sign(registration(&alice, 1, &alice2), &alice, Some(&alice2))]);
    assert!(c.apply(&[b]).unwrap_err().contains("UnexpectedSecondSignatureError"));

    let b = c.block(vec![sign(registration(&alice, 1, &alice2), &alice, None)]);
    c.apply(&[b]).unwrap();
    assert_eq!(
        c.storage.get_wallet(&alice.address(63).unwrap()).unwrap().unwrap().second_public_key.as_deref(),
        Some(alice2.public_key_hex().as_str())
    );

    let missing = c.block(vec![sign(transfer(&alice, 2, &bob), &alice, None)]);
    assert!(c.apply(&[missing]).unwrap_err().contains("MissingSecondSignatureError"));

    let wrong = c.block(vec![sign(transfer(&alice, 2, &bob), &alice, Some(&carol))]);
    assert!(c.apply(&[wrong]).unwrap_err().contains("InvalidSecondSignatureError"));

    let again = c.block(vec![sign(registration(&alice, 2, &carol), &alice, None)]);
    assert!(c.apply(&[again]).unwrap_err().contains("SecondSignatureAlreadyRegisteredError"));

    let unexpected = c.block(vec![sign(transfer(&carol, 1, &bob), &carol, Some(&alice2))]);
    assert!(c.apply(&[unexpected]).unwrap_err().contains("UnexpectedSecondSignatureError"));

    let ok = c.block(vec![sign(transfer(&alice, 2, &bob), &alice, Some(&alice2))]);
    c.apply(&[ok]).unwrap();
    assert_eq!(c.tip.height, 3);
}

#[test]
fn registration_and_spend_in_one_batch() {
    let mut c = Chain::new();
    let dave = KeyPair::from_passphrase("dave").unwrap();
    let dave2 = KeyPair::from_passphrase("dave second").unwrap();
    let bob = KeyPair::from_passphrase("bob").unwrap().address(63).unwrap();
    let b2 = c.block(vec![sign(registration(&dave, 1, &dave2), &dave, None)]);
    c.tip_block = b2.clone();
    let b3 = c.block(vec![sign(transfer(&dave, 2, &bob), &dave, Some(&dave2))]);
    c.tip_block = mainnet_block().unwrap();
    c.apply(&[b2, b3]).unwrap();
    assert_eq!(c.tip.height, 3);
}

#[tokio::test]
async fn mempool_enforces_second_signature() {
    let mut c = Chain::new();
    let erin = KeyPair::from_passphrase("erin").unwrap();
    let erin2 = KeyPair::from_passphrase("erin second").unwrap();
    let funder = KeyPair::from_passphrase("funder").unwrap();
    let erin_addr = erin.address(63).unwrap();
    let bob = KeyPair::from_passphrase("bob").unwrap().address(63).unwrap();
    // fund erin (storage apply does not check balances) and register her second key
    let b = c.block(vec![sign(transfer_amount(&funder, 1, &erin_addr, 10_000_000_000), &funder, None)]);
    c.apply(&[b]).unwrap();
    let b = c.block(vec![sign(registration(&erin, 1, &erin2), &erin, None)]);
    c.apply(&[b]).unwrap();

    let pool = Mempool::new(c.storage.clone(), vec![], 100);
    let (_, errors) = pool.add_many(vec![sign(transfer(&erin, 2, &bob), &erin, None)]).await;
    assert!(errors.values().next().unwrap().message.contains("MissingSecondSignatureError"), "{errors:?}");
    let (_, errors) = pool.add_many(vec![sign(transfer(&erin, 2, &bob), &erin, Some(&erin2))]).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(pool.len().await, 1);
}
