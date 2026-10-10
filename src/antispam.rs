//! Author: TechnoL0g
//!
//! `mempool.antispam` — pool POLICY against cheap-transaction floods (never consensus: blocks are not judged by it).
//! Per-sender pending caps, per-peer rate limits with bans for invalid batches, operator deny list, replace-by-fee
//! eviction when the pool is full, counters for the metrics page and a `spam-wave` alert.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AntiSpamConfig {
    pub enabled: bool,
    /// Pending transactions one sender address may hold in this pool.
    pub max_per_sender: usize,
    /// Wire bytes one sender may hold in this pool.
    pub max_bytes_per_sender: usize,
    /// Transactions accepted from one peer (legacy IP / iroh EndpointId) per minute; the rest of a batch is dropped.
    pub peer_tx_per_min: u32,
    /// Invalid transactions (bad signature / bytes / network / type — never nonce) from one peer per minute before it is banned.
    pub peer_invalid_per_min: u32,
    pub ban_secs: u64,
    /// Full pool: a transaction paying a higher fee per byte evicts the cheapest pending one (sender's last nonce only).
    pub replace_by_fee: bool,
    /// Peers whose transactions are always dropped (IP or EndpointId).
    pub deny_peers: Vec<String>,
    /// Alert `spam-wave` when more than this many transactions are dropped within one minute (0 = off).
    pub alert_drops_per_min: u64,
}

impl Default for AntiSpamConfig {
    fn default() -> Self {
        Self { enabled: true, max_per_sender: 64, max_bytes_per_sender: 65_536, peer_tx_per_min: 600, peer_invalid_per_min: 20, ban_secs: 3600, replace_by_fee: true, deny_peers: Vec::new(), alert_drops_per_min: 500 }
    }
}

/// Where a batch of transactions came from (peer limits apply to network origins only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxOrigin {
    /// REST API / wallet stream / operator — the node's own clients.
    Local,
    /// Legacy WebSocket peer (IP).
    LegacyPeer(String),
    /// Iroh gossip neighbour (EndpointId).
    IrohPeer(String),
}

impl TxOrigin {
    pub fn peer(&self) -> Option<&str> {
        match self {
            TxOrigin::Local => None,
            TxOrigin::LegacyPeer(p) | TxOrigin::IrohPeer(p) => Some(p),
        }
    }
}

#[derive(Debug, Clone)]
struct PeerWindow {
    started: Instant,
    accepted: u32,
    invalid: u32,
    banned_until: Option<Instant>,
    dropped: u64,
    bans: u32,
}

impl PeerWindow {
    fn new(now: Instant) -> Self {
        Self { started: now, accepted: 0, invalid: 0, banned_until: None, dropped: 0, bans: 0 }
    }

    fn roll(&mut self, now: Instant) {
        if now.duration_since(self.started) >= Duration::from_secs(60) {
            self.started = now;
            self.accepted = 0;
            self.invalid = 0;
        }
    }
}

/// Counters since start (metrics page `mempool.antispam`).
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AntiSpamCounters {
    pub dropped: u64,
    pub rate_limited: u64,
    pub sender_limited: u64,
    pub denied: u64,
    pub banned_dropped: u64,
    pub evicted: u64,
    pub bans: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BannedPeer {
    pub peer: String,
    pub seconds_left: u64,
    pub invalid: u32,
    pub dropped: u64,
    pub bans: u32,
}

pub struct AntiSpam {
    cfg: AntiSpamConfig,
    peers: Mutex<HashMap<String, PeerWindow>>,
    dropped: AtomicU64,
    rate_limited: AtomicU64,
    sender_limited: AtomicU64,
    denied: AtomicU64,
    banned_dropped: AtomicU64,
    evicted: AtomicU64,
    bans: AtomicU64,
    /// Drops in the current minute + whether the wave alert already fired for it.
    wave: Mutex<(Instant, u64, bool)>,
}

/// Outcome of the per-peer gate for a batch of `n` transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Admit the first `n` transactions of the batch, drop the rest.
    Allow(usize),
    Denied,
    Banned,
}

impl AntiSpam {
    pub fn new(cfg: AntiSpamConfig) -> Self {
        Self { cfg, peers: Default::default(), dropped: Default::default(), rate_limited: Default::default(), sender_limited: Default::default(), denied: Default::default(), banned_dropped: Default::default(), evicted: Default::default(), bans: Default::default(), wave: Mutex::new((Instant::now(), 0, false)) }
    }

