//! Author: TechnoL0g
//!
//! Quantum Shield stage C: version-1 blocks with a hybrid secp256k1 + ML-DSA-44 signature under milestone `pq.blocks`
//! (wire format / id / storage round trip, activation gate, grace window, key from the state before the block).

use sth_core::crypto::pq::PqKeyPair;
use sth_core::crypto::{
    block_id, block_pq_message, deserialize_block_header, serialize_block, sign_schnorr_legacy, transaction_id, transaction_pq_message,
    transaction_signing_hash, verify_block, verify_block_pq_signature, KeyPair,
};
use sth_core::delegate::block_builder::{forge_block, forge_block_with};
use sth_core::delegate::round::slot_start;
use sth_core::models::{Block, PqSignatureBlock, Transaction, BLOCK_VERSION_PQ};
use sth_core::newnet::{generate, NewNetOptions};
use sth_core::node_config::NodeConfig;
use sth_core::storage::Storage;
use sth_core::sync::{apply_blocks, ChainTip};
use std::sync::Arc;

const PQ_AT: u64 = 2;
const PQ_BLOCKS_AT: u64 = 4;
const GRACE: u64 = 21;

struct Net {
    storage: Arc<Storage>,
    network: sth_core::config::Network,
    delegates: Vec<KeyPair>,
    tip: Block,
}

fn newnet(seed: &str) -> Net {
    let dir = tempfile::tempdir().unwrap();
    let opts = NewNetOptions { ticker: "PQC".into(), title: "PqNet".into(), delegates: 3, pubkey_hash: 30, p2p_port: 4102, api_port: 4104, metrics_port: 4989, seed: seed.into(), tokens_at: None, pq_at: Some(PQ_AT), pq_blocks_at: Some(PQ_BLOCKS_AT), finality_hard: false, treasury_supply: 1_000_000 * 100_000_000, delegate_stake: 1000 * 100_000_000 };
    let n = generate(&opts, dir.path()).unwrap();
    let network = NodeConfig::load(&dir.path().join("node.yaml")).unwrap().load_network().unwrap();
    let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    storage.set_undo_enabled(true);
    let delegates = n.delegate_passphrases.iter().map(|p| KeyPair::from_passphrase(p).unwrap()).collect();
    let tip = storage.get_last_block().unwrap().unwrap();
    Net { storage, network, delegates, tip }
}

impl Net {
    fn try_block(&mut self, by: usize, pq: Option<&PqKeyPair>, txs: Vec<Transaction>) -> Result<Block, String> {
        let slot = self.tip.height + 1;
        let b = forge_block_with(&self.network, &self.delegates[by], pq, &self.tip, slot_start(slot, 8), txs).unwrap();
        self.apply(b)
    }
    fn apply(&mut self, b: Block) -> Result<Block, String> {
        apply_blocks(&self.storage, &self.network, &[b.clone()], ChainTip { height: self.tip.height, id: self.tip.id.clone() }, true).map_err(|e| e.to_string())?;
        self.tip = b.clone();
        Ok(b)
    }
    /// v3 `pq-register` of `keys` with `pqk` (proof block by the new key), fee = static + surcharge.
    fn pq_register(&self, keys: &KeyPair, pqk: &PqKeyPair, old: Option<&PqKeyPair>) -> Transaction {
        let nonce = self.storage.get_wallet(&keys.address(30).unwrap()).unwrap().map(|w| w.nonce).unwrap_or(0) + 1;
        let fee = self.network.milestone(self.tip.height + 1).static_fee("secondSignature") + 10_000 * (3 + 2420) * (1 + old.is_some() as u64);
        let v = serde_json::json!({ "version": 3, "network": 30, "typeGroup": 1, "type": 1, "nonce": nonce.to_string(), "senderPublicKey": keys.public_key_hex(), "amount": "0", "fee": fee.to_string(), "expiration": 0,
            "asset": { "signature": { "algorithm": 1, "publicKey": pqk.public_key_hex() } } });
        let mut t: Transaction = serde_json::from_value(v).unwrap();
        t.signature = Some(sign_schnorr_legacy(&transaction_signing_hash(&t).unwrap(), keys.private_key()).unwrap());
        let m2 = transaction_pq_message(&t).unwrap();
        let mut blocks: Vec<PqSignatureBlock> = old.map(|o| PqSignatureBlock { algorithm: 1, signature: hex::encode(o.sign(&m2).unwrap()) }).into_iter().collect();
        blocks.push(PqSignatureBlock { algorithm: 1, signature: hex::encode(pqk.sign(&m2).unwrap()) });
        t.second_signatures = Some(blocks);
        t.id = Some(transaction_id(&t).unwrap());
        t
    }
}

