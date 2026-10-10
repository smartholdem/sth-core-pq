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
    // on disk (not /dev/shm): up to 60 000 transactions per chain
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(Storage::open_with_cache(dir.path(), Network::mainnet(), 16).unwrap());
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
                tx_json(n, &senders[(n % 3) as usize].public_key_hex(), &recipient)
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
