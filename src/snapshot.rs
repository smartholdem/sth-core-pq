//! Author: TechnoL0g
//!
//! Snapshot import — reads dumps produced by `yarn sth snapshot:dump` (core-snapshots,
//! codec "default" = MessagePack codec) and applies them to Sled. Also downloads the
//! latest `<start>-<end>.tgz` from https://snapshots.smartholdem.io/.
//!
//! Dump layout: `<start>-<end>/{meta.json, blocks, transactions, rounds}`.
//! Each stream is gzip (unless `skipCompression`) of `[u32 LE length][record]*`:
//!   * blocks       → serialised block header incl. signature (`Blocks.Serializer.serialize(block, true)`)
//!   * transactions → msgpack `[id, blockId, blockHeight, sequence, timestamp, serialized(bin)]`
//!   * rounds       → msgpack `[publicKey, balance, round]` (delegate ranking — not needed by a relay)

use crate::config::Network;
use crate::crypto::{block_payload_hash, deserialize_block_header, deserialize_transaction, verify_block};
use crate::error::{Error, Result};
use crate::models::{Block, Transaction};
use crate::storage::Storage;
use crate::sync::ChainTip;
use flate2::read::GzDecoder;
use futures::StreamExt;
use indicatif::ProgressBar;
use rmpv::Value;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const DEFAULT_SNAPSHOT_BASE_URL: &str = "https://snapshots.smartholdem.io/";

/// Blocks applied per sled transaction during import.
pub const IMPORT_CHUNK: usize = 1_000;

#[derive(Debug, Clone, Deserialize)]
pub struct MetaRange {
    pub count: u64,
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMeta {
    pub blocks: MetaRange,
    pub transactions: MetaRange,
    pub rounds: MetaRange,
    pub folder: String,
    #[serde(default)]
    pub skip_compression: bool,
    pub network: String,
    #[serde(default)]
    pub package_version: String,
    #[serde(default = "default_codec")]
    pub codec: String,
}

fn default_codec() -> String {
    "default".into()
}

impl SnapshotMeta {
    pub fn read(dir: &Path) -> Result<Self> {
        let raw = std::fs::read(dir.join("meta.json"))
            .map_err(|e| Error::Sync(format!("cannot read {}: {e}", dir.join("meta.json").display())))?;
        Ok(serde_json::from_slice(&raw)?)
    }
}

/// Length-prefixed record stream (`[u32 LE len][bytes]*`), optionally gzip-compressed.
pub struct RecordReader {
    inner: Box<dyn Read>,
    pub count: u64,
}

impl RecordReader {
    pub fn open(path: &Path, compressed: bool) -> Result<Self> {
        let file = File::open(path).map_err(|e| Error::Sync(format!("cannot open {}: {e}", path.display())))?;
        let buffered = BufReader::with_capacity(1 << 20, file);
        let inner: Box<dyn Read> = if compressed {
            Box::new(BufReader::with_capacity(1 << 20, GzDecoder::new(buffered)))
        } else {
            Box::new(buffered)
        };
        Ok(Self { inner, count: 0 })
    }

    /// Next record, `None` at a clean end of stream.
    pub fn next_record(&mut self) -> Result<Option<Vec<u8>>> {
        let mut len = [0u8; 4];
        match self.inner.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(Error::Sync(format!("snapshot read error: {e}"))),
        }
        let n = u32::from_le_bytes(len) as usize;
        let mut data = vec![0u8; n];
        self.inner
            .read_exact(&mut data)
            .map_err(|e| Error::Sync(format!("snapshot record truncated (record {}): {e}", self.count)))?;
        self.count += 1;
        Ok(Some(data))
    }
}

