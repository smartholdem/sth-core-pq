//! Author: TechnoL0g
//!
//! Compact transaction index outside sled (0.27.0). The per-transaction sled records `t:`, `tl:` and `wt:` of
//! everything older than the hot window (last 50 000 transactions) are replaced by three flat files in the cold store
//! directory (`cold.rs`), ~75 bytes per transaction instead of ~380 in sled and no resident memory:
//!
//!   `txs.dat`       38-byte records `[height BE u32][seq BE u16][transaction id, 32 raw bytes]` in chain order; the
//!                   position of a record is the transaction's ordinal (height range → ordinal range by binary search)
//!   `txi-<n>.seg`   sorted `[first 8 bytes of the id][ordinal BE u32]` — id → transaction (confirmed against txs.dat)
//!   `txw-<n>.seg`   sorted `[address, 21 raw bytes][ordinal BE u32]` — history of one address in chain order
//!
//! The cursor lives in the cold manifest (`tx.height`, `tx.count`): blocks at or below `tx.height` are answered from the
//! files, everything above from sled. Order per batch: the sled records are compared with the ones derived from the
//! blocks → files appended and fsynced → read back → one atomic sled batch removes the records and stores the manifest.
//! Only blocks deeper than the undo depth are indexed here, so a rollback never reaches them.

use super::cold::{id_bytes, io, lock, read_at, write_at, Cold, Manifest, Seg, SegSet, TXS, TX_REC};
use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Transactions whose indexes stay in the main database.
pub const HOT_TX_WINDOW: u64 = 50_000;
/// Transactions moved per batch (bounded memory + short `apply_lock` hold).
const ARCHIVE_BATCH_TXS: u64 = 2_000;
/// 0.26.0 experimental second sled database (`history:` section) — its records are taken back at open.
const KEY_HIST_COPIED: &[u8] = b"meta:hist_copied";
const KEY_HIST_PRUNED: &[u8] = b"meta:hist_pruned";
const KEY_HIST_PATH: &[u8] = b"meta:hist_path";

#[derive(Debug, Clone)]
pub struct HistoryOptions {
    /// Off = the transaction indexes stay in sled (already moved ones keep being read from the flat files).
    pub enabled: bool,
    pub hot_window: u64,
}

impl Default for HistoryOptions {
    fn default() -> Self {
        Self { enabled: true, hot_window: HOT_TX_WINDOW }
    }
}

/// `/api/ntfry/metrics` → `history`, `sth-core history`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryStatus {
    pub enabled: bool,
    pub path: String,
    pub hot_window: u64,
    pub total_txs: u64,
    pub hot_txs: u64,
    pub cold_txs: u64,
    /// Transactions still to move out of the main database.
    pub pending_txs: u64,
    pub copied_height: u64,
    pub running: bool,
    pub progress_percent: f64,
    pub main_bytes: u64,
    pub history_bytes: u64,
    pub segments: usize,
    pub main_cache_mb: u32,
    pub last_error: Option<String>,
}

pub(super) type RawIter = Box<dyn Iterator<Item = sled::Result<(IVec, IVec)>>>;
type EntryIter<'a> = Box<dyn Iterator<Item = Result<TxIndexEntry>> + 'a>;

pub(super) fn raw_index(tree: &sled::Tree, prefix: &[u8], lo: u64, hi: u64, ascending: bool) -> RawIter {
    let mut start = prefix.to_vec();
    start.extend_from_slice(&lo.to_be_bytes());
    start.extend_from_slice(&0u32.to_be_bytes());
    let mut end = prefix.to_vec();
    end.extend_from_slice(&hi.to_be_bytes());
    end.extend_from_slice(&u32::MAX.to_be_bytes());
    let range = tree.range(start..=end);
    if ascending { Box::new(range) } else { Box::new(range.rev()) }
}

type Records = Vec<(Vec<u8>, Vec<u8>)>;

