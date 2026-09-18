//! Author: TechnoL0g
//!
//! Quantum Shield stage B: version-3 transactions (ML-DSA-44 second-signature blocks) — wire format, activation gate,
//! registration / migration / rotation, PQ-locked spending, commitment grace window, fee surcharge, mempool codes, rollback.

use sth_core::config::{Network, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ, MAINNET_MILESTONES_JSON, MAINNET_NETWORK_JSON};
use sth_core::crypto::pq::{self, PqKeyPair};
use sth_core::crypto::{
    deserialize_transaction, serialize_transaction, sign_schnorr_legacy, transaction_id, transaction_pq_message, transaction_signing_hash, KeyPair,
    SerializeOptions,
};
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::genesis::mainnet_block;
use sth_core::mempool::Mempool;
use sth_core::models::{Block, PqSignatureBlock, Transaction};
use sth_core::storage::Storage;
use sth_core::sync::{apply_blocks, ChainTip};
use std::sync::Arc;

const FEE_PER_BYTE: u64 = 10_000;
const SURCHARGE_PQ: u64 = FEE_PER_BYTE * (3 + pq::SIG_LEN as u64);
const SURCHARGE_LEGACY: u64 = FEE_PER_BYTE * (3 + 64);

/// mainnet rules with pq active from height `at` (grace 3 blocks so the commitment window can be crossed in a test).
fn network_pq(at: u64) -> Network {
    let mut list: Vec<serde_json::Value> = serde_json::from_str(MAINNET_MILESTONES_JSON).unwrap();
    list[0]["aip11"] = serde_json::json!(true);
    list.push(serde_json::json!({ "height": at, "pq": { "active": true, "feePerByte": FEE_PER_BYTE, "commitmentGrace": 3 } }));
    Network::from_json(MAINNET_NETWORK_JSON, &serde_json::to_string(&list).unwrap(), MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap()
}

fn tx(keys: &KeyPair, nonce: u64, version: u8, body: serde_json::Value) -> Transaction {
    let mut v = serde_json::json!({ "version": version, "network": 63, "typeGroup": 1, "nonce": nonce.to_string(), "senderPublicKey": keys.public_key_hex(), "amount": "0", "expiration": 0 });
    v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
    serde_json::from_value(v).unwrap()
}

fn transfer(keys: &KeyPair, nonce: u64, version: u8, to: &str, fee: u64) -> Transaction {
    tx(keys, nonce, version, serde_json::json!({ "type": 0, "fee": fee.to_string(), "amount": "100000000", "recipientId": to, "vendorField": "pq" }))
}

fn pq_registration(keys: &KeyPair, nonce: u64, new: &PqKeyPair, fee: u64) -> Transaction {
    tx(keys, nonce, 3, serde_json::json!({ "type": 1, "fee": fee.to_string(), "asset": { "signature": { "algorithm": 1, "publicKey": new.public_key_hex() } } }))
}

enum Second<'a> {
    Legacy(&'a KeyPair),
    Pq(&'a PqKeyPair),
}

/// First signature, then v3 blocks in the given order (or a legacy v2 second signature).
fn sign(mut t: Transaction, keys: &KeyPair, blocks: &[Second<'_>]) -> Transaction {
    let h = transaction_signing_hash(&t).unwrap();
    t.signature = Some(sign_schnorr_legacy(&h, keys.private_key()).unwrap());
    let m2 = transaction_pq_message(&t).unwrap();
    if t.version == 3 {
        let v: Vec<PqSignatureBlock> = blocks
            .iter()
            .map(|b| match b {
                Second::Legacy(k) => PqSignatureBlock { algorithm: 0, signature: sign_schnorr_legacy(&m2, k.private_key()).unwrap() },
                Second::Pq(k) => PqSignatureBlock { algorithm: 1, signature: hex::encode(k.sign(&m2).unwrap()) },
            })
            .collect();
        t.second_signatures = (!v.is_empty()).then_some(v);
    } else if let Some(Second::Legacy(k)) = blocks.first() {
        t.second_signature = Some(sign_schnorr_legacy(&m2, k.private_key()).unwrap());
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
    fn new(network: Network) -> Self {
        let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
        sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
        storage.set_undo_enabled(true);
        let genesis = mainnet_block().unwrap();
        Self { network, storage, forger: KeyPair::from_passphrase("forger").unwrap(), tip_block: genesis.clone(), tip: ChainTip { height: 1, id: genesis.id.clone() }, slot: 1 }
    }
    fn try_block(&mut self, txs: Vec<Transaction>) -> Result<(), String> {
        self.slot += 1;
        let b = forge_block(&self.network, &self.forger, &self.tip_block, slot_start(self.slot, 8), txs).unwrap();
        match apply_blocks(&self.storage, &self.network, &[b.clone()], self.tip.clone(), true) {
            Ok(tip) => {
                self.tip = tip;
                self.tip_block = b;
                Ok(())
            }
            Err(e) => {
                self.slot -= 1;
                Err(e.to_string())
            }
        }
    }
    fn height(&self) -> u64 {
        self.tip.height
    }
    fn pq_key(&self, addr: &str) -> Option<String> {
        self.storage.get_wallet(addr).unwrap().unwrap().pq_key.map(|k| k.public_key)
    }
}

#[test]
fn v3_wire_format_round_trips_and_id_is_stable() {
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let pqk = PqKeyPair::from_passphrase("alice pq");
    let bob = KeyPair::from_passphrase("bob").unwrap().address(63).unwrap();

    // registration with a legacy proof block + the new key
    let legacy = KeyPair::from_passphrase("alice second").unwrap();
    let reg = sign(pq_registration(&alice, 1, &pqk, 500_000_000 + SURCHARGE_LEGACY + SURCHARGE_PQ), &alice, &[Second::Legacy(&legacy), Second::Pq(&pqk)]);
    let bytes = serialize_transaction(&reg, SerializeOptions::default(), Network::mainnet_ref()).unwrap();
    assert_eq!(bytes[1], 3, "version byte");
    let back = deserialize_transaction(&bytes).unwrap();
    assert_eq!(back.id, reg.id);
    assert_eq!(back.second_signatures, reg.second_signatures);
    assert_eq!(back.asset.as_ref().unwrap().signature.as_ref().unwrap().algorithm, Some(1));
    assert_eq!(back.asset.as_ref().unwrap().signature.as_ref().unwrap().public_key, pqk.public_key_hex());
    assert!(bytes.len() > 59 + 3 + pq::PK_LEN + 64 + 3 + 64 + 3 + pq::SIG_LEN - 10, "size ≈ spec §8: {}", bytes.len());

    // PQ-locked transfer: one ML-DSA block; the signing hash excludes the blocks, M2 includes SIG1
    let t = sign(transfer(&alice, 2, 3, &bob, 100_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk)]);
    let bytes = serialize_transaction(&t, SerializeOptions::default(), Network::mainnet_ref()).unwrap();
    let back = deserialize_transaction(&bytes).unwrap();
    assert_eq!(back, Transaction { block_id: None, ..t.clone() });
    assert_eq!(t.pq_surcharge(FEE_PER_BYTE), SURCHARGE_PQ);
    let body = serialize_transaction(&t, SerializeOptions::for_signing(), Network::mainnet_ref()).unwrap();
    assert_eq!(bytes.len(), body.len() + 64 + 3 + pq::SIG_LEN);
    let m2 = transaction_pq_message(&t).unwrap();
    assert!(pq::verify(&pqk.public_key(), &m2, &hex::decode(&t.pq_blocks()[0].signature).unwrap()).unwrap());
}

#[test]
fn v3_activation_gate_registration_lock_and_rollback() {
    let net = network_pq(4);
    let mut c = Chain::new(net.clone());
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let bob = KeyPair::from_passphrase("bob").unwrap();
    let (a, b) = (alice.address(63).unwrap(), bob.address(63).unwrap());
    c.storage.update_wallet_state(&a, 100_000_000_000, 0).unwrap();
    c.storage.update_wallet_state(&b, 100_000_000_000, 0).unwrap();
    let pqk = PqKeyPair::from_passphrase("alice pq");

    // height 2: v3 before activation is refused (block and pool), v2 works
    let e = c.try_block(vec![sign(pq_registration(&alice, 1, &pqk, 500_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk)])]).unwrap_err();
    assert!(e.contains("ERR_PQ_NOT_ACTIVE"), "{e}");
    c.try_block(vec![sign(transfer(&alice, 1, 2, &b, 100_000_000), &alice, &[])]).unwrap();
    c.try_block(vec![]).unwrap();
    assert_eq!(c.height(), 3);

    // height 4: activation. Wrong fee, missing block, wrong key → refused; then a clean registration
    let e = c.try_block(vec![sign(pq_registration(&alice, 2, &pqk, 500_000_000), &alice, &[Second::Pq(&pqk)])]).unwrap_err();
    assert!(e.contains("ERR_PQ_FEE"), "{e}");
    let e = c.try_block(vec![sign(pq_registration(&alice, 2, &pqk, 500_000_000 + SURCHARGE_PQ), &alice, &[])]).unwrap_err();
    assert!(e.contains("ERR_PQ_SECOND_SIGNATURE_REQUIRED"), "{e}");
    let other = PqKeyPair::from_passphrase("someone else");
    let e = c.try_block(vec![sign(pq_registration(&alice, 2, &pqk, 500_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&other)])]).unwrap_err();
    assert!(e.contains("ERR_PQ_SECOND_SIGNATURE_INVALID"), "{e}");
    c.try_block(vec![sign(pq_registration(&alice, 2, &pqk, 500_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk)])]).unwrap();
    assert_eq!(c.pq_key(&a).as_deref(), Some(pqk.public_key_hex().as_str()));
    assert_eq!(c.storage.get_wallet(&a).unwrap().unwrap().pq_key.unwrap().since, 4);

    // PQ-locked: v2 refused, v3 without block refused, wrong key refused, correct block accepted; bob (no PQ) still sends v2
    let e = c.try_block(vec![sign(transfer(&alice, 3, 2, &b, 100_000_000), &alice, &[])]).unwrap_err();
    assert!(e.contains("ERR_PQ_SECOND_SIGNATURE_REQUIRED"), "{e}");
    let e = c.try_block(vec![sign(transfer(&alice, 3, 3, &b, 100_000_000), &alice, &[])]).unwrap_err();
    assert!(e.contains("ERR_PQ_SECOND_SIGNATURE_REQUIRED"), "{e}");
    let e = c.try_block(vec![sign(transfer(&alice, 3, 3, &b, 100_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&other)])]).unwrap_err();
    assert!(e.contains("ERR_PQ_SECOND_SIGNATURE_INVALID"), "{e}");
    let ok = sign(transfer(&alice, 3, 3, &b, 100_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk)]);
    c.try_block(vec![ok, sign(transfer(&bob, 1, 2, &a, 100_000_000), &bob, &[])]).unwrap();
    // v3 by a wallet without any second key must not carry blocks
    let e = c.try_block(vec![sign(transfer(&bob, 2, 3, &a, 100_000_000 + SURCHARGE_PQ), &bob, &[Second::Pq(&other)])]).unwrap_err();
    assert!(e.contains("UnexpectedSecondSignatureError"), "{e}");
    c.try_block(vec![sign(transfer(&bob, 2, 3, &a, 100_000_000), &bob, &[])]).unwrap();

    // rotation: [old PQ key, new PQ key]; afterwards only the new key spends
    let pqk2 = PqKeyPair::from_passphrase("alice pq 2");
    let e = c.try_block(vec![sign(pq_registration(&alice, 4, &pqk2, 500_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk2)])]).unwrap_err();
    assert!(e.contains("ERR_PQ_LEGACY_PROOF_REQUIRED"), "{e}");
    c.try_block(vec![sign(pq_registration(&alice, 4, &pqk2, 500_000_000 + 2 * SURCHARGE_PQ), &alice, &[Second::Pq(&pqk), Second::Pq(&pqk2)])]).unwrap();
    let e = c.try_block(vec![sign(transfer(&alice, 5, 3, &b, 100_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk)])]).unwrap_err();
    assert!(e.contains("ERR_PQ_SECOND_SIGNATURE_INVALID"), "{e}");
    c.try_block(vec![sign(transfer(&alice, 5, 3, &b, 100_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk2)])]).unwrap();
    assert_eq!(c.pq_key(&a).as_deref(), Some(pqk2.public_key_hex().as_str()));

    // rollback the rotation and everything after: the first key is back
    let h = c.height();
    c.storage.rollback_to(h - 2).unwrap();
    assert_eq!(c.pq_key(&a).as_deref(), Some(pqk.public_key_hex().as_str()));
    c.storage.rollback_to(3).unwrap();
    assert_eq!(c.pq_key(&a), None);
}

