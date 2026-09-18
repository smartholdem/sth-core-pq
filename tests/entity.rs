//! Author: TechnoL0g
//!
//! AIP-36 entity transactions: wire format, activation gate, legacy handler rules, state, rollback, mempool, API storage queries.

use sth_core::config::{Network, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ, MAINNET_MILESTONES_JSON, MAINNET_NETWORK_JSON};
use sth_core::crypto::{deserialize_transaction, serialize_transaction, sign_schnorr_legacy, transaction_id, transaction_signing_hash, KeyPair, SerializeOptions};
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::genesis::mainnet_block;
use sth_core::mempool::Mempool;
use sth_core::models::{entity, Block, Transaction};
use sth_core::storage::Storage;
use sth_core::sync::{apply_blocks, ChainTip};
use std::sync::Arc;

/// mainnet with aip36 active from genesis
fn network_aip36() -> Network {
    let ms = MAINNET_MILESTONES_JSON.replace("\"aip11\": true", "\"aip11\": true, \"aip36\": true");
    Network::from_json(MAINNET_NETWORK_JSON, &ms, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap()
}

fn tx(keys: &KeyPair, nonce: u64, body: serde_json::Value) -> Transaction {
    let mut v = serde_json::json!({
        "version": 2, "network": 63, "typeGroup": 1, "nonce": nonce.to_string(),
        "senderPublicKey": keys.public_key_hex(), "amount": "0", "expiration": 0
    });
    v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
    serde_json::from_value(v).unwrap()
}

fn entity_tx(keys: &KeyPair, nonce: u64, fee: u64, asset: serde_json::Value) -> Transaction {
    tx(keys, nonce, serde_json::json!({ "typeGroup": 2, "type": 6, "fee": fee.to_string(), "asset": asset }))
}

fn register(keys: &KeyPair, nonce: u64, type_: u8, name: &str) -> Transaction {
    entity_tx(keys, nonce, entity::FEE_REGISTER, serde_json::json!({ "type": type_, "subType": 0, "action": 0, "data": { "name": name, "ipfsData": "QmV1a2b3c4d5e6f7g8h9" } }))
}

fn update(keys: &KeyPair, nonce: u64, type_: u8, reg: &str, ipfs: &str) -> Transaction {
    entity_tx(keys, nonce, entity::FEE_UPDATE, serde_json::json!({ "type": type_, "subType": 0, "action": 1, "registrationId": reg, "data": { "ipfsData": ipfs } }))
}

fn resign(keys: &KeyPair, nonce: u64, type_: u8, reg: &str) -> Transaction {
    entity_tx(keys, nonce, entity::FEE_RESIGN, serde_json::json!({ "type": type_, "subType": 0, "action": 2, "registrationId": reg, "data": {} }))
}

fn sign(mut t: Transaction, keys: &KeyPair) -> Transaction {
    let h = transaction_signing_hash(&t).unwrap();
    t.signature = Some(sign_schnorr_legacy(&h, keys.private_key()).unwrap());
    t.id = Some(transaction_id(&t).unwrap());
    t
}

fn transfer(keys: &KeyPair, nonce: u64, to: &str, amount: u64) -> Transaction {
    tx(keys, nonce, serde_json::json!({ "type": 0, "fee": "10000000", "amount": amount.to_string(), "recipientId": to }))
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
    fn new(network: Network) -> Self {
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
    fn try_block(&mut self, txs: Vec<Transaction>) -> Result<(), String> {
        let b = self.block(txs);
        let r = self.apply(&[b]);
        if r.is_err() {
            self.slot -= 1;
        }
        r
    }
}

#[test]
fn wire_format_matches_core_magistrate() {
    let keys = KeyPair::from_passphrase("alice").unwrap();
    let t = sign(register(&keys, 1, 0, "SmartHoldem"), &keys);
    let bytes = serialize_transaction(&t, SerializeOptions::default(), Network::mainnet_ref()).unwrap();
    assert_eq!(&bytes[..3], &[0xff, 0x02, 0x3f]);
    assert_eq!(&bytes[3..7], &2u32.to_le_bytes());
    assert_eq!(&bytes[7..9], &6u16.to_le_bytes());
    // header: 9 + nonce 8 + pk 33 + fee 8 + vendorField length byte (always written, 0 here)
    assert_eq!(bytes[9 + 8 + 33 + 8], 0);
    let payload = &bytes[9 + 8 + 33 + 8 + 1..];
    assert_eq!(&payload[..4], &[0, 0, 0, 0], "type, subType, action, registrationId length");
    assert_eq!(payload[4], 11);
    assert_eq!(&payload[5..16], b"SmartHoldem");
    assert_eq!(payload[16], 20);
    assert_eq!(&payload[17..37], b"QmV1a2b3c4d5e6f7g8h9");
    assert_eq!(payload.len(), 37 + 64, "payload + first signature");
    let back = deserialize_transaction(&bytes).unwrap();
    assert_eq!(back.entity_asset(), t.entity_asset());
    assert_eq!(back.id, t.id);
}

#[test]
fn rejected_before_aip36_activation() {
    let mut c = Chain::new(Network::mainnet());
    let keys = KeyPair::from_passphrase("alice").unwrap();
    let err = c.try_block(vec![sign(register(&keys, 1, 0, "Early"), &keys)]).unwrap_err();
    assert!(err.contains("before aip36 activation"), "{err}");
    assert!(!Network::mainnet().milestone(11_799_999).aip36);
    assert!(Network::mainnet().milestone(11_800_000).aip36);
}

#[test]
fn entity_lifecycle_rules_state_and_rollback() {
    let mut c = Chain::new(network_aip36());
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let bob = KeyPair::from_passphrase("bob").unwrap();
    let alice_addr = alice.address(63).unwrap();

    // wrong fee → StaticFeeMismatchError
    let bad = entity_tx(&alice, 1, 100, serde_json::json!({ "type": 0, "subType": 0, "action": 0, "data": { "name": "Fee" } }));
    assert!(c.try_block(vec![sign(bad, &alice)]).unwrap_err().contains("StaticFeeMismatchError"));
    // delegate entity by a non-delegate
    assert!(c.try_block(vec![sign(register(&alice, 1, 4, "alice"), &alice)]).unwrap_err().contains("EntitySenderIsNotDelegateError"));

    let reg = sign(register(&alice, 1, 0, "SmartHoldem"), &alice);
    let reg_id = reg.id.clone().unwrap();
    c.try_block(vec![reg]).unwrap();
    let w = c.storage.get_wallet(&alice_addr).unwrap().unwrap();
    assert_eq!(w.entities[&reg_id].data.name.as_deref(), Some("SmartHoldem"));
    assert_eq!(c.storage.entity_by_name("smartholdem", 0).unwrap().as_deref(), Some(reg_id.as_str()));
    assert!(c.storage.entity_by_name("smartholdem", 1).unwrap().is_none(), "uniqueness is per (name, type)");

    // same name, other sender, same type → taken (case-insensitive)
    assert!(c.try_block(vec![sign(register(&bob, 1, 0, "SMARTHOLDEM"), &bob)]).unwrap_err().contains("EntityNameAlreadyRegisteredError"));
    // bob cannot update alice's entity
    assert!(c.try_block(vec![sign(update(&bob, 1, 0, &reg_id, "QmNew"), &bob)]).unwrap_err().contains("EntityNotRegisteredError"));
    // wrong type on update
    assert!(c.try_block(vec![sign(update(&alice, 2, 1, &reg_id, "QmNew"), &alice)]).unwrap_err().contains("EntityWrongTypeError"));

    // update + resign in one batch, then a second resign fails
    let b_upd = c.block(vec![sign(update(&alice, 2, 0, &reg_id, "QmUpdated"), &alice)]);
    c.tip_block = b_upd.clone();
    let b_res = c.block(vec![sign(resign(&alice, 3, 0, &reg_id), &alice)]);
    c.tip_block = c.storage.get_last_block().unwrap().unwrap();
    c.storage.set_undo_enabled(true);
    c.apply(&[b_upd, b_res]).unwrap();
    let (owner, rec) = c.storage.get_entity(&reg_id).unwrap().unwrap();
    assert_eq!(owner, alice_addr);
    assert_eq!(rec.data.ipfs_data.as_deref(), Some("QmUpdated"));
    assert!(rec.resigned);
    assert!(c.try_block(vec![sign(resign(&alice, 4, 0, &reg_id), &alice)]).unwrap_err().contains("EntityAlreadyResignedError"));

    // rollback the resign block → entity active again; rollback the registration → name index gone
    c.storage.rollback_last_block().unwrap();
    assert!(!c.storage.get_entity(&reg_id).unwrap().unwrap().1.resigned);
    c.storage.rollback_last_block().unwrap();
    assert_eq!(c.storage.get_entity(&reg_id).unwrap().unwrap().1.data.ipfs_data.as_deref(), Some("QmV1a2b3c4d5e6f7g8h9"));
    assert_eq!(c.storage.all_entities().unwrap().len(), 1);
}

#[test]
fn register_and_update_in_one_batch() {
    let mut c = Chain::new(network_aip36());
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let reg = sign(register(&alice, 1, 2, "plugin-x"), &alice);
    let reg_id = reg.id.clone().unwrap();
    let b2 = c.block(vec![reg]);
    c.tip_block = b2.clone();
    let b3 = c.block(vec![sign(update(&alice, 2, 2, &reg_id, "QmBatch"), &alice)]);
    c.tip_block = mainnet_block().unwrap();
    c.apply(&[b2, b3]).unwrap();
    assert_eq!(c.storage.get_entity(&reg_id).unwrap().unwrap().1.data.ipfs_data.as_deref(), Some("QmBatch"));
}

#[tokio::test]
async fn mempool_rules() {
    let mut c = Chain::new(network_aip36());
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let bob = KeyPair::from_passphrase("bob").unwrap();
    let funder = KeyPair::from_passphrase("funder").unwrap();
    let b = c.block(vec![
        sign(transfer(&funder, 1, &alice.address(63).unwrap(), 100_000_000_000), &funder),
        sign(transfer(&funder, 2, &bob.address(63).unwrap(), 100_000_000_000), &funder),
    ]);
    c.apply(&[b]).unwrap();
    let pool = Mempool::new(c.storage.clone(), vec![], 100);
    let (_, errors) = pool.add_many(vec![sign(register(&alice, 1, 0, "Dupe"), &alice)]).await;
    assert!(errors.is_empty(), "{errors:?}");
    let (_, errors) = pool.add_many(vec![sign(register(&bob, 1, 0, "dupe"), &bob)]).await;
    assert_eq!(errors.values().next().unwrap().type_, "ERR_PENDING", "{errors:?}");
    let (_, errors) = pool.add_many(vec![sign(register(&bob, 1, 1, "dupe"), &bob)]).await;
    assert!(errors.is_empty(), "other type is fine: {errors:?}");

    // before activation the pool refuses entities
    let c2 = Chain::new(Network::mainnet());
    let pool2 = Mempool::new(c2.storage.clone(), vec![], 100);
    let (_, errors) = pool2.add_many(vec![sign(register(&alice, 1, 0, "Early"), &alice)]).await;
    assert!(errors.values().next().unwrap().message.contains("aip36"), "{errors:?}");
}