#[test]
fn milestone_gates_block_versions() {
    let n = newnet("pqc-gate");
    assert_eq!(n.network.pq_blocks_activation_height(), Some(PQ_BLOCKS_AT));
    assert_eq!(n.network.block_versions_allowed(3), (false, true), "before activation only v0");
    assert_eq!(n.network.block_versions_allowed(PQ_BLOCKS_AT), (true, true), "grace: both");
    assert_eq!(n.network.block_versions_allowed(PQ_BLOCKS_AT + GRACE - 1), (true, true));
    assert_eq!(n.network.block_versions_allowed(PQ_BLOCKS_AT + GRACE), (true, false), "after grace only v1");
}

#[test]
fn hybrid_block_round_trips_and_is_verified_against_the_registered_key() {
    let mut n = newnet("pqc-hybrid");
    let pqk = PqKeyPair::from_passphrase("delegate0 pq");
    // height 2 (pq active): delegate 0 registers its PQ key; height 3: plain v0 block
    let reg = n.pq_register(&n.delegates[0].clone(), &pqk, None);
    n.try_block(0, None, vec![reg]).unwrap();
    n.try_block(1, None, vec![]).unwrap();
    // before activation a v1 block is refused even with a valid signature
    let early = forge_block_with(&n.network, &n.delegates[0], Some(&pqk), &n.storage.get_block_by_height(2).unwrap().unwrap(), slot_start(3, 8), vec![]).unwrap();
    assert_eq!(early.version, 0, "forge_block_with stays v0 while pq.blocks is off");

    // height 4: pq.blocks on → hybrid block
    let b4 = n.try_block(0, Some(&pqk), vec![]).unwrap();
    assert_eq!(b4.version, BLOCK_VERSION_PQ);
    let pq = b4.pq_signature.as_ref().unwrap();
    assert_eq!((pq.algorithm, pq.signature.len()), (1, 2420 * 2));
    assert!(verify_block_pq_signature(&b4, &pqk.public_key_hex()).unwrap());
    assert!(!verify_block_pq_signature(&b4, &PqKeyPair::from_passphrase("other").public_key_hex()).unwrap());
    // wire: header || DER sig || alg || len u16 || sig; id covers everything
    let bytes = serialize_block(&b4, true).unwrap();
    let classic = serialize_block(&Block { pq_signature: None, ..b4.clone() }, true).unwrap();
    assert_eq!(bytes.len(), classic.len() + 3 + 2420);
    assert_eq!(bytes[classic.len()], 1);
    assert_eq!(u16::from_le_bytes([bytes[classic.len() + 1], bytes[classic.len() + 2]]), 2420);
    assert_eq!(block_id(&b4).unwrap(), b4.id.clone().unwrap());
    assert_ne!(block_id(&Block { pq_signature: None, ..b4.clone() }).unwrap(), b4.id.clone().unwrap(), "id changes with the PQ signature");
    assert_eq!(block_pq_message(&b4).unwrap(), sth_core::crypto::sha256(&classic));
    let back = deserialize_block_header(&bytes, &n.network).unwrap();
    assert_eq!(back.pq_signature, b4.pq_signature);
    assert_eq!(back.id, b4.id);
    // storage keeps the PQ signature (compact encoding uses the same serializer)
    let stored = n.storage.get_block_by_height(4).unwrap().unwrap();
    assert_eq!(stored.pq_signature, b4.pq_signature);
    assert_eq!(stored.id, b4.id);
    assert!(verify_block(&stored, &n.network).verified);

    // grace: delegate 1 (no PQ key) still forges v0
    let b5 = n.try_block(1, None, vec![]).unwrap();
    assert_eq!(b5.version, 0);

    // a v1 block signed with a key that is not the delegate's registered one → rejected
    let wrong = PqKeyPair::from_passphrase("wrong");
    let bad = forge_block_with(&n.network, &n.delegates[0], Some(&wrong), &n.tip, slot_start(6, 8), vec![]).unwrap();
    let err = n.apply(bad).unwrap_err();
    assert!(err.contains("BlockPqSignatureError"), "{err}");
    // a v1 block by a delegate without any registered key → BlockPqKeyMissingError
    let bad = forge_block_with(&n.network, &n.delegates[1], Some(&wrong), &n.tip, slot_start(6, 8), vec![]).unwrap();
    let err = n.apply(bad).unwrap_err();
    assert!(err.contains("BlockPqKeyMissingError"), "{err}");
    // a v1 block whose pqSignature was stripped → format error
    let mut stripped = forge_block_with(&n.network, &n.delegates[0], Some(&pqk), &n.tip, slot_start(6, 8), vec![]).unwrap();
    stripped.pq_signature = None;
    stripped.id = Some(block_id(&stripped).unwrap());
    let err = n.apply(stripped).unwrap_err();
    assert!(err.contains("BlockPqSignatureMissingError"), "{err}");
}

