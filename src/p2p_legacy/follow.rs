//! Author: TechnoL0g
//!
//! Block intake over the legacy P2P port: parallel catch-up (400-block ranges pulled from the
//! N best peers concurrently, applied strictly in order) followed by a live follow loop.

use super::{PeerTable, LegacyPeer, MAX_BLOCKS_PER_REQUEST};
use crate::error::{Error, Result};
use crate::models::Block;
use crate::storage::Storage;
use crate::delegate::forger::group;
use crate::sync::{apply_blocks, progress_style, ChainTip};
use indicatif::ProgressBar;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Per-peer request spacing enforced by legacy nodes (`p2p.blocks.getBlocks` rate limit).
const MIN_ROUND: Duration = Duration::from_millis(1_100);
/// Health probes are short: an unreachable peer must not stall the table refresh.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
const PROBE_CONCURRENCY: usize = 16;

#[derive(Clone)]
pub struct P2pOptions {
    /// Full cryptographic verification of every block.
    pub verify: bool,
    /// Peers pulled from concurrently during catch-up.
    pub parallel: usize,
    /// Hide the progress bar.
    pub quiet: bool,
    /// How often the peer table is re-probed.
    pub refresh_interval: Duration,
    pub timeout: Duration,
    /// iroh layer: its peers join the download scheduler next to the legacy IPs.
    pub iroh: Option<Arc<crate::p2p_iroh::IrohNode>>,
}

impl Default for P2pOptions {
    fn default() -> Self {
        Self { verify: true, parallel: 4, quiet: false, refresh_interval: Duration::from_secs(60), timeout: Duration::from_secs(20), iroh: None }
    }
}

impl std::fmt::Debug for P2pOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("P2pOptions").field("verify", &self.verify).field("parallel", &self.parallel).field("iroh", &self.iroh.is_some()).finish()
    }
}

/// Where a block range is pulled from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Source {
    Legacy(String),
    Iroh(iroh::EndpointId),
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Legacy(ip) => write!(f, "{ip}:4001"),
            Source::Iroh(id) => write!(f, "iroh:{}", id.fmt_short()),
        }
    }
}

/// Fetch one range over iroh RPC.
async fn fetch_range_iroh(node: &crate::p2p_iroh::IrohNode, id: iroh::EndpointId, from: u64, limit: u32) -> Option<Vec<Block>> {
    match tokio::time::timeout(Duration::from_secs(30), crate::p2p_iroh::fetch_blocks(&node.endpoint, &node.peers, id, from, limit)).await {
        Ok(Ok(blocks)) => Some(blocks),
        Ok(Err(e)) => {
            tracing::debug!(peer = %id.fmt_short(), from, error = %e, "iroh GetBlocks failed");
            None
        }
        Err(_) => {
            node.peers.record_rpc(id, None, None);
            None
        }
    }
}

fn tip_of(storage: &Storage) -> Result<ChainTip> {
    Ok(match storage.get_last_block()? {
        Some(b) => ChainTip { height: b.height, id: b.id },
        None => ChainTip { height: 0, id: None },
    })
}

/// Fetch one range from a peer (fresh connection: legacy peers reset the socket after a large reply).
async fn fetch_range(table: &PeerTable, ip: &str, from: u64, limit: u32, timeout: Duration) -> Option<Vec<Block>> {
    let started = Instant::now();
    let result = async {
        let mut peer = LegacyPeer::connect(ip, table.port(), timeout).await?;
        peer.get_blocks(from, limit).await
    }
    .await;
    match result {
        Ok(blocks) => {
            let reached = blocks.last().map(|b| b.height);
            table.record_success(ip, started.elapsed(), reached);
            Some(blocks)
        }
        Err(e) => {
            tracing::debug!(peer = ip, from, error = %e, "getBlocks failed");
            table.record_failure(ip);
            None
        }
    }
}

async fn apply(storage: &Arc<Storage>, blocks: Vec<Block>, tip: ChainTip, verify: bool) -> Result<ChainTip> {
    let st = storage.clone();
    let net = storage.network().clone();
    tokio::task::spawn_blocking(move || apply_blocks(&st, &net, &blocks, tip, verify))
        .await
        .map_err(|e| Error::Sync(format!("apply task failed: {e}")))?
}

/// Range scheduler shared by the download workers: next range to fetch, ranges to retry,
/// per-peer request spacing and the set of peers currently busy.
#[derive(Default)]
struct Scheduler {
    next_from: u64,
    target: u64,
    window_end: u64,
    retry: std::collections::BTreeSet<u64>,
    busy: std::collections::HashSet<Source>,
    last_request: std::collections::HashMap<Source, Instant>,
    done: bool,
}

