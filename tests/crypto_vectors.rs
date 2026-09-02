//! Author: TechnoL0g
//!
//! Bit-exact compatibility vectors taken from the live SmartHoldem mainnet
//! (https://node0.smartholdem.io/api/..., `transform=false` = raw core format).
//! Every id / signature below was produced by the original JS `@smartholdem/core`.

use sth_core::config::Network;
use sth_core::crypto::{
    address_from_public_key, block_id, block_payload_hash, get_id, sha256, sign_ecdsa, sign_schnorr_legacy,
    transaction_id, transaction_signing_hash, validate_address, verify_block, verify_block_signature,
    verify_ecdsa, verify_schnorr_legacy, verify_signature, verify_transaction_signature, ChainObject, KeyPair,
};
use sth_core::models::{Block, Transaction};

// ---------------------------------------------------------------- real vectors

/// Height 11704428, empty block.
const BLOCK_11704428: &str = r#"{"id": "ee83dd37a1ee0df793de24371a911dc75a8ca41212e122b93e816f37206fc870", "version": 0, "timestamp": 95104536, "previousBlock": "6baf15c3d7f3a2203953e44e766d44646dcc2aada206f420aabe4fe73693c4fb", "height": 11704428, "numberOfTransactions": 0, "totalAmount": "0", "totalFee": "0", "reward": "0", "payloadLength": 0, "payloadHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "generatorPublicKey": "023bff30613a00d39f9190c6d8460fd4bf01efbe0949955c62b0dbad547047d16a", "blockSignature": "3044022025929db39d4e345285afa080d7e669b0eb93dd5bccbcfa5fdd740cbfd5c0d73102207cb21e12db735ab79ff62495539c8b2bed946fcbdb9eb1aeca69b19ec938b8bd"}"#;

/// Height 11704043, 1 transfer (596236ba…).
const BLOCK_11704043: &str = r#"{"id": "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d", "version": 0, "timestamp": 95101456, "previousBlock": "f7f523ce32716bc968383afff31c0def91acefa72df9ba71bb95ab506a0592a7", "height": 11704043, "numberOfTransactions": 1, "totalAmount": "314159265", "totalFee": "100000000", "reward": "0", "payloadLength": 32, "payloadHash": "fdee5b08437ad279fd6461bdfcb48fe8c36a5d394203566ba5bf33819fe2d2e2", "generatorPublicKey": "03d67017411e92a8e93d7b0318e2748f013c130d6e4e01b1248da0ee0959ee9df8", "blockSignature": "3045022100bfcfed36e8019c760490fd453cc28a2118241907d63c3ed0d3004687907107ff02200d300e64fdf5c5ca358e3266794b12b6b900004084b7fba838ceedcb1364e658",
"transactions": [{"version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": "10103", "senderPublicKey": "036560f20d578da7b8433248d0c82e68121163958d533dc74b0e7d8dbabc0606a0", "fee": "100000000", "amount": "314159265", "expiration": 0, "recipientId": "SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw", "signature": "1d311090b61358077d2f59972b0913ec3687ec82f8a7b752121df934b701a7bc07e3e0d7bf051bf939a5291790ae5ed43ed59d1b6feb8dda0f76f07f747d8601", "id": "596236ba37bc2d419f5825b2d771a74e151e7719923afefdef0b0baa9a8cfcb8", "blockId": "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d", "blockHeight": 11704043, "sequence": 0}]}"#;

