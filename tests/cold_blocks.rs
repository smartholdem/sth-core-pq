//! Author: TechnoL0g
//!
//! Cold block store: blocks deeper than the keep margin move from sled to flat files (compressed since 0.27.0). Every reader must return
//! exactly what it returned before, an interrupted move must resume, garbage after a crash must be cut off, and the
//! rebuilt main database must hold the same state.

use serde_json::Value;
use sth_core::api::{serve_listener, AppState};
use sth_core::config::Network;
use sth_core::crypto::{transaction_id, KeyPair};
use sth_core::mempool::Mempool;
use sth_core::models::Block;
use sth_core::storage::{ColdOptions, HistoryOptions, Storage, StorageOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const BLOCKS: u64 = 3_000;
const SIG: &str = "3045022100bfcfed36e8019c760490fd453cc28a2118241907d63c3ed0d3004687907107ff02200d300e64fdf5c5ca358e3266794b12b6b900004084b7fba838ceedcb1364e658";

fn opts(rebuild: bool) -> StorageOptions {
    StorageOptions { cache_mb: 16, history: HistoryOptions { enabled: false, ..Default::default() }, cold: ColdOptions { rebuild_after: 1_000, ..Default::default() }, rebuild }
}

fn block(h: u64, prev: &str, pk: &str, to: &str) -> Block {
    let txs = if h % 10 == 0 {
        format!(
            r#"{{"version":2,"network":63,"typeGroup":1,"type":0,"nonce":"{}","senderPublicKey":"{pk}","fee":"100000000","amount":"{}","recipientId":"{to}","expiration":0,"signature":"{}","asset":null}}"#,
            h / 10,
            h * 1000,
            "1".repeat(128)
        )
    } else {
        String::new()
    };
    let id = "0".repeat(64);
    let mut b: Block = serde_json::from_str(&format!(
        r#"{{"id":"{id}","version":0,"timestamp":{},"previousBlock":"{prev}","height":{h},"numberOfTransactions":{},"totalAmount":"0","totalFee":"0","reward":"0","payloadLength":0,"payloadHash":"{}","generatorPublicKey":"{pk}","blockSignature":"{SIG}","transactions":[{txs}]}}"#,
        h * 8,
        usize::from(h % 10 == 0),
        "9".repeat(64)
    ))
    .unwrap();
    for tx in b.transactions.iter_mut() {
        tx.id = Some(transaction_id(tx).unwrap());
    }
    // the real id (hash of the header), as on a verified chain
    // (height 1 always decodes to the network's genesis id)
    b.id = Some(if h == 1 { Network::mainnet().genesis_block_id } else { sth_core::crypto::block_id(&b).unwrap() });
    b
}

/// Everything the block readers return, as one comparable value.
fn fingerprint(s: &Storage, ids: &[String]) -> Value {
    let j = |b: &Block| serde_json::to_value(b).unwrap();
    let tip = s.get_last_height().unwrap();
    let mut by_height = Vec::new();
    for h in 0..=tip + 1 {
        by_height.push(s.get_block_by_height(h).unwrap().as_ref().map(j));
    }
    let by_id: Vec<Value> = ids.iter().step_by(7).map(|id| s.get_block_by_id(id).unwrap().as_ref().map(j).unwrap_or(Value::Null)).collect();
    let page = |o, l| s.get_blocks(o, l).unwrap().iter().map(|b| b.height).collect::<Vec<_>>();
    let from = |f, l| s.get_blocks_from(f, l).unwrap().iter().map(|b| b.height).collect::<Vec<_>>();
    let txs: Vec<Value> = s.transaction_index(None, 0..=u64::MAX, true).map(|e| serde_json::to_value(s.get_transaction(&e.unwrap().id).unwrap().unwrap()).unwrap()).collect();
    let audit = s.audit_all(|_| {}).unwrap();
    serde_json::json!({
        "byHeight": by_height, "byId": by_id, "unknownId": s.get_block_by_id(&"0".repeat(64)).unwrap().is_some(),
        "pages": [page(0, 10), page(95, 10), page(100, 10), page(1500, 5), page(2995, 10), page(3000, 5)],
        "from": [from(1, 5), from(2890, 30), from(2990, 400), from(0, 3), from(5000, 3)],
        "first": s.first_height().unwrap(), "count": s.block_count(), "last": j(&s.get_last_block().unwrap().unwrap()),
        "atTs": [s.height_at_or_after(8).unwrap(), s.height_at_or_after(12_001).unwrap(), s.height_at_or_after(23_990).unwrap()],
        "txs": txs, "audit": [audit.height, audit.wallets, audit.checked, audit.drifted.len() as u64],
    })
}

async fn api(storage: Arc<Storage>, paths: &[String]) -> Vec<Value> {
    let mempool = Arc::new(Mempool::new(storage.clone(), vec![], 100));
    let state = Arc::new(AppState::new(storage, mempool, vec![]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { serve_listener(state, listener).await.unwrap() });
    let mut out = Vec::new();
    for p in paths {
        out.push(reqwest::get(format!("{base}{p}")).await.unwrap().json().await.unwrap());
    }
    server.abort();
    let _ = server.await;
    out
}

/// The API server's connection tasks hold the storage for a moment after the last request.
async fn release(s: Arc<Storage>) {
    let mut s = Some(s);
    for _ in 0..200 {
        match Arc::try_unwrap(s.take().unwrap()) {
            Ok(storage) => {
                storage.flush().unwrap();
                return;
            }
            Err(again) => s = Some(again),
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("storage still shared");
}

#[tokio::test]
async fn cold_blocks_keep_every_reader_identical_and_survive_interruptions() {
    let dir = tempfile::tempdir().unwrap();
    let kp = KeyPair::from_passphrase("cold sender").unwrap();
    let (pk, addr) = (kp.public_key_hex(), kp.address(63).unwrap());
    let never = AtomicBool::new(false);
    let mut ids = Vec::new();
    let mut chain = Vec::new();
    {
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        s.update_wallet_state(&addr, 1_000_000_000, 0).unwrap();
        let mut prev = "f".repeat(64);
        for h in 1..=BLOCKS + 1 {
            let b = block(h, &prev, &pk, &addr);
            prev = b.id.clone().unwrap();
            if h <= BLOCKS {
                s.save_block(&b).unwrap();
                ids.push(prev.clone());
            }
            chain.push(b);
        }
        assert!(!s.cold_status().unwrap().enabled);
        s.flush().unwrap();
    }
    let paths: Vec<String> = vec![
        "/api/blocks?limit=5".into(),
        "/api/blocks?limit=20&page=148".into(),
        "/api/blocks/1500".into(),
        format!("/api/blocks/{}", ids[41]),
        format!("/api/blocks/{}/transactions", ids[1499]),
        "/api/blocks?height=77".into(),
        "/api/transactions?limit=100&page=3".into(),
        format!("/api/wallets/{addr}/transactions?limit=5&orderBy=timestamp:asc"),
        "/api/blockchain".into(),
    ];
    let (before, api_before) = {
        let s = Arc::new(Storage::open(dir.path(), Network::mainnet()).unwrap());
        let out = (fingerprint(&s, &ids), api(s.clone(), &paths).await);
        release(s).await;
        out
    };
    assert_eq!(api_before[2]["data"]["height"], 1500);

    // first pass: interrupted after 20 batches of 100 blocks
    {
        let s = Arc::new(Storage::open_full(dir.path(), Network::mainnet(), opts(false)).unwrap());
        assert_eq!(s.cold_status().unwrap().pending_blocks, BLOCKS - 1_100, "default margin: the newest 1 100 blocks stay in sled");
        s.set_cold_tuning(100, 100);
        assert!(s.cold_due());
        let cancel = AtomicBool::new(false);
        assert_eq!(s.archive_blocks(&cancel, |_| cancel.store(true, Ordering::Relaxed)).unwrap(), 2_000);
        let st = s.cold_status().unwrap();
        assert_eq!((st.blocks, st.first_height, st.last_height, st.hot_blocks, st.pending_blocks), (2_000, 1, 2_000, 1_000, 0));
        assert!(st.segments <= 8, "id segments are merged in tiers, got {}", st.segments);
        assert_eq!(fingerprint(&s, &ids), before, "half-moved chain");
        assert_eq!(api(s.clone(), &paths).await, api_before, "API on the half-moved chain");
        assert!(s.rollback_last_block().is_err(), "no undo records here, but never a silent success");
        release(s).await;
    }
    // upgrade in the middle of the 0.26.0 move: what is already cold is converted, the move goes on in the new format
    {
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        s.cold_export_v1().unwrap();
        s.flush().unwrap();
        drop(s);
        let s = Storage::open_full(dir.path(), Network::mainnet(), opts(false)).unwrap();
        let st = s.cold_status().unwrap();
        assert_eq!((st.format, st.blocks, st.hot_blocks, st.last_error.clone()), (2, 2_000, 1_000, None));
        assert!(!dir.path().join("cold").join("blocks.dat").exists());
        assert_eq!(fingerprint(&s, &ids), before, "converted half-moved chain");
        s.flush().unwrap();
    }
    // a crash left an uncommitted tail and a stray segment behind
    {
        use std::io::Write;
        let cold = dir.path().join("cold");
        std::fs::OpenOptions::new().append(true).open(cold.join("blocks.zst")).unwrap().write_all(&[0xAB; 5_000]).unwrap();
        std::fs::OpenOptions::new().append(true).open(cold.join("blocks.zix")).unwrap().write_all(&[0xCD; 64]).unwrap();
        std::fs::write(cold.join("bid2-99999999.seg"), [0u8; 400]).unwrap();
        std::fs::create_dir_all(dir.path().join("rebuild.tmp")).unwrap();
    }
    // plain `Storage::open` (CLI commands) finds the cold store; the move resumes and completes
    {
        let s = Arc::new(Storage::open(dir.path(), Network::mainnet()).unwrap());
        assert!(!dir.path().join("cold").join("bid2-99999999.seg").exists());
        assert_eq!(fingerprint(&s, &ids), before, "after the simulated crash");
        // … but this handle moves nothing (cold_blocks.enabled: false semantics)
        s.set_cold_tuning(100, 100);
        assert!(!s.cold_due());
        assert_eq!(s.archive_blocks(&never, |_| {}).unwrap(), 0);
        s.flush().unwrap();
        drop(s);
        let s = Arc::new(Storage::open_full(dir.path(), Network::mainnet(), opts(false)).unwrap());
        s.set_cold_tuning(100, 100);
        assert_eq!(s.archive_blocks(&never, |_| {}).unwrap(), 900);
        assert_eq!(s.archive_blocks(&never, |_| {}).unwrap(), 0);
        let st = s.cold_status().unwrap();
        assert_eq!((st.blocks, st.last_height, st.hot_blocks, st.pending_blocks, st.rebuild_pending), (2_900, 2_900, 100, 0, true));
        assert_eq!(fingerprint(&s, &ids), before, "fully moved chain");
        // sled really let go of the moved blocks and their id index
        assert_eq!((s.sled_key_count("b:"), s.sled_key_count("bid:")), (100, 100));
        // the chain keeps growing on top
        s.save_block(&chain[BLOCKS as usize]).unwrap();
        assert_eq!(s.get_last_height().unwrap(), BLOCKS + 1);
        assert_eq!(s.get_blocks(0, 3).unwrap().iter().map(|b| b.height).collect::<Vec<_>>(), vec![3001, 3000, 2999]);
        s.flush().unwrap();
    }
    // a marker without the rebuilt copy (e.g. the copy was deleted by hand) must not cost the database
    std::fs::write(dir.path().join("rebuild.ready"), b"").unwrap();
    {
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        assert_eq!(s.get_last_height().unwrap(), BLOCKS + 1);
        assert!(!dir.path().join("rebuild.ready").exists());
        s.flush().unwrap();
    }
    let size = |p: &std::path::Path| std::fs::metadata(p.join("db")).unwrap().len();
    let before_rebuild = size(dir.path());
    // node start: the main database is rebuilt, nothing else changes
    {
        let s = Arc::new(Storage::open_full(dir.path(), Network::mainnet(), opts(true)).unwrap());
        assert!(!dir.path().join("rebuild.tmp").exists() && !dir.path().join("rebuild.ready").exists() && !dir.path().join("rebuild.move").exists());
        let st = s.cold_status().unwrap();
        assert_eq!((st.blocks, st.rebuild_pending), (2_900, false));
        assert!(size(dir.path()) < before_rebuild, "rebuilt main database is smaller: {} vs {before_rebuild}", size(dir.path()));
        assert_eq!(s.get_last_height().unwrap(), BLOCKS + 1);
        assert_eq!(s.get_wallet(&addr).unwrap().unwrap().balance, 1_000_000_000);
        let mut want = before.clone();
        let got = fingerprint(&s, &ids);
        // only the tip moved (one block was added before the rebuild)
        for k in ["byId", "unknownId", "first", "txs", "atTs"] {
            assert_eq!(got[k], want[k], "{k} after rebuild");
        }
        want["byHeight"].as_array_mut().unwrap().truncate(BLOCKS as usize + 1);
        assert_eq!(got["byHeight"].as_array().unwrap()[..=BLOCKS as usize], want["byHeight"].as_array().unwrap()[..]);
        assert_eq!(got["count"], BLOCKS + 1);
        // confirmations moved with the tip; the blocks themselves are the same
        for (got, want) in api(s.clone(), &paths[2..4].to_vec()).await.iter().zip(&api_before[2..4]) {
            assert_eq!((&got["data"]["id"], &got["data"]["previous"], &got["data"]["transactions"]), (&want["data"]["id"], &want["data"]["previous"], &want["data"]["transactions"]));
        }
        release(s).await;
    }
    // a 0.26.0 store (uncompressed files, 40-byte id index) is converted at open; every reader answers the same
    let cold_size = |d: &std::path::Path| std::fs::read_dir(d.join("cold")).unwrap().flatten().map(|e| e.metadata().unwrap().len()).sum::<u64>();
    {
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        let before_v1 = fingerprint(&s, &ids);
        s.cold_export_v1().unwrap();
        s.flush().unwrap();
        drop(s);
        // (the files of the newer format are still lying around: they must not confuse the conversion)
        let cold = dir.path().join("cold");
        let v1_size: u64 = ["blocks.dat", "blocks.idx", "bid-00000000.seg"].iter().map(|f| std::fs::metadata(cold.join(f)).unwrap().len()).sum();
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        let st = s.cold_status().unwrap();
        assert_eq!((st.format, st.blocks, st.last_height), (2, 2_900, 2_900));
        assert!(st.last_error.is_none(), "{:?}", st.last_error);
        assert!(!cold.join("blocks.dat").exists() && !cold.join("blocks.idx").exists() && !cold.join("bid-00000000.seg").exists(), "0.26.0 files are removed after the conversion");
        assert_eq!(fingerprint(&s, &ids), before_v1, "converted store");
        assert!(cold_size(dir.path()) * 2 < v1_size, "compressed store is at least twice smaller here: {} vs {v1_size}", cold_size(dir.path()));
        s.flush().unwrap();
    }
    // a conversion that cannot complete never costs the old files: the store stays readable and it is retried
    {
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        let want = fingerprint(&s, &ids);
        s.cold_export_v1().unwrap();
        s.flush().unwrap();
        drop(s);
        // a directory sits where the new id segment has to be written
        let cold = dir.path().join("cold");
        for n in 0..300 {
            std::fs::create_dir_all(cold.join(format!("bid2-{n:08}.tmp"))).unwrap();
        }
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        let st = s.cold_status().unwrap();
        assert_eq!(st.format, 1, "conversion failed → the 0.26.0 files stay in use");
        assert!(st.last_error.as_deref().unwrap_or("").contains("conversion"), "{:?}", st.last_error);
        assert!(cold.join("blocks.dat").exists());
        assert_eq!(fingerprint(&s, &ids), want, "read-only 0.26.0 store");
        assert_eq!(s.archive_blocks(&never, |_| {}).unwrap(), 0, "nothing is appended to the old format");
        // the transaction mover still works there and leaves the 0.26.0 id index alone
        assert!(s.get_block_by_id(&ids[1234]).unwrap().is_some());
        s.flush().unwrap();
        drop(s);
        let s = Storage::open_full(dir.path(), Network::mainnet(), StorageOptions { history: HistoryOptions::default(), ..opts(false) }).unwrap();
        s.set_history_window(100, 500);
        assert_eq!(s.archive_history(&never, |_| {}).unwrap(), 200);
        assert_eq!(s.sled_key_count("tl:"), 100);
        assert_eq!(s.cold_status().unwrap().format, 1);
        assert!(cold.join("bid-00000000.seg").exists());
        assert_eq!(fingerprint(&s, &ids), want, "0.26.0 block files + flat transaction index");
        s.flush().unwrap();
        drop(s);
        for n in 0..300 {
            std::fs::remove_dir_all(cold.join(format!("bid2-{n:08}.tmp"))).unwrap();
        }
        // next start: the conversion succeeds
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        assert_eq!(s.cold_status().unwrap().format, 2);
        assert_eq!(fingerprint(&s, &ids), want, "converted at the second attempt");
        s.flush().unwrap();
    }
    // a lost id segment is derived data: it is rebuilt from the blocks at open
    {
        let cold = dir.path().join("cold");
        let seg = std::fs::read_dir(&cold).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with("bid2-")).unwrap().path();
        std::fs::remove_file(seg).unwrap();
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        assert_eq!(s.cold_status().unwrap().segments, 1);
        for (i, id) in ids.iter().enumerate().step_by(13) {
            assert_eq!(s.get_block_by_id(id).unwrap().unwrap().height, i as u64 + 1);
        }
        s.flush().unwrap();
    }
    // a damaged cold store is refused instead of serving a chain with holes
    std::fs::OpenOptions::new().write(true).open(dir.path().join("cold").join("blocks.zst")).unwrap().set_len(10).unwrap();
    let err = Storage::open(dir.path(), Network::mainnet()).err().expect("open must fail").to_string();
    assert!(err.contains("cold block store") && err.contains("2900 blocks"), "{err}");
}

/// The real apply path (wallet deltas, undo records) on top of a cold store: rollback works for the blocks still in
/// sled, is refused at the cold boundary, and the chain continues after it.
#[test]
fn rollback_stops_at_the_cold_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let s = Storage::open_full(dir.path(), Network::mainnet(), opts(false)).unwrap();
    let kp = KeyPair::from_passphrase("cold forger").unwrap();
    let (pk, addr) = (kp.public_key_hex(), kp.address(63).unwrap());
    s.set_undo_enabled(true);
    let mut prev = "f".repeat(64);
    let mut blocks = Vec::new();
    for h in 11_704_043u64..11_704_049 {
        let b = block(h, &prev, &pk, &addr);
        prev = b.id.clone().unwrap();
        s.apply_block(&b).unwrap();
        blocks.push(b);
    }
    s.set_cold_tuning(2, 3);
    assert_eq!(s.archive_blocks(&AtomicBool::new(false), |_| {}).unwrap(), 4);
    let st = s.cold_status().unwrap();
    assert_eq!((st.first_height, st.last_height, st.hot_blocks), (11_704_043, 11_704_046, 2));
    assert_eq!(s.get_wallet(&addr).unwrap().unwrap().produced_blocks, 6);
    assert_eq!(s.first_height().unwrap(), 11_704_043);

    assert_eq!(s.rollback_last_block().unwrap(), 11_704_047);
    assert_eq!(s.rollback_last_block().unwrap(), 11_704_046);
    let err = s.rollback_last_block().unwrap_err().to_string();
    assert!(err.contains("cold store"), "{err}");
    assert_eq!(s.get_last_height().unwrap(), 11_704_046);
    assert_eq!(s.get_last_block().unwrap().unwrap().id, blocks[3].id);
    assert_eq!(s.get_wallet(&addr).unwrap().unwrap().produced_blocks, 4);

    // the tip is a cold block now: the next block links to it and applies
    s.apply_block(&blocks[4]).unwrap();
    s.apply_block(&blocks[5]).unwrap();
    assert_eq!(s.get_last_height().unwrap(), 11_704_048);
    assert_eq!(s.get_blocks(0, 6).unwrap().iter().map(|b| b.id.clone()).collect::<Vec<_>>(), blocks.iter().rev().map(|b| b.id.clone()).collect::<Vec<_>>());
    // a block that does not extend the cold tip is still refused
    s.rollback_to(11_704_046).unwrap();
    let mut fork = block(11_704_047, &"e".repeat(64), &pk, &addr);
    fork.timestamp += 1;
    fork.id = Some(sth_core::crypto::block_id(&fork).unwrap());
    s.apply_block(&fork).unwrap_or_else(|e| assert!(e.to_string().contains("previousBlock") || e.to_string().contains("tip"), "{e}"));
}
