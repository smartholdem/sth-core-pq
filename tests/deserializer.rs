//! Author: TechnoL0g
//!
//! Deserialiser round-trips against real mainnet objects: bytes → struct must reproduce the
//! exact JSON the legacy API returns, and header bytes must reproduce the block id.

use sth_core::config::Network;
use sth_core::crypto::{
    deserialize_block_header, deserialize_transaction, serialize_block, serialize_transaction, SerializeOptions,
};
use sth_core::models::{Block, Transaction};

const TX_TRANSFER_VENDORFIELD: &str = r#"{"version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": "7", "senderPublicKey": "034915c944effcaee31df0f8586655dd871be9eb443e148b8ac44bc3b7914f0bb1", "fee": "1000000", "amount": "1", "vendorField": "xkey:4axrTHl6MRI3Z74J95dmwHWIEetFEq0SFZN4x2z3ZW4=|ef251d59259d2f8bc08073cbe9a083f07be1bcb2ba9da5819714c67b2988c40c", "expiration": 0, "recipientId": "SPTv5XL8W2Xbw9VmArWUKpBWdfQRKepM6R", "signature": "f7acebc21dc7b71866272a4924441811f6cb36ea5976a520da0423879ed7391990dae255e76242874940d389772e566ee797928ed0e25aa1985918ea1c40eb9f", "id": "3f5861bbfc8f04f070eb22bb98501f7b808431dee7d05fdfa5760c837704c0b4", "blockId": "4570db7f5f304eff1eba9e7e67180eafd9a2ee087b8cdc1f00fecb1992781a59", "blockHeight": 11704006, "sequence": 0}"#;

const TX_VOTE: &str = r#"{"version": 2, "network": 63, "typeGroup": 1, "type": 3, "nonce": "8", "senderPublicKey": "02d9afa5ad1ed3f80032a09537a31bbaa3f3463274e7e58989a0506ca281047065", "fee": "100000000", "amount": "0", "asset": {"votes": ["-0344efe631ac747d1031202dcb1e6253436caaa4da75ae133a7b7e9890bd36f20b", "+0344efe631ac747d1031202dcb1e6253436caaa4da75ae133a7b7e9890bd36f20b"]}, "signature": "0e94b810faa0b1879d40bd94d9f0402ba5de671d133de0b0038fa954eb9e7904aa92083bfcf885c21d5820a2fbad38936586455a78571f3bdedd2bc80e97e52a", "id": "2bef9de12adc3207d9bfe1f9e2b7c2987902277b7b5aa4ecb0da1bc648501423", "blockId": "01d0502aea836b3c786b9552d226f3c0e91aee8f98c0bde0f91cee235b057c6c", "blockHeight": 9662005, "sequence": 0}"#;

const TX_MULTIPAYMENT: &str = r#"{"version": 2, "network": 63, "typeGroup": 1, "type": 6, "nonce": "19", "senderPublicKey": "03bd4bf7d18df019d500b348670b2979c980ba79a33fe34bb87d405c076e7d470d", "fee": "10000000", "amount": "0", "vendorField": "seed:reward", "asset": {"payments": [{"amount": "118996409386", "recipientId": "SboZHv8y3ohGbA5ophSCifZA5zCiD6rQyq"}, {"amount": "213789310260", "recipientId": "SSU6TvycebBMTvc8WHiKTfG9xmX1QSniZu"}]}, "signature": "d13845987e802560f4e9ecf154c58244a437f493bae2b123581eaa449d408916d8447f02fd1c43f0e77adf5380539580ae0229fb73dfa93454e4abb9680f6ad1", "id": "25820be000f73506741533e84ddfb731cfa0a2b6d39500a7b7864b91f92556e2", "blockId": "b15263bead74cc4a7df6dd7d2f7a5ed0a89c744f7d2cc69c8c0fe542f5dc20b2", "blockHeight": 11661263, "sequence": 0}"#;