/// Height 11521618, 2 transfers with vendorField (API returns them in reverse sequence order).
const BLOCK_11521618: &str = r#"{"id": "4fc426e6314e9bcead265613106725c1aac6ec31b1d250665a8c286312775a61", "version": 0, "timestamp": 93641720, "previousBlock": "8fab02508df3833d923555f0eb5f26ad3c2641608e88a5251e67b87c05d7ee79", "height": 11521618, "numberOfTransactions": 2, "totalAmount": "200", "totalFee": "2000000", "reward": "0", "payloadLength": 64, "payloadHash": "b9e06a08f9a3f4c488a5b1c1aac0ebe0e4d3a1b1d5a93f34a7abb7b68d2a6c53", "generatorPublicKey": "03ac459c15e32cd9f8189e76322d7f6461349d3eeaaddbda23ab8221800a11f688", "blockSignature": "3045022100bd70bfada498fd10c207999349ced156942389ca915e52b6aec33398f460920e02202654e060b857f9b4dffc910921948b7097750fe420fa94349f2e9fa986be6b28",
"transactions": [{"version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": "34", "senderPublicKey": "03d2fe717a115cf019791bba65509902b5cd8b76972f980990f4d0d99182df6634", "fee": "1000000", "amount": "100", "vendorField": "xkey:9QrfImkS1ePZOjz3B+NXi4mq8H/drsxPJfEv9R7Ezhk=|ec3a9f1989b4054dd72badfabb77465906173a087fd0249253efedd0a7351226", "expiration": 0, "recipientId": "SeeQyvfpghaFLjVfLUTxLvpQq78LMQnAmE", "signature": "acfbca40288bb053d5716bbcbf55f11897ebf3ddac882689cdec8648e04b1f83b79df2f13298057059b27e84bcee1a5ba0052698139c6c75960eba317ff3c0cb", "id": "09ed2b84d0f1a23a7d3b057d6d6d3092f6e40da812387c4f81e09d62d0500e1d", "blockId": "4fc426e6314e9bcead265613106725c1aac6ec31b1d250665a8c286312775a61", "blockHeight": 11521618, "sequence": 1}, {"version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": "33", "senderPublicKey": "03d2fe717a115cf019791bba65509902b5cd8b76972f980990f4d0d99182df6634", "fee": "1000000", "amount": "100", "vendorField": "xkey:9QrfImkS1ePZOjz3B+NXi4mq8H/drsxPJfEv9R7Ezhk=|ec3a9f1989b4054dd72badfabb77465906173a087fd0249253efedd0a7351226", "expiration": 0, "recipientId": "SeeQyvfpghaFLjVfLUTxLvpQq78LMQnAmE", "signature": "c3c4ec46974f5a9e37182b5d5d798149bae1131ea30692273c64f98a74fb739d819adbd2169638e0a502c35adad647d6e8c61d61adafa34f83e7a56ab3bc4a3c", "id": "6d013382efa6627dab0350a5fd90540ce916049de88125deb03e1bb5e732c07b", "blockId": "4fc426e6314e9bcead265613106725c1aac6ec31b1d250665a8c286312775a61", "blockHeight": 11521618, "sequence": 0}]}"#;

/// Genesis header (height 1). Core notes its computed id differs, so only the signature is checked.
const GENESIS_HEADER: &str = r#"{"id": "ea60ebb15e3e8abe9e47a7ef18145d65c570c3dfe2b0ac678c665787860bae32", "version": 0, "timestamp": 0, "previousBlock": "0000000000000000000000000000000000000000000000000000000000000000", "height": 1, "numberOfTransactions": 1855, "totalAmount": "24977000000000000", "totalFee": "0", "reward": "0", "payloadLength": 288953, "payloadHash": "b0987b45a3a754da1362bc6818548cb34f65750c9ac81d28c93e7545224df2d2", "generatorPublicKey": "0205dc9ea85e527cb739c8fad48a3c26cb881a2df444df02f3c908e98e1705cb09", "blockSignature": "3044022028c1c8a5dfbca4887542233d51736c41258263c8f4367cd91c85d2a7ee26d4b7022079f4bce6d3b62ea91adfe207fd682619a1d3696e06f9907f892519eb865206e2"}"#;

/// Type 0 transfer.
const TX_TRANSFER: &str = r#"{"version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": "10103", "senderPublicKey": "036560f20d578da7b8433248d0c82e68121163958d533dc74b0e7d8dbabc0606a0", "fee": "100000000", "amount": "314159265", "expiration": 0, "recipientId": "SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw", "signature": "1d311090b61358077d2f59972b0913ec3687ec82f8a7b752121df934b701a7bc07e3e0d7bf051bf939a5291790ae5ed43ed59d1b6feb8dda0f76f07f747d8601", "id": "596236ba37bc2d419f5825b2d771a74e151e7719923afefdef0b0baa9a8cfcb8", "blockId": "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d", "blockHeight": 11704043, "sequence": 0}"#;