struct Fetched {
    from: u64,
    source: Source,
    blocks: Option<Vec<Block>>,
}

/// Candidate sources for a range ending at `need_height`: iroh peers that have it (fastest first),
/// then the best legacy peers.
fn candidates(table: &PeerTable, iroh: Option<&crate::p2p_iroh::IrohNode>, need_height: u64) -> Vec<Source> {
    let mut out: Vec<Source> = Vec::new();
    if let Some(node) = iroh {
        let mut peers: Vec<_> = node.peers.snapshot().into_iter().filter(|p| p.height >= need_height && p.failures < 3).collect();
        peers.sort_by_key(|p| p.latency_ms.unwrap_or(u64::MAX));
        out.extend(peers.into_iter().map(|p| Source::Iroh(p.id)));
    }
    out.extend(table.best(8).into_iter().map(Source::Legacy));
    out
}

/// One download worker: repeatedly takes the next range (retries first) and the best idle peer.
async fn worker(
    table: Arc<PeerTable>,
    iroh: Option<Arc<crate::p2p_iroh::IrohNode>>,
    sched: Arc<std::sync::Mutex<Scheduler>>,
    out: tokio::sync::mpsc::Sender<Fetched>,
    timeout: Duration,
) {
    loop {
        let job = {
            let mut s = sched.lock().unwrap_or_else(|e| e.into_inner());
            if s.done {
                return;
            }
            let from = s.retry.pop_first().or_else(|| {
                (s.next_from < s.target && s.next_from < s.window_end).then(|| {
                    let f = s.next_from;
                    s.next_from += MAX_BLOCKS_PER_REQUEST as u64;
                    f
                })
            });
            match from {
                None => None,
                Some(from) => {
                    let now = Instant::now();
                    let limit = (s.target - from).min(MAX_BLOCKS_PER_REQUEST as u64) as u32;
                    let source = candidates(&table, iroh.as_deref(), from + limit as u64).into_iter().find(|src| {
                        // legacy peers allow one getBlocks per second; iroh peers have no such limit
                        !s.busy.contains(src)
                            && (matches!(src, Source::Iroh(_)) || s.last_request.get(src).map_or(true, |t| now.duration_since(*t) >= MIN_ROUND))
                    });
                    match source {
                        Some(src) => {
                            s.busy.insert(src.clone());
                            s.last_request.insert(src.clone(), now);
                            Some((from, limit, src))
                        }
                        None => {
                            // no idle peer right now: put the range back and wait a little
                            s.retry.insert(from);
                            None
                        }
                    }
                }
            }
        };
        let Some((from, limit, source)) = job else {
            tokio::time::sleep(Duration::from_millis(150)).await;
            continue;
        };
        let blocks = match (&source, &iroh) {
            (Source::Legacy(ip), _) => fetch_range(&table, ip, from, limit, timeout).await,
            (Source::Iroh(id), Some(node)) => fetch_range_iroh(node, *id, from, limit).await,
            (Source::Iroh(_), None) => None,
        };
        {
            let mut s = sched.lock().unwrap_or_else(|e| e.into_inner());
            s.busy.remove(&source);
        }
        if out.send(Fetched { from, source, blocks }).await.is_err() {
            return;
        }
    }
}

