//! Author: TechnoL0g
//!
//! Memory / disk probe for the cold store (ignored; run by hand):
//!   HIST_DB=/tmp/histbench HIST_MODE=build   cargo test --test bench_history -- --ignored --nocapture
//!   HIST_DB=/tmp/histbench HIST_MODE=legacy  …   (single-database layout, idle RSS after a small workload)
//!   HIST_DB=/tmp/histbench HIST_MODE=migrate …   (moves everything older than the hot window)
//!   HIST_DB=/tmp/histbench HIST_MODE=cold    …   (same workload on the migrated database)

use sth_core::config::Network;
use sth_core::crypto::{transaction_id, KeyPair};
use sth_core::models::Block;
use sth_core::storage::{ColdOptions, HistoryOptions, Storage, StorageOptions};

#[allow(dead_code)]
const SIG: &str = "3045022100bfcfed36e8019c760490fd453cc28a2118241907d63c3ed0d3004687907107ff02200d300e64fdf5c5ca358e3266794b12b6b900004084b7fba838ceedcb1364e658";

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn rss_mb() -> f64 {
    sth_core::mem::rss_bytes().unwrap_or(0) as f64 / 1_048_576.0
}

fn workload(s: &Storage) {
    let tip = s.get_last_height().unwrap();
    for h in (1..=tip).rev().take(200) {
        s.get_block_by_height(h).unwrap();
    }
    let newest: Vec<_> = s.transaction_index(None, 0..=u64::MAX, false).take(300).map(|e| e.unwrap()).collect();
    let mut cache = None;
    for e in &newest {
        s.transaction_at(e, &mut cache).unwrap();
        s.get_transaction(&e.id).unwrap();
    }
    for i in 0..500u64 {
        s.get_block_by_height(1 + (i * 7919) % tip).unwrap();
    }
}

#[test]
#[ignore]