#[test]
fn legacy_second_signature_migrates_and_commitment_grace_applies() {
    let net = network_pq(3);
    let mut c = Chain::new(net.clone());
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let legacy = KeyPair::from_passphrase("alice legacy second").unwrap();
    let a = alice.address(63).unwrap();
    c.storage.update_wallet_state(&a, 100_000_000_000, 0).unwrap();
    let pqk = PqKeyPair::from_passphrase("alice pq");
    let other = PqKeyPair::from_passphrase("attacker");

    // height 2 (pre-activation): legacy second signature registered + stage-A commitment for pqk
    let reg2 = tx(&alice, 1, 2, serde_json::json!({ "type": 1, "fee": "500000000", "asset": { "signature": { "publicKey": legacy.public_key_hex() } } }));
    let commit = tx(&alice, 2, 2, serde_json::json!({ "type": 0, "fee": "100000000", "amount": "1", "recipientId": a, "vendorField": pqk.commitment() }));
    c.try_block(vec![sign(reg2, &alice, &[]), sign(commit, &alice, &[Second::Legacy(&legacy)])]).unwrap();

    // height 3 (active, inside the grace window): a different key is refused, legacy proof is required, then migration succeeds
    let e = c.try_block(vec![sign(pq_registration(&alice, 3, &other, 500_000_000 + SURCHARGE_LEGACY + SURCHARGE_PQ), &alice, &[Second::Legacy(&legacy), Second::Pq(&other)])]).unwrap_err();
    assert!(e.contains("ERR_PQ_COMMITMENT_MISMATCH"), "{e}");
    let e = c.try_block(vec![sign(pq_registration(&alice, 3, &pqk, 500_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk)])]).unwrap_err();
    assert!(e.contains("ERR_PQ_LEGACY_PROOF_REQUIRED"), "{e}");
    // v3 transfer with the legacy key as algorithm-0 block works before migration
    let bob = KeyPair::from_passphrase("bob").unwrap().address(63).unwrap();
    c.try_block(vec![sign(transfer(&alice, 3, 3, &bob, 100_000_000 + SURCHARGE_LEGACY), &alice, &[Second::Legacy(&legacy)])]).unwrap();
    c.try_block(vec![sign(pq_registration(&alice, 4, &pqk, 500_000_000 + SURCHARGE_LEGACY + SURCHARGE_PQ), &alice, &[Second::Legacy(&legacy), Second::Pq(&pqk)])]).unwrap();
    let w = c.storage.get_wallet(&a).unwrap().unwrap();
    assert_eq!(w.second_public_key, None, "legacy key cleared");
    assert_eq!(w.pq_key.as_ref().map(|k| k.public_key.as_str()), Some(pqk.public_key_hex().as_str()));
    // the legacy key no longer spends
    let e = c.try_block(vec![sign(transfer(&alice, 5, 2, &bob, 100_000_000), &alice, &[Second::Legacy(&legacy)])]).unwrap_err();
    assert!(e.contains("ERR_PQ_SECOND_SIGNATURE_REQUIRED"), "{e}");

    // a second wallet with a commitment registers a different key once the grace window (3 blocks after height 3) is over
    let carol = KeyPair::from_passphrase("carol").unwrap();
    let ca = carol.address(63).unwrap();
    c.storage.update_wallet_state(&ca, 100_000_000_000, 0).unwrap();
    c.try_block(vec![sign(tx(&carol, 1, 2, serde_json::json!({ "type": 0, "fee": "100000000", "amount": "1", "recipientId": ca, "vendorField": pqk.commitment() })), &carol, &[])]).unwrap();
    while c.height() < 6 {
        c.try_block(vec![]).unwrap();
    }
    c.try_block(vec![sign(pq_registration(&carol, 2, &other, 500_000_000 + SURCHARGE_PQ), &carol, &[Second::Pq(&other)])]).unwrap();
    assert_eq!(c.pq_key(&ca).as_deref(), Some(other.public_key_hex().as_str()));
}

