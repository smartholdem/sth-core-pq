//! Author: TechnoL0g
//!
//! legacy bootstrap sync over the public REST API of existing SmartHoldem
//! nodes (port 4003 behind HTTPS), rate-limit aware.
//!
//! TEMPORARY: SmartHoldem nodes talk to each other over the dedicated P2P port
//! (`p2p.blocks.getBlocks` over WebSocket/protobuf); this module only exists to catch up
//! quickly with the chain. Phase 5 (iroh) replaces it.
//!
//! Pipeline: `[fetch ranges concurrently through NodePool] -> mpsc -> [verify + apply in order]`.
//! * `NodePool` enforces ≤4 req/s per node, ≤250 req/60 s per node, parks a node for 60 s on
//!   HTTP 429 and after 5 consecutive failures, and spreads workers over all nodes.
//! * Errors back off 1 s -> 2 s -> 4 s -> 8 s -> 16 s (max) before the next attempt.
//! * Every block is linked to the stored tip (`previousBlock == tip.id`), fully verified and
//!   applied atomically via `Storage::apply_block`, so Ctrl+C can never corrupt state and
//!   `sth-core sync` resumes from `get_last_height()`.

use crate::config::Network;
use crate::crypto::{block_id, verify_block};
use crate::error::{Error, Result};
use crate::models::{Block, Transaction};
use crate::node_pool::{Lease, NodePool, RateLimitConfig, API_WINDOW_LIMIT};
use crate::storage::Storage;
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Public legacy nodes (REST API reverse-proxied over HTTPS).
pub const NODES: &[&str] = &[
    "https://node0.smartholdem.io",
    "https://node1.smartholdem.io",
    "https://node2.smartholdem.io",
    "https://node3.smartholdem.io",
    "https://node4.smartholdem.io",
    "https://node5.smartholdem.io",
];

/// Hard limit of the legacy API (`limit` > 100 is rejected).
pub const MAX_API_LIMIT: u64 = 100;

#[derive(Debug, Clone)]
pub struct SyncConfig {
    pub nodes: Vec<String>,
    /// Blocks per range request (1..=100). 100 halves the request count vs. 50.
    pub batch_size: u64,
    /// Range requests in flight (spread over the pool by `NodePool`).
    pub concurrency: usize,
    pub request_timeout: Duration,
    /// Full cryptographic verification of every block (recommended). When off, only
    /// chain linkage and block ids are checked.
    pub verify: bool,
    /// Attempts per range before the sync aborts.
    pub max_attempts: u32,
    /// Keep following the chain after catching up.
    pub follow: bool,
    /// Hide the progress bar.
    pub quiet: bool,
    pub rate_limit: RateLimitConfig,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            nodes: NODES.iter().map(|s| s.to_string()).collect(),
            batch_size: MAX_API_LIMIT,
            concurrency: 8,
            request_timeout: Duration::from_secs(30),
            verify: true,
            max_attempts: 30,
            follow: false,
            quiet: false,
            rate_limit: RateLimitConfig::default(),
        }
    }
}

/// Last applied block (height 0 / no id = empty database).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainTip {
    pub height: u64,
    pub id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SyncReport {
    pub start_height: u64,
    pub end_height: u64,
    pub network_height: u64,
    pub blocks_applied: u64,
    pub elapsed: Duration,
    pub interrupted: bool,
}

#[derive(Deserialize)]
struct ApiList<T> {
    data: Vec<T>,
}

#[derive(Deserialize)]
struct ApiItem<T> {
    data: T,
}

#[derive(Deserialize)]
struct BlockchainInfo {
    block: BlockchainTip,
}

#[derive(Deserialize)]
struct BlockchainTip {
    height: u64,
}

enum Outcome {
    Finished,
    Interrupted,
}

/// Request failure classes that drive the retry policy.
enum RequestError {
    RateLimited,
    Transient(Error),
}

pub struct Syncer {
    cfg: SyncConfig,
    client: reqwest::Client,
    storage: Arc<Storage>,
    network: Network,
    pool: NodePool,
    multi: MultiProgress,
    bar: ProgressBar,
    status: ProgressBar,
}

