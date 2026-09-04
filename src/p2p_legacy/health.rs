//! Author: TechnoL0g
//!
//! Peer health table for the legacy P2P network: per-peer latency (EMA), reported height,
//! consecutive failures and temporary bans. `best(n)` picks the peers to pull blocks from /
//! relay transactions to; `refresh()` probes every known peer in parallel and discovers new ones.

use super::LegacyPeer;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Latency assumed for a peer that has never answered (keeps probed peers ahead of unknown ones).
const UNKNOWN_LATENCY_MS: u64 = 2_000;
/// Consecutive failures before a peer is parked.
const BAN_AFTER_FAILURES: u32 = 3;
const BAN_STEP: Duration = Duration::from_secs(30);
const MAX_BAN: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct PeerStats {
    pub ip: String,
    pub height: u64,
    pub latency_ms: u64,
    pub failures: u32,
    pub successes: u64,
    pub total_failures: u64,
    pub version: String,
    pub last_ok: Option<Instant>,
    pub banned_until: Option<Instant>,
    /// Most recent latency samples (ms), newest last.
    pub latency_history: std::collections::VecDeque<u64>,
    /// `getBlocks` speed: EMA of the time a full 400-block range takes (ms). None = never pulled blocks.
    pub blocks_latency_ms: Option<u64>,
    /// Consecutive `getBlocks` failures (timeouts / rejected ranges); not reset by status probes.
    pub blocks_failures: u32,
    /// Peer is skipped as a block source until then.
    pub blocks_parked_until: Option<Instant>,
}

const HISTORY_LEN: usize = 20;
/// Score assumed for a peer whose `getBlocks` speed is unknown: after proven-fast peers, before proven-slow ones.
const UNKNOWN_BLOCKS_MS: u64 = 1_500;
const BLOCKS_PARK_STEP: Duration = Duration::from_secs(30);
/// Time budget one `getBlocks` should fit into (well below the 20 s socket timeout).
const BLOCKS_BUDGET_MS: u64 = 12_000;
/// First request to a peer with unknown `getBlocks` speed.
pub const BLOCKS_PROBE_LIMIT: u32 = 100;
const BLOCKS_MIN_LIMIT: u32 = 50;

impl PeerStats {
    fn new(ip: &str) -> Self {
        Self {
            ip: ip.to_string(),
            height: 0,
            latency_ms: UNKNOWN_LATENCY_MS,
            failures: 0,
            successes: 0,
            total_failures: 0,
            version: String::new(),
            last_ok: None,
            banned_until: None,
            latency_history: std::collections::VecDeque::with_capacity(HISTORY_LEN),
            blocks_latency_ms: None,
            blocks_failures: 0,
            blocks_parked_until: None,
        }
    }

    pub fn is_banned(&self, now: Instant) -> bool {
        self.banned_until.map(|t| t > now).unwrap_or(false)
    }

    pub fn is_blocks_parked(&self, now: Instant) -> bool {
        self.blocks_parked_until.map(|t| t > now).unwrap_or(false)
    }

    /// Lower is better: latency + 50 ms per block behind the best peer + 1 s per consecutive failure.
    pub fn score(&self, best_height: u64) -> u64 {
        let lag = best_height.saturating_sub(self.height);
        self.latency_ms + lag.min(10_000) * 50 + self.failures as u64 * 1_000
    }

    /// Block-source score: measured `getBlocks` speed (unknown peers slot in after proven-fast ones) + lag + failures.
    pub fn blocks_score(&self, best_height: u64) -> u64 {
        let lag = best_height.saturating_sub(self.height);
        self.blocks_latency_ms.unwrap_or(self.latency_ms + UNKNOWN_BLOCKS_MS) + lag.min(10_000) * 50 + self.blocks_failures as u64 * 1_000
    }

    /// How many blocks to ask this peer for so the reply fits the time budget.
    pub fn blocks_limit(&self) -> u32 {
        match self.blocks_latency_ms {
            None => BLOCKS_PROBE_LIMIT,
            Some(ms) => ((super::MAX_BLOCKS_PER_REQUEST as u64 * BLOCKS_BUDGET_MS / ms.max(1)) as u32).clamp(BLOCKS_MIN_LIMIT, super::MAX_BLOCKS_PER_REQUEST),
        }
    }
}

pub struct PeerTable {
    port: u16,
    peers: Mutex<HashMap<String, PeerStats>>,
}

