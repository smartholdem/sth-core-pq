//! Author: TechnoL0g
//! Quantum Shield stage A: ML-DSA-44 keys from a passphrase and commitment indexing in a self-transfer vendorField.

use sth_core::config::Network;
use sth_core::crypto::pq::{self, PqKeyPair};
use sth_core::models::Block;
use sth_core::storage::Storage;
use sha2::Digest as _;

const SIG: &str = "3045022100bfcfed36e8019c760490fd453cc28a2118241907d63c3ed0d3004687907107ff02200d300e64fdf5c5ca358e3266794b12b6b900004084b7fba838ceedcb1364e658";

#[test]
fn ml_dsa_44_keys_are_deterministic_and_sign_verify() {
    let a = PqKeyPair::from_passphrase("second passphrase of the wallet");
    let b = PqKeyPair::from_passphrase("second passphrase of the wallet");
    let c = PqKeyPair::from_passphrase("another passphrase");
    assert_eq!(a.public_key(), b.public_key(), "same passphrase → same key on every device");
    assert_ne!(a.public_key(), c.public_key());
    assert_eq!(a.public_key().len(), pq::PK_LEN);

    let msg = b"sth transfer bytes";
    let sig = a.sign(msg).unwrap();
    assert_eq!(sig.len(), pq::SIG_LEN);
    assert_eq!(sig, a.sign(msg).unwrap(), "deterministic signing");
    assert!(pq::verify(&a.public_key(), msg, &sig).unwrap());
    assert!(!pq::verify(&c.public_key(), msg, &sig).unwrap());
    assert!(!pq::verify(&a.public_key(), b"other message", &sig).unwrap());
    let mut tampered = sig.clone();
    tampered[100] ^= 1;
    assert!(!pq::verify(&a.public_key(), msg, &tampered).unwrap());
    assert!(!pq::verify(&a.public_key()[..100], msg, &sig).unwrap(), "wrong key length is rejected, not a panic");
}

#[test]
fn commitment_format_round_trips_and_rejects_garbage() {
    let k = PqKeyPair::from_passphrase("x");
    let c = k.commitment();
    assert_eq!(c.len(), 74);
    assert!(c.starts_with("sthpq1:01:"));
    let (alg, hash) = pq::parse_commitment(&c).unwrap();
    assert_eq!(alg, pq::ALG_ML_DSA_44);
    assert_eq!(hash, hex::encode(pq::commitment_hash(&k.public_key())));
    for bad in ["sthpq1:02:".to_string() + &"a".repeat(64), "sthpq1:01:".to_string() + &"a".repeat(63), "sthpq1:01:".to_string() + &"A".repeat(64), "hello".into(), "sthpq1:1:".to_string() + &"a".repeat(64)] {
        assert!(pq::parse_commitment(&bad).is_none(), "{bad}");
    }
}

fn transfer(id: &str, pk: &str, nonce: u64, recipient: &str, vendor: &str) -> String {
    format!(
        r#"{{"id":"{id}","version":2,"network":63,"typeGroup":1,"type":0,"nonce":"{nonce}","senderPublicKey":"{pk}","fee":"100000000","amount":"1","recipientId":"{recipient}","vendorField":"{vendor}","expiration":0,"signature":"{}"}}"#,
        "1".repeat(128)
    )
}

fn block(id: &str, prev: &str, height: u64, gen: &str, txs: &[String]) -> Block {
    let json = format!(
        r#"{{"id":"{id}","version":0,"timestamp":{},"previousBlock":"{prev}","height":{height},"numberOfTransactions":{},"totalAmount":"{}","totalFee":"{}","reward":"0","payloadLength":0,"payloadHash":"{}","generatorPublicKey":"{gen}","blockSignature":"{SIG}","transactions":[{}]}}"#,
        height * 8, txs.len(), txs.len(), txs.len() as u64 * 100_000_000, "9".repeat(64), txs.join(",")
    );
    serde_json::from_str(&json).unwrap()
}