/// Decode one msgpack transaction record into a `Transaction` with block coordinates.
pub fn decode_transaction_record(record: &[u8]) -> Result<Transaction> {
    let value = rmpv::decode::read_value(&mut &record[..])
        .map_err(|e| Error::Sync(format!("transaction record is not msgpack: {e}")))?;
    let items = match value {
        Value::Array(items) if items.len() >= 6 => items,
        other => return Err(Error::Sync(format!("unexpected transaction record shape: {other}"))),
    };
    let str_at = |i: usize| -> Result<String> {
        items[i]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| Error::Sync(format!("transaction record field {i} is not a string")))
    };
    let u64_at = |i: usize| -> Result<u64> {
        items[i]
            .as_u64()
            .ok_or_else(|| Error::Sync(format!("transaction record field {i} is not an integer")))
    };
    let id = str_at(0)?;
    let block_id = str_at(1)?;
    let block_height = u64_at(2)?;
    let sequence = u64_at(3)? as u32;
    let serialized: Vec<u8> = match &items[5] {
        Value::Binary(b) => b.clone(),
        Value::String(s) => hex::decode(s.as_str().unwrap_or_default())?,
        other => return Err(Error::Sync(format!("transaction {id}: serialized field has type {other}"))),
    };

    let mut tx = deserialize_transaction(&serialized)?;
    if tx.id.as_deref() != Some(id.as_str()) {
        return Err(Error::BlockValidation(format!(
            "snapshot transaction id {id} does not match its bytes ({})",
            tx.id.unwrap_or_default()
        )));
    }
    tx.block_id = Some(block_id);
    tx.block_height = Some(block_height);
    tx.sequence = Some(sequence);
    Ok(tx)
}

/// Load the whole `transactions` stream grouped by block height (the chain has few transactions).
pub fn load_transactions(dir: &Path, meta: &SnapshotMeta) -> Result<HashMap<u64, Vec<Transaction>>> {
    let mut reader = RecordReader::open(&dir.join("transactions"), !meta.skip_compression)?;
    let mut by_height: HashMap<u64, Vec<Transaction>> = HashMap::new();
    while let Some(record) = reader.next_record()? {
        let tx = decode_transaction_record(&record)?;
        let h = tx.block_height.unwrap_or(0);
        by_height.entry(h).or_default().push(tx);
    }
    for txs in by_height.values_mut() {
        txs.sort_by_key(|t| t.sequence.unwrap_or(u32::MAX));
    }
    Ok(by_height)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ImportOptions {
    /// Skip secp256k1 signature verification (trusted dump). Ids, linkage and payload hashes are still checked.
    pub fast: bool,
    /// History audit: additionally run the wallet-aware rules (second signatures, smart objects) on every block,
    /// exactly as the live node does — the import stops at the first block legacy accepted but we would reject.
    pub strict: bool,
}

#[derive(Debug, Clone)]
pub struct ImportReport {
    pub snapshot_end: u64,
    pub start_height: u64,
    pub end_height: u64,
    pub imported: u64,
    pub skipped: u64,
    pub elapsed: Duration,
    pub interrupted: bool,
}

/// Find the directory holding `meta.json` (accepts the folder itself, its parent, or a `.tgz`).
pub fn locate_snapshot_dir(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name.ends_with(".tgz") || name.ends_with(".tar.gz") {
            let out = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
            return extract_tgz(path, &out);
        }
        return Err(Error::Sync(format!("{} is not a snapshot directory or .tgz", path.display())));
    }
    if path.join("meta.json").is_file() {
        return Ok(path.to_path_buf());
    }
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(path)
        .map_err(|e| Error::Sync(format!("cannot list {}: {e}", path.display())))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("meta.json").is_file())
        .collect();
    candidates.sort();
    candidates
        .pop()
        .ok_or_else(|| Error::Sync(format!("no meta.json found under {}", path.display())))
}