#[test]
fn after_the_grace_window_version_0_is_refused_and_key_rotation_applies_next_block() {
    let mut n = newnet("pqc-grace");
    let pqk = PqKeyPair::from_passphrase("d0 pq");
    let reg = n.pq_register(&n.delegates[0].clone(), &pqk, None);
    n.try_block(0, None, vec![reg]).unwrap();
    while n.tip.height + 1 < PQ_BLOCKS_AT + GRACE {
        n.try_block(0, Some(&pqk), vec![]).unwrap();
    }
    assert_eq!(n.tip.height + 1, PQ_BLOCKS_AT + GRACE);
    // grace closed: v0 → BlockPqRequiredError
    let v0 = forge_block(&n.network, &n.delegates[1], &n.tip, slot_start(n.tip.height + 1, 8), vec![]).unwrap();
    let err = n.apply(v0).unwrap_err();
    assert!(err.contains("BlockPqRequiredError"), "{err}");
    // rotation inside block H is signed with the OLD key (state before the block); the new key is valid from H+1
    let pqk2 = PqKeyPair::from_passphrase("d0 pq v2");
    let rot = n.pq_register(&n.delegates[0].clone(), &pqk2, Some(&pqk));
    let with_new = forge_block_with(&n.network, &n.delegates[0], Some(&pqk2), &n.tip, slot_start(n.tip.height + 1, 8), vec![rot.clone()]).unwrap();
    let err = n.apply(with_new).unwrap_err();
    assert!(err.contains("BlockPqSignatureError"), "{err}");
    n.try_block(0, Some(&pqk), vec![rot]).unwrap();
    assert!(n.try_block(0, Some(&pqk), vec![]).unwrap_err().contains("BlockPqSignatureError"), "old key no longer valid");
    n.try_block(0, Some(&pqk2), vec![]).unwrap();
    // rollback restores the previous key through the wallet undo record
    n.storage.rollback_last_block().unwrap();
    n.storage.rollback_last_block().unwrap();
    let w = n.storage.get_wallet(&n.delegates[0].address(30).unwrap()).unwrap().unwrap();
    assert_eq!(w.pq_key.unwrap().public_key, pqk.public_key_hex());
}