#[test]
fn self_transfer_commitment_is_indexed_last_wins_and_rolls_back() {
    let storage = Storage::temporary(Network::mainnet()).unwrap();
    let user = sth_core::crypto::KeyPair::from_passphrase("user").unwrap();
    let other = sth_core::crypto::KeyPair::from_passphrase("other").unwrap();
    let (upk, uaddr, oaddr) = (user.public_key_hex(), user.address(63).unwrap(), other.address(63).unwrap());
    storage.update_wallet_state(&uaddr, 10_000_000_000, 0).unwrap();
    let c1 = PqKeyPair::from_passphrase("pq one").commitment();
    let c2 = PqKeyPair::from_passphrase("pq two").commitment();

    // block 1: commitment to someone else (ignored) + commitment to self (indexed)
    let b1 = block(&"a".repeat(64), &"f".repeat(64), 100, &upk, &[transfer(&"1".repeat(64), &upk, 1, &oaddr, &c1), transfer(&"2".repeat(64), &upk, 2, &uaddr, &c1)]);
    storage.apply_block(&b1).unwrap();
    let w = storage.get_wallet(&uaddr).unwrap().unwrap();
    let got = w.pq_commitment.clone().expect("self-transfer commitment indexed");
    assert_eq!(storage.pq_commitment_count(), 1);
    assert_eq!((got.algorithm, got.height), (1, 100));
    assert_eq!(got.commitment, pq::parse_commitment(&c1).unwrap().1);
    assert!(storage.get_wallet(&oaddr).unwrap().unwrap().pq_commitment.is_none(), "recipient of a foreign commitment is untouched");

    // block 2: a newer commitment replaces the old one; rollback restores it
    storage.set_undo_enabled(true);
    let b2 = block(&"b".repeat(64), &"a".repeat(64), 101, &upk, &[transfer(&"3".repeat(64), &upk, 3, &uaddr, &c2)]);
    storage.apply_block(&b2).unwrap();
    let got2 = storage.get_wallet(&uaddr).unwrap().unwrap().pq_commitment.unwrap();
    assert_eq!((got2.height, got2.commitment.as_str()), (101, pq::parse_commitment(&c2).unwrap().1.as_str()));
    storage.rollback_last_block().unwrap();
    let back = storage.get_wallet(&uaddr).unwrap().unwrap().pq_commitment.unwrap();
    assert_eq!(back.height, 100);
    assert_eq!(back.commitment, got.commitment);

    // a plain self-transfer without the prefix changes nothing
    let b3 = block(&"c".repeat(64), &"a".repeat(64), 101, &upk, &[transfer(&"4".repeat(64), &upk, 3, &uaddr, "hello")]);
    storage.apply_block(&b3).unwrap();
    assert_eq!(storage.get_wallet(&uaddr).unwrap().unwrap().pq_commitment.unwrap().height, 100);
    assert_eq!(storage.pq_commitment_count(), 1);

    // a wallet whose first commitment is rolled back disappears from the index
    let fresh = sth_core::crypto::KeyPair::from_passphrase("fresh").unwrap();
    let (fpk, faddr) = (fresh.public_key_hex(), fresh.address(63).unwrap());
    storage.update_wallet_state(&faddr, 10_000_000_000, 0).unwrap();
    let b4 = block(&"d".repeat(64), &"c".repeat(64), 102, &upk, &[transfer(&"5".repeat(64), &fpk, 1, &faddr, &c2)]);
    storage.apply_block(&b4).unwrap();
    assert_eq!(storage.pq_commitment_count(), 2);
    storage.rollback_last_block().unwrap();
    assert_eq!(storage.pq_commitment_count(), 1);
    assert!(storage.get_wallet(&faddr).unwrap().unwrap().pq_commitment.is_none());
}

/// Vectors for the wallet team (`tests/vectors/pq_v3.json`): passphrase → seed → pk → commitment → deterministic signature.
#[test]
fn published_vectors_match() {
    let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/pq_v3.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    for case in v["vectors"].as_array().unwrap() {
        let k = PqKeyPair::from_passphrase(case["passphrase"].as_str().unwrap());
        let pk = k.public_key();
        assert_eq!(hex::encode(sha2::Sha256::digest(&pk)), case["publicKeySha256"].as_str().unwrap());
        assert_eq!(hex::encode(&pk[..32]), case["publicKeyPrefix32"].as_str().unwrap());
        assert_eq!(k.commitment(), case["commitment"].as_str().unwrap());
        let msg = hex::decode(case["messageHex"].as_str().unwrap()).unwrap();
        let sig = k.sign(&msg).unwrap();
        assert_eq!(hex::encode(sha2::Sha256::digest(&sig)), case["signatureSha256"].as_str().unwrap());
        assert!(pq::verify(&pk, &msg, &sig).unwrap());
    }
}

#[test]
#[ignore]
fn print_vectors() {
    use sha2::Digest;
    let mut out = Vec::new();
    for (p, m) in [("this is a top secret passphrase", "00"), ("second passphrase of the wallet", "73746820706f73742d7175616e74756d"), ("", "ff")] {
        let k = PqKeyPair::from_passphrase(p);
        let pk = k.public_key();
        let msg = hex::decode(m).unwrap();
        let sig = k.sign(&msg).unwrap();
        out.push(serde_json::json!({
            "passphrase": p,
            "seedHex": hex::encode(sha2::Sha256::new().chain_update(b"sth-pq-v1").chain_update(p.as_bytes()).finalize()),
            "publicKeyLen": pk.len(), "publicKeyPrefix32": hex::encode(&pk[..32]), "publicKeySha256": hex::encode(sha2::Sha256::digest(&pk)),
            "commitment": k.commitment(), "messageHex": m, "signatureLen": sig.len(), "signatureSha256": hex::encode(sha2::Sha256::digest(&sig)),
        }));
    }
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "algorithm": "ML-DSA-44 (FIPS 204), alg_id 0x01", "seed": "sha256(\"sth-pq-v1\" || passphrase)", "context": "sth-pq-v1",
        "signing": "ML-DSA.Sign deterministic variant with ctx", "vectors": out })).unwrap());
}
