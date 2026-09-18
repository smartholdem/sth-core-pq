//! Author: TechnoL0g
//!
//! SmartObject (sObject, typeGroup 2 / type 6) transactions: wire format, activation gate, legacy handler rules, state, rollback, mempool, API storage queries.

use sth_core::config::{Network, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ, MAINNET_MILESTONES_JSON, MAINNET_NETWORK_JSON};
use sth_core::crypto::{deserialize_transaction, serialize_transaction, sign_schnorr_legacy, transaction_id, transaction_signing_hash, KeyPair, SerializeOptions};
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::genesis::mainnet_block;
use sth_core::mempool::Mempool;
use sth_core::models::{sobj, Block, Transaction};
use sth_core::storage::Storage;
use sth_core::sync::{apply_blocks, ChainTip};
use std::sync::Arc;

/// mainnet with aip36 active from genesis
fn network_aip36() -> Network {
    let ms = MAINNET_MILESTONES_JSON.replace("\"ship11\": true", "\"ship11\": true, \"aip36\": true");
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

fn sobj_tx(keys: &KeyPair, nonce: u64, fee: u64, asset: serde_json::Value) -> Transaction {
    tx(keys, nonce, serde_json::json!({ "typeGroup": 2, "type": 6, "fee": fee.to_string(), "asset": asset }))
}

fn register(keys: &KeyPair, nonce: u64, type_: u8, name: &str) -> Transaction {
    sobj_tx(keys, nonce, sobj::FEE_REGISTER, serde_json::json!({ "type": type_, "subType": 0, "action": 0, "data": { "name": name, "ipfsData": "QmV1a2b3c4d5e6f7g8h9" } }))
}

fn update(keys: &KeyPair, nonce: u64, type_: u8, reg: &str, ipfs: &str) -> Transaction {
    sobj_tx(keys, nonce, sobj::FEE_UPDATE, serde_json::json!({ "type": type_, "subType": 0, "action": 1, "registrationId": reg, "data": { "ipfsData": ipfs } }))
}

fn resign(keys: &KeyPair, nonce: u64, type_: u8, reg: &str) -> Transaction {
    sobj_tx(keys, nonce, sobj::FEE_RESIGN, serde_json::json!({ "type": type_, "subType": 0, "action": 2, "registrationId": reg, "data": {} }))
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
    assert_eq!(back.sobj_asset(), t.sobj_asset());
    assert_eq!(back.id, t.id);
}

#[test]
fn rejected_before_aip36_activation() {
    let mut c = Chain::new(Network::mainnet());
    let keys = KeyPair::from_passphrase("alice").unwrap();
    let err = c.try_block(vec![sign(register(&keys, 1, 0, "Early"), &keys)]).unwrap_err();
    assert!(err.contains("before sobj (ship13) activation"), "{err}");
    assert!(!Network::mainnet().milestone(11_799_999).sobj_active);
    assert!(Network::mainnet().milestone(11_800_000).sobj_active);
}

#[test]
fn sobj_lifecycle_rules_state_and_rollback() {
    let mut c = Chain::new(network_aip36());
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let bob = KeyPair::from_passphrase("bob").unwrap();
    let alice_addr = alice.address(63).unwrap();

    // wrong fee → StaticFeeMismatchError
    let bad = sobj_tx(&alice, 1, 100, serde_json::json!({ "type": 0, "subType": 0, "action": 0, "data": { "name": "Fee" } }));
    assert!(c.try_block(vec![sign(bad, &alice)]).unwrap_err().contains("StaticFeeMismatchError"));
    // delegate sObject by a non-delegate
    assert!(c.try_block(vec![sign(register(&alice, 1, 4, "alice"), &alice)]).unwrap_err().contains("SmartObjectSenderIsNotDelegateError"));

    let reg = sign(register(&alice, 1, 0, "SmartHoldem"), &alice);
    let reg_id = reg.id.clone().unwrap();
    c.try_block(vec![reg]).unwrap();
    let w = c.storage.get_wallet(&alice_addr).unwrap().unwrap();
    assert_eq!(w.sobjects[&reg_id].data.name.as_deref(), Some("SmartHoldem"));
    assert_eq!(c.storage.sobj_by_name("smartholdem", 0).unwrap().as_deref(), Some(reg_id.as_str()));
    assert!(c.storage.sobj_by_name("smartholdem", 1).unwrap().is_none(), "uniqueness is per (name, type)");

    // same name, other sender, same type → taken (case-insensitive)
    assert!(c.try_block(vec![sign(register(&bob, 1, 0, "SMARTHOLDEM"), &bob)]).unwrap_err().contains("SmartObjectNameAlreadyRegisteredError"));
    // bob cannot update alice's sObject
    assert!(c.try_block(vec![sign(update(&bob, 1, 0, &reg_id, "QmNew"), &bob)]).unwrap_err().contains("SmartObjectNotRegisteredError"));
    // wrong type on update
    assert!(c.try_block(vec![sign(update(&alice, 2, 1, &reg_id, "QmNew"), &alice)]).unwrap_err().contains("SmartObjectWrongTypeError"));

    // update + resign in one batch, then a second resign fails
    let b_upd = c.block(vec![sign(update(&alice, 2, 0, &reg_id, "QmUpdated"), &alice)]);
    c.tip_block = b_upd.clone();
    let b_res = c.block(vec![sign(resign(&alice, 3, 0, &reg_id), &alice)]);
    c.tip_block = c.storage.get_last_block().unwrap().unwrap();
    c.storage.set_undo_enabled(true);
    c.apply(&[b_upd, b_res]).unwrap();
    let (owner, rec) = c.storage.get_sobject(&reg_id).unwrap().unwrap();
    assert_eq!(owner, alice_addr);
    assert_eq!(rec.data.ntfry_data.as_deref(), Some("QmUpdated"));
    assert!(rec.resigned);
    assert!(c.try_block(vec![sign(resign(&alice, 4, 0, &reg_id), &alice)]).unwrap_err().contains("SmartObjectAlreadyResignedError"));

    // rollback the resign block → sObject active again; rollback the registration → name index gone
    c.storage.rollback_last_block().unwrap();
    assert!(!c.storage.get_sobject(&reg_id).unwrap().unwrap().1.resigned);
    c.storage.rollback_last_block().unwrap();
    assert_eq!(c.storage.get_sobject(&reg_id).unwrap().unwrap().1.data.ntfry_data.as_deref(), Some("QmV1a2b3c4d5e6f7g8h9"));
    assert_eq!(c.storage.all_sobjects().unwrap().len(), 1);
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
    assert_eq!(c.storage.get_sobject(&reg_id).unwrap().unwrap().1.data.ntfry_data.as_deref(), Some("QmBatch"));
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

    // before activation the pool refuses sObjects
    let c2 = Chain::new(Network::mainnet());
    let pool2 = Mempool::new(c2.storage.clone(), vec![], 100);
    let (_, errors) = pool2.add_many(vec![sign(register(&alice, 1, 0, "Early"), &alice)]).await;
    assert!(errors.values().next().unwrap().message.contains("ship13"), "{errors:?}");
}

/// Milestone `sobjV2`: free-form `ntfryData` (≤ 255 bytes, no base58 requirement) and type-5 names must be tickers.
#[test]
fn sobj_v2_rules_and_ntfry_data_alias() {
    let ms = MAINNET_MILESTONES_JSON.replace("\"ship11\": true", "\"ship11\": true, \"aip36\": true, \"sobjV2\": true");
    let net = Network::from_json(MAINNET_NETWORK_JSON, &ms, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap();
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let mut c = Chain::new(net);
    c.storage.update_wallet_state(&alice.address(63).unwrap(), 100_000_000_000_000, 0).unwrap();

    // legacy JSON key still parses; the canonical name is ntfryData
    let t = register(&alice, 1, 1, "shop");
    let e = t.sobj_asset().unwrap();
    assert_eq!(e.data.ntfry_data.as_deref(), Some("QmV1a2b3c4d5e6f7g8h9"));
    assert!(serde_json::to_string(&e).unwrap().contains("\"ntfryData\""));

    // v2: a URL / JSON pointer is fine (not base58), up to 255 bytes; control characters and > 255 are not
    let url = sign(sobj_tx(&alice, 1, sobj::FEE_REGISTER, serde_json::json!({ "type": 1, "subType": 0, "action": 0, "data": { "name": "shop", "ntfryData": "sth://shop/manifest.json?v=1" } })), &alice);
    c.try_block(vec![url.clone()]).unwrap();
    assert_eq!(c.storage.get_sobject(url.id.as_deref().unwrap()).unwrap().unwrap().1.data.ntfry_data.as_deref(), Some("sth://shop/manifest.json?v=1"));
    let max = sign(sobj_tx(&alice, 2, sobj::FEE_REGISTER, serde_json::json!({ "type": 1, "subType": 0, "action": 0, "data": { "name": "shop2", "ntfryData": "x".repeat(255) } })), &alice);
    c.try_block(vec![max]).unwrap();
    let ctrl = sign(sobj_tx(&alice, 3, sobj::FEE_REGISTER, serde_json::json!({ "type": 1, "subType": 0, "action": 0, "data": { "name": "shop2", "ntfryData": "a\nb" } })), &alice);
    assert!(c.try_block(vec![ctrl]).unwrap_err().contains("invalid ntfryData"));

    // type 5 = ticker: enforced by the network in v2
    let e = c.try_block(vec![sign(register(&alice, 3, 5, "coffee"), &alice)]).unwrap_err();
    assert!(e.contains("SmartObjectTickerInvalidError"), "{e}");
    let e = c.try_block(vec![sign(register(&alice, 3, 5, "TOOLONGTICKER"), &alice)]).unwrap_err();
    assert!(e.contains("SmartObjectTickerInvalidError"), "{e}");
    c.try_block(vec![sign(register(&alice, 3, 5, "COFFEE"), &alice)]).unwrap();
    // other types keep the free 1–40 char names
    c.try_block(vec![sign(register(&alice, 4, 1, "my-shop.v2"), &alice)]).unwrap();

    // legacy rules (sobjV2 off): lowercase type-5 name is accepted, non-base58 data is not
    let mut legacy = Chain::new(network_aip36());
    legacy.storage.update_wallet_state(&alice.address(63).unwrap(), 100_000_000_000_000, 0).unwrap();
    legacy.try_block(vec![sign(register(&alice, 1, 5, "coffee"), &alice)]).unwrap();
    let bad = sign(sobj_tx(&alice, 2, sobj::FEE_REGISTER, serde_json::json!({ "type": 1, "subType": 0, "action": 0, "data": { "name": "shop", "ntfryData": "sth://shop" } })), &alice);
    assert!(legacy.try_block(vec![bad]).unwrap_err().contains("invalid ntfryData"));
}

/// sobjV2 transfer: sObject + token registry move to the new owner; old owner loses control; rollback restores; A→B→C in one block.
#[test]
fn sobj_transfer_moves_object_and_token() {
    use sth_core::models::token;
    let ms = MAINNET_MILESTONES_JSON.replace("\"ship11\": true", "\"ship11\": true, \"aip36\": true, \"sobjV2\": true, \"tokens\": true");
    let net = Network::from_json(MAINNET_NETWORK_JSON, &ms, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap();
    let (alice, bob, carol) = (KeyPair::from_passphrase("alice").unwrap(), KeyPair::from_passphrase("bob").unwrap(), KeyPair::from_passphrase("carol").unwrap());
    let (a, b, c_addr) = (alice.address(63).unwrap(), bob.address(63).unwrap(), carol.address(63).unwrap());
    let mut c = Chain::new(net);
    for w in [&a, &b, &c_addr] {
        c.storage.update_wallet_state(w, 100_000_000_000_000, 0).unwrap();
    }
    let transfer_tx = |keys: &KeyPair, nonce: u64, reg: &str, to: &str| sign(sobj_tx(keys, nonce, sobj::FEE_TRANSFER, serde_json::json!({ "type": 5, "subType": 0, "action": 3, "registrationId": reg, "recipientId": to, "data": {} })), keys);

    let reg = sign(register(&alice, 1, 5, "COFFEE"), &alice);
    c.try_block(vec![reg.clone()]).unwrap();
    let id = reg.id.clone().unwrap();
    let init = sign(tx(&alice, 2, serde_json::json!({ "typeGroup": 3, "type": token::INIT, "fee": "50000000000", "asset": { "token": { "id": id, "decimals": 0, "flags": 3, "initialSupply": "1000", "supplyCap": "5000" } } })), &alice);
    c.try_block(vec![init]).unwrap();

    // wire round-trip keeps recipientId (travels in the ntfryData slot)
    let t = transfer_tx(&alice, 3, &id, &b);
    let back = deserialize_transaction(&serialize_transaction(&t, SerializeOptions::default(), Network::mainnet_ref()).unwrap()).unwrap();
    assert_eq!(back.sobj_asset().unwrap().recipient_id.as_deref(), Some(b.as_str()));
    assert_eq!(back.sobj_asset().unwrap().data.ntfry_data, None);
    assert_eq!(back.id, t.id);

    // rules: self, non-owner, delegate sObject, wrong fee
    let e = c.try_block(vec![transfer_tx(&alice, 3, &id, &a)]).unwrap_err();
    assert!(e.contains("SmartObjectTransferToSelfError"), "{e}");
    let e = c.try_block(vec![transfer_tx(&bob, 1, &id, &c_addr)]).unwrap_err();
    assert!(e.contains("SmartObjectNotRegisteredError"), "{e}");
    let e = c.try_block(vec![sign(sobj_tx(&alice, 3, sobj::FEE_UPDATE + 1, serde_json::json!({ "type": 5, "subType": 0, "action": 3, "registrationId": id, "recipientId": b, "data": {} })), &alice)]).unwrap_err();
    assert!(e.contains("StaticFeeMismatchError"), "{e}");
    let e = c.try_block(vec![sign(sobj_tx(&alice, 3, sobj::FEE_TRANSFER, serde_json::json!({ "type": 5, "subType": 0, "action": 1, "registrationId": id, "recipientId": b, "data": {} })), &alice)]).unwrap_err();
    assert!(e.contains("recipientId is only valid for transfer"), "{e}");

    // transfer A→B: sObject + token registry (owner, supply) move; balances stay where they are
    c.storage.set_undo_enabled(true);
    c.try_block(vec![transfer_tx(&alice, 3, &id, &b)]).unwrap();
    let (owner, rec) = c.storage.get_sobject(&id).unwrap().unwrap();
    assert_eq!((owner.as_str(), rec.data.name.as_deref()), (b.as_str(), Some("COFFEE")));
    assert!(c.storage.get_wallet(&a).unwrap().unwrap().sobjects.get(&id).is_none());
    assert!(c.storage.get_wallet(&a).unwrap().unwrap().tokens_issued.get(&id).is_none());
    let st = c.storage.token_state(&id).unwrap().unwrap();
    assert_eq!((st.owner.as_str(), st.supply, st.symbol.as_str()), (b.as_str(), 1000, "COFFEE"));
    assert_eq!(c.storage.token_owner(&id).unwrap().as_deref(), Some(b.as_str()));
    assert_eq!(c.storage.get_wallet(&a).unwrap().unwrap().tokens.get(&id), Some(&1000), "alice keeps her token balance");
    // old owner can no longer mint / update; new owner can
    let e = c.try_block(vec![sign(tx(&alice, 4, serde_json::json!({ "typeGroup": 3, "type": token::MINT, "fee": "100000000", "asset": { "token": { "id": id, "amount": "10", "recipientId": a } } })), &alice)]).unwrap_err();
    assert!(e.contains("TokenNotOwnerError"), "{e}");
    let e = c.try_block(vec![sign(update(&alice, 4, 5, &id, "QmX"), &alice)]).unwrap_err();
    assert!(e.contains("SmartObjectNotRegisteredError"), "{e}");
    c.try_block(vec![sign(tx(&bob, 1, serde_json::json!({ "typeGroup": 3, "type": token::MINT, "fee": "100000000", "asset": { "token": { "id": id, "amount": "10", "recipientId": b } } })), &bob)]).unwrap();
    assert_eq!(c.storage.token_state(&id).unwrap().unwrap().supply, 1010);

    // rollback both blocks → alice owns again
    c.storage.rollback_last_block().unwrap();
    c.storage.rollback_last_block().unwrap();
    assert_eq!(c.storage.get_sobject(&id).unwrap().unwrap().0, a);
    assert_eq!(c.storage.token_owner(&id).unwrap().as_deref(), Some(a.as_str()));
    assert_eq!(c.storage.token_state(&id).unwrap().unwrap().owner, a);
    assert_eq!(c.storage.token_state(&id).unwrap().unwrap().supply, 1000);
    assert!(c.storage.get_wallet(&b).unwrap().unwrap().sobjects.get(&id).is_none());

    // A→B and B→C in one block; then rollback → A
    c.try_block(vec![transfer_tx(&alice, 3, &id, &b), transfer_tx(&bob, 1, &id, &c_addr)]).unwrap();
    assert_eq!(c.storage.get_sobject(&id).unwrap().unwrap().0, c_addr);
    assert_eq!(c.storage.token_state(&id).unwrap().unwrap().owner, c_addr);
    // and alice acting after handing over inside the same block is rejected
    let e = c.try_block(vec![transfer_tx(&carol, 1, &id, &a), transfer_tx(&carol, 2, &id, &b)]).unwrap_err();
    assert!(e.contains("SmartObjectNotRegisteredError"), "{e}");
    c.storage.rollback_last_block().unwrap();
    assert_eq!(c.storage.get_sobject(&id).unwrap().unwrap().0, a);
    assert_eq!(c.storage.token_owner(&id).unwrap().as_deref(), Some(a.as_str()));

    // legacy rules (sobjV2 off): action 3 is refused
    let mut legacy = Chain::new(network_aip36());
    legacy.storage.update_wallet_state(&a, 100_000_000_000_000, 0).unwrap();
    let reg = sign(register(&alice, 1, 1, "shop"), &alice);
    legacy.try_block(vec![reg.clone()]).unwrap();
    let e = legacy.try_block(vec![sign(sobj_tx(&alice, 2, sobj::FEE_TRANSFER, serde_json::json!({ "type": 1, "subType": 0, "action": 3, "registrationId": reg.id.clone().unwrap(), "recipientId": b, "data": {} })), &alice)]).unwrap_err();
    assert!(e.contains("SmartObjectTransferNotActiveError"), "{e}");
}

/// sobjV2 market: sell order (price), cancel with 0, buy pays the owner and moves sObject + token registry; rollback restores.
#[test]
fn sobj_sell_and_buy() {
    use sth_core::models::token;
    let ms = MAINNET_MILESTONES_JSON.replace("\"ship11\": true", "\"ship11\": true, \"aip36\": true, \"sobjV2\": true, \"tokens\": true");
    let net = Network::from_json(MAINNET_NETWORK_JSON, &ms, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap();
    let (alice, bob, carol) = (KeyPair::from_passphrase("alice").unwrap(), KeyPair::from_passphrase("bob").unwrap(), KeyPair::from_passphrase("carol").unwrap());
    let (a, b, c_addr) = (alice.address(63).unwrap(), bob.address(63).unwrap(), carol.address(63).unwrap());
    let mut c = Chain::new(net);
    c.storage.update_wallet_state(&a, 100_000_000_000_000, 0).unwrap();
    c.storage.update_wallet_state(&b, 200_100_000_000, 0).unwrap(); // 2 001 coins
    c.storage.update_wallet_state(&c_addr, 100_000_000_000, 0).unwrap(); // 1 000 coins
    let bal = |c: &Chain, w: &str| c.storage.get_wallet(w).unwrap().unwrap().balance;
    let sell = |keys: &KeyPair, nonce: u64, reg: &str, price: u64| sign(sobj_tx(keys, nonce, sobj::FEE_SELL, serde_json::json!({ "type": 5, "subType": 0, "action": 4, "registrationId": reg, "price": price.to_string(), "data": {} })), keys);
    let buy = |keys: &KeyPair, nonce: u64, reg: &str| sign(sobj_tx(keys, nonce, sobj::FEE_BUY, serde_json::json!({ "type": 5, "subType": 0, "action": 5, "registrationId": reg, "data": {} })), keys);

    let reg = sign(register(&alice, 1, 5, "COFFEE"), &alice);
    c.try_block(vec![reg.clone()]).unwrap();
    let id = reg.id.clone().unwrap();
    c.try_block(vec![sign(tx(&alice, 2, serde_json::json!({ "typeGroup": 3, "type": token::INIT, "fee": "50000000000", "asset": { "token": { "id": id, "decimals": 0, "flags": 3, "initialSupply": "1000", "supplyCap": "5000" } } })), &alice)]).unwrap();

    // wire: price travels in the slot
    let t = sell(&alice, 3, &id, 200_000_000_000);
    let back = deserialize_transaction(&serialize_transaction(&t, SerializeOptions::default(), Network::mainnet_ref()).unwrap()).unwrap();
    assert_eq!((back.sobj_asset().unwrap().price, back.id), (Some(200_000_000_000), t.id));

    // no order yet → buy fails; non-owner cannot sell
    let e = c.try_block(vec![buy(&bob, 1, &id)]).unwrap_err();
    assert!(e.contains("SmartObjectNotForSaleError"), "{e}");
    let e = c.try_block(vec![sell(&bob, 1, &id, 5)]).unwrap_err();
    assert!(e.contains("SmartObjectNotRegisteredError"), "{e}");

    // order for 2 000 coins, visible in the record and in the market index; carol (1 000 coins) cannot afford it; alice cannot buy her own
    assert_eq!(c.storage.market_orders(0, 100).unwrap().1, 0);
    c.try_block(vec![sell(&alice, 3, &id, 200_000_000_000)]).unwrap();
    assert_eq!(c.storage.get_sobject(&id).unwrap().unwrap().1.price, Some(200_000_000_000));
    let (orders, total) = c.storage.market_orders(0, 100).unwrap();
    assert_eq!((total, orders[0].0.as_str(), orders[0].1.as_str(), orders[0].2.price), (1, id.as_str(), a.as_str(), Some(200_000_000_000)));
    assert_eq!(c.storage.market_orders(1, 100).unwrap().0.len(), 0, "offset past the end");
    let e = c.try_block(vec![buy(&carol, 1, &id)]).unwrap_err();
    assert!(e.contains("SmartObjectInsufficientBalanceError"), "{e}");
    let e = c.try_block(vec![buy(&alice, 4, &id)]).unwrap_err();
    assert!(e.contains("SmartObjectTransferToSelfError"), "{e}");

    // cancel with 0, buy fails again, re-list
    c.try_block(vec![sell(&alice, 4, &id, 0)]).unwrap();
    assert_eq!(c.storage.get_sobject(&id).unwrap().unwrap().1.price, None);
    assert_eq!(c.storage.market_orders(0, 100).unwrap().1, 0, "cancel clears the market index");
    assert!(c.try_block(vec![buy(&bob, 1, &id)]).unwrap_err().contains("SmartObjectNotForSaleError"));
    c.try_block(vec![sell(&alice, 5, &id, 200_000_000_000)]).unwrap();

    // bob buys: pays 2 000 + 1 fee, alice receives 2 000, sObject + token registry move, order closed
    c.storage.set_undo_enabled(true);
    let (a0, b0) = (bal(&c, &a), bal(&c, &b));
    c.try_block(vec![buy(&bob, 1, &id)]).unwrap();
    assert_eq!(bal(&c, &b), b0 - 200_000_000_000 - sobj::FEE_BUY as i64);
    assert_eq!(bal(&c, &a), a0 + 200_000_000_000);
    let (owner, rec) = c.storage.get_sobject(&id).unwrap().unwrap();
    assert_eq!((owner.as_str(), rec.price, rec.data.name.as_deref()), (b.as_str(), None, Some("COFFEE")));
    assert_eq!(c.storage.market_orders(0, 100).unwrap().1, 0, "purchase closes the order");
    assert_eq!(c.storage.token_state(&id).unwrap().unwrap().owner, b);
    assert_eq!(c.storage.token_owner(&id).unwrap().as_deref(), Some(b.as_str()));
    assert!(c.storage.get_wallet(&a).unwrap().unwrap().sobjects.get(&id).is_none());
    assert_eq!(c.storage.get_wallet(&a).unwrap().unwrap().tokens.get(&id), Some(&1000), "alice keeps her balance");
    // bob now mints; alice cannot
    c.try_block(vec![sign(tx(&bob, 2, serde_json::json!({ "typeGroup": 3, "type": token::MINT, "fee": "100000000", "asset": { "token": { "id": id, "amount": "5", "recipientId": b } } })), &bob)]).unwrap();
    assert!(c.try_block(vec![sell(&alice, 6, &id, 1)]).unwrap_err().contains("SmartObjectNotRegisteredError"));

    // rollback mint + purchase → alice owns, order open again, balances back
    c.storage.rollback_last_block().unwrap();
    c.storage.rollback_last_block().unwrap();
    assert_eq!((bal(&c, &a), bal(&c, &b)), (a0, b0));
    let (owner, rec) = c.storage.get_sobject(&id).unwrap().unwrap();
    assert_eq!((owner.as_str(), rec.price), (a.as_str(), Some(200_000_000_000)));
    assert_eq!(c.storage.token_owner(&id).unwrap().as_deref(), Some(a.as_str()));
    assert_eq!(c.storage.token_state(&id).unwrap().unwrap().supply, 1000);
    let (orders, total) = c.storage.market_orders(0, 100).unwrap();
    assert_eq!((total, orders[0].1.as_str()), (1, a.as_str()), "rollback re-opens the order under the previous owner");

    // sell + buy in one block, buyer immediately re-lists: order belongs to the new owner
    c.try_block(vec![sell(&alice, 6, &id, 100_000_000_000), buy(&bob, 1, &id), sell(&bob, 2, &id, 300_000_000_000)]).unwrap();
    let (owner, rec) = c.storage.get_sobject(&id).unwrap().unwrap();
    assert_eq!((owner.as_str(), rec.price), (b.as_str(), Some(300_000_000_000)));
    let (orders, total) = c.storage.market_orders(0, 100).unwrap();
    assert_eq!((total, orders[0].1.as_str(), orders[0].2.price), (1, b.as_str(), Some(300_000_000_000)));

    // funds that arrive earlier in the same block count for the purchase (batch-aware balance)
    let pay = sign(tx(&bob, 3, serde_json::json!({ "typeGroup": 1, "type": 0, "fee": "10000000", "amount": "250000000000", "recipientId": c_addr })), &bob);
    c.try_block(vec![pay, buy(&carol, 1, &id)]).unwrap();
    assert_eq!(c.storage.get_sobject(&id).unwrap().unwrap().0, c_addr);
    assert_eq!(bal(&c, &c_addr), 100_000_000_000 + 250_000_000_000 - 300_000_000_000 - sobj::FEE_BUY as i64);

    // legacy rules: sell / buy refused
    let mut legacy = Chain::new(network_aip36());
    legacy.storage.update_wallet_state(&a, 100_000_000_000_000, 0).unwrap();
    let reg = sign(register(&alice, 1, 1, "shop"), &alice);
    legacy.try_block(vec![reg.clone()]).unwrap();
    let e = legacy.try_block(vec![sign(sobj_tx(&alice, 2, sobj::FEE_SELL, serde_json::json!({ "type": 1, "subType": 0, "action": 4, "registrationId": reg.id.clone().unwrap(), "price": "1", "data": {} })), &alice)]).unwrap_err();
    assert!(e.contains("SmartObjectTransferNotActiveError"), "{e}");
}

/// Resign guard: a type-5 registry whose token has live supply cannot be resigned (block rules and mempool); burning the whole
/// supply unlocks the resign. Objects without a token resign as before.
#[tokio::test]
async fn resign_guard_protects_live_token_registry() {
    use sth_core::models::token;
    let ms = MAINNET_MILESTONES_JSON.replace("\"ship11\": true", "\"ship11\": true, \"aip36\": true, \"sobjV2\": true, \"tokens\": true");
    let net = Network::from_json(MAINNET_NETWORK_JSON, &ms, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap();
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let a = alice.address(63).unwrap();
    let mut c = Chain::new(net.clone());
    c.storage.update_wallet_state(&a, 100_000_000_000_000, 0).unwrap();

    let (reg, plain) = (sign(register(&alice, 1, 5, "COFFEE"), &alice), sign(register(&alice, 2, 5, "BEANS"), &alice));
    c.try_block(vec![reg.clone(), plain.clone()]).unwrap();
    let id = reg.id.clone().unwrap();
    let init = sign(tx(&alice, 3, serde_json::json!({ "typeGroup": 3, "type": token::INIT, "fee": "50000000000", "asset": { "token": { "id": id, "decimals": 0, "flags": 2, "initialSupply": "10", "supplyCap": "10" } } })), &alice);
    c.try_block(vec![init]).unwrap();

    // live supply → refused in a block and in the pool
    let e = c.try_block(vec![sign(resign(&alice, 4, 5, &id), &alice)]).unwrap_err();
    assert!(e.contains("TokenSmartObjectStillActiveError"), "{e}");
    let pool = Mempool::new(c.storage.clone(), vec![], 100);
    let (_, errors) = pool.add_many(vec![sign(resign(&alice, 4, 5, &id), &alice)]).await;
    assert!(errors.values().next().is_some_and(|e| e.message.contains("TokenSmartObjectStillActiveError")), "{errors:?}");

    // a registry without a token resigns as before
    c.try_block(vec![sign(resign(&alice, 4, 5, plain.id.as_deref().unwrap()), &alice)]).unwrap();

    // burn everything → resign allowed
    let burn = sign(tx(&alice, 5, serde_json::json!({ "typeGroup": 3, "type": token::BURN, "fee": "10000000", "asset": { "token": { "id": id, "amount": "10" } } })), &alice);
    c.try_block(vec![burn]).unwrap();
    c.try_block(vec![sign(resign(&alice, 6, 5, &id), &alice)]).unwrap();
    assert!(c.storage.get_sobject(&id).unwrap().unwrap().1.resigned);
}

/// Pool spend: an sObject buy waiting in the pool reserves its price, so a later transfer of the same sender that no longer fits
/// is refused with ERR_LOW_BALANCE (previously only amount + fee of the buy were counted).
#[tokio::test]
async fn pool_counts_buy_price_for_later_transactions() {
    let ms = MAINNET_MILESTONES_JSON.replace("\"ship11\": true", "\"ship11\": true, \"aip36\": true, \"sobjV2\": true");
    let net = Network::from_json(MAINNET_NETWORK_JSON, &ms, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap();
    let (alice, bob) = (KeyPair::from_passphrase("alice").unwrap(), KeyPair::from_passphrase("bob").unwrap());
    let (a, b) = (alice.address(63).unwrap(), bob.address(63).unwrap());
    let mut c = Chain::new(net);
    c.storage.update_wallet_state(&a, 100_000_000_000_000, 0).unwrap();
    c.storage.update_wallet_state(&b, 200_100_000_000 + 10_000_000, 0).unwrap(); // 2 001 coins + one transfer fee
    let reg = sign(register(&alice, 1, 1, "shop"), &alice);
    c.try_block(vec![reg.clone()]).unwrap();
    let id = reg.id.clone().unwrap();
    c.try_block(vec![sign(sobj_tx(&alice, 2, sobj::FEE_SELL, serde_json::json!({ "type": 1, "subType": 0, "action": 4, "registrationId": id, "price": "200000000000", "data": {} })), &alice)]).unwrap();

    let pool = Mempool::new(c.storage.clone(), vec![], 100);
    let buy = sign(sobj_tx(&bob, 1, sobj::FEE_BUY, serde_json::json!({ "type": 1, "subType": 0, "action": 5, "registrationId": id, "data": {} })), &bob);
    let (_, errors) = pool.add_many(vec![buy]).await;
    assert!(errors.is_empty(), "{errors:?}");
    // 2 001 coins are reserved by the buy: 1 coin more than the remaining 0.1 does not fit
    let (_, errors) = pool.add_many(vec![sign(transfer(&bob, 2, &a, 100_000_000), &bob)]).await;
    assert_eq!(errors.values().next().map(|e| e.type_.as_str()), Some("ERR_LOW_BALANCE"), "{errors:?}");
    // a transfer that only spends the fee still goes through
    let (_, errors) = pool.add_many(vec![sign(transfer(&bob, 2, &a, 0), &bob)]).await;
    assert!(errors.is_empty(), "{errors:?}");
}