/// Unpack `<x>.tgz` into `out_dir`, returning the directory that contains `meta.json`.
pub fn extract_tgz(archive: &Path, out_dir: &Path) -> Result<PathBuf> {
    let stem = archive
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.trim_end_matches(".tgz").trim_end_matches(".tar.gz").to_string())
        .unwrap_or_else(|| "snapshot".into());
    let already = out_dir.join(&stem);
    if already.join("meta.json").is_file() {
        return Ok(already);
    }
    let file = File::open(archive).map_err(|e| Error::Sync(format!("cannot open {}: {e}", archive.display())))?;
    let mut tar = tar::Archive::new(GzDecoder::new(BufReader::with_capacity(1 << 20, file)));
    std::fs::create_dir_all(out_dir).map_err(|e| Error::Sync(format!("cannot create {}: {e}", out_dir.display())))?;
    tar.unpack(out_dir)
        .map_err(|e| Error::Sync(format!("cannot extract {}: {e}", archive.display())))?;
    if already.join("meta.json").is_file() {
        return Ok(already);
    }
    // Archive may contain the files directly, or a folder with another name.
    if out_dir.join("meta.json").is_file() {
        return Ok(out_dir.to_path_buf());
    }
    locate_snapshot_dir(out_dir)
}

/// Stream the `blocks` file and apply everything above the local tip. Resumable: blocks at or
/// below the stored height are skipped, Ctrl+C (`cancel`) stops between chunks.
pub fn import_snapshot(
    storage: &Storage,
    dir: &Path,
    opts: ImportOptions,
    pb: &ProgressBar,
    cancel: &AtomicBool,
) -> Result<ImportReport> {
    let started = Instant::now();
    let network: &Network = storage.network();
    let meta = SnapshotMeta::read(dir)?;
    if meta.network != network.name.as_str() {
        return Err(Error::Sync(format!("snapshot is for network '{}', node runs '{}'", meta.network, network.name)));
    }
    if meta.codec != "default" {
        return Err(Error::Sync(format!("unsupported snapshot codec '{}' (only 'default' / MessagePack)", meta.codec)));
    }

    pb.set_message("loading transactions");
    let mut txs_by_height = load_transactions(dir, &meta)?;
    let tx_total: usize = txs_by_height.values().map(|v| v.len()).sum();
    if tx_total as u64 != meta.transactions.count {
        return Err(Error::Sync(format!(
            "snapshot transactions: read {tx_total}, meta.json says {}",
            meta.transactions.count
        )));
    }

    let mut tip = match storage.get_last_block()? {
        Some(b) => ChainTip { height: b.height, id: b.id },
        None => ChainTip { height: 0, id: None },
    };
    let start_height = tip.height;
    pb.set_length(meta.blocks.end);
    pb.set_position(tip.height);
    pb.reset_eta();
    pb.set_message(String::new());

    let mut reader = RecordReader::open(&dir.join("blocks"), !meta.skip_compression)?;
    let mut chunk: Vec<Block> = Vec::with_capacity(IMPORT_CHUNK);
    let mut imported = 0u64;
    let mut skipped = 0u64;
    let mut interrupted = false;

    while let Some(record) = reader.next_record()? {
        let mut block = deserialize_block_header(&record, network)?;
        if block.height <= tip.height {
            skipped += 1;
            continue;
        }
        if block.height != tip.height + 1 {
            return Err(Error::Sync(format!(
                "snapshot height gap: expected {}, got {}",
                tip.height + 1,
                block.height
            )));
        }
        if let Some(prev) = &tip.id {
            if &block.previous_block != prev {
                return Err(Error::BlockValidation(format!(
                    "block {} previousBlock {} does not match tip {prev}",
                    block.height, block.previous_block
                )));
            }
        }
        if block.number_of_transactions > 0 {
            block.transactions = txs_by_height.remove(&block.height).unwrap_or_default();
            if block.transactions.len() as u32 != block.number_of_transactions {
                return Err(Error::Sync(format!(
                    "block {}: snapshot has {} transactions, header says {}",
                    block.height,
                    block.transactions.len(),
                    block.number_of_transactions
                )));
            }
        }
        if opts.fast {
            if block.height > 1 && block_payload_hash(&block)? != block.payload_hash {
                return Err(Error::BlockValidation(format!("block {}: invalid payload hash", block.height)));
            }
        } else {
            let v = verify_block(&block, network);
            if !v.verified {
                return Err(Error::BlockValidation(format!("block {}: {}", block.height, v.errors.join("; "))));
            }
        }

        tip = ChainTip { height: block.height, id: block.id.clone() };
        chunk.push(block);
        if chunk.len() >= IMPORT_CHUNK {
            if opts.strict {
                crate::sync::check_stateful_rules(storage, network, &chunk)?;
            }
            storage.apply_blocks(&chunk)?;
            imported += chunk.len() as u64;
            chunk.clear();
            pb.set_position(tip.height);
            if cancel.load(Ordering::Relaxed) {
                interrupted = true;
                break;
            }
        }
    }
    if !chunk.is_empty() {
        if opts.strict {
            crate::sync::check_stateful_rules(storage, network, &chunk)?;
        }
        storage.apply_blocks(&chunk)?;
        imported += chunk.len() as u64;
        pb.set_position(tip.height);
    }
    storage.flush()?;

    Ok(ImportReport {
        snapshot_end: meta.blocks.end,
        start_height,
        end_height: tip.height,
        imported,
        skipped,
        elapsed: started.elapsed(),
        interrupted,
    })
}

