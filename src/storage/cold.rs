//! Author: TechnoL0g
//!
//! Cold store: flat files in `<db_path>/cold` for everything sled does not need at hand. sled keeps resident metadata
//! for every page of an open database, flat files cost no memory (docs/MEMORY-SLED-FINDINGS_RU.md).
//!
//! Blocks deeper than `UNDO_DEPTH + 100` (format 2, 0.27.0 — compressed):
//!   `blocks.zst`    chunks `[u32 LE raw len][u32 LE packed len][zstd]`; a chunk unpacks to `[u32 LE len][block record]*`
//!                   for up to `CHUNK_BLOCKS` consecutive heights (the records are the bytes sled stored under `b:<height>`)
//!   `blocks.zix`    `[u64 LE first height][u64 LE offset]` per chunk (held in memory: 16 bytes per chunk)
//!   `bid2-<n>.seg`  sorted 12-byte records `[first 8 bytes of the block id][height BE u32]`; a hit is confirmed against
//!                   the block itself
//! Transaction index older than the hot window (see `history.rs`): `txs.dat`, `txi-<n>.seg`, `txw-<n>.seg`.
//!
//! Format 1 (0.26.0: `blocks.dat`, `blocks.idx`, 40-byte `bid-<n>.seg`) is converted at the first start; the old files
//! are deleted only after the new ones are complete, fsynced and recorded. If the conversion fails the store keeps
//! working read-only on the old files.
//!
//! The manifest (`meta:cold` in the MAIN tree) is the only truth: files are appended and fsynced first, read back and
//! compared, and only then one atomic sled batch removes the moved records and stores the new manifest. After a crash
//! the files are cut back to the manifest and stray segments are deleted — a batch is either fully moved or not at all.

use super::*;
use rayon::prelude::*;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

const KEY_COLD: &[u8] = b"meta:cold";
/// Blocks per compressed chunk: one random read unpacks a chunk (a few hundred KB).
pub(super) const CHUNK_BLOCKS: usize = 1024;
/// Blocks moved per batch (short `apply_lock` hold, a few MB of buffers).
const COLD_BATCH_BLOCKS: u64 = 4 * CHUNK_BLOCKS as u64;
/// Blocks converted per step of the format 1 → 2 conversion.
const CONVERT_BATCH: u64 = 32 * CHUNK_BLOCKS as u64;
/// Measured on mainnet-like headers (tests/bench_history.rs, mode `ratio`): 15 is within 0.3 % of 19 and 3.7× faster.
const ZSTD_LEVEL: i32 = 15;
/// Records moved since the last rebuild after which the main database is rebuilt at the next start (frees sled's page table).
const REBUILD_THRESHOLD: u64 = 100_000;
const FORMAT: u32 = 2;
const DAT: &str = "blocks.zst";
const IDX: &str = "blocks.zix";
const V1_DAT: &str = "blocks.dat";
const V1_IDX: &str = "blocks.idx";
pub(super) const TXS: &str = "txs.dat";
pub(super) const TX_REC: usize = 38;
const SLED_FILES: [&str; 3] = ["conf", "db", "blobs"];
const REBUILD_DIR: &str = "rebuild.tmp";
const MARK_READY: &str = "rebuild.ready";
const MARK_MOVE: &str = "rebuild.move";

pub(super) type BlockIdFn = Arc<dyn Fn(&[u8]) -> Result<[u8; 32]> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct ColdOptions {
    /// Off = blocks stay in sled (an existing cold store is still opened — the blocks live there).
    pub enabled: bool,
    /// Directory of the flat files; `None` = `<db_path>/cold`.
    pub path: Option<PathBuf>,
    /// Records moved since the last rebuild after which the main database is rebuilt at the next start.
    pub rebuild_after: u64,
}