/// Pull blocks until the local tip reaches the best height known to the peer table.
/// `parallel` workers keep ranges in flight independently (a slow peer never stalls the others);
/// results are applied strictly in height order, failed or invalid ranges are re-queued.
pub async fn catch_up(storage: Arc<Storage>, table: Arc<PeerTable>, opts: &P2pOptions) -> Result<u64> {
    {
        let st = storage.clone();
        tokio::task::spawn_blocking(move || crate::genesis::ensure_genesis(&st, st.network()))
            .await
            .map_err(|e| Error::Sync(format!("genesis task failed: {e}")))??;
    }
    let mut tip = tip_of(&storage)?;
    let best_known = |table: &PeerTable| table.best_height().max(opts.iroh.as_ref().map(|n| n.peers.best_height()).unwrap_or(0));
    let target = best_known(&table);
    if target <= tip.height {
        return Ok(0);
    }
    let parallel = opts.parallel.max(1);
    let window = (parallel as u64 * 3) * MAX_BLOCKS_PER_REQUEST as u64;
    tracing::info!(from = tip.height, to = target, parallel, "catching up over legacy P2P");
    let pb = if opts.quiet { ProgressBar::hidden() } else { ProgressBar::new(target) };
    pb.set_style(progress_style("Syncing")?);
    pb.set_position(tip.height);
    let started = Instant::now();
    let mut applied = 0u64;
    let mut last_log = Instant::now();

    let sched = Arc::new(std::sync::Mutex::new(Scheduler {
        next_from: tip.height,
        target,
        window_end: tip.height + window,
        ..Default::default()
    }));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Fetched>(parallel * 2);
    let workers: Vec<_> = (0..parallel)
        .map(|_| tokio::spawn(worker(table.clone(), opts.iroh.clone(), sched.clone(), tx.clone(), opts.timeout)))
        .collect();
    drop(tx);

    let mut pending: std::collections::BTreeMap<u64, (Source, Vec<Block>)> = Default::default();
    let punish = |src: &Source| match src {
        Source::Legacy(ip) => table.record_failure(ip),
        Source::Iroh(id) => {
            if let Some(n) = &opts.iroh {
                n.peers.record_rpc(*id, None, None);
            }
        }
    };
    let mut failures_in_row = 0u32;
    let result: Result<()> = async {
        loop {
            // apply everything contiguous with the tip
            while let Some((source, blocks)) = pending.remove(&tip.height) {
                let from = tip.height;
                match apply(&storage, blocks, tip.clone(), opts.verify).await {
                    Ok(new_tip) => {
                        applied += new_tip.height - tip.height;
                        tip = new_tip;
                        pb.set_position(tip.height);
                        failures_in_row = 0;
                    }
                    Err(e) => {
                        tracing::warn!(peer = %source, from, error = %e, "peer blocks rejected, re-requesting range");
                        punish(&source);
                        pending.clear();
                        let mut s = sched.lock().unwrap_or_else(|e| e.into_inner());
                        s.retry.clear();
                        s.next_from = from;
                    }
                }
            }
            let current_target = best_known(&table);
            let finished = {
                let mut s = sched.lock().unwrap_or_else(|e| e.into_inner());
                s.target = s.target.max(current_target);
                s.window_end = tip.height + window;
                pb.set_length(s.target);
                tip.height >= s.target
            };
            if finished {
                break Ok(());
            }
            if opts.quiet && last_log.elapsed() >= Duration::from_secs(30) {
                let rate = applied as f64 / started.elapsed().as_secs_f64().max(0.001);
                tracing::info!(height = tip.height, target = current_target, rate = format!("{rate:.0} blocks/sec"), "catch-up progress");
                last_log = Instant::now();
            }
            let Some(f) = rx.recv().await else { break Err(Error::Sync("download workers stopped".into())) };
            match f.blocks {
                Some(b) if !b.is_empty() && b[0].height == f.from + 1 => {
                    failures_in_row = 0;
                    if f.from >= tip.height {
                        pending.insert(f.from, (f.source, b));
                    }
                }
                _ => {
                    failures_in_row += 1;
                    if f.from >= tip.height {
                        sched.lock().unwrap_or_else(|e| e.into_inner()).retry.insert(f.from);
                    }
                    if failures_in_row >= parallel as u32 * 3 {
                        tracing::warn!("legacy peers keep failing, re-probing the peer table");
                        table.refresh(PROBE_CONCURRENCY, PROBE_TIMEOUT).await;
                        failures_in_row = 0;
                    }
                }
            }
        }
    }
    .await;
    sched.lock().unwrap_or_else(|e| e.into_inner()).done = true;
    drop(rx);
    for w in workers {
        w.abort();
    }
    pb.finish_and_clear();
    result?;
    let secs = started.elapsed().as_secs_f64().max(0.001);
    tracing::info!(height = tip.height, applied, rate = format!("{:.0} blocks/sec", applied as f64 / secs), "catch-up complete");
    Ok(applied)
}