fn history_memory_probe() {
    let Ok(dir) = std::env::var("HIST_DB") else { return };
    let mode = std::env::var("HIST_MODE").unwrap_or_default();
    let (empty, full, per_block) = (env_u64("HIST_EMPTY", 1_000_000), env_u64("HIST_FULL", 100_000), env_u64("HIST_PER_BLOCK", 10));
    let legacy = HistoryOptions { enabled: false, ..Default::default() };
    match mode.as_str() {
        "build" => {
            let s = Storage::open_with_options(&dir, Network::mainnet(), 64, legacy).unwrap();
            let senders: Vec<KeyPair> = (0..200).map(|i| KeyPair::from_passphrase(&format!("bench sender {i}")).unwrap()).collect();
            let addrs: Vec<String> = senders.iter().map(|k| k.address(63).unwrap()).collect();
            // mainnet-like headers: 64 rotating forgers and a different (incompressible) signature in every block
            let gens: Vec<String> = senders.iter().take(64).map(|k| k.public_key_hex()).collect();
            let mut x = 0x9E37_79B9_7F4A_7C15u64;
            let mut rnd = |n: usize| -> String {
                (0..n)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        format!("{:02x}", (x >> 32) as u8)
                    })
                    .collect()
            };
            let (mut n, mut prev) = (0u64, "f".repeat(64));
            let every = (empty + full) / full.max(1);
            for h in 1..=empty + full {
                let count = if h % every == 0 { per_block } else { 0 };
                let txs: Vec<String> = (0..count)
                    .map(|_| {
                        n += 1;
                        format!(
                            r#"{{"version":2,"network":63,"typeGroup":1,"type":0,"nonce":"{n}","senderPublicKey":"{}","fee":"100000000","amount":"{}","recipientId":"{}","expiration":0,"signature":"{}","asset":null}}"#,
                            senders[(n % 200) as usize].public_key_hex(),
                            n * 1000,
                            addrs[((n * 31) % 200) as usize],
                            "1".repeat(128)
                        )
                    })
                    .collect();
                let id = format!("{h:064x}");
                let gen = &gens[(h % 64) as usize];
                let sig = format!("3045022100{}0220{}", rnd(32), rnd(32));

                let json = format!(
                    r#"{{"id":"{id}","version":0,"timestamp":{},"previousBlock":"{prev}","height":{h},"numberOfTransactions":{count},"totalAmount":"0","totalFee":"0","reward":"0","payloadLength":0,"payloadHash":"{}","generatorPublicKey":"{gen}","blockSignature":"{sig}","transactions":[{}]}}"#,
                    h * 8,
                    "9".repeat(64),
                    txs.join(",")
                );
                let mut b: Block = serde_json::from_str(&json).unwrap();
                for tx in b.transactions.iter_mut() {
                    tx.id = Some(transaction_id(tx).unwrap());
                }
                b.id = Some(if h == 1 { Network::mainnet().genesis_block_id } else { sth_core::crypto::block_id(&b).unwrap() });
                prev = b.id.clone().unwrap();
                s.save_block(&b).unwrap();
                if h % 100_000 == 0 {
                    println!("built {h} blocks, {n} txs, rss {:.0} MB", rss_mb());
                }
            }
            s.flush().unwrap();
            println!("BUILD done: {} blocks, {n} txs, rss {:.0} MB", empty + full, rss_mb());
        }
        // compression of real block records (HIST_DB = a 0.26.0 `blocks.dat`): ratio and speed per chunk size / level
        "ratio" => {
            let data = std::fs::read(&dir).unwrap();
            let (mut recs, mut pos) = (Vec::new(), 0usize);
            while pos + 4 <= data.len() && recs.len() < env_u64("HIST_BLOCKS", 300_000) as usize {
                let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
                recs.push(&data[pos..pos + 4 + len]);
                pos += 4 + len;
            }
            println!("RATIO: {} blocks, {:.1} MB raw ({} bytes per block)", recs.len(), pos as f64 / 1_048_576.0, pos / recs.len());
            for chunk in [256usize, 1024, 4096] {
                for level in [3, 9, 15, 19, 22] {
                    let t = std::time::Instant::now();
                    let (mut packed, mut frames) = (0usize, Vec::new());
                    for c in recs.chunks(chunk) {
                        let z = zstd::block::compress(&c.concat(), level).unwrap();
                        packed += z.len() + 8;
                        frames.push((z, c.iter().map(|r| r.len()).sum::<usize>()));
                    }
                    let pack_secs = t.elapsed().as_secs_f64();
                    let t = std::time::Instant::now();
                    for (z, raw) in &frames {
                        zstd::block::decompress(z, *raw).unwrap();
                    }
                    println!(
                        "  chunk {chunk:>5} level {level:>2}: {:>5.1} bytes per block ({:.1} % of raw) · pack {:>6.1} MB/s (1 core) · unpack one chunk {:.2} ms",
                        packed as f64 / recs.len() as f64,
                        packed as f64 * 100.0 / pos as f64,
                        pos as f64 / 1_048_576.0 / pack_secs,
                        t.elapsed().as_secs_f64() * 1000.0 / frames.len() as f64
                    );
                }
            }
        }
        // open a store (a 0.26.0 one is converted here) and time random / sequential block reads
        "open" => {
            let dir_size = |d: &std::path::Path| std::fs::read_dir(d).map(|r| r.flatten().map(|e| e.metadata().map(|m| m.len()).unwrap_or(0)).sum::<u64>()).unwrap_or(0) as f64 / 1_048_576.0;
            let cold_dir = std::path::Path::new(&dir).join("cold");
            let before = dir_size(&cold_dir);
            let t = std::time::Instant::now();
            let s = Storage::open_full(&dir, Network::mainnet(), StorageOptions { cache_mb: 16, history: HistoryOptions::default(), cold: ColdOptions::default(), rebuild: true }).unwrap();
            let st = s.cold_status().unwrap();
            println!("OPEN in {:?}: format {} · {} blocks · cold dir {before:.0} MB → {:.0} MB · {} id segments · error {:?} · rss {:.0} MB", t.elapsed(), st.format, st.blocks, dir_size(&cold_dir), st.segments, st.last_error, rss_mb());
            let h = s.history_status().unwrap();
            println!("  main db {:.0} MB · tx index: {} in sled, {} in flat files ({:.1} MB, {} segments) · rebuild pending {}", h.main_bytes as f64 / 1_048_576.0, h.hot_txs, h.cold_txs, h.history_bytes as f64 / 1_048_576.0, h.segments, st.rebuild_pending);
            let t = std::time::Instant::now();
            let newest: Vec<_> = s.transaction_index(None, 0..=u64::MAX, false).skip(100_000).take(100).map(|e| e.unwrap()).collect();
            let oldest = s.transaction_index(None, 0..=u64::MAX, true).take(100).count();
            if let Some(e) = newest.first() {
                let tx = s.get_transaction(&e.id).unwrap().unwrap();
                let addr = tx.recipient_id.clone().unwrap_or_default();
                let n = s.transaction_index_count(Some(&addr), 0..=u64::MAX).unwrap();
                let page = s.transaction_index(Some(&addr), 0..=u64::MAX, false).skip(500).take(100).count();
                println!("  tx index reads (page at offset 100 000, {oldest} oldest, by id, count {n} + page {page} of one address): {:?}", t.elapsed());
            }
            let tip = st.last_height.max(1);
            let t = std::time::Instant::now();
            for i in 0..2_000u64 {
                s.get_block_by_height(1 + (i * 7919) % tip).unwrap().unwrap();
            }
            println!("  2000 random block reads: {:?}", t.elapsed());
            let t = std::time::Instant::now();
            let n = s.get_blocks_from(tip / 2, 20_000).unwrap().len();
            println!("  {n} sequential block reads: {:?}", t.elapsed());
            let ids: Vec<String> = (0..500u64).map(|i| s.get_block_by_height(1 + (i * 104_729) % tip).unwrap().unwrap().id.unwrap()).collect();
            let t = std::time::Instant::now();
            for id in &ids {
                s.get_block_by_id(id).unwrap().unwrap();
            }
            println!("  500 block-by-id reads: {:?} · rss {:.0} MB", t.elapsed(), rss_mb());
        }
        // one hash over everything the readers return (compare a store before / after a conversion)
        "digest" => {
            use sha2::{Digest, Sha256};
            let s = Storage::open(&dir, Network::mainnet()).unwrap();
            let tip = s.get_last_height().unwrap();
            let mut hasher = Sha256::new();
            let mut ids = Vec::new();
            for h in 1..=tip {
                let b = s.get_block_by_height(h).unwrap().unwrap();
                hasher.update(serde_json::to_vec(&b).unwrap());
                if h % 97 == 0 {
                    ids.push((h, b.id.clone().unwrap()));
                }
            }
            for (h, id) in &ids {
                assert_eq!(s.get_block_by_id(id).unwrap().unwrap().height, *h);
                hasher.update(id.as_bytes());
            }
            let mut txs = 0u64;
            for e in s.transaction_index(None, 0..=u64::MAX, true) {
                let e = e.unwrap();
                hasher.update(serde_json::to_vec(&s.get_transaction(&e.id).unwrap().unwrap()).unwrap());
                hasher.update(format!("{}:{}:{}", e.height, e.seq, e.id));
                txs += 1;
            }
            for page in [0usize, 1_000, 500_000] {
                hasher.update(serde_json::to_vec(&s.get_blocks(page, 50).unwrap()).unwrap());
            }
            println!("DIGEST {tip} blocks · {} ids · {txs} txs · {}", ids.len(), hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>());
        }
        "legacy" | "cold" => {
            let opts = if mode == "legacy" { legacy } else { HistoryOptions::default() };
            let t = std::time::Instant::now();
            let s = Storage::open_full(&dir, Network::mainnet(), StorageOptions { cache_mb: env_u64("HIST_CACHE", 64) as u32, history: opts, cold: ColdOptions { enabled: mode == "cold", ..Default::default() }, rebuild: false }).unwrap();
            println!("{mode}: opened in {:?}, rss {:.0} MB", t.elapsed(), rss_mb());
            workload(&s);
            let st = s.history_status().unwrap();
            println!(
                "{mode}: after workload rss {:.0} MB · main {:.0} MB · history {:.0} MB · hot {} · cold {}",
                rss_mb(),
                st.main_bytes as f64 / 1_048_576.0,
                st.history_bytes as f64 / 1_048_576.0,
                st.hot_txs,
                st.cold_txs
            );
        }
        // the 0.26.0 upgrade path: move old blocks to flat files (as the running node does), then the rebuild at the next start
        "migrate" => {
            let full = |rebuild| StorageOptions { cache_mb: env_u64("HIST_CACHE", 64) as u32, history: HistoryOptions::default(), cold: ColdOptions::default(), rebuild };
            let s = Storage::open_full(&dir, Network::mainnet(), full(false)).unwrap();
            let t = std::time::Instant::now();
            let mut peak = 0f64;
            let moved = s
                .archive_blocks(&std::sync::atomic::AtomicBool::new(false), |st| {
                    peak = peak.max(rss_mb());
                    if st.blocks % 200_000 == 0 {
                        println!("  {:.1} % · {} blocks moved · {} id segments · rss {:.0} MB", st.progress_percent, st.blocks, st.segments, rss_mb());
                    }
                })
                .unwrap();
            let st = s.cold_status().unwrap();
            println!("MIGRATE done: {moved} blocks in {:?}, peak rss {:.0} MB · cold files {:.0} MB · {} id segments · rebuild pending {}", t.elapsed(), peak, st.bytes as f64 / 1_048_576.0, st.segments, st.rebuild_pending);
            s.flush().unwrap();
            drop(s);
            let t = std::time::Instant::now();
            let s = Storage::open_full(&dir, Network::mainnet(), full(true)).unwrap();
            println!("REBUILD at open: {:?} · main db {:.0} MB", t.elapsed(), s.history_status().unwrap().main_bytes as f64 / 1_048_576.0);
            let t = std::time::Instant::now();
            let txs = s.archive_history(&std::sync::atomic::AtomicBool::new(false), |_| {}).unwrap();
            let h = s.history_status().unwrap();
            println!("TX INDEX: {txs} moved in {:?} · {} in flat files ({:.1} MB) · {} in sled", t.elapsed(), h.cold_txs, h.history_bytes as f64 / 1_048_576.0, h.hot_txs);
        }
        // rebuild the main tree into a fresh database (what an offline compaction would produce)
        "compact" => {
            let src = sled::Config::new().path(&dir).use_compression(true).compression_factor(3).open().unwrap();
            let dst = sled::Config::new().path(format!("{dir}.compact")).use_compression(true).compression_factor(3).open().unwrap();
            let (a, b) = (src.open_tree("chain").unwrap(), dst.open_tree("chain").unwrap());
            let mut sizes: std::collections::BTreeMap<String, (u64, u64)> = Default::default();
            let mut batch = sled::Batch::default();
            for (i, item) in a.iter().enumerate() {
                let (k, v) = item.unwrap();
                let prefix = String::from_utf8_lossy(&k[..k.iter().position(|c| *c == b':').unwrap_or(0)]).into_owned();
                let e = sizes.entry(prefix).or_default();
                e.0 += 1;
                e.1 += (k.len() + v.len()) as u64;
                batch.insert(k, v);
                if i % 10_000 == 9_999 {
                    b.apply_batch(std::mem::take(&mut batch)).unwrap();
                }
            }
            b.apply_batch(batch).unwrap();
            dst.flush().unwrap();
            for (p, (n, bytes)) in sizes {
                println!("COMPACT prefix {p:6} {n:>10} keys {:>8.1} MB raw", bytes as f64 / 1_048_576.0);
            }
            println!("COMPACT done: {:.0} MB on disk", dst.size_on_disk().unwrap() as f64 / 1_048_576.0);
        }
        "rawopen" => {
            let base = rss_mb();
            let db = sled::Config::new().path(&dir).use_compression(true).compression_factor(3).cache_capacity(16 << 20).open().unwrap();
            let tree = db.open_tree(std::env::var("HIST_TREE").unwrap_or("chain".into())).unwrap();
            let _ = tree.get(b"meta:last_height").unwrap();
            println!("RAWOPEN {dir}: process baseline {base:.0} MB → rss {:.0} MB · {:.0} MB on disk", rss_mb(), db.size_on_disk().unwrap() as f64 / 1_048_576.0);
        }
        other => panic!("HIST_MODE={other}"),
    }
}