#[tokio::test]
async fn mempool_returns_pq_error_codes_and_locks_after_pending_registration() {
    let net = network_pq(2);
    let mut c = Chain::new(net.clone());
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let a = alice.address(63).unwrap();
    let bob = KeyPair::from_passphrase("bob").unwrap().address(63).unwrap();
    c.storage.update_wallet_state(&a, 100_000_000_000, 0).unwrap();
    c.try_block(vec![]).unwrap();
    let pqk = PqKeyPair::from_passphrase("alice pq");

    let pool = Mempool::new(c.storage.clone(), vec![], 100);
    let (_, errors) = pool.add_many(vec![sign(pq_registration(&alice, 1, &pqk, 500_000_000), &alice, &[Second::Pq(&pqk)])]).await;
    assert_eq!(errors.values().next().map(|e| e.type_.as_str()), Some("ERR_PQ_FEE"), "{errors:?}");
    let (_, errors) = pool.add_many(vec![sign(pq_registration(&alice, 1, &pqk, 500_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk)])]).await;
    assert!(errors.is_empty(), "{errors:?}");
    // while the registration waits in the pool the sender is already PQ-locked
    let (_, errors) = pool.add_many(vec![sign(transfer(&alice, 2, 2, &bob, 100_000_000), &alice, &[])]).await;
    assert_eq!(errors.values().next().map(|e| e.type_.as_str()), Some("ERR_PQ_SECOND_SIGNATURE_REQUIRED"), "{errors:?}");
    let (_, errors) = pool.add_many(vec![sign(transfer(&alice, 2, 3, &bob, 100_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk)])]).await;
    assert!(errors.is_empty(), "{errors:?}");
    let mut txs = pool.all().await;
    txs.sort_by_key(|t| t.nonce);
    assert_eq!(txs.len(), 2);
    c.try_block(txs).unwrap();
    assert!(c.pq_key(&a).is_some());
}