impl Default for ColdOptions {
    fn default() -> Self {
        Self { enabled: true, path: None, rebuild_after: REBUILD_THRESHOLD }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct SegMeta {
    pub file: String,
    pub count: u64,
}

/// Transaction index part of the manifest: every block at or below `height` is indexed in the flat files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct TxManifest {
    pub height: u64,
    pub count: u64,
    pub id_segs: Vec<SegMeta>,
    pub wt_segs: Vec<SegMeta>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct Manifest {
    pub first: u64,
    /// 0 = no blocks.
    pub last: u64,
    pub dat_len: u64,
    pub segs: Vec<SegMeta>,
    pub seg_seq: u64,
    pub moved_since_rebuild: u64,
    #[serde(default)]
    pub path: String,
    /// 0 / 1 = 0.26.0 files, 2 = compressed chunks.
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub chunks: u64,
    #[serde(default)]
    pub tx: TxManifest,
}

impl Manifest {
    fn count(&self) -> u64 {
        if self.last == 0 { 0 } else { self.last - self.first + 1 }
    }
}

pub(super) struct Seg {
    pub meta: SegMeta,
    file: Mutex<File>,
}

/// Set of immutable files of sorted fixed-size records (`R` bytes each), merged in size tiers.
pub(super) struct SegSet<const R: usize> {
    dir: PathBuf,
    prefix: &'static str,
    segs: RwLock<Vec<Arc<Seg>>>,
}

/// Ordered walk over a key range of every segment of a set.
pub(super) struct SegScan<const R: usize> {
    cur: Vec<(Arc<Seg>, u64, u64, VecDeque<[u8; R]>)>,
    ascending: bool,
}

pub(super) struct Cold {
    pub(super) dir: PathBuf,
    dat: Mutex<File>,
    idx: Mutex<File>,
    state: RwLock<Manifest>,
    /// (first height, offset in blocks.zst) per chunk.
    chunks: RwLock<Vec<(u64, u64)>>,
    /// Last unpacked chunk: consecutive heights cost one unpack.
    cache: Mutex<Option<(u64, Arc<Vec<u8>>)>>,
    bids: SegSet<12>,
    /// 0.26.0 files, only while their conversion has not succeeded: (blocks.dat, blocks.idx, id segments).
    v1: Option<(Mutex<File>, Mutex<File>, SegSet<40>)>,
    block_id: BlockIdFn,
    blocks_enabled: bool,
    pub(super) txs: Mutex<File>,
    pub(super) tx_ids: SegSet<12>,
    pub(super) tx_wt: SegSet<25>,
    pub(super) tx_enabled: bool,
    pub(super) tx_damaged: bool,
    pub(super) hot_window: AtomicU64,
    pub(super) tx_keep: AtomicU64,
    pub(super) tx_running: AtomicBool,
    pub(super) tx_error: Mutex<Option<String>>,
    /// One writer at a time (block mover, transaction mover, segment merges).
    pub(super) write_lock: Mutex<()>,
    keep_blocks: AtomicU64,
    batch: AtomicU64,
    rebuild_after: u64,
    running: AtomicBool,
    last_error: Mutex<Option<String>>,
}

/// `/api/ntfry/metrics` → `node.coldBlocks`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColdStatus {
    pub enabled: bool,
    pub path: String,
    /// 2 = compressed chunks; 1 = 0.26.0 files that could not be converted yet (read-only).
    pub format: u32,
    pub first_height: u64,
    pub last_height: u64,
    pub blocks: u64,
    /// Blocks still in sled that are deep enough to move.
    pub pending_blocks: u64,
    pub hot_blocks: u64,
    pub bytes: u64,
    pub segments: usize,
    pub running: bool,
    /// The main database is rebuilt at the next start (that is when the memory is released).
    pub rebuild_pending: bool,
    pub progress_percent: f64,
    pub last_error: Option<String>,
}

pub(super) fn io(e: std::io::Error) -> Error {
    Error::Storage(sled::Error::Io(e))
}

pub(super) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn read_at(f: &Mutex<File>, offset: u64, buf: &mut [u8]) -> Result<()> {
    let mut f = lock(f);
    f.seek(SeekFrom::Start(offset)).map_err(io)?;
    f.read_exact(buf).map_err(io)
}

/// Cut the file to `at`, append `bytes` and fsync.
pub(super) fn write_at(f: &Mutex<File>, at: u64, bytes: &[u8], sync: bool) -> Result<()> {
    let mut f = lock(f);
    f.set_len(at).map_err(io)?;
    f.seek(SeekFrom::Start(at)).map_err(io)?;
    f.write_all(bytes).map_err(io)?;
    if sync {
        f.sync_data().map_err(io)?;
    }
    Ok(())
}

pub(super) fn id_bytes(id: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(id, &mut out).ok()?;
    Some(out)
}

fn bid_rec(id: &[u8; 32], height: u64) -> Result<[u8; 12]> {
    let h = u32::try_from(height).map_err(|_| Error::Sync(format!("block {height}: height does not fit the cold id index")))?;
    let mut rec = [0u8; 12];
    rec[..8].copy_from_slice(&id[..8]);
    rec[8..].copy_from_slice(&h.to_be_bytes());
    Ok(rec)
}

fn write_synced(path: &Path, bytes: impl FnOnce(&mut BufWriter<File>) -> std::io::Result<()>) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut w = BufWriter::new(File::create(&tmp).map_err(io)?);
    bytes(&mut w).map_err(io)?;
    let f = w.into_inner().map_err(|e| io(e.into_error()))?;
    f.sync_all().map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)?;
    sync_dir(path.parent().unwrap_or(path));
    Ok(())
}

/// Make a rename / file creation survive a power loss (directory entry on disk). No-op where directories cannot be opened.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// One chunk: the records framed, compressed and unpacked again for comparison before anything is written.
fn pack<T: AsRef<[u8]>>(records: &[T]) -> Result<Vec<u8>> {
    let mut raw = Vec::with_capacity(records.iter().map(|r| r.as_ref().len() + 4).sum());
    for r in records {
        raw.extend_from_slice(&(r.as_ref().len() as u32).to_le_bytes());
        raw.extend_from_slice(r.as_ref());
    }
    let z = zstd::block::compress(&raw, ZSTD_LEVEL).map_err(io)?;
    if zstd::block::decompress(&z, raw.len()).map_err(io)? != raw {
        return Err(Error::Sync("cold block store: a compressed chunk does not unpack to its source — nothing was written".into()));
    }
    let mut out = Vec::with_capacity(z.len() + 8);
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    out.extend_from_slice(&(z.len() as u32).to_le_bytes());
    out.extend_from_slice(&z);
    Ok(out)
}

impl<const R: usize> SegSet<R> {
    /// The set plus `true` when a listed segment is missing or has the wrong size.
    fn open(dir: &Path, prefix: &'static str, metas: &[SegMeta]) -> (Self, bool) {
        let (mut segs, mut damaged) = (Vec::new(), false);
        for s in metas {
            match File::open(dir.join(&s.file)) {
                Ok(f) if f.metadata().map(|m| m.len()).unwrap_or(0) == s.count * R as u64 => segs.push(Arc::new(Seg { meta: s.clone(), file: Mutex::new(f) })),
                _ => damaged = true,
            }
        }
        (Self { dir: dir.to_path_buf(), prefix, segs: RwLock::new(segs) }, damaged)
    }