/// Type 0 transfer with vendorField (memo).
const TX_TRANSFER_VENDORFIELD: &str = r#"{"version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": "7", "senderPublicKey": "034915c944effcaee31df0f8586655dd871be9eb443e148b8ac44bc3b7914f0bb1", "fee": "1000000", "amount": "1", "vendorField": "xkey:4axrTHl6MRI3Z74J95dmwHWIEetFEq0SFZN4x2z3ZW4=|ef251d59259d2f8bc08073cbe9a083f07be1bcb2ba9da5819714c67b2988c40c", "expiration": 0, "recipientId": "SPTv5XL8W2Xbw9VmArWUKpBWdfQRKepM6R", "signature": "f7acebc21dc7b71866272a4924441811f6cb36ea5976a520da0423879ed7391990dae255e76242874940d389772e566ee797928ed0e25aa1985918ea1c40eb9f", "id": "3f5861bbfc8f04f070eb22bb98501f7b808431dee7d05fdfa5760c837704c0b4", "blockId": "4570db7f5f304eff1eba9e7e67180eafd9a2ee087b8cdc1f00fecb1992781a59", "blockHeight": 11704006, "sequence": 0}"#;

/// Type 3 vote (unvote + vote).
const TX_VOTE: &str = r#"{"version": 2, "network": 63, "typeGroup": 1, "type": 3, "nonce": "8", "senderPublicKey": "02d9afa5ad1ed3f80032a09537a31bbaa3f3463274e7e58989a0506ca281047065", "fee": "100000000", "amount": "0", "asset": {"votes": ["-0344efe631ac747d1031202dcb1e6253436caaa4da75ae133a7b7e9890bd36f20b", "+0344efe631ac747d1031202dcb1e6253436caaa4da75ae133a7b7e9890bd36f20b"]}, "signature": "0e94b810faa0b1879d40bd94d9f0402ba5de671d133de0b0038fa954eb9e7904aa92083bfcf885c21d5820a2fbad38936586455a78571f3bdedd2bc80e97e52a", "id": "2bef9de12adc3207d9bfe1f9e2b7c2987902277b7b5aa4ecb0da1bc648501423", "blockId": "01d0502aea836b3c786b9552d226f3c0e91aee8f98c0bde0f91cee235b057c6c", "blockHeight": 9662005, "sequence": 0}"#;

/// Type 6 multipayment with vendorField.
const TX_MULTIPAYMENT: &str = r#"{"version": 2, "network": 63, "typeGroup": 1, "type": 6, "nonce": "19", "senderPublicKey": "03bd4bf7d18df019d500b348670b2979c980ba79a33fe34bb87d405c076e7d470d", "fee": "10000000", "amount": "0", "vendorField": "seed:reward", "asset": {"payments": [{"amount": "118996409386", "recipientId": "SboZHv8y3ohGbA5ophSCifZA5zCiD6rQyq"}, {"amount": "213789310260", "recipientId": "SSU6TvycebBMTvc8WHiKTfG9xmX1QSniZu"}]}, "signature": "d13845987e802560f4e9ecf154c58244a437f493bae2b123581eaa449d408916d8447f02fd1c43f0e77adf5380539580ae0229fb73dfa93454e4abb9680f6ad1", "id": "25820be000f73506741533e84ddfb731cfa0a2b6d39500a7b7864b91f92556e2", "blockId": "b15263bead74cc4a7df6dd7d2f7a5ed0a89c744f7d2cc69c8c0fe542f5dc20b2", "blockHeight": 11661263, "sequence": 0}"#;

fn block(json: &str) -> Block {
    serde_json::from_str(json).expect("valid block json")
}

fn tx(json: &str) -> Transaction {
    serde_json::from_str(json).expect("valid tx json")
}

// ------------------------------------------------------------------- blocks

#[test]
fn block_id_matches_mainnet_empty_block() {
    let b = block(BLOCK_11704428);
    assert_eq!(block_id(&b).unwrap(), b.id.clone().unwrap());
    assert_eq!(get_id(&b).unwrap(), b.id.clone().unwrap());
}

#[test]
fn block_id_matches_mainnet_block_with_transactions() {
    let b = block(BLOCK_11704043);
    assert_eq!(b.get_id().unwrap(), "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d");
    let b2 = block(BLOCK_11521618);
    assert_eq!(b2.get_id().unwrap(), "4fc426e6314e9bcead265613106725c1aac6ec31b1d250665a8c286312775a61");
}