/// Deterministic v3 vectors for the wallet team (`tests/vectors/pq_v3.json` → `transactions`): every field of the wire format.
fn v3_vector_cases() -> Vec<(String, Transaction)> {
    let alice = KeyPair::from_passphrase("this is a top secret passphrase").unwrap();
    let legacy = KeyPair::from_passphrase("second passphrase of the wallet").unwrap();
    let pqk = PqKeyPair::from_passphrase("second passphrase of the wallet");
    let pqk2 = PqKeyPair::from_passphrase("rotated quantum passphrase");
    let to = KeyPair::from_passphrase("recipient").unwrap().address(63).unwrap();
    let to = to.as_str();
    vec![
        ("pq-register, wallet without second signature".into(), sign(pq_registration(&alice, 1, &pqk, 500_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk)])),
        ("pq-register, wallet with legacy second signature (alg 0 proof + alg 1)".into(), sign(pq_registration(&alice, 2, &pqk, 500_000_000 + SURCHARGE_LEGACY + SURCHARGE_PQ), &alice, &[Second::Legacy(&legacy), Second::Pq(&pqk)])),
        ("transfer 1 STH from a PQ-locked wallet, vendorField \"pq\"".into(), sign(transfer(&alice, 3, 3, to, 100_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk)])),
        ("pq-register rotation (old PQ key proof + new PQ key)".into(), sign(pq_registration(&alice, 4, &pqk2, 500_000_000 + 2 * SURCHARGE_PQ), &alice, &[Second::Pq(&pqk), Second::Pq(&pqk2)])),
        ("v3 transfer with legacy second signature as algorithm-0 block (before migration)".into(), sign(transfer(&alice, 5, 3, to, 100_000_000 + SURCHARGE_LEGACY), &alice, &[Second::Legacy(&legacy)])),
    ]
}