/// The index records `write_block` produces for one block.
fn index_records(block: &Block, network: u8, out: &mut Records) -> Result<()> {
    let height = block.height.to_be_bytes();
    for (seq, tx) in block.transactions.iter().enumerate() {
        let Some(id) = &tx.id else { continue };
        let seq = tx.sequence.unwrap_or(seq as u32);
        out.push((prefixed(PREFIX_TX, id), height.to_vec()));
        out.push((tx_order_key(PREFIX_TX_LIST, "", block.height, seq), id.as_bytes().to_vec()));
        for addr in tx_addresses(tx, network)? {
            out.push((tx_order_key(PREFIX_WALLET_TX, &addr, block.height, seq), id.as_bytes().to_vec()));
        }
    }
    Ok(())
}

/// Address as its 21 raw bytes (version + hash); `None` when it is not a canonical base58check address.
fn addr_key(addr: &str) -> Option<[u8; 21]> {
    let k: [u8; 21] = bs58::decode(addr).with_check(None).into_vec().ok()?.try_into().ok()?;
    (bs58::encode(k).with_check().into_string() == addr).then_some(k)
}

fn wt_key(addr: &[u8; 21], ordinal: u64) -> [u8; 25] {
    let mut k = [0u8; 25];
    k[..21].copy_from_slice(addr);
    k[21..].copy_from_slice(&(ordinal.min(u64::from(u32::MAX)) as u32).to_be_bytes());
    k
}

fn entry(rec: &[u8; TX_REC]) -> TxIndexEntry {
    TxIndexEntry {
        height: u64::from(u32::from_be_bytes(rec[..4].try_into().expect("4 bytes"))),
        seq: u32::from(u16::from_be_bytes(rec[4..6].try_into().expect("2 bytes"))),
        id: hex::encode(&rec[6..]),
    }
}

fn ordinal_of(rec: &[u8]) -> u64 {
    u64::from(u32::from_be_bytes(rec[rec.len() - 4..].try_into().expect("4 bytes")))
}

impl Cold {
    fn tx_read(&self, ordinal: u64, n: usize) -> Result<Vec<[u8; TX_REC]>> {
        let mut bytes = vec![0u8; n * TX_REC];
        read_at(&self.txs, ordinal * TX_REC as u64, &mut bytes)?;
        Ok(bytes.chunks_exact(TX_REC).map(|c| c.try_into().expect("record")).collect())
    }

    /// First ordinal (below `count`) whose transaction sits at `height` or above.
    fn tx_ordinal(&self, height: u64, count: u64) -> Result<u64> {
        let (mut lo, mut hi) = (0u64, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if entry(&self.tx_read(mid, 1)?[0]).height < height { lo = mid + 1 } else { hi = mid }
        }
        Ok(lo)
    }

    /// Ordinals `[a, b)` of the transactions in heights `lo..=hi`.
    fn tx_ordinals(&self, lo: u64, hi: u64) -> Result<(u64, u64)> {
        let (cursor, count) = self.tx_cursor();
        if count == 0 || lo > cursor {
            return Ok((0, 0));
        }
        Ok((self.tx_ordinal(lo, count)?, if hi >= cursor { count } else { self.tx_ordinal(hi + 1, count)? }))
    }
}

/// Take the records of the 0.26.0 experimental `history:` sled database back into the main tree (called at open,
/// before anything reads the indexes); the flat-file mover handles them from there.
pub(super) fn restore_legacy(main: &sled::Tree, main_path: &Path) -> Result<()> {
    let Some(cursor) = main.get(KEY_HIST_COPIED)? else { return Ok(()) };
    let archived = be_u64(cursor.get(8..16).unwrap_or(&[]));
    let path = main.get(KEY_HIST_PATH)?.map(|v| std::path::PathBuf::from(String::from_utf8_lossy(&v).into_owned())).unwrap_or_else(|| main_path.join("history"));
    if archived > 0 {
        let empty = || Error::Config(format!("history store {} is empty, but {archived} transactions were archived there — move the directory back", path.display()));
        if !path.join("db").exists() {
            return Err(empty());
        }
        let db = sled::Config::new().path(&path).use_compression(true).compression_factor(3).cache_capacity(8 << 20).open()?;
        let tree = db.open_tree("history")?;
        if tree.is_empty() {
            return Err(empty());
        }
        tracing::info!(transactions = archived, "transaction history: taking the records of the 0.26.0 `history` database back into the main database (one-time)");
        let (mut batch, mut n) = (sled::Batch::default(), 0u64);
        for item in tree.iter() {
            let (k, v) = item?;
            batch.insert(k, v);
            n += 1;
            if n % 10_000 == 0 {
                main.apply_batch(std::mem::take(&mut batch))?;
            }
        }
        main.apply_batch(batch)?;
        main.flush()?;
    }
    let mut batch = sled::Batch::default();
    for k in [KEY_HIST_COPIED, KEY_HIST_PRUNED, KEY_HIST_PATH] {
        batch.remove(k);
    }
    main.apply_batch(batch)?;
    main.flush()?;
    if archived > 0 {
        let _ = std::fs::remove_dir_all(&path);
    }
    Ok(())
}

