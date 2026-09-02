//! Author: TechnoL0g
//!
//! Rate-limit aware node pool for the legacy REST API.
//!
//! Legacy nodes allow 300 requests / 60 s per IP (≈5 req/s) and answer 429 above that.
//! Every node in the pool gets its own budget:
//!   * spacing: at most `per_node_rps` requests per second (default 4 = 20 % headroom),
//!   * sliding window: at most `window_limit` requests per `window` (default 250 / 60 s),
//!   * 429 > node is parked for `ban_on_429` (default 60 s),
//!   * `max_failures` consecutive network / 5xx errors > node is parked for `ban_on_failures`.
//! `acquire()` hands out the earliest free slot across all eligible nodes, so concurrent
//! workers are automatically spread over the pool (6 nodes × 4 req/s ≈ 24 req/s aggregate).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub per_node_rps: u32,
    pub window_limit: usize,
    pub window: Duration,
    pub ban_on_429: Duration,
    pub max_failures: u32,
    pub ban_on_failures: Duration,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            per_node_rps: 4,
            window_limit: 250,
            window: Duration::from_secs(60),
            ban_on_429: Duration::from_secs(60),
            max_failures: 5,
            ban_on_failures: Duration::from_secs(30),
        }
    }
}

/// Hard API limit used for the status line (`Requests: 180/300`).
pub const API_WINDOW_LIMIT: usize = 300;

#[derive(Debug)]
struct NodeState {
    url: String,
    host: String,
    /// Timestamps of requests (incl. reserved future slots) inside the sliding window.
    window: VecDeque<Instant>,
    /// Earliest instant the next request to this node may be sent.
    next_slot: Instant,
    parked_until: Option<Instant>,
    failures: u32,
}

impl NodeState {
    fn prune(&mut self, now: Instant, window: Duration) {
        while let Some(front) = self.window.front() {
            if now.saturating_duration_since(*front) >= window {
                self.window.pop_front();
            } else {
                break;
            }
        }
    }

    fn parked(&self, now: Instant) -> bool {
        matches!(self.parked_until, Some(t) if t > now)
    }
}

#[derive(Debug)]
struct Inner {
    nodes: Vec<NodeState>,
    last_used: usize,
}

/// A reserved request slot on one node.
#[derive(Debug, Clone)]
pub struct Lease {
    pub index: usize,
    pub url: String,
    pub host: String,
    /// Sleep until this instant before sending.
    pub slot: Instant,
}

/// Snapshot for the progress status line.
#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub host: String,
    pub requests_in_window: usize,
    pub parked_nodes: usize,
}

pub struct NodePool {
    cfg: RateLimitConfig,
    inner: Mutex<Inner>,
}