#[test]
fn block_signatures_verify_including_genesis() {
    for json in [BLOCK_11704428, BLOCK_11704043, BLOCK_11521618, GENESIS_HEADER] {
        let b = block(json);
        assert!(verify_block_signature(&b).unwrap(), "signature of block {} must verify", b.height);
    }
}

#[test]
fn tampered_block_fails_signature_and_id() {
    let mut b = block(BLOCK_11704428);
    b.timestamp += 1;
    assert!(!verify_block_signature(&b).unwrap());
    assert_ne!(block_id(&b).unwrap(), b.id.clone().unwrap());
}

#[test]
fn payload_hash_matches_transactions() {
    let empty = block(BLOCK_11704428);
    assert_eq!(block_payload_hash(&empty).unwrap(), empty.payload_hash);
    let one = block(BLOCK_11704043);
    assert_eq!(block_payload_hash(&one).unwrap(), one.payload_hash);
    let two = block(BLOCK_11521618);
    assert_eq!(block_payload_hash(&two).unwrap(), two.payload_hash);
}

#[test]
fn full_block_verification_passes_on_mainnet_blocks() {
    let net = Network::mainnet();
    for json in [BLOCK_11704428, BLOCK_11704043, BLOCK_11521618] {
        let v = verify_block(&block(json), &net);
        assert!(v.verified, "errors: {:?}", v.errors);
    }
}

#[test]
fn block_verification_detects_wrong_totals() {
    let net = Network::mainnet();
    let mut b = block(BLOCK_11704043);
    b.total_fee += 1;
    let v = verify_block(&b, &net);
    assert!(!v.verified);
    assert!(v.errors.iter().any(|e| e.contains("Invalid total fee")));
    assert!(v.errors.iter().any(|e| e.contains("block signature")));
}

#[test]
fn genesis_payload_hash_is_the_nethash() {
    let net = Network::mainnet();
    let g = block(GENESIS_HEADER);
    assert_eq!(g.payload_hash, net.nethash);
    // Header only (transactions not embedded): the count check fails, but the payload rule must not.
    let v = verify_block(&g, &net);
    assert!(v.errors.iter().any(|e| e.contains("Invalid number of transactions")));
    assert!(!v.errors.iter().any(|e| e.contains("payload hash")), "{:?}", v.errors);

    let mut bad = g.clone();
    bad.payload_hash = "00".repeat(32);
    let v = verify_block(&bad, &net);
    assert!(v.errors.iter().any(|e| e.contains("nethash")), "{:?}", v.errors);
}

#[test]
fn block_json_roundtrip_is_lossless() {
    let original: serde_json::Value = serde_json::from_str(BLOCK_11704043).unwrap();
    let b = block(BLOCK_11704043);
    let again: serde_json::Value = serde_json::to_value(&b).unwrap();
    assert_eq!(original, again);
}

// ------------------------------------------------------------- transactions

#[test]
fn transaction_ids_match_mainnet() {
    for json in [TX_TRANSFER, TX_TRANSFER_VENDORFIELD, TX_VOTE, TX_MULTIPAYMENT] {
        let t = tx(json);
        assert_eq!(transaction_id(&t).unwrap(), t.id.clone().unwrap(), "type {}", t.type_);
        assert_eq!(get_id(&t).unwrap(), t.id.clone().unwrap());
    }
}

#[test]
fn transaction_schnorr_signatures_verify() {
    for json in [TX_TRANSFER, TX_TRANSFER_VENDORFIELD, TX_VOTE, TX_MULTIPAYMENT] {
        let t = tx(json);
        assert!(verify_transaction_signature(&t).unwrap(), "type {}", t.type_);
        assert!(t.verify_signature().unwrap());
    }
}

#[test]
fn tampered_transaction_fails() {
    let mut t = tx(TX_TRANSFER);
    t.amount += 1;
    assert!(!verify_transaction_signature(&t).unwrap());
    assert_ne!(transaction_id(&t).unwrap(), t.id.clone().unwrap());

    let mut t2 = tx(TX_TRANSFER_VENDORFIELD);
    t2.vendor_field = Some("changed memo".into());
    assert!(!verify_transaction_signature(&t2).unwrap());
}