impl Syncer {
    pub fn new(storage: Arc<Storage>, cfg: SyncConfig) -> Result<Self> {
        if cfg.nodes.is_empty() {
            return Err(Error::Sync("node pool is empty".into()));
        }
        let cfg = SyncConfig {
            batch_size: cfg.batch_size.clamp(1, MAX_API_LIMIT),
            concurrency: cfg.concurrency.max(1),
            max_attempts: cfg.max_attempts.max(1),
            ..cfg
        };
        let client = reqwest::Client::builder()
            .timeout(cfg.request_timeout)
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("sth-core/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let pool = NodePool::new(&cfg.nodes, cfg.rate_limit.clone());

        let multi = if cfg.quiet {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        } else {
            MultiProgress::new()
        };
        let bar = multi.add(ProgressBar::new(0));
        bar.set_style(progress_style("Syncing")?);
        let status = multi.add(ProgressBar::new(0));
        status.set_style(
            ProgressStyle::with_template("{msg}").map_err(|e| Error::Sync(format!("status template: {e}")))?,
        );
        let network = storage.network().clone();
        Ok(Self { cfg, client, storage, network, pool, multi, bar, status })
    }

    /// Current tip from Sled.
    pub fn local_tip(&self) -> Result<ChainTip> {
        Ok(match self.storage.get_last_block()? {
            Some(b) => ChainTip { height: b.height, id: b.id },
            None => ChainTip { height: 0, id: None },
        })
    }

    /// Highest height reported by any reachable node (one request per node).
    pub async fn fetch_network_height(&self) -> Result<u64> {
        let mut best: Option<u64> = None;
        for _ in 0..self.pool.len() {
            let lease = self.pool.acquire().await;
            tokio::time::sleep_until(lease.slot.into()).await;
            let url = format!("{}/api/blockchain", lease.url);
            match self.get_json::<ApiItem<BlockchainInfo>>(&url).await {
                Ok(info) => {
                    self.pool.report_success(&lease);
                    let h = info.data.block.height;
                    best = Some(best.map_or(h, |b| b.max(h)));
                }
                Err(RequestError::RateLimited) => self.handle_429(&lease),
                Err(RequestError::Transient(e)) => {
                    self.pool.report_failure(&lease);
                    self.warn(format!("{}: cannot read network height: {e}", lease.host));
                }
            }
        }
        best.ok_or_else(|| Error::Sync("no node in the pool answered /api/blockchain".into()))
    }

    /// Sync to the network tip (and keep following it when `cfg.follow`).
    pub async fn run(self: Arc<Self>) -> Result<SyncReport> {
        let started = Instant::now();
        let start_tip = self.local_tip()?;
        let mut tip = start_tip.clone();
        let mut network_height = self.fetch_network_height().await?;
        let mut interrupted = false;

        self.bar.set_length(network_height.max(tip.height));
        self.bar.set_position(tip.height);
        self.bar.reset_eta();
        self.refresh_status();
        self.info(format!(
            "local height {} / network height {} ({} behind) | {} nodes, batch {}, concurrency {}, {} req/s per node, {}/{} per {}s window, verify={}",
            tip.height,
            network_height,
            network_height.saturating_sub(tip.height),
            self.pool.len(),
            self.cfg.batch_size,
            self.cfg.concurrency,
            self.cfg.rate_limit.per_node_rps,
            self.cfg.rate_limit.window_limit,
            API_WINDOW_LIMIT,
            self.cfg.rate_limit.window.as_secs(),
            self.cfg.verify
        ));

        loop {
            if tip.height < network_height {
                let (new_tip, outcome) = self.clone().sync_until(tip, network_height).await?;
                tip = new_tip;
                if matches!(outcome, Outcome::Interrupted) {
                    interrupted = true;
                    break;
                }
                // The chain kept growing while we were catching up.
                network_height = self.fetch_network_height().await.unwrap_or(network_height);
                if tip.height < network_height {
                    self.bar.set_length(network_height);
                    continue;
                }
            }
            if !self.cfg.follow {
                break;
            }
            let blocktime = self.network.milestone(tip.height.max(1)).blocktime as u64;
            tokio::select! {
                _ = tokio::signal::ctrl_c() => { interrupted = true; break; }
                _ = tokio::time::sleep(Duration::from_secs(blocktime)) => {}
            }
            match self.fetch_network_height().await {
                Ok(h) if h > tip.height => {
                    network_height = h;
                    self.bar.set_length(h);
                }
                Ok(_) => {}
                Err(e) => self.warn(e.to_string()),
            }
        }

        self.status.finish_and_clear();
        self.bar.finish_and_clear();
        self.storage.flush()?;
        Ok(SyncReport {
            start_height: start_tip.height,
            end_height: tip.height,
            network_height,
            blocks_applied: tip.height.saturating_sub(start_tip.height),
            elapsed: started.elapsed(),
            interrupted,
        })
    }

    /// Download `(tip, target]` through the concurrent pipeline and apply in order.
    async fn sync_until(self: Arc<Self>, mut tip: ChainTip, target: u64) -> Result<(ChainTip, Outcome)> {
        let batch = self.cfg.batch_size;
        let ranges: Vec<(u64, u64)> = (tip.height + 1..=target)
            .step_by(batch as usize)
            .map(|from| (from, (from + batch - 1).min(target)))
            .collect();

        let (sender, mut receiver) = mpsc::channel::<Result<Vec<Block>>>(self.cfg.concurrency * 2);
        let fetcher = self.clone();
        let producer = tokio::spawn(async move {
            let concurrency = fetcher.cfg.concurrency;
            let mut batches = stream::iter(ranges)
                .map(|(from, to)| {
                    let f = fetcher.clone();
                    async move { f.fetch_range(from, to).await }
                })
                .buffered(concurrency);
            while let Some(res) = batches.next().await {
                let failed = res.is_err();
                if sender.send(res).await.is_err() || failed {
                    break;
                }
            }
        });

        let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());
        let outcome = loop {
            tokio::select! {
                _ = &mut ctrl_c => {
                    self.info("SIGINT received - state is saved after every block; stopping (resume with `sth-core sync`)".to_string());
                    break Outcome::Interrupted;
                }
                next = receiver.recv() => match next {
                    None => break Outcome::Finished,
                    Some(Err(e)) => {
                        producer.abort();
                        return Err(e);
                    }
                    Some(Ok(blocks)) => {
                        if blocks.is_empty() {
                            continue;
                        }
                        tip = match self.apply_batch(tip.clone(), blocks).await {
                            Ok(t) => t,
                            Err(e) => {
                                producer.abort();
                                self.error(format!("chain validation failed, sync stopped: {e}"));
                                return Err(e);
                            }
                        };
                        self.bar.set_position(tip.height);
                        self.refresh_status();
                    }
                },
            }
        };
        producer.abort();
        Ok((tip, outcome))
    }