impl NodePool {
    pub fn new(urls: &[String], cfg: RateLimitConfig) -> Self {
        let now = Instant::now();
        let nodes = urls
            .iter()
            .map(|u| NodeState {
                url: u.trim_end_matches('/').to_string(),
                host: host_of(u),
                window: VecDeque::new(),
                next_slot: now,
                parked_until: None,
                failures: 0,
            })
            .collect();
        Self { cfg, inner: Mutex::new(Inner { nodes, last_used: 0 }) }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|i| i.nodes.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reserve the earliest available request slot across the pool (waits if every node is
    /// parked or has exhausted its window).
    pub async fn acquire(&self) -> Lease {
        loop {
            let wait = {
                let now = Instant::now();
                let spacing = Duration::from_millis(1000 / self.cfg.per_node_rps.max(1) as u64);
                let mut inner = match self.inner.lock() {
                    Ok(i) => i,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let window = self.cfg.window;
                for n in inner.nodes.iter_mut() {
                    n.prune(now, window);
                }

                let mut best: Option<(usize, Instant)> = None;
                for (i, n) in inner.nodes.iter().enumerate() {
                    if n.parked(now) || n.window.len() >= self.cfg.window_limit {
                        continue;
                    }
                    let slot = n.next_slot.max(now);
                    if best.map_or(true, |(_, s)| slot < s) {
                        best = Some((i, slot));
                    }
                }

                match best {
                    Some((i, slot)) => {
                        inner.last_used = i;
                        let n = &mut inner.nodes[i];
                        n.next_slot = slot + spacing;
                        n.window.push_back(slot);
                        return Lease { index: i, url: n.url.clone(), host: n.host.clone(), slot };
                    }
                    None => {
                        // Everything is parked / exhausted: wait for the earliest release.
                        let mut earliest: Option<Instant> = None;
                        for n in &inner.nodes {
                            let mut candidates = Vec::with_capacity(2);
                            if let Some(p) = n.parked_until {
                                candidates.push(p);
                            }
                            if n.window.len() >= self.cfg.window_limit {
                                if let Some(front) = n.window.front() {
                                    candidates.push(*front + window);
                                }
                            }
                            for c in candidates {
                                earliest = Some(earliest.map_or(c, |e| e.min(c)));
                            }
                        }
                        earliest
                            .map(|e| e.saturating_duration_since(now))
                            .unwrap_or(Duration::from_secs(1))
                            .max(Duration::from_millis(50))
                    }
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    pub fn report_success(&self, lease: &Lease) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(n) = inner.nodes.get_mut(lease.index) {
                n.failures = 0;
            }
        }
    }

    /// 429 received: park the node. Returns the host that will be used next (for logging).
    pub fn report_rate_limited(&self, lease: &Lease) -> Option<String> {
        let mut inner = self.inner.lock().ok()?;
        let until = Instant::now() + self.cfg.ban_on_429;
        if let Some(n) = inner.nodes.get_mut(lease.index) {
            n.parked_until = Some(until);
            n.failures = 0;
        }
        Self::next_free_host(&inner, lease.index)
    }

    /// Network / 5xx error. Returns `(consecutive_failures, switched_to)`.
    pub fn report_failure(&self, lease: &Lease) -> (u32, Option<String>) {
        let mut inner = match self.inner.lock() {
            Ok(i) => i,
            Err(p) => p.into_inner(),
        };
        let mut switched = None;
        if let Some(n) = inner.nodes.get_mut(lease.index) {
            n.failures += 1;
            if n.failures >= self.cfg.max_failures {
                n.parked_until = Some(Instant::now() + self.cfg.ban_on_failures);
                n.failures = 0;
                switched = Self::next_free_host(&inner, lease.index);
                return (self.cfg.max_failures, switched);
            }
            return (n.failures, switched);
        }
        (0, switched)
    }

    pub fn status(&self) -> PoolStatus {
        let now = Instant::now();
        let mut inner = match self.inner.lock() {
            Ok(i) => i,
            Err(p) => p.into_inner(),
        };
        let window = self.cfg.window;
        let last = inner.last_used;
        for n in inner.nodes.iter_mut() {
            n.prune(now, window);
        }
        let parked = inner.nodes.iter().filter(|n| n.parked(now)).count();
        match inner.nodes.get(last) {
            Some(n) => PoolStatus { host: n.host.clone(), requests_in_window: n.window.len(), parked_nodes: parked },
            None => PoolStatus { host: "-".into(), requests_in_window: 0, parked_nodes: parked },
        }
    }

    fn next_free_host(inner: &Inner, after: usize) -> Option<String> {
        let now = Instant::now();
        let len = inner.nodes.len();
        (1..=len)
            .map(|k| &inner.nodes[(after + k) % len])
            .find(|n| !n.parked(now))
            .map(|n| n.host.clone())
    }
}

fn host_of(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(n: usize, cfg: RateLimitConfig) -> NodePool {
        let urls: Vec<String> = (0..n).map(|i| format!("https://node{i}.example")).collect();
        NodePool::new(&urls, cfg)
    }

    #[tokio::test]
    async fn spreads_requests_across_nodes_and_spaces_them() {
        let p = pool(2, RateLimitConfig { per_node_rps: 4, ..Default::default() });
        let a = p.acquire().await;
        let b = p.acquire().await;
        let c = p.acquire().await;
        assert_ne!(a.index, b.index);
        // third request goes back to the first node, 250 ms after its previous slot
        assert_eq!(c.index, a.index);
        assert!(c.slot >= a.slot + Duration::from_millis(250));
    }

    #[tokio::test]
    async fn window_limit_switches_node() {
        let p = pool(2, RateLimitConfig { window_limit: 2, per_node_rps: 1000, ..Default::default() });
        let leases: Vec<Lease> = vec![p.acquire().await, p.acquire().await, p.acquire().await, p.acquire().await];
        let on_node0 = leases.iter().filter(|l| l.index == 0).count();
        let on_node1 = leases.iter().filter(|l| l.index == 1).count();
        assert_eq!((on_node0, on_node1), (2, 2));
        // both windows full: the pool must now wait instead of handing out a slot immediately
        let started = Instant::now();
        let fut = p.acquire();
        let res = tokio::time::timeout(Duration::from_millis(200), fut).await;
        assert!(res.is_err(), "pool should block while every window is exhausted");
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    #[tokio::test]
    async fn rate_limited_node_is_parked_and_next_host_reported() {
        let p = pool(3, RateLimitConfig::default());
        let l = p.acquire().await;
        let next = p.report_rate_limited(&l).unwrap();
        assert_ne!(next, l.host);
        for _ in 0..10 {
            let other = p.acquire().await;
            assert_ne!(other.index, l.index, "parked node must not be used");
        }
        assert_eq!(p.status().parked_nodes, 1);
    }

    #[tokio::test]
    async fn five_failures_park_the_node() {
        let p = pool(2, RateLimitConfig::default());
        let l = p.acquire().await;
        for i in 1..5 {
            let (f, switched) = p.report_failure(&l);
            assert_eq!(f, i);
            assert!(switched.is_none());
        }
        let (f, switched) = p.report_failure(&l);
        assert_eq!(f, 5);
        assert!(switched.is_some());
        let next = p.acquire().await;
        assert_ne!(next.index, l.index);
    }

    #[test]
    fn host_parsing() {
        assert_eq!(host_of("https://node2.smartholdem.io/"), "node2.smartholdem.io");
        assert_eq!(host_of("http://127.0.0.1:4003"), "127.0.0.1:4003");
    }
}