impl Storage {
    /// Height below or at which the transaction index is answered from the flat files (0 = everything is in sled).
    pub(super) fn tx_cold_height(&self) -> u64 {
        self.cold.as_ref().map(|c| c.tx_cursor().0).unwrap_or(0)
    }

    /// Test / tuning hook: hot window size and the number of newest blocks whose transactions always stay in sled.
    pub fn set_history_window(&self, hot_window: u64, keep_blocks: u64) {
        if let Some(c) = &self.cold {
            c.hot_window.store(hot_window, Ordering::Relaxed);
            c.tx_keep.store(keep_blocks, Ordering::Relaxed);
        }
    }

    pub(super) fn tx_height(&self, id: &str) -> Result<Option<u64>> {
        if let Some(v) = self.tree.get(prefixed(PREFIX_TX, id))? {
            return ivec_to_height(&v).map(Some);
        }
        let (Some(c), Some(raw)) = (&self.cold, id_bytes(id)) else { return Ok(None) };
        let count = c.tx_cursor().1;
        for rec in c.tx_ids.find(&raw[..8])? {
            let ordinal = ordinal_of(&rec);
            if ordinal < count {
                // the index holds 8 bytes of the id: txs.dat confirms the match
                let full = c.tx_read(ordinal, 1)?[0];
                if full[6..] == raw {
                    return Ok(Some(entry(&full).height));
                }
            }
        }
        Ok(None)
    }