fn v3_vector_json(name: &str, t: &Transaction) -> serde_json::Value {
    let net = Network::mainnet_ref();
    let body = serialize_transaction(t, SerializeOptions::for_signing(), net).unwrap();
    let with_sig1 = serialize_transaction(t, SerializeOptions { exclude_signature: false, exclude_second_signature: true, exclude_multi_signature: true }, net).unwrap();
    let full = serialize_transaction(t, SerializeOptions::default(), net).unwrap();
    serde_json::json!({
        "name": name, "json": t,
        "bodyHex": hex::encode(&body), "signingHash": hex::encode(sth_core::crypto::sha256(&body)),
        "bodyWithSignatureHex": hex::encode(&with_sig1), "m2": hex::encode(transaction_pq_message(t).unwrap()),
        "fullHex": hex::encode(&full), "size": full.len(), "id": t.id,
    })
}

/// Rebuilds every published transaction vector and compares bytes, hashes and ids.
#[test]
fn published_v3_transaction_vectors_match() {
    let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/pq_v3.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let cases = v3_vector_cases();
    let published = v["transactions"].as_array().expect("transactions section");
    assert_eq!(published.len(), cases.len());
    for (p, (name, t)) in published.iter().zip(&cases) {
        let ours = v3_vector_json(name, t);
        for key in ["name", "bodyHex", "signingHash", "bodyWithSignatureHex", "m2", "fullHex", "size", "id"] {
            assert_eq!(p[key], ours[key], "{name}: {key}");
        }
        let back = deserialize_transaction(&hex::decode(p["fullHex"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(back.id, t.id, "{name}: deserialised id");
    }
}

/// `cargo test --test pq_v3 print_v3_vectors -- --ignored --nocapture` → paste into tests/vectors/pq_v3.json "transactions".
#[test]
#[ignore]
fn print_v3_vectors() {
    let out: Vec<_> = v3_vector_cases().iter().map(|(n, t)| v3_vector_json(n, t)).collect();
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "transactions": out })).unwrap());
}

