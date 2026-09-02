//! Author: TechnoL0g
//!
//! Offline tests for the sync apply path (`apply_blocks`): chain linkage, verification, resume tip.

use sth_core::config::Network;
use sth_core::models::Block;
use sth_core::storage::Storage;
use sth_core::sync::{apply_blocks, ChainTip, SyncConfig, Syncer, NODES};
use std::sync::Arc;

const BLOCK_11704043: &str = r#"{"id": "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d", "version": 0, "timestamp": 95101456, "previousBlock": "f7f523ce32716bc968383afff31c0def91acefa72df9ba71bb95ab506a0592a7", "height": 11704043, "numberOfTransactions": 1, "totalAmount": "314159265", "totalFee": "100000000", "reward": "0", "payloadLength": 32, "payloadHash": "fdee5b08437ad279fd6461bdfcb48fe8c36a5d394203566ba5bf33819fe2d2e2", "generatorPublicKey": "03d67017411e92a8e93d7b0318e2748f013c130d6e4e01b1248da0ee0959ee9df8", "blockSignature": "3045022100bfcfed36e8019c760490fd453cc28a2118241907d63c3ed0d3004687907107ff02200d300e64fdf5c5ca358e3266794b12b6b900004084b7fba838ceedcb1364e658",
"transactions": [{"version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": "10103", "senderPublicKey": "036560f20d578da7b8433248d0c82e68121163958d533dc74b0e7d8dbabc0606a0", "fee": "100000000", "amount": "314159265", "expiration": 0, "recipientId": "SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw", "signature": "1d311090b61358077d2f59972b0913ec3687ec82f8a7b752121df934b701a7bc07e3e0d7bf051bf939a5291790ae5ed43ed59d1b6feb8dda0f76f07f747d8601", "id": "596236ba37bc2d419f5825b2d771a74e151e7719923afefdef0b0baa9a8cfcb8", "blockId": "53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d", "blockHeight": 11704043, "sequence": 0}]}"#;

fn block() -> Block {
    serde_json::from_str(BLOCK_11704043).unwrap()
}

fn store() -> Storage {
    Storage::temporary(Network::mainnet()).unwrap()
}

#[test]
fn applies_linked_block_and_advances_tip() {
    let s = store();
    let b = block();
    let tip = ChainTip { height: b.height - 1, id: Some(b.previous_block.clone()) };
    let new_tip = apply_blocks(&s, s.network(), &[b.clone()], tip, true).unwrap();
    assert_eq!(new_tip, ChainTip { height: b.height, id: b.id.clone() });
    assert_eq!(s.get_last_height().unwrap(), b.height);
}

#[test]
fn rejects_height_gap() {
    let s = store();
    let b = block();
    let tip = ChainTip { height: b.height - 2, id: None };
    let err = apply_blocks(&s, s.network(), &[b], tip, true).unwrap_err();
    assert!(err.to_string().contains("height gap"), "{err}");
    assert_eq!(s.get_last_height().unwrap(), 0);
}

#[test]
fn rejects_broken_chain_link() {
    let s = store();
    let b = block();
    let tip = ChainTip { height: b.height - 1, id: Some("00".repeat(32)) };
    let err = apply_blocks(&s, s.network(), &[b], tip, true).unwrap_err();
    assert!(err.to_string().contains("does not match local tip"), "{err}");
}

#[test]
fn rejects_tampered_block_even_without_full_verify() {
    let s = store();
    let mut b = block();
    b.total_fee += 1; // id no longer matches
    let tip = ChainTip { height: b.height - 1, id: Some(b.previous_block.clone()) };
    let err = apply_blocks(&s, s.network(), &[b], tip, false).unwrap_err();
    assert!(err.to_string().contains("computed"), "{err}");
}

#[test]
fn syncer_reads_local_tip_for_resume() {
    let s = Arc::new(store());
    s.apply_block(&block()).unwrap();
    let cfg = SyncConfig { quiet: true, ..SyncConfig::default() };
    assert_eq!(cfg.nodes.len(), NODES.len());
    let syncer = Syncer::new(s, cfg).unwrap();
    let tip = syncer.local_tip().unwrap();
    assert_eq!(tip.height, 11704043);
    assert_eq!(tip.id.as_deref(), Some("53423dd1a74cddcc8643084c605836cb7dd5f5993455796daedfefb5e4eb070d"));
}