impl PeerTable {
    pub fn new(port: u16, seeds: impl IntoIterator<Item = String>) -> Self {
        let table = Self { port, peers: Mutex::new(HashMap::new()) };
        table.add_many(seeds);
        table
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PeerStats>> {
        self.peers.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Add an IP; returns true when it was unknown.
    pub fn add(&self, ip: &str) -> bool {
        let ip = ip.trim();
        if ip.is_empty() {
            return false;
        }
        let mut map = self.lock();
        if map.contains_key(ip) {
            return false;
        }
        map.insert(ip.to_string(), PeerStats::new(ip));
        true
    }

    pub fn add_many(&self, ips: impl IntoIterator<Item = String>) -> usize {
        ips.into_iter().filter(|ip| self.add(ip)).count()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    pub fn record_success(&self, ip: &str, latency: Duration, height: Option<u64>) {
        let mut map = self.lock();
        let p = map.entry(ip.to_string()).or_insert_with(|| PeerStats::new(ip));
        let ms = latency.as_millis() as u64;
        p.latency_ms = if p.successes == 0 { ms } else { (p.latency_ms * 3 + ms) / 4 };
        if p.latency_history.len() == HISTORY_LEN {
            p.latency_history.pop_front();
        }
        p.latency_history.push_back(ms);
        if let Some(h) = height {
            p.height = p.height.max(h);
        }
        p.failures = 0;
        p.successes += 1;
        p.last_ok = Some(Instant::now());
        p.banned_until = None;
    }

    pub fn record_failure(&self, ip: &str) {
        let mut map = self.lock();
        let p = map.entry(ip.to_string()).or_insert_with(|| PeerStats::new(ip));
        p.failures += 1;
        p.total_failures += 1;
        if p.failures >= BAN_AFTER_FAILURES {
            let steps = (p.failures - BAN_AFTER_FAILURES + 1) as u32;
            let ban = BAN_STEP.saturating_mul(steps).min(MAX_BAN);
            p.banned_until = Some(Instant::now() + ban);
            tracing::debug!(peer = ip, failures = p.failures, ban_secs = ban.as_secs(), "peer parked");
        }
    }

    pub fn set_version(&self, ip: &str, version: &str) {
        if let Some(p) = self.lock().get_mut(ip) {
            p.version = version.to_string();
        }
    }

    /// A `getBlocks` reply of `count` blocks arrived after `latency`; speed is normalised to a full range.
    pub fn record_blocks_success(&self, ip: &str, latency: Duration, count: usize, reached: Option<u64>) {
        self.record_success(ip, latency, reached);
        let mut map = self.lock();
        let Some(p) = map.get_mut(ip) else { return };
        let per_range = latency.as_millis() as u64 * super::MAX_BLOCKS_PER_REQUEST as u64 / count.max(1) as u64;
        p.blocks_latency_ms = Some(match p.blocks_latency_ms {
            Some(prev) => (prev * 3 + per_range) / 4,
            None => per_range,
        });
        p.blocks_failures = 0;
        p.blocks_parked_until = None;
    }

    /// `getBlocks` timed out / failed / returned bad blocks: park the peer as a block source
    /// (30 s, 60 s, 120 s … max 10 min). Status probes do not lift this.
    pub fn record_blocks_failure(&self, ip: &str) {
        self.record_failure(ip);
        let mut map = self.lock();
        let Some(p) = map.get_mut(ip) else { return };
        p.blocks_failures += 1;
        let park = BLOCKS_PARK_STEP.saturating_mul(1u32 << (p.blocks_failures - 1).min(5)).min(MAX_BAN);
        p.blocks_parked_until = Some(Instant::now() + park);
        tracing::debug!(peer = ip, failures = p.blocks_failures, park_secs = park.as_secs(), "peer parked as block source");
    }

    /// Request size for `ip` (probe size for peers never pulled from).
    pub fn blocks_limit(&self, ip: &str) -> u32 {
        self.lock().get(ip).map(|p| p.blocks_limit()).unwrap_or(BLOCKS_PROBE_LIMIT)
    }

    /// Up to `n` block sources, fastest `getBlocks` first; banned and parked peers are skipped.
    pub fn best_for_blocks(&self, n: usize) -> Vec<String> {
        let now = Instant::now();
        let map = self.lock();
        let best = map.values().filter(|p| !p.is_banned(now)).map(|p| p.height).max().unwrap_or(0);
        let mut v: Vec<&PeerStats> = map.values().filter(|p| !p.is_banned(now) && !p.is_blocks_parked(now)).collect();
        v.sort_by_key(|p| (p.blocks_score(best), &p.ip));
        v.into_iter().take(n).map(|p| p.ip.clone()).collect()
    }

    /// Highest height reported by any non-banned peer.
    pub fn best_height(&self) -> u64 {
        let now = Instant::now();
        self.lock().values().filter(|p| !p.is_banned(now)).map(|p| p.height).max().unwrap_or(0)
    }

    /// All peers sorted best-first (banned peers last).
    pub fn snapshot(&self) -> Vec<PeerStats> {
        let now = Instant::now();
        let map = self.lock();
        let best = map.values().filter(|p| !p.is_banned(now)).map(|p| p.height).max().unwrap_or(0);
        let mut v: Vec<PeerStats> = map.values().cloned().collect();
        v.sort_by_key(|p| (p.is_banned(now), p.score(best), p.ip.clone()));
        v
    }

    /// Up to `n` distinct non-banned peers, best score first.
    pub fn best(&self, n: usize) -> Vec<String> {
        let now = Instant::now();
        self.snapshot().into_iter().filter(|p| !p.is_banned(now)).take(n).map(|p| p.ip).collect()
    }

    /// Non-banned peers that answered at least once.
    pub fn alive(&self) -> usize {
        let now = Instant::now();
        self.lock().values().filter(|p| !p.is_banned(now) && p.successes > 0).count()
    }

    /// Probe every known peer (`getStatus`, plus `getPeers` for discovery) with bounded concurrency.
    /// Returns the number of peers that answered.
    pub async fn refresh(&self, concurrency: usize, timeout: Duration) -> usize {
        let ips: Vec<String> = self.lock().keys().cloned().collect();
        let port = self.port;
        let results: Vec<(String, Option<Probe>)> = futures::stream::iter(ips)
            .map(|ip| async move {
                let r = probe(&ip, port, timeout).await;
                (ip, r)
            })
            .buffer_unordered(concurrency.max(1))
            .collect()
            .await;
        let mut alive = 0usize;
        let mut discovered = 0usize;
        for (ip, r) in results {
            match r {
                Some(p) => {
                    alive += 1;
                    self.record_success(&ip, p.latency, Some(p.height));
                    self.set_version(&ip, &p.version);
                    discovered += self.add_many(p.peers);
                }
                None => self.record_failure(&ip),
            }
        }
        tracing::info!(alive, known = self.len(), discovered, best_height = self.best_height(), "peer table refreshed");
        alive
    }
}

struct Probe {
    latency: Duration,
    height: u64,
    version: String,
    peers: Vec<String>,
}

async fn probe(ip: &str, port: u16, timeout: Duration) -> Option<Probe> {
    let started = Instant::now();
    let mut peer = LegacyPeer::connect(ip, port, timeout).await.ok()?;
    let status = peer.get_status().await.ok()?;
    let latency = started.elapsed();
    let height = status.state.as_ref().map(|s| s.height as u64).unwrap_or(0);
    let version = status.config.map(|c| c.version).unwrap_or_default();
    let peers = match peer.get_peers().await {
        Ok(list) => list.into_iter().filter(|p| p.port as u16 == port).map(|p| p.ip).collect(),
        Err(_) => Vec::new(),
    };
    Some(Probe { latency, height, version, peers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_prefers_low_latency_and_high_height() {
        let t = PeerTable::new(4001, ["a".to_string(), "b".to_string(), "c".to_string()]);
        t.record_success("a", Duration::from_millis(50), Some(100));
        t.record_success("b", Duration::from_millis(10), Some(100));
        t.record_success("c", Duration::from_millis(5), Some(50)); // 50 blocks behind → +2500
        assert_eq!(t.best(2), vec!["b".to_string(), "a".to_string()]);
        assert_eq!(t.best_height(), 100);
        assert_eq!(t.alive(), 3);
    }

    #[test]
    fn failures_ban_and_success_unbans() {
        let t = PeerTable::new(4001, ["a".to_string(), "b".to_string()]);
        t.record_success("b", Duration::from_millis(10), Some(10));
        for _ in 0..3 {
            t.record_failure("a");
        }
        assert_eq!(t.best(5), vec!["b".to_string()]);
        t.record_success("a", Duration::from_millis(1), Some(10));
        assert_eq!(t.best(5).len(), 2);
        assert_eq!(t.snapshot()[0].ip, "a");
    }

    #[test]
    fn add_many_dedupes() {
        let t = PeerTable::new(4001, ["a".to_string()]);
        assert_eq!(t.add_many(["a".to_string(), "b".to_string(), " ".to_string()]), 1);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn blocks_parking_survives_status_refresh() {
        let t = PeerTable::new(4001, ["fast".to_string(), "slow".to_string(), "new".to_string()]);
        for ip in ["fast", "slow", "new"] {
            t.record_success(ip, Duration::from_millis(20), Some(100));
        }
        assert_eq!(t.blocks_limit("fast"), BLOCKS_PROBE_LIMIT);
        t.record_blocks_success("fast", Duration::from_millis(500), 400, Some(100));
        assert_eq!(t.blocks_limit("fast"), 400);
        // 100 blocks in 8 s → 32 s per range → ~150 blocks fit the 12 s budget
        t.record_blocks_success("slow", Duration::from_secs(8), 100, Some(100));
        assert_eq!(t.blocks_limit("slow"), 150);
        assert_eq!(t.best_for_blocks(3), vec!["fast".to_string(), "new".to_string(), "slow".to_string()]);
        t.record_blocks_failure("slow");
        assert_eq!(t.best_for_blocks(3), vec!["fast".to_string(), "new".to_string()]);
        // a status probe succeeds → general ban lifted, block parking stays
        t.record_success("slow", Duration::from_millis(20), Some(100));
        assert_eq!(t.best(3).len(), 3);
        assert_eq!(t.best_for_blocks(3).len(), 2);
    }
}
