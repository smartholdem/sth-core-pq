//! Author: TechnoL0g
//!
//! Snapshot import tests on a synthetic dump written in the exact core-snapshots format
//! (gzip streams of `[u32 LE len][record]`, msgpack transaction records).

use flate2::write::GzEncoder;
use flate2::Compression;
use indicatif::ProgressBar;
use sth_core::config::Network;
use sth_core::crypto::{block_payload_hash, serialize_block, serialize_transaction, SerializeOptions};
use sth_core::models::{Block, Transaction};
use sth_core::snapshot::{
    decode_transaction_record, import_snapshot, locate_snapshot_dir, pick_latest_archive, ImportOptions,
    RecordReader, SnapshotMeta,
};
use sth_core::storage::Storage;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;

const TX_TRANSFER: &str = r#"{"version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": "10103", "senderPublicKey": "036560f20d578da7b8433248d0c82e68121163958d533dc74b0e7d8dbabc0606a0", "fee": "100000000", "amount": "314159265", "expiration": 0, "recipientId": "SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw", "signature": "1d311090b61358077d2f59972b0913ec3687ec82f8a7b752121df934b701a7bc07e3e0d7bf051bf939a5291790ae5ed43ed59d1b6feb8dda0f76f07f747d8601", "id": "596236ba37bc2d419f5825b2d771a74e151e7719923afefdef0b0baa9a8cfcb8"}"#;

const GENERATOR: &str = "0205dc9ea85e527cb739c8fad48a3c26cb881a2df444df02f3c908e98e1705cb09";
const EMPTY_PAYLOAD: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// Structurally valid DER blob (0x30, len 6) — enough for the header codec; `fast` import skips signatures.
const FAKE_SIG: &str = "3006020101020101";

fn header(height: u64, previous: &str, txs: Vec<Transaction>) -> Block {
    let mut b = Block {
        id: None,
        version: 0,
        timestamp: height as u32 * 8,
        previous_block: previous.to_string(),
        height,
        number_of_transactions: txs.len() as u32,
        total_amount: txs.iter().map(|t| t.amount).sum(),
        total_fee: txs.iter().map(|t| t.fee).sum(),
        reward: 0,
        payload_length: 32 * txs.len() as u32,
        payload_hash: EMPTY_PAYLOAD.into(),
        generator_public_key: GENERATOR.into(),
        block_signature: Some(FAKE_SIG.into()),
        transactions: txs,
    };
    if height == 1 {
        b.payload_hash = Network::mainnet().nethash.to_string();
    } else if !b.transactions.is_empty() {
        b.payload_hash = block_payload_hash(&b).unwrap();
    }
    b.id = Some(if height == 1 {
        Network::mainnet().genesis_block_id.to_string()
    } else {
        sth_core::crypto::block_id(&b).unwrap()
    });
    b
}

fn write_records(path: &Path, records: &[Vec<u8>]) {
    let mut gz = GzEncoder::new(std::fs::File::create(path).unwrap(), Compression::fast());
    for r in records {
        gz.write_all(&(r.len() as u32).to_le_bytes()).unwrap();
        gz.write_all(r).unwrap();
    }
    gz.finish().unwrap();
}

/// Three-block chain: genesis → empty block → block with one real transaction.
fn build_snapshot(dir: &Path) -> Vec<Block> {
    let net = Network::mainnet();
    let tx: Transaction = serde_json::from_str(TX_TRANSFER).unwrap();
    let g = header(1, &"0".repeat(64), vec![]);
    let b2 = header(2, g.id.as_deref().unwrap(), vec![]);
    let b3 = header(3, b2.id.as_deref().unwrap(), vec![tx.clone()]);

    let blocks = vec![g, b2, b3];
    let block_records: Vec<Vec<u8>> = blocks.iter().map(|b| serialize_block(b, true).unwrap()).collect();
    write_records(&dir.join("blocks"), &block_records);

    let serialized = serialize_transaction(&tx, SerializeOptions::default(), &net).unwrap();
    let record = rmpv::Value::Array(vec![
        rmpv::Value::from(tx.id.clone().unwrap()),
        rmpv::Value::from(blocks[2].id.clone().unwrap()),
        rmpv::Value::from(3u64),
        rmpv::Value::from(0u64),
        rmpv::Value::from(24u64),
        rmpv::Value::Binary(serialized),
    ]);
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &record).unwrap();
    write_records(&dir.join("transactions"), &[buf]);
    write_records(&dir.join("rounds"), &[]);

    std::fs::write(
        dir.join("meta.json"),
        r#"{"blocks":{"count":3,"start":1,"end":3},"transactions":{"count":1,"start":0,"end":24},"rounds":{"count":0,"start":1,"end":1},"folder":"1-3","skipCompression":false,"network":"mainnet","packageVersion":"3.8.2","codec":"default"}"#,
    )
    .unwrap();
    blocks
}