/// Parse `<a href="1-11705253.tgz">` links and return the archive with the highest end height.
pub fn pick_latest_archive(index_html: &str) -> Option<(String, u64)> {
    let mut best: Option<(String, u64)> = None;
    for part in index_html.split("href=\"").skip(1) {
        let href = match part.split('"').next() {
            Some(h) => h.trim(),
            None => continue,
        };
        let name = href.rsplit('/').next().unwrap_or(href);
        let stem = match name.strip_suffix(".tgz") {
            Some(s) => s,
            None => continue,
        };
        let mut it = stem.split('-');
        let (Some(start), Some(end), None) = (it.next(), it.next(), it.next()) else { continue };
        if start.parse::<u64>().is_err() {
            continue;
        }
        let Ok(end) = end.parse::<u64>() else { continue };
        if best.as_ref().map_or(true, |(_, b)| end > *b) {
            best = Some((href.to_string(), end));
        }
    }
    best
}

/// Download the newest snapshot archive into `out_dir` (skipped when already present and complete).
pub async fn download_latest(base_url: &str, out_dir: &Path, pb: &ProgressBar) -> Result<PathBuf> {
    let base = if base_url.ends_with('/') { base_url.to_string() } else { format!("{base_url}/") };
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("sth-core/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let index = client.get(&base).send().await?.error_for_status()?.text().await?;
    let (href, end) = pick_latest_archive(&index)
        .ok_or_else(|| Error::Sync(format!("no <start>-<end>.tgz links found at {base}")))?;
    let url = if href.starts_with("http") { href.clone() } else { format!("{base}{href}") };
    let file_name = href.rsplit('/').next().unwrap_or(&href).to_string();
    std::fs::create_dir_all(out_dir).map_err(|e| Error::Sync(format!("cannot create {}: {e}", out_dir.display())))?;
    let target = out_dir.join(&file_name);

    let resp = client.get(&url).send().await?.error_for_status()?;
    let total = resp.content_length().unwrap_or(0);
    if total > 0 && target.metadata().map(|m| m.len() == total).unwrap_or(false) {
        tracing::info!(file = %target.display(), height = end, "snapshot archive already downloaded");
        return Ok(target);
    }
    tracing::info!(url, size_mb = total / 1_048_576, height = end, "downloading snapshot");
    pb.set_length(total);
    pb.set_position(0);
    let tmp = out_dir.join(format!("{file_name}.part"));
    let mut file = File::create(&tmp).map_err(|e| Error::Sync(format!("cannot create {}: {e}", tmp.display())))?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).map_err(|e| Error::Sync(format!("write error: {e}")))?;
        pb.inc(chunk.len() as u64);
    }
    file.flush().map_err(|e| Error::Sync(format!("flush error: {e}")))?;
    drop(file);
    std::fs::rename(&tmp, &target).map_err(|e| Error::Sync(format!("cannot move {}: {e}", tmp.display())))?;
    Ok(target)
}