    pub fn config(&self) -> &AntiSpamConfig {
        &self.cfg
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Per-peer gate for a batch of `n` transactions: deny list, active ban, then the per-minute budget.
    pub fn gate(&self, origin: &TxOrigin, n: usize) -> Gate {
        let Some(peer) = origin.peer().filter(|_| self.cfg.enabled) else { return Gate::Allow(n) };
        if self.cfg.deny_peers.iter().any(|d| d == peer) {
            self.denied.fetch_add(n as u64, Ordering::Relaxed);
            self.drop_n(n as u64);
            return Gate::Denied;
        }
        let now = Instant::now();
        let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        let w = peers.entry(peer.to_string()).or_insert_with(|| PeerWindow::new(now));
        if w.banned_until.is_some_and(|t| t > now) {
            w.dropped += n as u64;
            self.banned_dropped.fetch_add(n as u64, Ordering::Relaxed);
            self.drop_n(n as u64);
            return Gate::Banned;
        }
        w.banned_until = None;
        w.roll(now);
        let budget = (self.cfg.peer_tx_per_min.saturating_sub(w.accepted)) as usize;
        let allowed = n.min(budget);
        let over = n - allowed;
        w.accepted += allowed as u32;
        if over > 0 {
            w.dropped += over as u64;
            self.rate_limited.fetch_add(over as u64, Ordering::Relaxed);
            self.drop_n(over as u64);
            tracing::warn!(peer, dropped = over, per_min = self.cfg.peer_tx_per_min, "antispam: peer over its transaction budget");
        }
        Gate::Allow(allowed)
    }

    /// Does this rejection count against the peer's invalid budget? Only unforgeable faults: bad signature / bytes,
    /// wrong network, unsupported type. `ERR_BAD_NONCE` is NOT counted — honest peers relay out of order routinely.
    pub fn counts_as_invalid(code: &str) -> bool {
        matches!(code, "ERR_BAD_DATA" | "ERR_WRONG_NETWORK" | "ERR_UNKNOWN")
    }

    /// Book `invalid` rejected transactions from a peer; returns `true` when this crossed the ban threshold.
    pub fn note_invalid(&self, origin: &TxOrigin, invalid: u32) -> bool {
        if invalid == 0 {
            return false;
        }
        let Some(peer) = origin.peer().filter(|_| self.cfg.enabled) else { return false };
        let now = Instant::now();
        let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        let w = peers.entry(peer.to_string()).or_insert_with(|| PeerWindow::new(now));
        w.roll(now);
        w.invalid += invalid;
        if w.invalid >= self.cfg.peer_invalid_per_min && w.banned_until.is_none_or(|t| t <= now) {
            w.banned_until = Some(now + Duration::from_secs(self.cfg.ban_secs));
            w.bans += 1;
            self.bans.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(peer, invalid = w.invalid, ban_secs = self.cfg.ban_secs, "antispam: peer banned for invalid transactions");
            return true;
        }
        false
    }

    pub fn is_banned(&self, peer: &str) -> bool {
        let now = Instant::now();
        self.peers.lock().unwrap_or_else(|e| e.into_inner()).get(peer).is_some_and(|w| w.banned_until.is_some_and(|t| t > now))
    }

    /// Per-sender cap check: `pending` / `pending_bytes` = what the sender already holds in the pool.
    pub fn check_sender(&self, sender: &str, pending: usize, pending_bytes: usize, size: usize) -> std::result::Result<(), (String, String)> {
        if !self.cfg.enabled {
            return Ok(());
        }
        if pending >= self.cfg.max_per_sender {
            self.sender_limited.fetch_add(1, Ordering::Relaxed);
            self.drop_n(1);
            return Err(("ERR_SPAM_SENDER".into(), format!("Sender {sender} already has {pending} pending transactions in this pool (max {})", self.cfg.max_per_sender)));
        }
        if pending_bytes + size > self.cfg.max_bytes_per_sender {
            self.sender_limited.fetch_add(1, Ordering::Relaxed);
            self.drop_n(1);
            return Err(("ERR_SPAM_SENDER".into(), format!("Sender {sender} already holds {pending_bytes} pending bytes in this pool (max {})", self.cfg.max_bytes_per_sender)));
        }
        Ok(())
    }

    pub fn replace_by_fee(&self) -> bool {
        self.cfg.enabled && self.cfg.replace_by_fee
    }

    pub fn note_evicted(&self) {
        self.evicted.fetch_add(1, Ordering::Relaxed);
    }

    fn drop_n(&self, n: u64) {
        self.dropped.fetch_add(n, Ordering::Relaxed);
        if self.cfg.alert_drops_per_min == 0 {
            return;
        }
        let now = Instant::now();
        let mut w = self.wave.lock().unwrap_or_else(|e| e.into_inner());
        if now.duration_since(w.0) >= Duration::from_secs(60) {
            *w = (now, 0, false);
        }
        w.1 += n;
        if w.1 > self.cfg.alert_drops_per_min && !w.2 {
            w.2 = true;
            let msg = format!("SPAM WAVE: {} transactions dropped within a minute (limit {}) — check mempool.antispam on the metrics page", w.1, self.cfg.alert_drops_per_min);
            tracing::warn!("{msg}");
            crate::alerts::notify("spam-wave", msg, false);
        }
    }

    pub fn counters(&self) -> AntiSpamCounters {
        AntiSpamCounters {
            dropped: self.dropped.load(Ordering::Relaxed),
            rate_limited: self.rate_limited.load(Ordering::Relaxed),
            sender_limited: self.sender_limited.load(Ordering::Relaxed),
            denied: self.denied.load(Ordering::Relaxed),
            banned_dropped: self.banned_dropped.load(Ordering::Relaxed),
            evicted: self.evicted.load(Ordering::Relaxed),
            bans: self.bans.load(Ordering::Relaxed),
        }
    }

    /// Peers currently banned (metrics page).
    pub fn banned(&self) -> Vec<BannedPeer> {
        let now = Instant::now();
        let mut out: Vec<BannedPeer> = self
            .peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|(peer, w)| {
                let until = w.banned_until.filter(|t| *t > now)?;
                Some(BannedPeer { peer: peer.clone(), seconds_left: until.duration_since(now).as_secs(), invalid: w.invalid, dropped: w.dropped, bans: w.bans })
            })
            .collect();
        out.sort_by(|a, b| b.seconds_left.cmp(&a.seconds_left));
        out
    }

    /// Forget idle peer windows (called from the pool's housekeeping).
    pub fn gc(&self) {
        let now = Instant::now();
        self.peers.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, w| w.banned_until.is_some_and(|t| t > now) || now.duration_since(w.started) < Duration::from_secs(600));
    }
}