    pub(super) fn list(&self) -> Vec<Arc<Seg>> {
        self.segs.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(super) fn metas(&self) -> Vec<SegMeta> {
        self.list().iter().map(|s| s.meta.clone()).collect()
    }

    pub(super) fn bytes(&self) -> u64 {
        self.list().iter().map(|s| s.meta.count * R as u64).sum()
    }

    pub(super) fn set(&self, segs: Vec<Arc<Seg>>) {
        *self.segs.write().unwrap_or_else(|e| e.into_inner()) = segs;
    }

    pub(super) fn push(&self, seg: Arc<Seg>) {
        self.segs.write().unwrap_or_else(|e| e.into_inner()).push(seg);
    }

    fn read(seg: &Seg, i: u64, n: usize) -> Result<Vec<[u8; R]>> {
        let mut bytes = vec![0u8; n * R];
        read_at(&seg.file, i * R as u64, &mut bytes)?;
        Ok(bytes.chunks_exact(R).map(|c| c.try_into().expect("chunk of R bytes")).collect())
    }

    /// Index of the first record whose leading `key.len()` bytes are not below `key`.
    fn lower_bound(seg: &Seg, key: &[u8]) -> Result<u64> {
        let (mut lo, mut hi) = (0u64, seg.meta.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if Self::read(seg, mid, 1)?[0][..key.len()] < *key { lo = mid + 1 } else { hi = mid }
        }
        Ok(lo)
    }

    pub(super) fn seg_contains(seg: &Seg, rec: &[u8; R]) -> Result<bool> {
        let i = Self::lower_bound(seg, rec)?;
        Ok(i < seg.meta.count && Self::read(seg, i, 1)?[0] == *rec)
    }

    /// Every record that starts with `key`.
    pub(super) fn find(&self, key: &[u8]) -> Result<Vec<[u8; R]>> {
        let mut out = Vec::new();
        for s in self.list().iter().rev() {
            let mut i = Self::lower_bound(s, key)?;
            while i < s.meta.count {
                let rec = Self::read(s, i, 1)?[0];
                if !rec.starts_with(key) {
                    break;
                }
                out.push(rec);
                i += 1;
            }
        }
        Ok(out)
    }

    /// Number of records in `[lo, hi)`.
    pub(super) fn count(&self, lo: &[u8], hi: &[u8]) -> Result<u64> {
        let mut n = 0;
        for s in self.list() {
            n += Self::lower_bound(&s, hi)?.saturating_sub(Self::lower_bound(&s, lo)?);
        }
        Ok(n)
    }

    /// Records in `[lo, hi)` in key order (or reversed) across all segments.
    pub(super) fn scan(&self, lo: &[u8], hi: &[u8], ascending: bool) -> Result<SegScan<R>> {
        let mut cur = Vec::new();
        for s in self.list() {
            let (a, b) = (Self::lower_bound(&s, lo)?, Self::lower_bound(&s, hi)?);
            if a < b {
                cur.push((s, a, b, VecDeque::new()));
            }
        }
        Ok(SegScan { cur, ascending })
    }

    /// Sort `records` into a new fsynced segment file. The caller records it in the manifest and `push`es it.
    pub(super) fn write(&self, seq: u64, records: &mut [[u8; R]]) -> Result<Arc<Seg>> {
        records.sort_unstable();
        let name = format!("{}{seq:08}.seg", self.prefix);
        let path = self.dir.join(&name);
        write_synced(&path, |w| records.iter().try_for_each(|r| w.write_all(r)))?;
        Ok(Arc::new(Seg { meta: SegMeta { file: name, count: records.len() as u64 }, file: Mutex::new(File::open(&path).map_err(io)?) }))
    }

    /// Size-tiered merge step: four or more segments within 4× of the smallest become one. Returns the files that are
    /// obsolete once the manifest with the new list is stored.
    fn merge_once(&self, seq: u64) -> Result<Option<Vec<String>>> {
        let mut segs = self.list();
        segs.sort_by_key(|s| s.meta.count);
        let Some(smallest) = segs.first().map(|s| s.meta.count.max(1)) else { return Ok(None) };
        let group: Vec<Arc<Seg>> = segs.iter().filter(|s| s.meta.count < smallest * 4).take(8).cloned().collect();
        if group.len() < 4 {
            return Ok(None);
        }
        let name = format!("{}{seq:08}.seg", self.prefix);
        let path = self.dir.join(&name);
        let mut inputs = Vec::new();
        for s in &group {
            let mut r = BufReader::with_capacity(1 << 16, File::open(self.dir.join(&s.meta.file)).map_err(io)?);
            let mut head = [0u8; R];
            let live = r.read_exact(&mut head).is_ok();
            inputs.push((r, head, live));
        }
        let mut count = 0u64;
        write_synced(&path, |w| {
            while let Some(i) = (0..inputs.len()).filter(|i| inputs[*i].2).min_by(|a, b| inputs[*a].1.cmp(&inputs[*b].1)) {
                w.write_all(&inputs[i].1)?;
                count += 1;
                let (r, head, live) = &mut inputs[i];
                *live = r.read_exact(head).is_ok();
            }
            Ok(())
        })?;
        if count != group.iter().map(|s| s.meta.count).sum::<u64>() {
            let _ = std::fs::remove_file(&path);
            return Err(Error::Sync("cold store: segment merge lost records — old segments kept".into()));
        }
        let merged = Arc::new(Seg { meta: SegMeta { file: name, count }, file: Mutex::new(File::open(&path).map_err(io)?) });
        let gone: Vec<String> = group.iter().map(|s| s.meta.file.clone()).collect();
        let mut live: Vec<Arc<Seg>> = self.list().into_iter().filter(|s| !gone.contains(&s.meta.file)).collect();
        live.push(merged);
        self.set(live);
        Ok(Some(gone))
    }

    /// Delete segment files of this set that the manifest does not list (left by a crash).
    fn sweep(&self) {
        let live = self.metas();
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(self.prefix) && !live.iter().any(|s| s.file == name) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

impl<const R: usize> Iterator for SegScan<R> {
    type Item = Result<[u8; R]>;
    fn next(&mut self) -> Option<Self::Item> {
        let asc = self.ascending;
        for (seg, lo, hi, buf) in self.cur.iter_mut() {
            if buf.is_empty() && lo < hi {
                let n = (*hi - *lo).min(256);
                let start = if asc { *lo } else { *hi - n };
                match SegSet::<R>::read(seg, start, n as usize) {
                    Ok(v) => buf.extend(v),
                    Err(e) => {
                        *lo = *hi;
                        return Some(Err(e));
                    }
                }
                if asc { *lo += n } else { *hi -= n }
            }
        }
        let head = |c: &(Arc<Seg>, u64, u64, VecDeque<[u8; R]>)| if asc { c.3.front().copied() } else { c.3.back().copied() };
        let live = (0..self.cur.len()).filter(|i| !self.cur[*i].3.is_empty());
        let pick = if asc { live.min_by_key(|i| head(&self.cur[*i])) } else { live.max_by_key(|i| head(&self.cur[*i])) }?;
        let buf = &mut self.cur[pick].3;
        (if asc { buf.pop_front() } else { buf.pop_back() }).map(Ok)
    }
}

impl Cold {
    pub(super) fn open(main: &sled::Tree, main_path: &Path, opts: &ColdOptions, tx: &HistoryOptions, block_id: BlockIdFn) -> Result<Option<Self>> {
        let mut m: Manifest = match main.get(KEY_COLD)? {
            Some(v) => serde_json::from_slice(&v)?,
            None => Manifest::default(),
        };
        if !opts.enabled && !tx.enabled && m.last == 0 && m.tx.height == 0 {
            return Ok(None);
        }
        let dir = match (&opts.path, m.path.is_empty()) {
            (Some(p), _) => p.clone(),
            (None, false) => PathBuf::from(&m.path),
            (None, true) => main_path.join("cold"),
        };
        if let Some(p) = &opts.path {
            m.path = p.to_string_lossy().into_owned();
        }
        if m.last == 0 {
            m.version = FORMAT;
        }
        std::fs::create_dir_all(&dir).map_err(io)?;
        let rw = |name: &str| OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(name)).map_err(io);
        let missing = |what: &str| Error::Config(format!("cold block store {} is damaged: {what} — {} blocks were moved there; restore the directory or fix cold_blocks.path", dir.display(), m.count()));
        let (dat, idx) = (rw(DAT)?, rw(IDX)?);
        let (mut chunks, mut v1, mut bids_damaged) = (Vec::new(), None, false);
        let bids;
        if m.version >= FORMAT {
            if dat.metadata().map_err(io)?.len() < m.dat_len || idx.metadata().map_err(io)?.len() < m.chunks * 16 {
                return Err(missing("blocks.zst / blocks.zix are shorter than recorded"));
            }
            // a batch that was appended but never committed
            dat.set_len(m.dat_len).map_err(io)?;
            idx.set_len(m.chunks * 16).map_err(io)?;
            let mut raw = vec![0u8; (m.chunks * 16) as usize];
            (&idx).seek(SeekFrom::Start(0)).map_err(io)?;
            (&idx).read_exact(&mut raw).map_err(io)?;
            chunks = raw.chunks_exact(16).map(|c| (u64::from_le_bytes(c[..8].try_into().expect("8 bytes")), u64::from_le_bytes(c[8..].try_into().expect("8 bytes")))).collect();
            (bids, bids_damaged) = SegSet::open(&dir, "bid2-", &m.segs);
            // 0.26.0 files left by a conversion that was recorded but not cleaned up
            for e in std::fs::read_dir(&dir).map_err(io)?.flatten().filter(|_| m.last != 0) {
                let name = e.file_name().to_string_lossy().into_owned();
                if name == V1_DAT || name == V1_IDX || name.starts_with("bid-") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        } else {
            let (old_dat, old_idx) = (rw(V1_DAT)?, rw(V1_IDX)?);
            if old_dat.metadata().map_err(io)?.len() < m.dat_len || old_idx.metadata().map_err(io)?.len() < m.count() * 8 {
                return Err(missing("blocks.dat / blocks.idx are shorter than recorded"));
            }
            old_dat.set_len(m.dat_len).map_err(io)?;
            old_idx.set_len(m.count() * 8).map_err(io)?;
            let (old_segs, damaged) = SegSet::<40>::open(&dir, "bid-", &m.segs);
            if damaged {
                tracing::warn!("cold block store: a 0.26.0 id segment is missing — the id index is rebuilt during the conversion");
            }
            old_segs.sweep();
            v1 = Some((Mutex::new(old_dat), Mutex::new(old_idx), old_segs));
            bids = SegSet::open(&dir, "bid2-", &[]).0;
        }
        sync_dir(&dir);
        let txs = rw(TXS)?;
        let mut tx_damaged = txs.metadata().map_err(io)?.len() < m.tx.count * TX_REC as u64;
        if !tx_damaged {
            txs.set_len(m.tx.count * TX_REC as u64).map_err(io)?;
        }
        let (tx_ids, d1) = SegSet::open(&dir, "txi-", &m.tx.id_segs);
        let (tx_wt, d2) = SegSet::open(&dir, "txw-", &m.tx.wt_segs);
        tx_damaged |= d1 || d2;
        let mut cold = Self {
            dir,
            dat: Mutex::new(dat),
            idx: Mutex::new(idx),
            state: RwLock::new(m.clone()),
            chunks: RwLock::new(chunks),
            cache: Default::default(),
            bids,
            v1,
            block_id,
            blocks_enabled: opts.enabled,
            txs: Mutex::new(txs),
            tx_ids,
            tx_wt,
            tx_enabled: tx.enabled,
            tx_damaged,
            hot_window: AtomicU64::new(tx.hot_window),
            tx_keep: AtomicU64::new(UNDO_DEPTH + 100),
            tx_running: AtomicBool::new(false),
            tx_error: Default::default(),
            write_lock: Default::default(),
            keep_blocks: AtomicU64::new(UNDO_DEPTH + 100),
            batch: AtomicU64::new(COLD_BATCH_BLOCKS),
            rebuild_after: opts.rebuild_after,
            running: AtomicBool::new(false),
            last_error: Default::default(),
        };
        if cold.v1.is_some() {
            if let Err(e) = cold.convert(main) {
                tracing::warn!(error = %e, "cold block store: conversion to the compressed format failed — the 0.26.0 files stay in use (read-only), it is retried at the next start");
                *lock(&cold.last_error) = Some(format!("conversion to the compressed format failed: {e}"));
                let _ = lock(&cold.dat).set_len(0);
                let _ = lock(&cold.idx).set_len(0);
                cold.chunks.write().unwrap_or_else(|e| e.into_inner()).clear();
                *lock(&cold.cache) = None;
            }
        } else if bids_damaged {
            // the id index is derived data: rebuild it from the blocks
            tracing::warn!(blocks = m.count(), "cold block store: id segment missing or damaged — rebuilding the id index from the blocks");
            cold.rebuild_segments(main)?;
        }
        cold.bids.sweep();
        if !cold.tx_damaged {
            cold.tx_ids.sweep();
            cold.tx_wt.sweep();
        }
        Ok(Some(cold))
    }

    /// 0.26.0 → 0.27.0: pack `blocks.dat` into compressed chunks and write the short id index. The old files are
    /// untouched until the new ones are complete, fsynced and recorded in the manifest.
    fn convert(&mut self, main: &sled::Tree) -> Result<()> {
        let started = std::time::Instant::now();
        let mut m = self.manifest();
        let (total, old_bytes) = (m.count(), m.dat_len + m.count() * 8 + m.segs.iter().map(|s| s.count * 40).sum::<u64>());
        tracing::info!(blocks = total, "cold block store: one-time conversion to the compressed format (the 0.26.0 files are kept until it is complete)");
        lock(&self.dat).set_len(0).map_err(io)?;
        lock(&self.idx).set_len(0).map_err(io)?;
        let mut reader = BufReader::with_capacity(1 << 20, File::open(self.dir.join(V1_DAT)).map_err(io)?);
        let (mut h, mut dat_len, mut chunk_no, mut seq) = (m.first, 0u64, 0u64, m.seg_seq);
        let (mut ids, mut segs, mut logged) = (Vec::new(), Vec::new(), std::time::Instant::now());
        while total > 0 && h <= m.last {
            let n = (m.last - h + 1).min(CONVERT_BATCH);
            let mut recs = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let mut len = [0u8; 4];
                reader.read_exact(&mut len).map_err(io)?;
                let mut bytes = vec![0u8; u32::from_le_bytes(len) as usize];
                reader.read_exact(&mut bytes).map_err(io)?;
                recs.push(bytes);
            }
            let (len, added) = self.write_chunks(dat_len, chunk_no, h, &recs, false)?;
            let batch_ids: Vec<[u8; 32]> = recs.par_iter().map(|r| (self.block_id)(r)).collect::<Result<_>>()?;
            for (i, id) in batch_ids.iter().enumerate() {
                ids.push(bid_rec(id, h + i as u64)?);
            }
            // sample read-back from the new files against the old ones
            for i in [0, n / 2, (n - 1) / CHUNK_BLOCKS as u64 * CHUNK_BLOCKS as u64, n - 1] {
                if self.read_v2(h + i)?.as_deref() != Some(recs[i as usize].as_slice()) || self.read_v1(&m, h + i)? != recs[i as usize] {
                    return Err(Error::Sync(format!("block {} differs after conversion", h + i)));
                }
            }
            (dat_len, chunk_no, h) = (len, chunk_no + added, h + n);
            if ids.len() >= 400_000 || h > m.last {
                segs.push(self.bids.write(seq, &mut ids)?);
                seq += 1;
                ids.clear();
            }
            if logged.elapsed().as_secs() >= 10 {
                logged = std::time::Instant::now();
                tracing::info!("cold block store: converting {:.1} % · {} of {total} blocks", (h - m.first) as f64 * 100.0 / total as f64, h - m.first);
            }
        }
        drop(reader);
        lock(&self.dat).sync_all().map_err(io)?;
        lock(&self.idx).sync_all().map_err(io)?;
        (m.version, m.dat_len, m.chunks, m.seg_seq) = (FORMAT, dat_len, chunk_no, seq);
        m.segs = segs.iter().map(|s| s.meta.clone()).collect();
        main.insert(KEY_COLD, serde_json::to_vec(&m)?)?;
        main.flush()?;
        self.bids.set(segs);
        let new_bytes = dat_len + chunk_no * 16 + self.bids.bytes();
        *self.state.write().unwrap_or_else(|e| e.into_inner()) = m;
        // close the old files before deleting them
        let old = self.v1.take();
        let old_segs = old.as_ref().map(|v| v.2.metas()).unwrap_or_default();
        drop(old);
        for s in old_segs {
            let _ = std::fs::remove_file(self.dir.join(s.file));
        }
        let _ = std::fs::remove_file(self.dir.join(V1_DAT));
        let _ = std::fs::remove_file(self.dir.join(V1_IDX));
        while self.merge(main)? {}
        tracing::info!(blocks = total, elapsed = ?started.elapsed(), "cold block store: converted · {} MB → {} MB", old_bytes >> 20, new_bytes >> 20);
        Ok(())
    }

    /// Recreate every `bid2-*.seg` from the blocks themselves (bounded memory).
    fn rebuild_segments(&mut self, main: &sled::Tree) -> Result<()> {
        let mut m = self.manifest();
        let (mut fresh, mut ids, mut h) = (Vec::new(), Vec::new(), m.first);
        while m.last != 0 && h <= m.last {
            let bytes = self.read_v2(h)?.ok_or_else(|| Error::NotFound(format!("cold block {h}")))?;
            ids.push(bid_rec(&(self.block_id)(&bytes)?, h)?);
            h += 1;
            if ids.len() >= 400_000 || h > m.last {
                fresh.push(self.bids.write(m.seg_seq, &mut ids)?);
                m.seg_seq += 1;
                ids.clear();
            }
        }
        m.segs = fresh.iter().map(|s| s.meta.clone()).collect();
        main.insert(KEY_COLD, serde_json::to_vec(&m)?)?;
        main.flush()?;
        self.bids.set(fresh);
        *self.state.write().unwrap_or_else(|e| e.into_inner()) = m;
        Ok(())
    }

    pub(super) fn manifest(&self) -> Manifest {
        self.state.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Manifest for the next commit: the current one with the live segment lists.
    pub(super) fn next_manifest(&self) -> Manifest {
        let mut m = self.manifest();
        (m.tx.id_segs, m.tx.wt_segs) = (self.tx_ids.metas(), self.tx_wt.metas());
        // while the 0.26.0 files are in use the manifest keeps listing their id segments
        if self.v1.is_none() {
            m.segs = self.bids.metas();
        }
        m
    }

    pub(super) fn install(&self, m: Manifest) {
        *self.state.write().unwrap_or_else(|e| e.into_inner()) = m;
    }

    /// Store `m` in the main tree (flushed) and make it current.
    pub(super) fn commit(&self, main: &sled::Tree, m: Manifest) -> Result<()> {
        main.insert(KEY_COLD, serde_json::to_vec(&m)?)?;
        main.flush()?;
        self.install(m);
        Ok(())
    }

    pub(super) fn manifest_key() -> &'static [u8] {
        KEY_COLD
    }

    pub(super) fn range(&self) -> (u64, u64) {
        let m = self.state.read().unwrap_or_else(|e| e.into_inner());
        (m.first, m.last)
    }

    pub(super) fn tx_cursor(&self) -> (u64, u64) {
        let m = self.state.read().unwrap_or_else(|e| e.into_inner());
        (m.tx.height, m.tx.count)
    }

    fn read_v1(&self, m: &Manifest, height: u64) -> Result<Vec<u8>> {
        let (dat, idx, _) = self.v1.as_ref().ok_or_else(|| Error::NotFound("0.26.0 cold files".into()))?;
        let mut off = [0u8; 8];
        read_at(idx, (height - m.first) * 8, &mut off)?;
        let off = u64::from_le_bytes(off);
        let mut len = [0u8; 4];
        read_at(dat, off, &mut len)?;
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        read_at(dat, off + 4, &mut buf)?;
        Ok(buf)
    }

    fn chunk(&self, offset: u64) -> Result<Arc<Vec<u8>>> {
        if let Some((at, raw)) = &*lock(&self.cache) {
            if *at == offset {
                return Ok(raw.clone());
            }
        }
        let mut head = [0u8; 8];
        read_at(&self.dat, offset, &mut head)?;
        let (raw_len, z_len) = (u32::from_le_bytes(head[..4].try_into().expect("4 bytes")) as usize, u32::from_le_bytes(head[4..].try_into().expect("4 bytes")) as usize);
        let mut z = vec![0u8; z_len];
        read_at(&self.dat, offset + 8, &mut z)?;
        let raw = Arc::new(zstd::block::decompress(&z, raw_len).map_err(io)?);
        *lock(&self.cache) = Some((offset, raw.clone()));
        Ok(raw)
    }

    /// Record of `height` from the chunk files, whatever the manifest says (read-back of a batch not yet committed).
    fn read_v2(&self, height: u64) -> Result<Option<Vec<u8>>> {
        let (first, offset) = {
            let chunks = self.chunks.read().unwrap_or_else(|e| e.into_inner());
            match chunks.partition_point(|c| c.0 <= height).checked_sub(1) {
                Some(i) => chunks[i],
                None => return Ok(None),
            }
        };
        let raw = self.chunk(offset)?;
        let mut pos = 0usize;
        for _ in first..height {
            if next_frame(&raw, &mut pos)?.is_none() {
                return Ok(None);
            }
        }
        Ok(next_frame(&raw, &mut pos)?.map(|r| r.to_vec()))
    }

    /// Stored record of the block at `height` (the bytes sled held under `b:<height>`).
    pub(super) fn read(&self, height: u64) -> Result<Option<Vec<u8>>> {
        let legacy = {
            let m = self.state.read().unwrap_or_else(|e| e.into_inner());
            if m.last == 0 || height < m.first || height > m.last {
                return Ok(None);
            }
            (m.version < FORMAT).then(|| m.clone())
        };
        match legacy {
            Some(m) => self.read_v1(&m, height).map(Some),
            None => self.read_v2(height)?.map(Some).ok_or_else(|| Error::NotFound(format!("cold block {height}: chunk is damaged"))),
        }
    }

    pub(super) fn height_of(&self, id: &str) -> Result<Option<u64>> {
        let Some(id) = id_bytes(id) else { return Ok(None) };
        if let Some((_, _, old)) = &self.v1 {
            return Ok(old.find(&id)?.first().map(|r| be_u64(&r[32..])));
        }
        for rec in self.bids.find(&id[..8])? {
            let h = u64::from(u32::from_be_bytes(rec[8..].try_into().expect("4 bytes")));
            // the index holds 8 bytes of the id: the block itself confirms the match
            if let Some(bytes) = self.read(h)? {
                if (self.block_id)(&bytes)? == id {
                    return Ok(Some(h));
                }
            }
        }
        Ok(None)
    }

    /// Pack `records` (consecutive heights from `first`) into chunks at `dat_at` / chunk `chunk_no`.
    /// Returns (new data length, chunks written).
    fn write_chunks(&self, dat_at: u64, chunk_no: u64, first: u64, records: &[Vec<u8>], sync: bool) -> Result<(u64, u64)> {
        let frames: Vec<Vec<u8>> = records.par_chunks(CHUNK_BLOCKS).map(pack).collect::<Result<_>>()?;
        let (mut data, mut index, mut entries) = (Vec::new(), Vec::new(), Vec::new());
        for (i, f) in frames.iter().enumerate() {
            let entry = (first + (i * CHUNK_BLOCKS) as u64, dat_at + data.len() as u64);
            index.extend_from_slice(&entry.0.to_le_bytes());
            index.extend_from_slice(&entry.1.to_le_bytes());
            entries.push(entry);
            data.extend_from_slice(f);
        }
        *lock(&self.cache) = None;
        write_at(&self.dat, dat_at, &data, sync)?;
        write_at(&self.idx, chunk_no * 16, &index, sync)?;
        let mut chunks = self.chunks.write().unwrap_or_else(|e| e.into_inner());
        chunks.truncate(chunk_no as usize);
        chunks.extend(entries);
        Ok((dat_at + data.len() as u64, frames.len() as u64))
    }

    /// Append `blocks` (consecutive heights from `first`), fsync and read everything back. Nothing is visible to
    /// readers until the returned manifest is installed and the segment pushed.
    fn append(&self, first: u64, blocks: &[Vec<u8>], ids: &[[u8; 32]]) -> Result<(Manifest, Arc<Seg>)> {
        let mut m = self.next_manifest();
        if m.last != 0 && first != m.last + 1 {
            return Err(Error::Sync(format!("cold block store ends at {}, cannot append {first}", m.last)));
        }
        let (dat_len, added) = self.write_chunks(m.dat_len, m.chunks, first, blocks, true)?;
        let mut recs = Vec::with_capacity(ids.len());
        for (i, id) in ids.iter().enumerate() {
            recs.push(bid_rec(id, first + i as u64)?);
        }
        let seg = self.bids.write(m.seg_seq, &mut recs)?;
        for (i, bytes) in blocks.iter().enumerate() {
            let h = first + i as u64;
            if self.read_v2(h)?.as_deref() != Some(bytes.as_slice()) || !SegSet::seg_contains(&seg, &bid_rec(&ids[i], h)?)? {
                return Err(Error::Sync(format!("cold block store verification failed at block {h} — nothing was removed from the main database")));
            }
        }
        if m.last == 0 {
            m.first = first;
        }
        m.last = first + blocks.len() as u64 - 1;
        (m.dat_len, m.chunks, m.version) = (dat_len, m.chunks + added, FORMAT);
        m.segs.push(seg.meta.clone());
        m.seg_seq += 1;
        m.moved_since_rebuild += blocks.len() as u64;
        Ok((m, seg))
    }

    /// One merge step over the segment sets; `true` when something was merged.
    pub(super) fn merge(&self, main: &sled::Tree) -> Result<bool> {
        let seq = self.manifest().seg_seq;
        let gone = match self.bids.merge_once(seq)? {
            Some(g) => Some(g),
            None => match self.tx_ids.merge_once(seq)? {
                Some(g) => Some(g),
                None => self.tx_wt.merge_once(seq)?,
            },
        };
        let Some(gone) = gone else { return Ok(false) };
        let mut m = self.next_manifest();
        m.seg_seq = seq + 1;
        self.commit(main, m)?;
        for f in gone {
            let _ = std::fs::remove_file(self.dir.join(f));
        }
        Ok(true)
    }
}

/// Finish an interrupted swap of a rebuilt main database (called before sled opens `path`). Two markers make every step
/// repeatable: `rebuild.ready` → delete the old sled files; `rebuild.move` → move the new ones in.
pub(super) fn finish_rebuild(path: &Path) -> Result<()> {
    let tmp = path.join(REBUILD_DIR);
    let is_sled = |name: &str| SLED_FILES.contains(&name) || name.starts_with("snap.");
    if path.join(MARK_READY).exists() && !tmp.join("db").is_file() {
        // the marker without a complete copy next to it must never cost the old database
        tracing::warn!("main database: rebuild marker found without the rebuilt copy — keeping the existing database");
        std::fs::remove_file(path.join(MARK_READY)).map_err(io)?;
    }
    if path.join(MARK_READY).exists() {
        for e in std::fs::read_dir(path).map_err(io)?.flatten() {
            if is_sled(&e.file_name().to_string_lossy()) {
                let p = e.path();
                if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) }.map_err(io)?;
            }
        }
        std::fs::rename(path.join(MARK_READY), path.join(MARK_MOVE)).map_err(io)?;
    }
    if path.join(MARK_MOVE).exists() {
        if tmp.is_dir() {
            for e in std::fs::read_dir(&tmp).map_err(io)?.flatten() {
                std::fs::rename(e.path(), path.join(e.file_name())).map_err(io)?;
            }
            std::fs::remove_dir_all(&tmp).map_err(io)?;
        }
        std::fs::remove_file(path.join(MARK_MOVE)).map_err(io)?;
        tracing::info!("main database: rebuilt copy is in place");
    }
    // an unfinished `rebuild.tmp` is left alone here: this runs before the sled lock is taken, and another process may
    // be writing it right now; `rebuild` clears it under the lock
    Ok(())
}