/// Byte budget: the pool refuses a transaction once the wire bytes of pending transactions would exceed `max_bytes`,
/// even though the count limit still has room; pruning after the block frees the budget.
#[tokio::test]
async fn mempool_byte_budget_limits_heavy_v3_transactions() {
    let net = network_pq(2);
    let mut c = Chain::new(net.clone());
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let a = alice.address(63).unwrap();
    let bob = KeyPair::from_passphrase("bob").unwrap().address(63).unwrap();
    c.storage.update_wallet_state(&a, 100_000_000_000, 0).unwrap();
    c.try_block(vec![]).unwrap();
    let pqk = PqKeyPair::from_passphrase("alice pq");
    c.try_block(vec![sign(pq_registration(&alice, 1, &pqk, 500_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk)])]).unwrap();

    // budget for one v3 transfer (~2.6 KB) but not two; count limit 100
    let pool = Mempool::new(c.storage.clone(), vec![], 100).with_max_bytes(3_000);
    let t1 = sign(transfer(&alice, 2, 3, &bob, 100_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk)]);
    let t2 = sign(transfer(&alice, 3, 3, &bob, 100_000_000 + SURCHARGE_PQ), &alice, &[Second::Pq(&pqk)]);
    let (_, errors) = pool.add_many(vec![t1.clone()]).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert!(pool.bytes() > 2_500 && pool.bytes() < 3_000, "{}", pool.bytes());
    let (_, errors) = pool.add_many(vec![t2.clone()]).await;
    assert_eq!(errors.values().next().map(|e| e.type_.as_str()), Some("ERR_POOL_FULL"), "{errors:?}");

    // forged → pruned → budget free again
    c.try_block(vec![t1]).unwrap();
    pool.prune_confirmed().await;
    assert_eq!(pool.bytes(), 0);
    let (_, errors) = pool.add_many(vec![t2]).await;
    assert!(errors.is_empty(), "{errors:?}");
}