#[test]
fn record_reader_and_transaction_record_decode() {
    let tmp = tempfile::tempdir().unwrap();
    build_snapshot(tmp.path());
    let meta = SnapshotMeta::read(tmp.path()).unwrap();
    assert_eq!(meta.blocks.end, 3);
    assert!(!meta.skip_compression);

    let mut r = RecordReader::open(&tmp.path().join("blocks"), true).unwrap();
    let mut n = 0;
    while r.next_record().unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, 3);

    let mut t = RecordReader::open(&tmp.path().join("transactions"), true).unwrap();
    let rec = t.next_record().unwrap().unwrap();
    let tx = decode_transaction_record(&rec).unwrap();
    assert_eq!(tx.id.as_deref(), Some("596236ba37bc2d419f5825b2d771a74e151e7719923afefdef0b0baa9a8cfcb8"));
    assert_eq!(tx.block_height, Some(3));
    assert_eq!(tx.sequence, Some(0));
    assert_eq!(tx.amount, 314159265);
}

#[test]
fn fast_import_applies_chain_and_is_resumable() {
    let tmp = tempfile::tempdir().unwrap();
    let blocks = build_snapshot(tmp.path());
    let storage = Storage::temporary(Network::mainnet()).unwrap();
    let pb = ProgressBar::hidden();
    let cancel = AtomicBool::new(false);

    let report = import_snapshot(&storage, tmp.path(), ImportOptions { fast: true }, &pb, &cancel).unwrap();
    assert_eq!((report.imported, report.skipped, report.end_height), (3, 0, 3));
    assert!(!report.interrupted);
    assert_eq!(storage.get_last_block().unwrap().unwrap().id, blocks[2].id);
    assert_eq!(storage.get_block_by_height(1).unwrap().unwrap().id.as_deref(), Some(Network::mainnet().genesis_block_id));
    assert_eq!(storage.get_wallet("SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw").unwrap().unwrap().balance, 314159265);
    assert!(storage.get_transaction("596236ba37bc2d419f5825b2d771a74e151e7719923afefdef0b0baa9a8cfcb8").unwrap().is_some());

    // second run: everything already applied
    let again = import_snapshot(&storage, tmp.path(), ImportOptions { fast: true }, &pb, &cancel).unwrap();
    assert_eq!((again.imported, again.skipped), (0, 3));
}

#[test]
fn full_verification_rejects_fake_signatures() {
    let tmp = tempfile::tempdir().unwrap();
    build_snapshot(tmp.path());
    let storage = Storage::temporary(Network::mainnet()).unwrap();
    let err = import_snapshot(&storage, tmp.path(), ImportOptions { fast: false }, &ProgressBar::hidden(), &AtomicBool::new(false))
        .unwrap_err();
    assert!(err.to_string().contains("block 1"), "{err}");
    assert_eq!(storage.get_last_height().unwrap(), 0);
}

#[test]
fn locate_dir_and_latest_archive_parsing() {
    let tmp = tempfile::tempdir().unwrap();
    let inner = tmp.path().join("1-3");
    std::fs::create_dir(&inner).unwrap();
    build_snapshot(&inner);
    assert_eq!(locate_snapshot_dir(tmp.path()).unwrap(), inner);
    assert_eq!(locate_snapshot_dir(&inner).unwrap(), inner);

    let html = r#"<h2>SmartHoldem Blockchain Snapshots</h2>
<p><a href="1-8133951.tgz">1-8133951.tgz</a> [07.10.2025]</p>
<p><a href="1-6833820.tgz">1-6322323.tgz</a> [01.06.2025]</p>
<p><a href="1-11705253.tgz">1-11705253.tgz</a> [02.09.2026]</p>
<p><a href="readme.txt">readme</a></p>"#;
    assert_eq!(pick_latest_archive(html), Some(("1-11705253.tgz".to_string(), 11705253)));
    assert_eq!(pick_latest_archive("<p>nothing</p>"), None);
}