/// True when enough records left sled since the last rebuild for one to pay off.
pub(super) fn rebuild_due(tree: &sled::Tree, threshold: u64) -> bool {
    tree.get(KEY_COLD).ok().flatten().and_then(|v| serde_json::from_slice::<Manifest>(&v).ok()).is_some_and(|m| m.moved_since_rebuild >= threshold.max(1))
}

/// Copy every live record of the main tree into a fresh database next to it and mark it ready; the caller drops its
/// handles and calls `finish_rebuild`. The old database is not touched before the copy is complete and fsynced.
pub(super) fn rebuild(tree: &sled::Tree, path: &Path, cache_mb: u32) -> Result<()> {
    let started = std::time::Instant::now();
    let tmp = path.join(REBUILD_DIR);
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp).map_err(io)?;
    }
    tracing::info!("main database: one-time rebuild after moving old records to the cold store (releases memory; the old database stays untouched until the copy is complete)");
    let dst = sled::Config::new().path(&tmp).use_compression(true).compression_factor(3).cache_capacity(u64::from(cache_mb.max(8)) * 1024 * 1024).open()?;
    let out = dst.open_tree("chain")?;
    let (mut batch, mut n) = (sled::Batch::default(), 0u64);
    for item in tree.iter() {
        let (k, v) = item?;
        if k.as_ref() == KEY_COLD {
            let mut m: Manifest = serde_json::from_slice(&v)?;
            m.moved_since_rebuild = 0;
            batch.insert(k, serde_json::to_vec(&m)?);
        } else {
            batch.insert(k, v);
        }
        n += 1;
        if n % 20_000 == 0 {
            out.apply_batch(std::mem::take(&mut batch))?;
        }
    }
    out.apply_batch(batch)?;
    if out.len() as u64 != n {
        return Err(Error::Sync(format!("main database rebuild: copied {} of {n} records — old database kept", out.len())));
    }
    out.flush()?;
    dst.flush()?;
    drop(out);
    drop(dst);
    File::create(path.join(MARK_READY)).and_then(|f| f.sync_all()).map_err(io)?;
    tracing::info!(records = n, elapsed = ?started.elapsed(), "main database: rebuild complete");
    Ok(())
}