#[test]
fn transaction_json_roundtrip_is_lossless() {
    for json in [TX_TRANSFER, TX_TRANSFER_VENDORFIELD, TX_VOTE, TX_MULTIPAYMENT] {
        let original: serde_json::Value = serde_json::from_str(json).unwrap();
        let again = serde_json::to_value(tx(json)).unwrap();
        assert_eq!(original, again);
    }
}

#[test]
fn transaction_accepts_numeric_amounts_and_serializes_as_strings() {
    let t: Transaction = serde_json::from_str(
        r#"{"version":2,"typeGroup":1,"type":0,"nonce":5,"senderPublicKey":"036560f20d578da7b8433248d0c82e68121163958d533dc74b0e7d8dbabc0606a0","fee":100000000,"amount":42}"#,
    )
    .unwrap();
    assert_eq!(t.nonce, Some(5));
    let v = serde_json::to_value(&t).unwrap();
    assert_eq!(v["amount"], "42");
    assert_eq!(v["fee"], "100000000");
    assert_eq!(v["nonce"], "5");
}

// ----------------------------------------------------------------- addresses

#[test]
fn addresses_derive_from_public_keys_with_network_63() {
    assert_eq!(
        address_from_public_key("036560f20d578da7b8433248d0c82e68121163958d533dc74b0e7d8dbabc0606a0", 63).unwrap(),
        "SR1W4qS8DCPN65oV9Jd8JSLbfU5vhmEEky"
    );
    assert_eq!(
        address_from_public_key("034915c944effcaee31df0f8586655dd871be9eb443e148b8ac44bc3b7914f0bb1", 63).unwrap(),
        "SPTv5XL8W2Xbw9VmArWUKpBWdfQRKepM6R"
    );
    assert_eq!(
        address_from_public_key("02399ed18f1cc75e6b2d9f8c6b89fb4ff94cf73591983f0f78669800e2eaafaf5a", 63).unwrap(),
        "SR14LqgCqQMsxhZuxaJPS51HLeBqHVtPnH"
    );
    assert!(validate_address("SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw", 63));
    assert!(!validate_address("SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dx", 63));
    assert!(!validate_address("SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw", 30));
}

// --------------------------------------------------------- sign / verify loop

#[test]
fn schnorr_legacy_sign_verify_roundtrip() {
    let kp = KeyPair::from_passphrase("sth core rust test passphrase").unwrap();
    let hash = sha256(b"hello smartholdem");
    let sig = sign_schnorr_legacy(&hash, kp.private_key()).unwrap();
    assert_eq!(sig.len(), 128);
    let sig_bytes = hex::decode(&sig).unwrap();
    assert!(verify_schnorr_legacy(&hash, &sig_bytes, &kp.public_key_hex()).unwrap());
    assert!(verify_signature(&hash, &sig, &kp.public_key_hex()).unwrap());
    assert!(!verify_schnorr_legacy(&sha256(b"other"), &sig_bytes, &kp.public_key_hex()).unwrap());
}

#[test]
fn ecdsa_sign_verify_roundtrip() {
    let kp = KeyPair::from_passphrase("delegate passphrase").unwrap();
    let hash = sha256(b"block header bytes");
    let sig = sign_ecdsa(&hash, kp.private_key()).unwrap();
    let der = hex::decode(&sig).unwrap();
    assert!(verify_ecdsa(&hash, &der, &kp.public_key_hex()).unwrap());
    assert!(verify_signature(&hash, &sig, &kp.public_key_hex()).unwrap());
    assert!(!verify_ecdsa(&sha256(b"x"), &der, &kp.public_key_hex()).unwrap());
}

#[test]
fn freshly_signed_transfer_verifies_like_core() {
    let kp = KeyPair::from_passphrase("sender secret").unwrap();
    let mut t = tx(TX_TRANSFER);
    t.sender_public_key = kp.public_key_hex();
    t.nonce = Some(1);
    t.signature = None;
    t.id = None;
    let hash = transaction_signing_hash(&t).unwrap();
    t.signature = Some(sign_schnorr_legacy(&hash, kp.private_key()).unwrap());
    assert!(verify_transaction_signature(&t).unwrap());
    let id = transaction_id(&t).unwrap();
    assert_eq!(id.len(), 64);
}
