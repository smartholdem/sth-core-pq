//! Author: TechnoL0g
//!
//! Relay guard (`p2p.iroh.relay_guard`, optional module, on by default).
//!
//! iroh picks the home relay by latency only. A public n0 relay (`*.relay.n0.iroh.link`, rate-limited, "not for
//! production" per n0 docs) can win the ping race against the NETFORY n1 relays and then silently drop wallet
//! handshakes: the wallet reaches us through OUR home relay, our reply is throttled, the client times out — and iroh
//! never re-evaluates because latency looks fine. This module counts handshake timeouts per relay and, once a
//! non-protected relay exceeds `max_failures` within `window_secs`, removes it from the endpoint's relay map at runtime
//! (`Endpoint::remove_relay`, iroh ≥ 1.1) so the home relay migrates to n1. Protected = n1 + `p2p.iroh.relays`.
//! Observation only — consensus and the legacy layer are untouched; a disabled guard is a no-op.

use crate::node_config::RelayGuardConfig;
use iroh::endpoint::IncomingAddr;
use iroh::{Endpoint, RelayUrl};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovedRelay {
    pub url: String,
    pub failures: usize,
    pub at_unix: i64,
}

#[derive(Default)]
struct State {
    failures: HashMap<String, VecDeque<Instant>>,
    removed: Vec<RemovedRelay>,
}

pub struct RelayGuard {
    cfg: RelayGuardConfig,
    protected: HashSet<String>,
    endpoint: Option<Endpoint>,
    state: Mutex<State>,
}