impl Storage {
    /// Raw `b:` record from sled or the cold store.
    pub(super) fn block_bytes(&self, height: u64) -> Result<Option<Vec<u8>>> {
        if let Some(c) = &self.cold {
            if let Some(v) = c.read(height)? {
                return Ok(Some(v));
            }
        }
        Ok(self.tree.get(height_key(height))?.map(|v| v.to_vec()))
    }

    /// (first, last) heights held by the cold store; (0, 0) when empty / off.
    pub(super) fn cold_range(&self) -> (u64, u64) {
        self.cold.as_ref().map(|c| c.range()).unwrap_or((0, 0))
    }

    pub(super) fn cold_height_of(&self, id: &str) -> Result<Option<u64>> {
        match &self.cold {
            Some(c) => c.height_of(id),
            None => Ok(None),
        }
    }

    fn sled_first_height(&self) -> Result<u64> {
        match self.tree.scan_prefix(PREFIX_BLOCK).keys().next() {
            Some(k) => Ok(be_u64(k?.get(PREFIX_BLOCK.len()..).unwrap_or(&[]))),
            None => Ok(0),
        }
    }

    /// Records under `prefix` in the main sled tree (full key scan — diagnostics and tests).
    pub fn sled_key_count(&self, prefix: &str) -> usize {
        self.tree.scan_prefix(prefix.as_bytes()).keys().count()
    }

