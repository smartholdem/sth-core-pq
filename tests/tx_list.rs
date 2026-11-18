//! Author: TechnoL0g
//!
//! `/api/transactions` must page through the height-ordered sled index without a full chain scan:
//! O(1) totalCount for plain lists, bounded scans + `totalCountIsEstimate` for filtered ones.

use serde_json::Value;
use sth_core::api::{serve_listener, AppState};
use sth_core::config::Network;
use sth_core::crypto::{transaction_id, KeyPair};
use std::collections::HashMap;
use sth_core::mempool::Mempool;
use sth_core::models::Block;
use sth_core::storage::Storage;
use std::sync::Arc;
use std::time::Instant;

const BLOCKS: u64 = 600;
const PER_BLOCK: u64 = 100;
const TOTAL: u64 = BLOCKS * PER_BLOCK;
const SIG: &str = "3045022100bfcfed36e8019c760490fd453cc28a2118241907d63c3ed0d3004687907107ff02200d300e64fdf5c5ca358e3266794b12b6b900004084b7fba838ceedcb1364e658";

fn tx_json(n: u64, sender_pk: &str, recipient: &str) -> String {
    let type_ = if n % 50 == 0 { 3 } else { 0 };
    let asset = if type_ == 3 { format!(r#"{{"votes":["+{sender_pk}"]}}"#) } else { "null".into() };
    format!(
        r#"{{"version":2,"network":63,"typeGroup":1,"type":{type_},"nonce":"{n}","senderPublicKey":"{sender_pk}","fee":"100000000","amount":"{}","recipientId":"{recipient}","expiration":0,"signature":"{}","asset":{asset}}}"#,
        n * 1000,
        "1".repeat(128)
    )
}

struct Chain {
    storage: Arc<Storage>,
    senders: Vec<KeyPair>,
    recipient: String,
    /// real transaction id → ordinal n (1-based, chain order)
    ordinal: HashMap<String, u64>,
    _dir: tempfile::TempDir,
}

impl Chain {
    fn n(&self, id: &str) -> u64 {
        self.ordinal[id]
    }
    fn id(&self, n: u64) -> &str {
        self.ordinal.iter().find(|(_, v)| **v == n).map(|(k, _)| k.as_str()).unwrap()
    }
    fn ids(&self, v: &Value) -> Vec<u64> {
        v["data"].as_array().unwrap().iter().map(|t| self.n(t["id"].as_str().unwrap())).collect()
    }
}

fn build_chain() -> Chain {
    build_chain_with(BLOCKS, PER_BLOCK, |h| (h * 8) as u32)
}

fn build_chain_with(blocks: u64, per_block: u64, ts: impl Fn(u64) -> u32) -> Chain {
    build_chain_full(blocks, per_block, ts, false)
}

/// `own_votes`: a vote names its sender as the recipient (what wallets send); otherwise the common recipient — a field
/// the stored block does not keep.
fn build_chain_full(blocks: u64, per_block: u64, ts: impl Fn(u64) -> u32, own_votes: bool) -> Chain {
    // on disk (not /dev/shm): up to 60 000 transactions per chain
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(Storage::open_with_options(dir.path(), Network::mainnet(), 16, sth_core::storage::HistoryOptions::default()).unwrap());
    let senders: Vec<KeyPair> = (0..3).map(|i| KeyPair::from_passphrase(&format!("sender {i}")).unwrap()).collect();
    let recipient = KeyPair::from_passphrase("recipient").unwrap().address(63).unwrap();
    // save_block skips wallet state: register the wallets so /api/wallets/:id resolves
    for kp in &senders {
        storage.update_wallet_state(&kp.address(63).unwrap(), 1_000_000_000, 0).unwrap();
    }
    storage.update_wallet_state(&recipient, 1_000_000_000, 0).unwrap();
    let gen = senders[0].public_key_hex();
    let mut n = 0u64;
    let mut ordinal = HashMap::new();
    let mut prev = "f".repeat(64);
    for h in 1..=blocks {
        let txs: Vec<String> = (0..per_block)
            .map(|_| {
                n += 1;
                let sender = &senders[(n % 3) as usize];
                let to = if own_votes && n % 50 == 0 { sender.address(63).unwrap() } else { recipient.clone() };
                tx_json(n, &sender.public_key_hex(), &to)
            })
            .collect();
        let id = format!("{h:064x}");
        let block = format!(
            r#"{{"id":"{id}","version":0,"timestamp":{},"previousBlock":"{prev}","height":{h},"numberOfTransactions":{per_block},"totalAmount":"0","totalFee":"0","reward":"0","payloadLength":0,"payloadHash":"{}","generatorPublicKey":"{gen}","blockSignature":"{}","transactions":[{}]}}"#,
            ts(h),
            "9".repeat(64),
            SIG.to_string(),
            txs.join(",")
        );
        prev = id;
        let mut b: Block = serde_json::from_str(&block).unwrap();
        for (i, tx) in b.transactions.iter_mut().enumerate() {
            tx.id = Some(transaction_id(tx).unwrap());
            ordinal.insert(tx.id.clone().unwrap(), (h - 1) * per_block + i as u64 + 1);
        }
        storage.save_block(&b).unwrap();
    }
    Chain { storage, senders, recipient, ordinal, _dir: dir }
}

async fn spawn_api(storage: Arc<Storage>) -> String {
    let mempool = Arc::new(Mempool::new(storage.clone(), vec![], 100));
    let state = Arc::new(AppState::new(storage, mempool, vec![]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { serve_listener(state, listener).await.unwrap() });
    format!("http://{addr}")
}

async fn get(base: &str, path: &str) -> (Value, std::time::Duration) {
    let t = Instant::now();
    let v: Value = reqwest::get(format!("{base}{path}")).await.unwrap().json().await.unwrap();
    (v, t.elapsed())
}

#[test]
fn transaction_counter_follows_blocks_and_rollback() {
    let chain = build_chain();
    let s = &chain.storage;
    assert_eq!(s.transaction_count().unwrap(), TOTAL);
    assert_eq!(s.transaction_index_count(None, 0..=u64::MAX).unwrap(), TOTAL);
    assert_eq!(s.transaction_index_count(None, 10..=19).unwrap(), 10 * PER_BLOCK);
    assert_eq!(s.transaction_index_count(Some(&chain.recipient), 0..=u64::MAX).unwrap(), TOTAL);
    let sender0 = chain.senders[0].address(63).unwrap();
    assert_eq!(s.transaction_index_count(Some(&sender0), 0..=u64::MAX).unwrap(), TOTAL / 3);

    // newest first, lazily: the first entry is the last tx of the last block
    let first = s.transaction_index(None, 0..=u64::MAX, false).next().unwrap().unwrap();
    assert_eq!((first.height, first.id.as_str()), (BLOCKS, chain.id(TOTAL)));
    let oldest = s.transaction_index(None, 0..=u64::MAX, true).next().unwrap().unwrap();
    assert_eq!((oldest.height, oldest.seq, oldest.id.as_str()), (1, 0, chain.id(1)));

    // block timestamps are h*8: [80, 160] covers heights 10..=20
    assert_eq!(s.heights_for_timestamps(Some(80), Some(160)).unwrap(), Some(10..=20));
    assert_eq!(s.heights_for_timestamps(Some(81), Some(87)).unwrap(), None);
    assert_eq!(s.heights_for_timestamps(None, Some(8)).unwrap(), Some(1..=1));
    assert_eq!(s.heights_for_timestamps(Some((BLOCKS * 8 + 1) as u32), None).unwrap(), None);
}

#[test]
fn rollback_decrements_transaction_counter() {
    let storage = Storage::temporary(Network::mainnet()).unwrap();
    let sender = KeyPair::from_passphrase("counter sender").unwrap();
    let (pk, addr) = (sender.public_key_hex(), sender.address(63).unwrap());
    storage.update_wallet_state(&addr, 100_000_000_000, 0).unwrap();
    storage.set_undo_enabled(true);
    let block = format!(
        r#"{{"id":"{}","version":0,"timestamp":100,"previousBlock":"{}","height":11704043,"numberOfTransactions":1,"totalAmount":"1000","totalFee":"100000000","reward":"0","payloadLength":0,"payloadHash":"{}","generatorPublicKey":"{pk}","blockSignature":"{}","transactions":[{}]}}"#,
        "c".repeat(64),
        "f".repeat(64),
        "9".repeat(64),
        SIG.to_string(),
        tx_json(1, &pk, &addr)
    );
    let b: Block = serde_json::from_str(&block).unwrap();
    storage.apply_block(&b).unwrap();
    assert_eq!(storage.transaction_count().unwrap(), 1);
    storage.rollback_last_block().unwrap();
    assert_eq!(storage.transaction_count().unwrap(), 0);
    assert_eq!(storage.transaction_index(None, 0..=u64::MAX, false).count(), 0);
}

#[tokio::test]
async fn plain_list_is_paged_from_the_index_with_exact_total() {
    let chain = build_chain();
    let base = spawn_api(chain.storage.clone()).await;

    let (v, dt) = get(&base, "/api/transactions?limit=10").await;
    assert_eq!(v["meta"]["totalCount"], TOTAL);
    assert_eq!(v["meta"]["totalCountIsEstimate"], false);
    assert_eq!(v["meta"]["pageCount"], TOTAL / 10);
    assert_eq!(chain.ids(&v), (TOTAL - 9..=TOTAL).rev().collect::<Vec<_>>());
    eprintln!("plain list page 1: {dt:?}");
    assert!(dt.as_millis() < 1500, "plain list took {dt:?}");

    let (v, _) = get(&base, "/api/transactions?limit=10&page=2").await;
    assert_eq!(chain.ids(&v)[0], TOTAL - 10);
    assert_eq!(v["meta"]["previous"].as_str().unwrap(), "/transactions?limit=10&page=1&transform=true");

    let (v, _) = get(&base, "/api/transactions?limit=5&orderBy=timestamp:asc").await;
    assert_eq!(chain.ids(&v), vec![1, 2, 3, 4, 5]);

    // deep page: still index arithmetic, no scan of the pages before it
    let (v, dt) = get(&base, &format!("/api/transactions?limit=100&page={}", TOTAL / 100)).await;
    assert_eq!(chain.ids(&v).last().copied(), Some(1));
    eprintln!("plain list last page: {dt:?}");
    assert!(dt.as_millis() < 1500, "last page took {dt:?}");
}

#[tokio::test]
async fn wallet_and_timestamp_lists_use_index_ranges() {
    let chain = build_chain();
    let base = spawn_api(chain.storage.clone()).await;
    let sender1 = chain.senders[1].address(63).unwrap();

    let (v, _) = get(&base, &format!("/api/transactions?address={sender1}&limit=3")).await;
    assert_eq!(v["meta"]["totalCount"], TOTAL / 3);
    assert_eq!(v["meta"]["totalCountIsEstimate"], false);
    assert!(chain.ids(&v).iter().all(|n| n % 3 == 1), "{:?}", chain.ids(&v));

    let (v, _) = get(&base, &format!("/api/wallets/{sender1}/transactions?limit=3")).await;
    assert_eq!(v["meta"]["totalCount"], TOTAL / 3);

    // senderId needs the decoded tx (wallet index holds both roles) but is bounded by the wallet index
    let (v, _) = get(&base, &format!("/api/transactions?senderId={sender1}&limit=3")).await;
    assert_eq!(v["meta"]["totalCount"], TOTAL / 3);
    let (v, _) = get(&base, &format!("/api/wallets/{sender1}/transactions/received?limit=3")).await;
    assert_eq!(v["meta"]["totalCount"], 0);
    let (v, _) = get(&base, &format!("/api/wallets/{}/transactions/received?limit=3", chain.recipient)).await;
    // votes carry no recipient on the wire (legacy: recipientId = sender), so only the transfers count
    assert_eq!(v["meta"]["totalCount"], TOTAL - TOTAL / 50);

    // timestamp window → height window 10..=20 → 11 blocks
    let (v, _) = get(&base, "/api/transactions?timestamp.from=80&timestamp.to=160&limit=10").await;
    assert_eq!(v["meta"]["totalCount"], 11 * PER_BLOCK);
    assert_eq!(chain.ids(&v)[0], 20 * PER_BLOCK);
    let (v, _) = get(&base, "/api/transactions?timestamp=80&limit=10").await;
    assert_eq!(v["meta"]["totalCount"], PER_BLOCK);
    let (v, _) = get(&base, "/api/transactions?timestamp=81&limit=10").await;
    assert_eq!(v["meta"]["totalCount"], 0);
    assert_eq!(v["data"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn filtered_lists_stop_after_the_scan_budget_and_flag_the_estimate() {
    let chain = build_chain();
    let base = spawn_api(chain.storage.clone()).await;

    // 1 vote per 50 tx → 1200 votes over 60 000 entries: the page fills long before the budget (50 000) is spent,
    // the scan continues to the budget and the remainder is extrapolated
    let (v, dt) = get(&base, "/api/votes?limit=10").await;
    assert_eq!(v["meta"]["totalCountIsEstimate"], true);
    let total = v["meta"]["totalCount"].as_u64().unwrap();
    assert!((1100..=1300).contains(&total), "estimated {total}");
    assert_eq!(chain.ids(&v)[0], TOTAL);
    assert!(chain.ids(&v).iter().all(|n| n % 50 == 0));
    eprintln!("budgeted /api/votes scan (50 000 entries): {dt:?}");
    assert!(dt.as_secs() < 20, "budgeted scan took {dt:?}");

    // a filter that fits inside the budget is exact
    let (v, _) = get(&base, "/api/transactions?type=3&timestamp.from=80&timestamp.to=160&limit=10").await;
    assert_eq!(v["meta"]["totalCountIsEstimate"], false);
    assert_eq!(v["meta"]["totalCount"], 22);

    // small chain slice with a per-tx filter that matches nothing: exact zero, no estimate
    let (v, _) = get(&base, "/api/transactions?type=7&timestamp.to=800&limit=10").await;
    assert_eq!(v["meta"]["totalCount"], 0);
    assert_eq!(v["meta"]["totalCountIsEstimate"], false);
}

#[tokio::test]
async fn node_fees_scans_the_index_and_groups_by_calendar_day() {
    // 40 blocks × 5 tx, one block every 3 h ending now → ~5 calendar days
    let net = Network::mainnet();
    let now = net.now_epoch();
    let chain = build_chain_with(40, 5, move |h| now - (40 - h as u32) * 3 * 3600 - 60);
    let base = spawn_api(chain.storage.clone()).await;

    // legacy shape: every tx of the window is a transfer or a vote (fee 1 STH each)
    let (v, dt) = get(&base, "/api/node/fees?days=7").await;
    let core = &v["data"]["1"];
    let count = core["transfer"]["sum"].as_str().unwrap().parse::<u64>().unwrap() / 100_000_000 + core["vote"]["sum"].as_str().unwrap().parse::<u64>().unwrap() / 100_000_000;
    assert_eq!(count, 200);
    assert_eq!(core["transfer"]["avg"], "100000000");
    eprintln!("node/fees 7d: {dt:?}");

    // daily buckets, oldest first, counts add up to the whole chain
    let (v, dt) = get(&base, "/api/node/fees?days=7&groupBy=day").await;
    eprintln!("node/fees groupBy=day cold: {dt:?}");
    let days = v["data"].as_array().unwrap();
    assert_eq!(days.len(), 7);
    assert_eq!(v["meta"]["groupBy"], "day");
    let total: u64 = days.iter().map(|d| d["count"].as_u64().unwrap()).sum();
    assert_eq!(total, 200);
    assert!(days.windows(2).all(|w| w[0]["from"].as_i64() < w[1]["from"].as_i64()));
    assert!(days.iter().all(|d| d["to"].as_i64().unwrap() - d["from"].as_i64().unwrap() == 86_400));
    let last = days.last().unwrap();
    assert!(last["count"].as_u64().unwrap() >= 5, "today has blocks: {last}");
    assert_eq!(last["day"].as_str().unwrap().len(), 10);
    let busy = days.iter().find(|d| d["count"].as_u64().unwrap() >= 40).unwrap();
    assert_eq!(busy["blocks"].as_u64().unwrap(), busy["count"].as_u64().unwrap() / 5);
    let fee_sum: u64 = busy["sum"].as_str().unwrap().parse().unwrap();
    assert_eq!(fee_sum, busy["count"].as_u64().unwrap() * 100_000_000);
    assert!(busy["types"]["1"]["transfer"]["count"].as_u64().unwrap() > 0);

    // second call serves the closed days from the cache and rescans only today
    let (v2, dt) = get(&base, "/api/node/fees?days=7&groupBy=day").await;
    eprintln!("node/fees groupBy=day warm: {dt:?}");
    assert!(v2["meta"]["cachedDays"].as_u64().unwrap() >= 6);
    assert_eq!(v2["data"], v["data"]);

    let (v3, _) = get(&base, "/api/node/fees?days=2&groupBy=day").await;
    assert_eq!(v3["data"].as_array().unwrap().len(), 2);
    assert_eq!(v3["data"][1], v["data"][6]);
}

fn chain_block(h: u64, prev: &str, pk: &str, tx: &str) -> Block {
    serde_json::from_str(&format!(
        r#"{{"id":"{h:064x}","version":0,"timestamp":{},"previousBlock":"{prev}","height":{h},"numberOfTransactions":1,"totalAmount":"1000","totalFee":"100000000","reward":"0","payloadLength":0,"payloadHash":"{}","generatorPublicKey":"{pk}","blockSignature":"{}","transactions":[{tx}]}}"#,
        h * 8,
        "9".repeat(64),
        SIG
    ))
    .unwrap()
}

/// 0.27.0: indexes older than the hot window move to compact flat files. API answers stay identical, an interrupted
/// move continues, a reopened database serves both parts and sled really lets the records go.
#[tokio::test]
async fn cold_history_keeps_api_identical_and_migration_resumes() {
    let chain = build_chain_full(BLOCKS, PER_BLOCK, |h| (h * 8) as u32, true);
    let sender0 = chain.senders[0].address(63).unwrap();
    let paths = vec![
        "/api/transactions?limit=100".to_string(),
        "/api/transactions?limit=100&page=50".into(),
        "/api/transactions?limit=100&page=300".into(),
        format!("/api/transactions?limit=100&page={}", TOTAL / 100),
        "/api/transactions?limit=5&orderBy=timestamp:asc".into(),
        "/api/transactions?limit=100&page=551&orderBy=timestamp:asc".into(),
        format!("/api/transactions?address={sender0}&limit=100&page=150"),
        format!("/api/wallets/{sender0}/transactions?limit=3"),
        format!("/api/wallets/{sender0}/transactions?limit=100&page=184&orderBy=timestamp:asc"),
        format!("/api/wallets/{}/transactions/received?limit=100&page=545", chain.recipient),
        format!("/api/wallets/{}/transactions/received?limit=100&page=551&orderBy=timestamp:asc", chain.recipient),
        "/api/transactions?timestamp.from=80&timestamp.to=160&limit=10".into(),
        "/api/transactions?timestamp.from=4300&timestamp.to=4500&limit=100&page=20".into(),
        "/api/transactions?type=3&timestamp.from=80&timestamp.to=4700&limit=10".into(),
        "/api/votes?limit=10".into(),
        format!("/api/transactions/{}", chain.id(1)),
        format!("/api/transactions/{}", chain.id(30_000)),
        format!("/api/transactions/{}", chain.id(TOTAL)),
        format!("/api/blocks/300/transactions?limit=100"),
    ];
    let base = spawn_api(chain.storage.clone()).await;
    let mut before = Vec::new();
    for p in &paths {
        before.push(get(&base, p).await.0);
    }
    assert_eq!(before[0]["data"].as_array().unwrap().len(), 100);

    let s = &chain.storage;
    let index = |s: &Storage| -> Vec<Vec<(u64, u32, String)>> {
        let walk = |a: Option<&str>, r: std::ops::RangeInclusive<u64>, asc| s.transaction_index(a, r, asc).map(|e| e.unwrap()).map(|e| (e.height, e.seq, e.id)).collect::<Vec<_>>();
        vec![walk(None, 0..=u64::MAX, true), walk(None, 0..=u64::MAX, false), walk(Some(&sender0), 0..=u64::MAX, true), walk(Some(&sender0), 0..=u64::MAX, false), walk(Some(&chain.recipient), 495..=555, false), walk(None, 540..=560, true), walk(Some(&sender0), 1..=1, true), walk(None, 500..=500, false)]
    };
    let counts = |s: &Storage| vec![s.transaction_index_count(Some(&sender0), 0..=u64::MAX).unwrap(), s.transaction_index_count(None, 540..=560).unwrap(), s.transaction_index_count(Some(&chain.recipient), 495..=555).unwrap(), s.transaction_index_count(Some(&sender0), 2..=2).unwrap(), s.transaction_index_count(None, 0..=10).unwrap()];
    let (index_before, counts_before) = (index(s), counts(s));
    // 2 of 100 transactions are votes (sender only)
    assert_eq!(counts_before, vec![TOTAL / 3, 21 * PER_BLOCK, 61 * (PER_BLOCK - 2), 33, 10 * PER_BLOCK]);
    let never = std::sync::atomic::AtomicBool::new(false);
    // default window (50 000) + default keep margin (1 100 blocks): a 600-block chain is entirely "hot"
    assert!(!s.history_due());
    assert_eq!(s.archive_history(&never, |_| {}).unwrap(), 0);

    // hot window 5 000 transactions, newest 10 blocks never moved
    s.set_history_window(5_000, 10);
    assert!(s.history_due());
    let st = s.history_status().unwrap();
    assert_eq!((st.enabled, st.total_txs, st.hot_txs, st.cold_txs, st.pending_txs), (true, TOTAL, TOTAL, 0, TOTAL - 5_000));
    let sled_before = (s.sled_key_count("t:"), s.sled_key_count("tl:"), s.sled_key_count("wt:"));
    assert_eq!(sled_before, (TOTAL as usize, TOTAL as usize, (2 * TOTAL - TOTAL / 50) as usize));

    // interrupted run: the cancel flag is raised from the progress callback (every 25 batches of 2 000)
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let moved = s.archive_history(&cancel, |_| cancel.store(true, std::sync::atomic::Ordering::Relaxed)).unwrap();
    assert_eq!(moved, 50_000, "stopped after 25 batches");
    let st = s.history_status().unwrap();
    assert_eq!((st.cold_txs, st.hot_txs, st.copied_height), (50_000, 10_000, 500));
    // half-moved state answers exactly like before
    assert_eq!(counts(s), counts_before);
    assert_eq!(index(s), index_before, "index walk mid-move");
    for (p, want) in paths.iter().zip(&before) {
        assert_eq!(&get(&base, p).await.0, want, "{p} (mid-move)");
    }

    // resume
    let moved = s.archive_history(&never, |_| {}).unwrap();
    assert_eq!(moved, 5_000);
    let st = s.history_status().unwrap();
    assert_eq!((st.cold_txs, st.hot_txs, st.pending_txs, st.copied_height), (55_000, 5_000, 0, 550));
    assert_eq!(st.progress_percent, 100.0);
    // 38 bytes per transaction + 12 (id) + 25 per address (sender, recipient; a vote has one)
    assert_eq!(st.history_bytes, 55_000 * (38 + 12) + 25 * (2 * 55_000 - 1_100));
    assert!(st.segments <= 16, "segments are merged in tiers, got {}", st.segments);
    assert!(!s.history_due());
    assert_eq!(s.archive_history(&never, |_| {}).unwrap(), 0, "idempotent");
    // sled let go of exactly the moved records
    assert_eq!((s.sled_key_count("t:"), s.sled_key_count("tl:"), s.sled_key_count("wt:")), (5_000, 5_000, 9_900));
    for (p, want) in paths.iter().zip(&before) {
        assert_eq!(&get(&base, p).await.0, want, "{p} (after the move)");
    }
    assert_eq!(index(s), index_before, "index walk after the move");
    assert_eq!(counts(s), counts_before);
    assert_eq!(s.transaction_count().unwrap(), TOTAL);
    assert_eq!(s.transaction_ids(None).unwrap().len() as u64, TOTAL);
    for n in [1, 2, 29_999, 54_999, 55_000, 55_001, TOTAL] {
        assert_eq!(s.get_transaction(chain.id(n)).unwrap().unwrap().block_height, Some((n - 1) / PER_BLOCK + 1), "transaction {n}");
    }
    assert!(s.get_transaction(&"0".repeat(64)).unwrap().is_none());
    // an id that shares the 8-byte index prefix with a moved transaction is not mistaken for it
    let twin = format!("{}{}", &chain.id(7)[..16], "0".repeat(48));
    assert!(s.get_transaction(&twin).unwrap().is_none());
    // moved blocks are never rolled back
    assert!(s.rollback_last_block().is_err());
    s.flush().unwrap();
}

/// sled holds address records that the stored blocks do not explain (here: votes whose JSON named a foreign
/// recipient). The mover must not drop them silently: nothing moves, every answer stays the same.
#[test]
fn index_records_the_blocks_do_not_explain_stay_in_sled() {
    let chain = build_chain_with(60, 100, |h| (h * 8) as u32);
    let s = &chain.storage;
    let walk = |s: &Storage| s.transaction_index(Some(&chain.recipient), 0..=u64::MAX, false).map(|e| e.unwrap().id).collect::<Vec<_>>();
    let before = walk(s);
    assert_eq!(before.len(), 6_000);
    s.set_history_window(1_000, 10);
    assert!(s.history_due());
    let err = s.archive_history(&std::sync::atomic::AtomicBool::new(false), |_| {}).unwrap_err().to_string();
    assert!(err.contains("stays in the main database"), "{err}");
    let st = s.history_status().unwrap();
    assert_eq!((st.cold_txs, st.hot_txs, st.copied_height), (0, 6_000, 0));
    assert!(st.last_error.is_some());
    assert_eq!(walk(s), before);
    assert_eq!((s.sled_key_count("t:"), s.sled_key_count("tl:"), s.sled_key_count("wt:")), (6_000, 6_000, 12_000));
}

/// A reopened database finds the flat index; lost or damaged index files are rebuilt from the blocks; the records of
/// a 0.26.0 experimental `history` database are taken back.
#[test]
fn cold_history_survives_reopen_and_file_loss() {
    use sth_core::storage::HistoryOptions;
    let dir = tempfile::tempdir().unwrap();
    let kp = KeyPair::from_passphrase("history sender").unwrap();
    let (pk, addr) = (kp.public_key_hex(), kp.address(63).unwrap());
    let never = std::sync::atomic::AtomicBool::new(false);
    let mut ids = Vec::new();
    {
        // single-database layout first
        let s = Storage::open_with_options(dir.path(), Network::mainnet(), 16, HistoryOptions { enabled: false, ..Default::default() }).unwrap();
        let mut prev = "f".repeat(64);
        for h in 1..=40u64 {
            let mut b = chain_block(h, &prev, &pk, &tx_json(h, &pk, &addr));
            b.transactions[0].id = Some(transaction_id(&b.transactions[0]).unwrap());
            ids.push(b.transactions[0].id.clone().unwrap());
            prev = b.id.clone().unwrap();
            s.save_block(&b).unwrap();
        }
        assert!(!s.history_status().unwrap().enabled);
        assert!(!dir.path().join("cold").exists());
        s.flush().unwrap();
    }
    let check = |s: &Storage, what: &str| {
        let asc: Vec<String> = s.transaction_index(None, 0..=u64::MAX, true).map(|e| e.unwrap().id).collect();
        assert_eq!(asc, ids, "{what}");
        let desc: Vec<String> = s.transaction_index(Some(&addr), 0..=u64::MAX, false).map(|e| e.unwrap().id).collect();
        assert_eq!(desc, ids.iter().rev().cloned().collect::<Vec<_>>(), "{what}");
        assert_eq!(s.transaction_index(None, 25..=35, true).count(), 11, "{what}");
        assert_eq!(s.transaction_index_count(Some(&addr), 25..=35).unwrap(), 11, "{what}");
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(s.get_transaction(id).unwrap().unwrap().block_height, Some(i as u64 + 1), "{what}");
        }
    };
    {
        let s = Storage::open_with_options(dir.path(), Network::mainnet(), 16, HistoryOptions::default()).unwrap();
        s.set_history_window(10, 5);
        assert_eq!(s.archive_history(&never, |_| {}).unwrap(), 30);
        let st = s.history_status().unwrap();
        assert_eq!((st.cold_txs, st.hot_txs, st.copied_height), (30, 10, 30));
        // sender == recipient here: one address record per transaction
        assert_eq!(st.history_bytes, 30 * (38 + 12 + 25));
        check(&s, "moved");
        s.flush().unwrap();
    }
    {
        // plain open (CLI commands): the flat index is found, nothing more is moved
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        s.set_history_window(1, 0);
        assert!(!s.history_due());
        assert_eq!(s.archive_history(&never, |_| {}).unwrap(), 0);
        assert_eq!(s.history_status().unwrap().cold_txs, 30);
        check(&s, "reopened");
        s.flush().unwrap();
    }
    // lost segment / truncated txs.dat / garbage tail: the index is derived data and is rebuilt from the blocks
    let cold = dir.path().join("cold");
    let find = |prefix: &str| std::fs::read_dir(&cold).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with(prefix)).unwrap().path();
    for damage in 0..3 {
        match damage {
            0 => std::fs::remove_file(find("txw-")).unwrap(),
            1 => std::fs::OpenOptions::new().write(true).open(cold.join("txs.dat")).unwrap().set_len(100).unwrap(),
            _ => {
                use std::io::Write;
                std::fs::OpenOptions::new().append(true).open(cold.join("txs.dat")).unwrap().write_all(&[7u8; 999]).unwrap();
                std::fs::write(cold.join("txi-99999999.seg"), [1u8; 24]).unwrap();
            }
        }
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        assert_eq!(s.history_status().unwrap().cold_txs, 30);
        assert_eq!(std::fs::metadata(cold.join("txs.dat")).unwrap().len(), 30 * 38);
        assert!(!cold.join("txi-99999999.seg").exists());
        check(&s, &format!("after damage {damage}"));
        s.flush().unwrap();
    }
}

/// Records parked in the 0.26.0 experimental second sled database return to the main tree at open.
#[test]
fn legacy_history_database_is_taken_back() {
    let dir = tempfile::tempdir().unwrap();
    let kp = KeyPair::from_passphrase("history sender").unwrap();
    let (pk, addr) = (kp.public_key_hex(), kp.address(63).unwrap());
    let mut ids = Vec::new();
    {
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        let mut prev = "f".repeat(64);
        for h in 1..=20u64 {
            let mut b = chain_block(h, &prev, &pk, &tx_json(h, &pk, &addr));
            b.transactions[0].id = Some(transaction_id(&b.transactions[0]).unwrap());
            ids.push(b.transactions[0].id.clone().unwrap());
            prev = b.id.clone().unwrap();
            s.save_block(&b).unwrap();
        }
        s.flush().unwrap();
    }
    // what 0.26.0 with `history.enabled: true` left behind: index records of heights 1..=12 in `<db>/history`
    {
        let main = sled::Config::new().path(dir.path()).use_compression(true).compression_factor(3).open().unwrap();
        let tree = main.open_tree("chain").unwrap();
        let hist = sled::Config::new().path(dir.path().join("history")).use_compression(true).compression_factor(3).open().unwrap();
        let cold = hist.open_tree("history").unwrap();
        let mut moved = 0;
        for prefix in ["t:", "tl:", "wt:"] {
            for item in tree.scan_prefix(prefix) {
                let (k, v) = item.unwrap();
                let height = if prefix == "t:" { u64::from_be_bytes(v.as_ref().try_into().unwrap()) } else { u64::from_be_bytes(k[k.len() - 12..k.len() - 4].try_into().unwrap()) };
                if height <= 12 {
                    cold.insert(&k, v).unwrap();
                    tree.remove(&k).unwrap();
                    moved += 1;
                }
            }
        }
        assert_eq!(moved, 36);
        let mut cursor = 12u64.to_be_bytes().to_vec();
        cursor.extend_from_slice(&12u64.to_be_bytes());
        tree.insert("meta:hist_copied", cursor).unwrap();
        tree.insert("meta:hist_pruned", &12u64.to_be_bytes()).unwrap();
        hist.flush().unwrap();
        main.flush().unwrap();
    }
    {
        let s = Storage::open(dir.path(), Network::mainnet()).unwrap();
        let asc: Vec<String> = s.transaction_index(None, 0..=u64::MAX, true).map(|e| e.unwrap().id).collect();
        assert_eq!(asc, ids);
        assert_eq!((s.sled_key_count("t:"), s.sled_key_count("tl:"), s.sled_key_count("wt:"), s.sled_key_count("meta:hist")), (20, 20, 20, 0));
        assert_eq!(s.transaction_index(Some(&addr), 0..=u64::MAX, false).count(), 20);
        assert!(!dir.path().join("history").exists());
        s.flush().unwrap();
    }
    // a cursor that points at a missing database must not be mistaken for "nothing archived"
    {
        let main = sled::Config::new().path(dir.path()).use_compression(true).compression_factor(3).open().unwrap();
        let mut cursor = 12u64.to_be_bytes().to_vec();
        cursor.extend_from_slice(&12u64.to_be_bytes());
        main.open_tree("chain").unwrap().insert("meta:hist_copied", cursor).unwrap();
        main.flush().unwrap();
    }
    let err = Storage::open(dir.path(), Network::mainnet()).err().expect("open must fail").to_string();
    assert!(err.contains("history store") && err.contains("12 transactions"), "{err}");
}

/// The transaction index of blocks inside the undo depth never leaves sled, so a rollback only ever meets sled
/// records; a block whose index was moved is refused instead of being rolled back half-way.
#[test]
fn rollback_never_reaches_the_flat_index() {
    let dir = tempfile::tempdir().unwrap();
    let s = Storage::open_with_options(dir.path(), Network::mainnet(), 16, sth_core::storage::HistoryOptions::default()).unwrap();
    let kp = KeyPair::from_passphrase("counter sender").unwrap();
    let (pk, addr) = (kp.public_key_hex(), kp.address(63).unwrap());
    s.update_wallet_state(&addr, 100_000_000_000, 0).unwrap();
    s.set_undo_enabled(true);
    let mut prev = "f".repeat(64);
    let mut blocks = Vec::new();
    for (i, h) in (11_704_043u64..11_704_047).enumerate() {
        let b = chain_block(h, &prev, &pk, &tx_json(i as u64 + 1, &pk, &addr));
        prev = b.id.clone().unwrap();
        s.apply_block(&b).unwrap();
        blocks.push(b);
    }
    let ids: Vec<String> = s.transaction_index(None, 0..=u64::MAX, true).map(|e| e.unwrap().id).collect();
    assert_eq!(ids.len(), 4);
    // hot window of one transaction, the two newest blocks stay in sled
    s.set_history_window(1, 2);
    assert_eq!(s.archive_history(&std::sync::atomic::AtomicBool::new(false), |_| {}).unwrap(), 2);
    assert_eq!(s.history_status().unwrap().copied_height, 11_704_044);

    s.rollback_to(11_704_044).unwrap();
    let st = s.history_status().unwrap();
    assert_eq!((st.total_txs, st.cold_txs, st.hot_txs, st.copied_height), (2, 2, 0, 11_704_044));
    assert!(s.get_transaction(&ids[2]).unwrap().is_none() && s.get_transaction(&ids[3]).unwrap().is_none());
    assert_eq!(s.transaction_index(Some(&addr), 0..=u64::MAX, false).count(), 2);
    let err = s.rollback_last_block().unwrap_err().to_string();
    assert!(err.contains("cold store"), "{err}");
    assert_eq!(s.get_last_height().unwrap(), 11_704_044);

    s.apply_block(&blocks[2]).unwrap();
    assert_eq!(s.get_transaction(&ids[2]).unwrap().unwrap().block_height, Some(11_704_045));
    let again: Vec<String> = s.transaction_index(None, 0..=u64::MAX, true).map(|e| e.unwrap().id).collect();
    assert_eq!(again, ids[..3]);
}
