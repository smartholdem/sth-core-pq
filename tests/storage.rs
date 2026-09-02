//! Author: TechnoL0g
//!
//! Sled storage layer tests: block persistence, indexes, wallet state, atomic block application.

use sth_core::config::Network;
use sth_core::models::{Block, Transaction};
use sth_core::storage::Storage;

const BLOCK_11704043: &str = r#"{"id": "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d", "version": 0, "timestamp": 95101456, "previousBlock": "f7f523ce32716bc968383afff31c0def91acefa72df9ba71bb95ab506a0592a7", "height": 11704043, "numberOfTransactions": 1, "totalAmount": "314159265", "totalFee": "100000000", "reward": "0", "payloadLength": 32, "payloadHash": "fdee5b08437ad279fd6461bdfcb48fe8c36a5d394203566ba5bf33819fe2d2e2", "generatorPublicKey": "03d67017411e92a8e93d7b0318e2748f013c130d6e4e01b1248da0ee0959ee9df8", "blockSignature": "3045022100bfcfed36e8019c760490fd453cc28a2118241907d63c3ed0d3004687907107ff02200d300e64fdf5c5ca358e3266794b12b6b900004084b7fba838ceedcb1364e658",
"transactions": [{"version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": "10103", "senderPublicKey": "036560f20d578da7b8433248d0c82e68121163958d533dc74b0e7d8dbabc0606a0", "fee": "100000000", "amount": "314159265", "expiration": 0, "recipientId": "SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw", "signature": "1d311090b61358077d2f59972b0913ec3687ec82f8a7b752121df934b701a7bc07e3e0d7bf051bf939a5291790ae5ed43ed59d1b6feb8dda0f76f07f747d8601", "id": "596236ba37bc2d419f5825b2d771a74e151e7719923afefdef0b0baa9a8cfcb8", "blockId": "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d", "blockHeight": 11704043, "sequence": 0}]}"#;

const BLOCK_11704428: &str = r#"{"id": "ee83dd37a1ee0df793de24371a911dc75a8ca41212e122b93e816f37206fc870", "version": 0, "timestamp": 95104536, "previousBlock": "6baf15c3d7f3a2203953e44e766d44646dcc2aada206f420aabe4fe73693c4fb", "height": 11704428, "numberOfTransactions": 0, "totalAmount": "0", "totalFee": "0", "reward": "0", "payloadLength": 0, "payloadHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "generatorPublicKey": "023bff30613a00d39f9190c6d8460fd4bf01efbe0949955c62b0dbad547047d16a", "blockSignature": "3044022025929db39d4e345285afa080d7e669b0eb93dd5bccbcfa5fdd740cbfd5c0d73102207cb21e12db735ab79ff62495539c8b2bed946fcbdb9eb1aeca69b19ec938b8bd"}"#;

const TX_MULTIPAYMENT: &str = r#"{"version": 2, "network": 63, "typeGroup": 1, "type": 6, "nonce": "19", "senderPublicKey": "03bd4bf7d18df019d500b348670b2979c980ba79a33fe34bb87d405c076e7d470d", "fee": "10000000", "amount": "0", "vendorField": "seed:reward", "asset": {"payments": [{"amount": "118996409386", "recipientId": "SboZHv8y3ohGbA5ophSCifZA5zCiD6rQyq"}, {"amount": "213789310260", "recipientId": "SSU6TvycebBMTvc8WHiKTfG9xmX1QSniZu"}]}, "signature": "d13845987e802560f4e9ecf154c58244a437f493bae2b123581eaa449d408916d8447f02fd1c43f0e77adf5380539580ae0229fb73dfa93454e4abb9680f6ad1", "id": "25820be000f73506741533e84ddfb731cfa0a2b6d39500a7b7864b91f92556e2", "blockId": "b15263bead74cc4a7df6dd7d2f7a5ed0a89c744f7d2cc69c8c0fe542f5dc20b2", "blockHeight": 11661263, "sequence": 0}"#;

const SENDER: &str = "SR1W4qS8DCPN65oV9Jd8JSLbfU5vhmEEky";
const RECIPIENT: &str = "SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw";
/// Address of generator 03d67017…
const GENERATOR: &str = "SXsQxVCEux4kkRFNrQJiQg6JT9K6SKLgS7";

fn store() -> Storage {
    Storage::temporary(Network::mainnet()).unwrap()
}

fn block(json: &str) -> Block {
    serde_json::from_str(json).unwrap()
}

#[test]
fn save_and_load_blocks_by_height_and_id() {
    let s = store();
    assert_eq!(s.get_last_height().unwrap(), 0);
    assert!(s.get_last_block().unwrap().is_none());

    let b = block(BLOCK_11704043);
    s.save_block(&b).unwrap();

    let loaded = s.get_block_by_height(11704043).unwrap().unwrap();
    assert_eq!(loaded, b);
    let by_id = s.get_block_by_id(b.id.as_deref().unwrap()).unwrap().unwrap();
    assert_eq!(by_id.height, 11704043);
    assert_eq!(s.get_last_height().unwrap(), 11704043);
    assert_eq!(s.get_last_block().unwrap().unwrap().id, b.id);
    assert!(s.get_block_by_height(1).unwrap().is_none());
    assert!(s.get_block_by_id("deadbeef").unwrap().is_none());
}