    /// Test / tuning hook: number of newest blocks that always stay in sled and the batch size.
    pub fn set_cold_tuning(&self, keep: u64, batch: u64) {
        if let Some(c) = &self.cold {
            c.batch.store(batch.max(1), Ordering::Relaxed);
            c.keep_blocks.store(keep, Ordering::Relaxed);
        }
    }

    /// (next height to move, deepest height allowed to move); (0, 0) = the mover is off.
    fn cold_window(&self, c: &Cold) -> Result<(u64, u64)> {
        if c.v1.is_some() || (!c.blocks_enabled && c.range().1 == 0) {
            return Ok((0, 0));
        }
        let (_, last) = c.range();
        let next = if last == 0 { self.sled_first_height()? } else { last + 1 };
        Ok((next, self.get_last_height()?.saturating_sub(c.keep_blocks.load(Ordering::Relaxed))))
    }

    /// One batch: up to `max` consecutive blocks from sled into the flat files. Returns the blocks moved.
    fn cold_step(&self, max: u64) -> Result<u64> {
        let Some(c) = &self.cold else { return Ok(0) };
        if !c.blocks_enabled {
            return Ok(0);
        }
        let _writer = lock(&c.write_lock);
        let (next, limit) = self.cold_window(c)?;
        if next == 0 || next > limit {
            return Ok(0);
        }
        let (mut blocks, mut ids, mut keys) = (Vec::new(), Vec::new(), Vec::new());
        for item in self.tree.range(height_key(next)..=height_key(limit.min(next + max - 1))) {
            let (k, v) = item?;
            let height = next + blocks.len() as u64;
            if be_u64(&k[PREFIX_BLOCK.len()..]) != height {
                break; // gap (sparse test database): only a contiguous chain is moved
            }
            let id = decode_block(&v, &self.network)?.id.ok_or_else(|| Error::Sync(format!("block {height} has no id")))?;
            let raw = id_bytes(&id).ok_or_else(|| Error::Sync(format!("block {height}: id {id} is not 32 bytes of hex")))?;
            let bid = prefixed(PREFIX_BLOCK_ID, &id);
            // the id index entry that leaves sled must be exactly the one the cold store will answer for
            if self.tree.get(&bid)?.map(|v| be_u64(&v)) != Some(height) {
                return Err(Error::Sync(format!("block {height}: id index does not match the stored block ({id}) — not moved")));
            }
            keys.push((k.to_vec(), bid));
            blocks.push(v.to_vec());
            ids.push(raw);
        }
        if blocks.is_empty() {
            return Ok(0);
        }
        // fsynced and read back from disk before anything leaves sled
        let (m, seg) = c.append(next, &blocks, &ids)?;
        c.bids.push(seg);
        c.install(m.clone());
        let mut batch = sled::Batch::default();
        for (b, bid) in &keys {
            batch.remove(b.as_slice());
            batch.remove(bid.as_slice());
        }
        batch.insert(Cold::manifest_key(), serde_json::to_vec(&m)?);
        let _guard = self.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.tree.apply_batch(batch)?;
        Ok(blocks.len() as u64)
    }