    /// Index entries of heights `lo..=hi` held by the flat files, in chain order (or reversed).
    fn cold_index(&self, address: Option<&str>, lo: u64, hi: u64, ascending: bool) -> EntryIter<'_> {
        let Some(c) = &self.cold else { return Box::new(std::iter::empty()) };
        let build = || -> Result<EntryIter<'_>> {
            let (mut a, mut b) = c.tx_ordinals(lo, hi)?;
            if a >= b {
                return Ok(Box::new(std::iter::empty()));
            }
            match address {
                None => {
                    let mut buf = VecDeque::new();
                    Ok(Box::new(std::iter::from_fn(move || {
                        if buf.is_empty() {
                            if a >= b {
                                return None;
                            }
                            let n = (b - a).min(256);
                            match c.tx_read(if ascending { a } else { b - n }, n as usize) {
                                Ok(v) => buf.extend(v),
                                Err(e) => {
                                    a = b;
                                    return Some(Err(e));
                                }
                            }
                            if ascending { a += n } else { b -= n }
                        }
                        (if ascending { buf.pop_front() } else { buf.pop_back() }).map(|rec| Ok(entry(&rec)))
                    })))
                }
                Some(addr) => {
                    let Some(k) = addr_key(addr) else { return Ok(Box::new(std::iter::empty())) };
                    let scan = c.tx_wt.scan(&wt_key(&k, a), &wt_key(&k, b), ascending)?;
                    Ok(Box::new(scan.map(move |rec| Ok(entry(&c.tx_read(ordinal_of(&rec?), 1)?[0])))))
                }
            }
        };
        build().unwrap_or_else(|e| Box::new(std::iter::once(Err(e))))
    }

    /// Height-ordered index walk: flat files for the heights they hold, sled above them.
    pub(super) fn merged_index(&self, address: Option<&str>, lo: u64, hi: u64, ascending: bool) -> EntryIter<'_> {
        let cursor = self.tx_cold_height();
        let prefix = tx_index_prefix(address);
        let plen = prefix.len();
        let hot_lo = if cursor == 0 { lo } else { lo.max(cursor + 1) };
        let hot: EntryIter<'_> = if hot_lo > hi {
            Box::new(std::iter::empty())
        } else {
            Box::new(raw_index(&self.tree, &prefix, hot_lo, hi, ascending).map(move |item| {
                let (k, v) = item?;
                let bad = || Error::Storage(sled::Error::Unsupported("corrupt transaction index key".into()));
                let height = u64::from_be_bytes(k.get(plen..plen + 8).ok_or_else(bad)?.try_into().map_err(|_| bad())?);
                let seq = u32::from_be_bytes(k.get(plen + 8..plen + 12).ok_or_else(bad)?.try_into().map_err(|_| bad())?);
                Ok(TxIndexEntry { height, seq, id: String::from_utf8_lossy(&v).into_owned() })
            }))
        };
        if cursor == 0 || lo > cursor {
            return hot;
        }
        let cold = self.cold_index(address, lo, hi, ascending);
        if ascending { Box::new(cold.chain(hot)) } else { Box::new(hot.chain(cold)) }
    }

    /// Entries in heights `lo..=hi`: O(log n) for the flat files, a key scan for the part still in sled.
    pub(super) fn merged_index_count(&self, address: Option<&str>, lo: u64, hi: u64) -> Result<u64> {
        let cursor = self.tx_cold_height();
        let mut n = 0u64;
        if let (Some(c), true) = (&self.cold, cursor != 0 && lo <= cursor) {
            let (a, b) = c.tx_ordinals(lo, hi)?;
            n += match address {
                None => b - a,
                Some(addr) => match addr_key(addr) {
                    Some(k) => c.tx_wt.count(&wt_key(&k, a), &wt_key(&k, b))?,
                    None => 0,
                },
            };
        }
        let hot_lo = if cursor == 0 { lo } else { lo.max(cursor + 1) };
        if hot_lo <= hi {
            for item in raw_index(&self.tree, &tx_index_prefix(address), hot_lo, hi, true) {
                item?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Heights of whole blocks with transactions in sled heights `from..=to`, at most `budget` transactions.
    /// Returns (heights, last height covered, transactions).
    fn collect_heights(&self, from: u64, to: u64, budget: u64) -> Result<(Vec<u64>, u64, u64)> {
        let plen = PREFIX_TX_LIST.len();
        let (mut heights, mut n, mut pending, mut exhausted) = (Vec::new(), 0u64, None::<(u64, u64)>, true);
        for item in raw_index(&self.tree, PREFIX_TX_LIST, from, to, true) {
            let (k, _) = item?;
            let height = be_u64(k.get(plen..plen + 8).unwrap_or(&[]));
            if let Some((ph, pc)) = pending {
                if ph == height {
                    pending = Some((ph, pc + 1));
                    continue;
                }
                pending = None;
                if n + pc > budget {
                    exhausted = false;
                    break;
                }
                heights.push(ph);
                n += pc;
                if n >= budget {
                    exhausted = false;
                    break;
                }
            }
            pending = Some((height, 1));
        }
        if let Some((ph, pc)) = pending {
            if n + pc <= budget {
                heights.push(ph);
                n += pc;
            } else {
                exhausted = false;
            }
        }
        let last = if exhausted { to } else { heights.last().copied().unwrap_or(from.saturating_sub(1)) };
        Ok((heights, last, n))
    }

    /// Append the index of `blocks` (ascending, above the cursor) to the flat files: fsynced and read back, not yet
    /// committed. Updates `m` (count, segment counter) and returns the new segments for the caller to push.
    fn tx_append(&self, c: &Cold, m: &mut Manifest, blocks: &[Block]) -> Result<Option<(Arc<Seg>, Arc<Seg>)>> {
        let (mut dat, mut ids, mut wts) = (Vec::new(), Vec::new(), Vec::new());
        let mut ordinal = m.tx.count;
        for block in blocks {
            let mut txs: Vec<(u32, &Transaction, &String)> = block.transactions.iter().enumerate().filter_map(|(i, tx)| tx.id.as_ref().map(|id| (tx.sequence.unwrap_or(i as u32), tx, id))).collect();
            txs.sort_by_key(|t| t.0);
            for (seq, tx, id) in txs {
                let not_moved = |why: &str| Error::Sync(format!("transaction {id} (block {}): {why} — its index stays in the main database", block.height));
                let raw = id_bytes(id).filter(|r| hex::encode(r) == *id).ok_or_else(|| not_moved("id is not 32 bytes of lowercase hex"))?;
                let (h, s, o) = (u32::try_from(block.height), u16::try_from(seq), u32::try_from(ordinal));
                let (Ok(h), Ok(s), Ok(o)) = (h, s, o) else { return Err(not_moved("height / sequence / ordinal does not fit the compact index")) };
                dat.extend_from_slice(&h.to_be_bytes());
                dat.extend_from_slice(&s.to_be_bytes());
                dat.extend_from_slice(&raw);
                let mut rec = [0u8; 12];
                rec[..8].copy_from_slice(&raw[..8]);
                rec[8..].copy_from_slice(&o.to_be_bytes());
                ids.push(rec);
                for addr in tx_addresses(tx, self.network.pubkey_hash)? {
                    wts.push(wt_key(&addr_key(&addr).ok_or_else(|| not_moved("an address is not canonical base58check"))?, ordinal));
                }
                ordinal += 1;
            }
        }
        if ids.is_empty() {
            return Ok(None);
        }
        write_at(&c.txs, m.tx.count * TX_REC as u64, &dat, true)?;
        let id_seg = c.tx_ids.write(m.seg_seq, &mut ids)?;
        let wt_seg = c.tx_wt.write(m.seg_seq + 1, &mut wts)?;
        let back: Vec<u8> = c.tx_read(m.tx.count, ids.len())?.concat();
        let mut ok = back == dat;
        for rec in &ids {
            ok = ok && SegSet::seg_contains(&id_seg, rec)?;
        }
        for rec in &wts {
            ok = ok && SegSet::seg_contains(&wt_seg, rec)?;
        }
        if !ok {
            return Err(Error::Sync("transaction index verification failed — nothing was removed from the main database".into()));
        }
        m.tx.count = ordinal;
        m.seg_seq += 2;
        Ok(Some((id_seg, wt_seg)))
    }

    /// `wt:` records in sled per height above the flat index (one key scan). sled may hold address records that the
    /// stored block does not explain (a block that arrived as JSON with extra fields): the mover compares the numbers
    /// and leaves such a range in sled instead of dropping records silently.
    fn wallet_index_counts(&self) -> Result<Vec<(u64, u64)>> {
        let cursor = self.tx_cold_height();
        let mut counts: HashMap<u64, u64> = HashMap::new();
        for k in self.tree.scan_prefix(PREFIX_WALLET_TX).keys() {
            let k = k?;
            let height = be_u64(k.get(k.len().saturating_sub(12)..k.len().saturating_sub(4)).unwrap_or(&[]));
            if height > cursor {
                *counts.entry(height).or_default() += 1;
            }
        }
        let mut counts: Vec<(u64, u64)> = counts.into_iter().collect();
        counts.sort_unstable();
        Ok(counts)
    }

    /// One resumable batch. Returns the transactions moved (0 = nothing left to do).
    /// `scanned_tip` is the chain height `wallet_counts` was taken at: nothing above its keep margin is touched.
    fn archive_step(&self, max_txs: u64, wallet_counts: &[(u64, u64)], scanned_tip: u64) -> Result<u64> {
        let Some(c) = &self.cold else { return Ok(0) };
        if !c.tx_enabled {
            return Ok(0);
        }
        let _writer = lock(&c.write_lock);
        let (cursor, count) = c.tx_cursor();
        let hot = self.transaction_count()?.saturating_sub(count);
        let window = c.hot_window.load(Ordering::Relaxed);
        let keep = c.tx_keep.load(Ordering::Relaxed);
        let limit = self.get_last_height()?.min(scanned_tip).saturating_sub(keep);
        if hot <= window || limit <= cursor {
            return Ok(0);
        }
        let (heights, last, n) = self.collect_heights(cursor + 1, limit, (hot - window).min(max_txs))?;
        if last <= cursor {
            return Ok(0);
        }
        let (mut blocks, mut records) = (Vec::with_capacity(heights.len()), Vec::new());
        for h in &heights {
            let block = self.get_block_by_height(*h)?.ok_or_else(|| Error::NotFound(format!("block {h} (transaction index move)")))?;
            index_records(&block, self.network.pubkey_hash, &mut records)?;
            blocks.push(block);
        }
        // what leaves sled must be exactly what the blocks say (and what the flat files are built from)
        let listed = blocks.iter().map(|b| b.transactions.iter().filter(|t| t.id.is_some()).count() as u64).sum::<u64>();
        if listed != n {
            return Err(Error::Sync(format!("transaction index of blocks {}..{last} lists {n} transactions, the blocks hold {listed} — not moved", cursor + 1)));
        }
        let in_sled: u64 = wallet_counts[wallet_counts.partition_point(|c| c.0 <= cursor)..wallet_counts.partition_point(|c| c.0 <= last)].iter().map(|c| c.1).sum();
        let derived = records.iter().filter(|(k, _)| k.starts_with(PREFIX_WALLET_TX)).count() as u64;
        if in_sled != derived {
            return Err(Error::Sync(format!("address index of blocks {}..{last} holds {in_sled} records, the blocks explain {derived} — the transaction index stays in the main database", cursor + 1)));
        }
        for (k, v) in &records {
            if self.tree.get(k)?.as_deref() != Some(v.as_slice()) {
                return Err(Error::Sync(format!("transaction index record {} does not match its block — not moved", String::from_utf8_lossy(&k[..k.len().min(24)]))));
            }
        }
        let mut m = c.next_manifest();
        let segs = self.tx_append(c, &mut m, &blocks)?;
        m.tx.height = last;
        m.moved_since_rebuild += n * 2;
        let _guard = self.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
        // a rollback moved the tip while the batch was written: leave sled alone, the next pass re-evaluates
        if self.get_last_height()?.saturating_sub(keep) < last {
            tracing::debug!(last, "transaction index: the tip moved back while a batch was written — batch dropped");
            lock(&c.txs).set_len(count * TX_REC as u64).map_err(io)?;
            for seg in segs.iter().flat_map(|(a, b)| [a, b]) {
                let _ = std::fs::remove_file(c.dir.join(&seg.meta.file));
            }
            return Ok(0);
        }
        if let Some((id_seg, wt_seg)) = segs {
            m.tx.id_segs.push(id_seg.meta.clone());
            m.tx.wt_segs.push(wt_seg.meta.clone());
            c.tx_ids.push(id_seg);
            c.tx_wt.push(wt_seg);
        }
        c.install(m.clone());
        let mut batch = sled::Batch::default();
        for (k, _) in &records {
            batch.remove(k.as_slice());
        }
        batch.insert(Cold::manifest_key(), serde_json::to_vec(&m)?);
        self.tree.apply_batch(batch)?;
        Ok(n.max(1))
    }

    /// Recreate the flat transaction index from the blocks (a file was lost or damaged). Called at open.
    pub(super) fn rebuild_tx_index(&self) -> Result<()> {
        let Some(c) = &self.cold else { return Ok(()) };
        let started = std::time::Instant::now();
        let mut m = c.next_manifest();
        tracing::warn!(upto = m.tx.height, "transaction index: a flat file is missing or damaged — rebuilding it from the blocks");
        (m.tx.count, m.tx.id_segs, m.tx.wt_segs) = (0, Vec::new(), Vec::new());
        lock(&c.txs).set_len(0).map_err(io)?;
        c.tx_ids.set(Vec::new());
        c.tx_wt.set(Vec::new());
        let (mut pending, mut n) = (Vec::new(), 0usize);
        let mut blocks = self.blocks_between(self.first_height()?, m.tx.height).peekable();
        while let Some(block) = blocks.next() {
            let block = block?;
            n += block.transactions.len();
            if !block.transactions.is_empty() {
                pending.push(block);
            }
            if n >= 100_000 || (blocks.peek().is_none() && !pending.is_empty()) {
                if let Some((id_seg, wt_seg)) = self.tx_append(c, &mut m, &pending)? {
                    m.tx.id_segs.push(id_seg.meta.clone());
                    m.tx.wt_segs.push(wt_seg.meta.clone());
                    c.tx_ids.push(id_seg);
                    c.tx_wt.push(wt_seg);
                }
                pending.clear();
                n = 0;
            }
        }
        drop(blocks);
        let count = m.tx.count;
        c.commit(&self.tree, m)?;
        self.cold_merge(c)?;
        tracing::info!(transactions = count, elapsed = ?started.elapsed(), "transaction index: rebuilt");
        Ok(())
    }

    /// True when the hot window overflowed by 10 %.
    pub fn history_due(&self) -> bool {
        let Some(c) = self.cold.as_ref().filter(|c| c.tx_enabled) else { return false };
        let (cursor, count) = c.tx_cursor();
        let window = c.hot_window.load(Ordering::Relaxed);
        let limit = self.get_last_height().unwrap_or(0).saturating_sub(c.tx_keep.load(Ordering::Relaxed));
        limit > cursor && self.transaction_count().unwrap_or(0).saturating_sub(count) > window + window / 10
    }

    /// Move the index of everything older than the hot window to the flat files. Safe on a running node, resumable
    /// after any interruption (`cancel`, crash, power loss). `progress` is called every 25 batches.
    pub fn archive_history(&self, cancel: &AtomicBool, mut progress: impl FnMut(&HistoryStatus)) -> Result<u64> {
        let Some(c) = self.cold.as_ref().filter(|c| c.tx_enabled) else { return Ok(0) };
        if c.tx_running.swap(true, Ordering::AcqRel) {
            return Ok(0);
        }
        let mut run = || -> Result<u64> {
            if !self.history_due() {
                return Ok(0);
            }
            let scanned_tip = self.get_last_height()?;
            let wallet_counts = self.wallet_index_counts()?;
            let (mut moved, mut batches) = (0u64, 0u64);
            while !cancel.load(Ordering::Relaxed) {
                let n = self.archive_step(ARCHIVE_BATCH_TXS, &wallet_counts, scanned_tip)?;
                if n == 0 {
                    break;
                }
                moved += n;
                batches += 1;
                self.cold_merge(c)?;
                if batches % 25 == 0 {
                    self.tree.flush()?;
                    progress(&self.history_status()?);
                }
            }
            if moved > 0 {
                self.tree.flush()?;
            }
            Ok(moved)
        };
        let res = run();
        c.tx_running.store(false, Ordering::Release);
        *lock(&c.tx_error) = res.as_ref().err().map(|e| e.to_string());
        res
    }

    pub fn history_status(&self) -> Result<HistoryStatus> {
        let total = self.transaction_count()?;
        let main_bytes = self.db.size_on_disk().unwrap_or(0);
        let off = HistoryStatus { total_txs: total, hot_txs: total, main_bytes, main_cache_mb: self.cache_mb, ..Default::default() };
        let Some(c) = &self.cold else { return Ok(off) };
        let (cursor, count) = c.tx_cursor();
        if !c.tx_enabled && count == 0 {
            return Ok(off);
        }
        let window = c.hot_window.load(Ordering::Relaxed);
        let running = c.tx_running.load(Ordering::Relaxed);
        let pending = total.saturating_sub(count).saturating_sub(window);
        let pending = if c.tx_enabled && (pending > window / 10 || running) { pending } else { 0 };
        let done = count as f64;
        Ok(HistoryStatus {
            enabled: true,
            path: c.dir.join(TXS).display().to_string(),
            hot_window: window,
            total_txs: total,
            hot_txs: total.saturating_sub(count),
            cold_txs: count,
            pending_txs: pending,
            copied_height: cursor,
            running,
            progress_percent: if pending == 0 { 100.0 } else { (done / (done + pending as f64) * 1000.0).floor() / 10.0 },
            main_bytes,
            history_bytes: count * TX_REC as u64 + c.tx_ids.bytes() + c.tx_wt.bytes(),
            segments: c.tx_ids.list().len() + c.tx_wt.list().len(),
            main_cache_mb: self.cache_mb,
            last_error: lock(&c.tx_error).clone(),
        })
    }
}