static GUARD: OnceLock<RelayGuard> = OnceLock::new();

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn norm(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// `true` for the public n0 relays (`https://euc1-1.relay.n0.iroh.link./`, `use1-1…`).
pub fn is_n0_relay(url: &str) -> bool {
    url.trim_end_matches('/').trim_end_matches('.').ends_with(".relay.n0.iroh.link")
}

/// Install once after the endpoint is bound. `protected` = relay URLs that are never removed (n1 + node.yaml `relays`).
pub fn init(cfg: &RelayGuardConfig, protected: &[String], endpoint: Endpoint) {
    let _ = GUARD.set(RelayGuard::new(cfg.clone(), protected, Some(endpoint)));
    if let Some(g) = GUARD.get() {
        if g.cfg.enabled {
            tracing::info!(max_failures = g.cfg.max_failures, window_secs = g.cfg.window_secs, protected = g.protected.len(), "relay guard enabled");
        }
    }
}

/// An incoming connection through `remote` failed its handshake (wallet API / events accept hook).
pub fn note_handshake_failure(remote: &IncomingAddr) {
    let Some(g) = GUARD.get().filter(|g| g.cfg.enabled) else { return };
    if let IncomingAddr::Relay { url, .. } = remote {
        if let Some((url, failures)) = g.note_failure(&url.to_string(), Instant::now()) {
            g.remove(url, failures);
        }
    }
}

/// `{ enabled, maxFailures, windowSecs, failures: {url: n}, removed: [...] }` for the metrics page.
pub fn snapshot() -> serde_json::Value {
    match GUARD.get() {
        Some(g) => g.snapshot(),
        None => serde_json::json!({ "enabled": false }),
    }
}

impl RelayGuard {
    pub fn new(cfg: RelayGuardConfig, protected: &[String], endpoint: Option<Endpoint>) -> Self {
        Self { cfg, protected: protected.iter().map(|u| norm(u)).collect(), endpoint, state: Mutex::new(State::default()) }
    }

    fn window(&self) -> Duration {
        Duration::from_secs(self.cfg.window_secs.max(1))
    }

    /// Record one failure; `Some((url, failures))` when the relay crossed the threshold and must be removed.
    pub fn note_failure(&self, url: &str, now: Instant) -> Option<(String, usize)> {
        let url = norm(url);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.removed.iter().any(|r| r.url == url) {
            return None;
        }
        let window = self.window();
        let q = st.failures.entry(url.clone()).or_default();
        q.push_back(now);
        while q.front().is_some_and(|t| now.duration_since(*t) > window) {
            q.pop_front();
        }
        let n = q.len();
        if n >= self.cfg.max_failures.max(1) && !self.protected.contains(&url) {
            st.removed.push(RemovedRelay { url: url.clone(), failures: n, at_unix: now_unix() });
            return Some((url, n));
        }
        if self.protected.contains(&url) && n >= self.cfg.max_failures.max(1) && n % self.cfg.max_failures.max(1) == 0 {
            tracing::warn!(relay = %url, failures = n, "relay guard: handshakes keep timing out through a PROTECTED relay (n1 / node.yaml relays) — check that relay server");
        }
        None
    }

    fn remove(&self, url: String, failures: usize) {
        let msg = format!(
            "relay guard: {failures} wallet handshakes timed out through relay {url} within {}s — removing it from the relay map so the home relay moves to {}",
            self.cfg.window_secs,
            if self.protected.is_empty() { "another relay".to_string() } else { self.protected.iter().cloned().collect::<Vec<_>>().join(", ") }
        );
        tracing::warn!("{msg}");
        crate::alerts::notify("relay-degraded", msg, false);
        let Some(endpoint) = self.endpoint.clone() else { return };
        tokio::spawn(async move {
            match format!("{url}/").parse::<RelayUrl>() {
                Ok(relay) => {
                    let removed = endpoint.remove_relay(&relay).await.is_some();
                    tracing::info!(relay = %url, removed, "relay guard: relay map updated (home relay is re-selected by iroh)");
                }
                Err(e) => tracing::warn!(relay = %url, error = %e, "relay guard: cannot parse relay url"),
            }
        });
    }

    pub fn snapshot(&self) -> serde_json::Value {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let window = self.window();
        let failures: serde_json::Map<String, serde_json::Value> = st
            .failures
            .iter()
            .map(|(u, q)| (u.clone(), serde_json::json!(q.iter().filter(|t| now.duration_since(**t) <= window).count())))
            .filter(|(_, n)| n.as_u64().unwrap_or(0) > 0)
            .collect();
        serde_json::json!({
            "enabled": self.cfg.enabled,
            "maxFailures": self.cfg.max_failures,
            "windowSecs": self.cfg.window_secs,
            "protected": self.protected.iter().collect::<Vec<_>>(),
            "failures": failures,
            "removed": st.removed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N0: &str = "https://euc1-1.relay.n0.iroh.link./";
    const N1: &str = "https://relay-fsn7.sth.cx";

    fn guard(max: usize, window: u64) -> RelayGuard {
        RelayGuard::new(RelayGuardConfig { enabled: true, max_failures: max, window_secs: window }, &[N1.to_string()], None)
    }

    #[test]
    fn n0_relay_is_removed_after_threshold_and_only_once() {
        let g = guard(3, 600);
        let t = Instant::now();
        assert!(g.note_failure(N0, t).is_none());
        assert!(g.note_failure(N0, t + Duration::from_secs(1)).is_none());
        let hit = g.note_failure(N0, t + Duration::from_secs(2));
        assert_eq!(hit, Some(("https://euc1-1.relay.n0.iroh.link.".to_string(), 3)));
        assert!(g.note_failure(N0, t + Duration::from_secs(3)).is_none(), "already removed → no second removal");
        let s = g.snapshot();
        assert_eq!(s["removed"].as_array().map(|a| a.len()), Some(1));
        assert_eq!(s["removed"][0]["failures"], 3);
    }

    #[test]
    fn protected_relay_is_never_removed_and_old_failures_expire() {
        let g = guard(2, 60);
        let t = Instant::now();
        assert!(g.note_failure(N1, t).is_none());
        assert!(g.note_failure(N1, t + Duration::from_secs(1)).is_none(), "n1 is protected");
        assert!(g.note_failure(&format!("{N1}/"), t + Duration::from_secs(2)).is_none());
        assert!(g.snapshot()["removed"].as_array().is_some_and(|a| a.is_empty()));

        let g = guard(2, 60);
        assert!(g.note_failure(N0, t).is_none());
        // the first failure is older than the window → the counter restarts
        assert!(g.note_failure(N0, t + Duration::from_secs(120)).is_none());
        assert!(g.note_failure(N0, t + Duration::from_secs(121)).is_some());
    }

    #[test]
    fn n0_detection() {
        assert!(is_n0_relay("https://euc1-1.relay.n0.iroh.link./"));
        assert!(is_n0_relay("https://use1-1.relay.n0.iroh.link"));
        assert!(!is_n0_relay("https://relay-fsn7.sth.cx/"));
    }
}