    /// True when at least 1 000 blocks are deep enough to leave sled.
    pub fn cold_due(&self) -> bool {
        let Some(c) = &self.cold else { return false };
        c.blocks_enabled && matches!(self.cold_window(c), Ok((next, limit)) if next != 0 && limit >= next + 999)
    }

    /// Move every block deeper than the keep margin to the cold store. Safe on a running node; an interrupted run
    /// continues from the manifest. `progress` is called every 20 batches.
    pub fn archive_blocks(&self, cancel: &AtomicBool, mut progress: impl FnMut(&ColdStatus)) -> Result<u64> {
        let Some(c) = &self.cold else { return Ok(0) };
        if c.v1.is_some() || c.running.swap(true, Ordering::AcqRel) {
            return Ok(0);
        }
        let mut run = || -> Result<u64> {
            let (mut moved, mut batches) = (0u64, 0u64);
            while !cancel.load(Ordering::Relaxed) {
                let n = self.cold_step(c.batch.load(Ordering::Relaxed))?;
                if n == 0 {
                    break;
                }
                moved += n;
                batches += 1;
                self.cold_merge(c)?;
                if batches % 20 == 0 {
                    self.tree.flush()?;
                    progress(&self.cold_status()?);
                }
            }
            if moved > 0 {
                self.tree.flush()?;
            }
            Ok(moved)
        };
        let res = run();
        c.running.store(false, Ordering::Release);
        *lock(&c.last_error) = res.as_ref().err().map(|e| e.to_string());
        res
    }