    /// Verify + apply a batch on the blocking pool; on failure re-fetch once from another node
    /// (protects against a single node serving a stale fork or a truncated page).
    async fn apply_batch(&self, tip: ChainTip, blocks: Vec<Block>) -> Result<ChainTip> {
        let from = blocks.first().map(|b| b.height).unwrap_or(tip.height + 1);
        let to = blocks.last().map(|b| b.height).unwrap_or(from);

        match self.apply_blocking(tip.clone(), blocks).await {
            Ok(t) => Ok(t),
            Err(e) => {
                self.warn(format!("batch {from}-{to} rejected ({e}); re-fetching from another node"));
                let retry = self.fetch_range(from, to).await?;
                self.apply_blocking(tip, retry).await
            }
        }
    }

    async fn apply_blocking(&self, tip: ChainTip, blocks: Vec<Block>) -> Result<ChainTip> {
        let storage = self.storage.clone();
        let network = self.network.clone();
        let verify = self.cfg.verify;
        tokio::task::spawn_blocking(move || apply_blocks(&storage, &network, &blocks, tip, verify))
            .await
            .map_err(|e| Error::Sync(format!("apply task failed: {e}")))?
    }

    // ------------------------------------------------------------------ HTTP

    /// Fetch blocks `[from, to]` (with transactions) with rate limiting, backoff and failover.
    async fn fetch_range(&self, from: u64, to: u64) -> Result<Vec<Block>> {
        let mut backoff_step: u32 = 0;
        let mut last_err: Option<Error> = None;
        for _attempt in 0..self.cfg.max_attempts {
            let lease = self.pool.acquire().await;
            tokio::time::sleep_until(lease.slot.into()).await;
            match self.fetch_range_from(&lease, from, to).await {
                Ok(blocks) => {
                    self.pool.report_success(&lease);
                    return Ok(blocks);
                }
                Err(RequestError::RateLimited) => {
                    self.handle_429(&lease);
                    last_err = Some(Error::Sync(format!("HTTP 429 from {}", lease.host)));
                }
                Err(RequestError::Transient(e)) => {
                    let (failures, switched) = self.pool.report_failure(&lease);
                    let delay = Duration::from_secs(1u64 << backoff_step.min(4));
                    backoff_step += 1;
                    match switched {
                        Some(next) => self.warn(format!(
                            "{}: blocks {from}-{to} failed ({e}); {failures} consecutive failures, switching to {next}",
                            lease.host
                        )),
                        None => self.warn(format!(
                            "{}: blocks {from}-{to} failed ({e}); failure {failures}/{}, retrying in {delay:?}",
                            lease.host, self.cfg.rate_limit.max_failures
                        )),
                    }
                    last_err = Some(e);
                    tokio::time::sleep(delay).await;
                }
            }
            self.refresh_status();
        }
        Err(last_err.unwrap_or_else(|| Error::Sync(format!("blocks {from}-{to}: gave up after {} attempts", self.cfg.max_attempts))))
    }