#[test]
fn last_block_tracks_highest_height_and_pagination_is_descending() {
    let s = store();
    s.save_block(&block(BLOCK_11704428)).unwrap();
    s.save_block(&block(BLOCK_11704043)).unwrap();
    assert_eq!(s.get_last_height().unwrap(), 11704428);
    assert_eq!(s.block_count(), 2);

    let page = s.get_blocks(0, 10).unwrap();
    assert_eq!(page.iter().map(|b| b.height).collect::<Vec<_>>(), vec![11704428, 11704043]);
    let page2 = s.get_blocks(1, 1).unwrap();
    assert_eq!(page2[0].height, 11704043);

    let asc = s.get_blocks_from(11704043, 1_000).unwrap();
    assert_eq!(asc.iter().map(|b| b.height).collect::<Vec<_>>(), vec![11704043, 11704428]);
}

#[test]
fn transaction_secondary_index_resolves_full_transaction() {
    let s = store();
    s.save_block(&block(BLOCK_11704043)).unwrap();
    let t = s
        .get_transaction("596236ba37bc2d419f5825b2d771a74e151e7719923afefdef0b0baa9a8cfcb8")
        .unwrap()
        .unwrap();
    assert_eq!(t.amount, 314159265);
    assert_eq!(t.recipient_id.as_deref(), Some(RECIPIENT));
    assert!(s.get_transaction("00").unwrap().is_none());
}

#[test]
fn update_wallet_state_applies_deltas_atomically() {
    let s = store();
    assert!(s.get_wallet(SENDER).unwrap().is_none());
    let w = s.update_wallet_state(SENDER, 1_000, 1).unwrap();
    assert_eq!(w.balance, 1_000);
    assert_eq!(w.nonce, 1);
    let w = s.update_wallet_state(SENDER, -400, 2).unwrap();
    assert_eq!(w.balance, 600);
    assert_eq!(w.nonce, 3);
    let stored = s.get_wallet(SENDER).unwrap().unwrap();
    assert_eq!(stored, w);

    let json = serde_json::to_value(&stored).unwrap();
    assert_eq!(json["balance"], "600");
    assert_eq!(json["nonce"], "3");
    assert_eq!(json["address"], SENDER);
}

#[test]
fn apply_block_moves_funds_fees_and_nonces() {
    let s = store();
    s.update_wallet_state(SENDER, 1_000_000_000, 10_102).unwrap();

    let b = block(BLOCK_11704043);
    s.apply_block(&b).unwrap();

    let sender = s.get_wallet(SENDER).unwrap().unwrap();
    assert_eq!(sender.balance, 1_000_000_000 - 314_159_265 - 100_000_000);
    assert_eq!(sender.nonce, 10_103);
    assert_eq!(sender.public_key.as_deref(), Some("036560f20d578da7b8433248d0c82e68121163958d533dc74b0e7d8dbabc0606a0"));

    let recipient = s.get_wallet(RECIPIENT).unwrap().unwrap();
    assert_eq!(recipient.balance, 314_159_265);
    assert_eq!(recipient.nonce, 0);

    let generator = s.get_wallet(GENERATOR).unwrap().unwrap();
    assert_eq!(generator.balance, 100_000_000);

    assert_eq!(s.get_last_height().unwrap(), 11704043);
    assert!(s.get_transaction("596236ba37bc2d419f5825b2d771a74e151e7719923afefdef0b0baa9a8cfcb8").unwrap().is_some());
}

#[test]
fn apply_block_handles_multipayment() {
    let s = store();
    let tx: Transaction = serde_json::from_str(TX_MULTIPAYMENT).unwrap();
    let mut b = block(BLOCK_11704428);
    b.transactions = vec![tx];
    b.number_of_transactions = 1;
    s.apply_block(&b).unwrap();

    let a = s.get_wallet("SboZHv8y3ohGbA5ophSCifZA5zCiD6rQyq").unwrap().unwrap();
    let c = s.get_wallet("SSU6TvycebBMTvc8WHiKTfG9xmX1QSniZu").unwrap().unwrap();
    assert_eq!(a.balance, 118_996_409_386);
    assert_eq!(c.balance, 213_789_310_260);

    let sender_addr = sth_core::crypto::address_from_public_key(
        "03bd4bf7d18df019d500b348670b2979c980ba79a33fe34bb87d405c076e7d470d",
        63,
    )
    .unwrap();
    let sender = s.get_wallet(&sender_addr).unwrap().unwrap();
    assert_eq!(sender.balance, -(118_996_409_386 + 213_789_310_260 + 10_000_000));
    assert_eq!(sender.nonce, 1);
}

#[test]
fn persistent_database_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        s.apply_block(&block(BLOCK_11704043)).unwrap();
        s.flush().unwrap();
    }
    let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
    assert_eq!(s.get_last_height().unwrap(), 11704043);
    assert_eq!(s.get_wallet(RECIPIENT).unwrap().unwrap().balance, 314_159_265);
}