    /// Test hook: rewrite the block part of the cold store in the 0.26.0 layout (`blocks.dat`, `blocks.idx`, one 40-byte
    /// `bid-*.seg`) so the conversion can be exercised. The handle must be dropped right after.
    #[doc(hidden)]
    pub fn cold_export_v1(&self) -> Result<()> {
        let Some(c) = &self.cold else { return Ok(()) };
        let mut m = c.next_manifest();
        let (mut data, mut index, mut ids) = (Vec::new(), Vec::new(), Vec::new());
        for h in m.first..=m.last {
            let bytes = c.read(h)?.ok_or_else(|| Error::NotFound(format!("cold block {h}")))?;
            let mut rec = [0u8; 40];
            rec[..32].copy_from_slice(&(c.block_id)(&bytes)?);
            rec[32..].copy_from_slice(&h.to_be_bytes());
            ids.push(rec);
            index.extend_from_slice(&(data.len() as u64).to_le_bytes());
            data.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            data.extend_from_slice(&bytes);
        }
        ids.sort_unstable();
        std::fs::write(c.dir.join(V1_DAT), &data).map_err(io)?;
        std::fs::write(c.dir.join(V1_IDX), &index).map_err(io)?;
        std::fs::write(c.dir.join("bid-00000000.seg"), ids.concat()).map_err(io)?;
        (m.version, m.chunks, m.dat_len, m.seg_seq) = (0, 0, data.len() as u64, m.seg_seq.max(1));
        m.segs = vec![SegMeta { file: "bid-00000000.seg".into(), count: ids.len() as u64 }];
        self.tree.insert(KEY_COLD, serde_json::to_vec(&m)?)?;
        self.tree.flush()?;
        Ok(())
    }

    pub(super) fn cold_merge(&self, c: &Cold) -> Result<()> {
        let _writer = lock(&c.write_lock);
        while c.merge(&self.tree)? {}
        Ok(())
    }

    pub fn cold_status(&self) -> Result<ColdStatus> {
        let tip = self.get_last_height()?;
        let Some(c) = &self.cold else { return Ok(ColdStatus { hot_blocks: tip, ..Default::default() }) };
        let m = c.manifest();
        if !c.blocks_enabled && m.last == 0 {
            return Ok(ColdStatus { hot_blocks: tip, ..Default::default() });
        }
        let (next, limit) = self.cold_window(c)?;
        let pending = if c.blocks_enabled && next != 0 && limit >= next { limit - next + 1 } else { 0 };
        let pending = if pending >= 1_000 || c.running.load(Ordering::Relaxed) { pending } else { 0 };
        let done = m.count() as f64;
        Ok(ColdStatus {
            enabled: true,
            path: c.dir.display().to_string(),
            format: m.version.max(1),
            first_height: m.first,
            last_height: m.last,
            blocks: m.count(),
            pending_blocks: pending,
            hot_blocks: tip.saturating_sub(m.last),
            bytes: match &c.v1 {
                Some((_, _, old)) => m.dat_len + m.count() * 8 + old.bytes(),
                None => m.dat_len + m.chunks * 16 + c.bids.bytes(),
            },
            segments: m.segs.len(),
            running: c.running.load(Ordering::Relaxed),
            rebuild_pending: m.moved_since_rebuild >= c.rebuild_after.max(1),
            progress_percent: if pending == 0 { 100.0 } else { (done / (done + pending as f64) * 1000.0).floor() / 10.0 },
            last_error: lock(&c.last_error).clone(),
        })
    }
}