    async fn fetch_range_from(&self, lease: &Lease, from: u64, to: u64) -> std::result::Result<Vec<Block>, RequestError> {
        let url = format!(
            "{}/api/blocks?height.from={from}&height.to={to}&limit={}&orderBy=height:asc&transform=false",
            lease.url, self.cfg.batch_size
        );
        let mut blocks = self.get_json::<ApiList<Block>>(&url).await?.data;
        blocks.sort_by_key(|b| b.height);
        self.refresh_status();

        // Transactions of non-empty blocks: each request goes through the pool as well.
        for block in blocks.iter_mut().filter(|b| b.number_of_transactions > 0) {
            block.transactions = self.fetch_block_transactions(block).await?;
        }
        Ok(blocks)
    }

    /// All transactions of one block, paginated, sorted by `sequence`.
    async fn fetch_block_transactions(&self, block: &Block) -> std::result::Result<Vec<Transaction>, RequestError> {
        let id = block
            .id
            .as_deref()
            .ok_or_else(|| RequestError::Transient(Error::Sync(format!("block {} has no id", block.height))))?;
        let expected = block.number_of_transactions as usize;
        let mut all: Vec<Transaction> = Vec::with_capacity(expected);
        let mut page = 1u32;
        loop {
            let lease = self.pool.acquire().await;
            tokio::time::sleep_until(lease.slot.into()).await;
            let url = format!(
                "{}/api/blocks/{id}/transactions?transform=false&limit={MAX_API_LIMIT}&page={page}",
                lease.url
            );
            let chunk = match self.get_json::<ApiList<Transaction>>(&url).await {
                Ok(list) => {
                    self.pool.report_success(&lease);
                    list.data
                }
                Err(RequestError::RateLimited) => {
                    self.handle_429(&lease);
                    return Err(RequestError::RateLimited);
                }
                Err(RequestError::Transient(e)) => {
                    self.pool.report_failure(&lease);
                    return Err(RequestError::Transient(e));
                }
            };
            let n = chunk.len();
            all.extend(chunk);
            if n < MAX_API_LIMIT as usize || all.len() >= expected {
                break;
            }
            page += 1;
        }
        all.sort_by_key(|t| t.sequence.unwrap_or(u32::MAX));
        all.dedup_by(|a, b| a.id.is_some() && a.id == b.id);
        if all.len() != expected {
            return Err(RequestError::Transient(Error::Sync(format!(
                "block {} ({id}): got {} transactions, header says {expected}",
                block.height,
                all.len()
            ))));
        }
        Ok(all)
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> std::result::Result<T, RequestError> {
        let resp = self.client.get(url).send().await.map_err(|e| RequestError::Transient(e.into()))?;
        let status = resp.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(RequestError::RateLimited);
        }
        if !status.is_success() {
            return Err(RequestError::Transient(Error::Sync(format!("HTTP {status} from {url}"))));
        }
        resp.json::<T>().await.map_err(|e| RequestError::Transient(e.into()))
    }