const BLOCK_11704043: &str = r#"{"id": "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d", "version": 0, "timestamp": 95101456, "previousBlock": "f7f523ce32716bc968383afff31c0def91acefa72df9ba71bb95ab506a0592a7", "height": 11704043, "numberOfTransactions": 1, "totalAmount": "314159265", "totalFee": "100000000", "reward": "0", "payloadLength": 32, "payloadHash": "fdee5b08437ad279fd6461bdfcb48fe8c36a5d394203566ba5bf33819fe2d2e2", "generatorPublicKey": "03d67017411e92a8e93d7b0318e2748f013c130d6e4e01b1248da0ee0959ee9df8", "blockSignature": "3045022100bfcfed36e8019c760490fd453cc28a2118241907d63c3ed0d3004687907107ff02200d300e64fdf5c5ca358e3266794b12b6b900004084b7fba838ceedcb1364e658"}"#;

const GENESIS_HEADER: &str = r#"{"id": "ea60ebb15e3e8abe9e47a7ef18145d65c570c3dfe2b0ac678c665787860bae32", "version": 0, "timestamp": 0, "previousBlock": "0000000000000000000000000000000000000000000000000000000000000000", "height": 1, "numberOfTransactions": 1855, "totalAmount": "24977000000000000", "totalFee": "0", "reward": "0", "payloadLength": 288953, "payloadHash": "b0987b45a3a754da1362bc6818548cb34f65750c9ac81d28c93e7545224df2d2", "generatorPublicKey": "0205dc9ea85e527cb739c8fad48a3c26cb881a2df444df02f3c908e98e1705cb09", "blockSignature": "3044022028c1c8a5dfbca4887542233d51736c41258263c8f4367cd91c85d2a7ee26d4b7022079f4bce6d3b62ea91adfe207fd682619a1d3696e06f9907f892519eb865206e2"}"#;

#[test]
fn transaction_bytes_roundtrip_reproduces_api_json() {
    let net = Network::mainnet();
    for json in [TX_TRANSFER_VENDORFIELD, TX_VOTE, TX_MULTIPAYMENT] {
        let original: Transaction = serde_json::from_str(json).unwrap();
        let bytes = serialize_transaction(&original, SerializeOptions::default(), &net).unwrap();
        let mut decoded = deserialize_transaction(&bytes).unwrap();
        assert_eq!(decoded.id, original.id, "id must be reproduced from bytes");
        // block coordinates are not part of the wire format
        decoded.block_id = original.block_id.clone();
        decoded.block_height = original.block_height;
        decoded.sequence = original.sequence;
        // votes / multipayment carry no `expiration` in the API JSON
        if original.expiration.is_none() {
            decoded.expiration = None;
        }
        assert_eq!(serde_json::to_value(&decoded).unwrap(), serde_json::from_str::<serde_json::Value>(json).unwrap());
    }
}

#[test]
fn block_header_roundtrip_reproduces_id() {
    let net = Network::mainnet();
    let original: Block = serde_json::from_str(BLOCK_11704043).unwrap();
    let bytes = serialize_block(&original, true).unwrap();
    let decoded = deserialize_block_header(&bytes, &net).unwrap();
    assert_eq!(decoded.id, original.id);
    assert_eq!(decoded.header(), original.header());
}

#[test]
fn genesis_header_gets_configured_id() {
    let net = Network::mainnet();
    let original: Block = serde_json::from_str(GENESIS_HEADER).unwrap();
    let bytes = serialize_block(&original, true).unwrap();
    let decoded = deserialize_block_header(&bytes, &net).unwrap();
    assert_eq!(decoded.id.as_deref(), Some(net.genesis_block_id.as_str()));
    assert_eq!(decoded.number_of_transactions, 1855);
}

#[test]
fn trailing_bytes_and_bad_marker_are_rejected() {
    let net = Network::mainnet();
    let original: Block = serde_json::from_str(BLOCK_11704043).unwrap();
    let mut bytes = serialize_block(&original, true).unwrap();
    bytes.push(0x00);
    assert!(deserialize_block_header(&bytes, &net).is_err());
    assert!(deserialize_transaction(&[0x00, 0x02, 0x3f]).is_err());
}