/// Steady state: poll the best peer every blocktime, apply new blocks. Returns `Ok(())` when the
/// node fell more than two ranges behind (caller re-runs `catch_up`).
async fn follow_live(storage: &Arc<Storage>, table: &PeerTable, opts: &P2pOptions) -> Result<()> {
    let network = storage.network().clone();
    // near the tip every block is applied on its own with an undo record → forks can be rolled back
    storage.set_undo_enabled(true);
    let mut fork_depth = 1u64;
    let mut last_ip = String::new();
    loop {
        let Some(ip) = table.best(1).into_iter().next() else {
            tracing::warn!("no healthy legacy peers, re-probing in 5s");
            tokio::time::sleep(Duration::from_secs(5)).await;
            table.refresh(PROBE_CONCURRENCY, PROBE_TIMEOUT).await;
            continue;
        };
        let started = Instant::now();
        let mut peer = match LegacyPeer::connect(&ip, table.port(), opts.timeout).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(peer = ip, error = %e, "legacy peer unreachable");
                table.record_failure(&ip);
                continue;
            }
        };
        if ip != last_ip {
            tracing::info!(peer = ip, port = table.port(), "following chain via legacy peer");
            last_ip = ip.clone();
        }
        // legacy nodes drop the socket right after a getBlocks reply → the next call fails by design
        let mut just_fetched = false;
        loop {
            let tip = tip_of(storage)?;
            let status = match peer.get_status().await {
                Ok(s) => s,
                Err(e) if just_fetched => {
                    tracing::debug!(peer = ip, error = %e, "socket reset after getBlocks (expected), reconnecting");
                    break;
                }
                Err(e) => {
                    tracing::warn!(peer = ip, error = %e, "getStatus failed, switching peer");
                    table.record_failure(&ip);
                    break;
                }
            };
            just_fetched = false;
            let peer_height = status.state.as_ref().map(|s| s.height as u64).unwrap_or(0);
            table.record_success(&ip, started.elapsed(), Some(peer_height));
            if peer_height <= tip.height {
                // poll fast: a forging node must see the previous slot's block within a second or two
                let _ = network;
                tokio::time::sleep(Duration::from_millis(1_200)).await;
                continue;
            }
            if peer_height - tip.height > 2 * MAX_BLOCKS_PER_REQUEST as u64 {
                return Ok(());
            }
            let blocks = match peer.get_blocks(tip.height, MAX_BLOCKS_PER_REQUEST).await {
                Ok(b) => b,
                Err(e) => {
                    // Legacy nodes often reset the socket right after a large getBlocks reply; just reconnect.
                    tracing::debug!(peer = ip, error = %e, "getBlocks connection dropped, reconnecting");
                    break;
                }
            };
            if blocks.is_empty() {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            let (from, to) = (blocks[0].height, blocks[blocks.len() - 1].height);
            let forked = blocks[0].height == tip.height + 1 && tip.id.as_deref().is_some_and(|id| blocks[0].previous_block != id);
            if forked {
                // the network continued on another branch: roll back and resync from the best peer
                let target = tip.height.saturating_sub(fork_depth).max(1);
                let st = storage.clone();
                let rolled = tokio::task::spawn_blocking(move || st.rollback_to(target)).await.map_err(|e| Error::Sync(e.to_string()))?;
                match rolled {
                    Ok(h) => tracing::warn!(peer = ip, from = tip.height, to = h, "fork detected, rolled back"),
                    Err(e) => tracing::error!(error = %e, "rollback failed (undo records missing?) — resync from snapshot may be required"),
                }
                fork_depth = (fork_depth * 3).min(crate::storage::UNDO_DEPTH);
                break;
            }
            let tx_count: usize = blocks.iter().map(|b| b.transactions.len()).sum();
            match apply(storage, blocks, tip, opts.verify).await {
                Ok(new_tip) => {
                    fork_depth = 1;
                    if from == to {
                        tracing::info!("Received new block at height {} with {} transactions from {}", group(to), tx_count, ip);
                    } else {
                        tracing::info!("Downloaded {} new blocks ({}..{}) accounting for a total of {} transactions from {}", to - from + 1, group(from), group(to), tx_count, ip);
                    }
                    if peer_height <= new_tip.height {
                        tracing::info!("Blockchain 100% in sync (height {})", group(new_tip.height));
                    }
                }
                Err(e) => {
                    tracing::error!(peer = ip, error = %e, "peer blocks rejected, switching peer");
                    table.record_failure(&ip);
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Catch up, then follow; the peer table is re-probed in the background every `refresh_interval`.
pub async fn run(storage: Arc<Storage>, table: Arc<PeerTable>, opts: P2pOptions) -> Result<()> {
    table.refresh(PROBE_CONCURRENCY, PROBE_TIMEOUT).await;
    if let Some(node) = &opts.iroh {
        node.refresh_peers().await;
    }
    let refresher = {
        let table = table.clone();
        let iroh = opts.iroh.clone();
        let every = opts.refresh_interval;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.tick().await;
            loop {
                tick.tick().await;
                table.refresh(PROBE_CONCURRENCY, PROBE_TIMEOUT).await;
                if let Some(node) = &iroh {
                    node.refresh_peers().await;
                }
            }
        })
    };
    let result = async {
        loop {
            catch_up(storage.clone(), table.clone(), &opts).await?;
            follow_live(&storage, &table, &opts).await?;
        }
    }
    .await;
    refresher.abort();
    result
}