    fn handle_429(&self, lease: &Lease) {
        let next = self.pool.report_rate_limited(lease).unwrap_or_else(|| "(all nodes parked)".into());
        self.warn(format!(
            "Rate limit hit on {}, switching to {next}, waiting {}s",
            lease.host,
            self.cfg.rate_limit.ban_on_429.as_secs()
        ));
        self.refresh_status();
    }

    // --------------------------------------------------------------- output

    /// `Current node: node2.smartholdem.io | Requests: 180/300`
    fn refresh_status(&self) {
        let s = self.pool.status();
        let parked = if s.parked_nodes > 0 { format!(" | Parked nodes: {}", s.parked_nodes) } else { String::new() };
        self.status.set_message(format!(
            "Current node: {} | Requests: {}/{API_WINDOW_LIMIT}{parked}",
            s.host, s.requests_in_window
        ));
    }

    fn info(&self, msg: String) {
        self.multi.suspend(|| tracing::info!("{msg}"));
    }

    fn warn(&self, msg: String) {
        self.multi.suspend(|| tracing::warn!("{msg}"));
    }

    fn error(&self, msg: String) {
        self.multi.suspend(|| tracing::error!("{msg}"));
    }
}

/// `Syncing: [=====>     ] 11704000 / 11704428 (4 blocks/sec, ETA: 1m 46s)`
pub fn progress_style(prefix: &str) -> Result<ProgressStyle> {
    ProgressStyle::with_template(&format!("{prefix}: [{{bar:40}}] {{pos}} / {{len}} ({{bps}}, ETA: {{eta}}) {{msg}}"))
        .map_err(|e| Error::Sync(format!("progress template: {e}")))
        .map(|s| {
            s.progress_chars("=> ").with_key("bps", |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                let _ = write!(w, "{:.0} blocks/sec", state.per_sec());
            })
        })
}

/// Link-check and verify `blocks` on top of `tip`, then apply them in one sled transaction.
/// Pure blocking function so it can run on the blocking pool and be unit-tested.
pub fn apply_blocks(storage: &Storage, network: &Network, blocks: &[Block], mut tip: ChainTip, verify: bool) -> Result<ChainTip> {
    for block in blocks {
        let height = block.height;
        if height != tip.height + 1 {
            return Err(Error::Sync(format!("height gap: expected {}, got {height}", tip.height + 1)));
        }
        let id = block
            .id
            .clone()
            .ok_or_else(|| Error::Sync(format!("block {height} has no id")))?;
        if let Some(prev) = &tip.id {
            if &block.previous_block != prev {
                return Err(Error::BlockValidation(format!(
                    "block {height} ({id}) previousBlock {} does not match local tip {prev}",
                    block.previous_block
                )));
            }
        }
        if verify {
            let v = verify_block(block, network);
            if !v.verified {
                return Err(Error::BlockValidation(format!("block {height} ({id}): {}", v.errors.join("; "))));
            }
        } else if height > 1 {
            let computed = block_id(block)?;
            if computed != id {
                return Err(Error::BlockValidation(format!("block {height}: id {id} != computed {computed}")));
            }
        }
        tip = ChainTip { height, id: Some(id) };
    }
    storage.apply_blocks(blocks)?;
    Ok(tip)
}
