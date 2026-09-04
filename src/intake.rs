//! Author: TechnoL0g
//!
//! Block intake statistics for the metrics page: which channel delivered the last block
//! (pull/legacy, push/legacy, gossip/iroh, forged) and how long after its slot started it arrived.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    PullLegacy,
    PushLegacy,
    GossipIroh,
    Forged,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::PullLegacy => "pull/legacy",
            Source::PushLegacy => "push/legacy",
            Source::GossipIroh => "gossip/iroh",
            Source::Forged => "forged",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastBlock {
    pub height: u64,
    pub source: Source,
    pub label: &'static str,
    pub peer: String,
    /// Milliseconds between the block's slot start and its arrival here.
    pub delay_ms: i64,
}

#[derive(Default)]
struct Intake {
    last: Option<LastBlock>,
    delays: VecDeque<(Source, i64)>,
}

const WINDOW: usize = 100;

fn intake() -> &'static Mutex<Intake> {
    static I: OnceLock<Mutex<Intake>> = OnceLock::new();
    I.get_or_init(Default::default)
}

/// Record a block that just entered the chain live (catch-up batches are not counted).
pub fn record(source: Source, peer: impl Into<String>, height: u64, block_epoch: u32, network: &crate::config::Network) {
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let delay_ms = now_ms - network.epoch_to_unix(block_epoch) * 1000;
    let mut i = intake().lock().unwrap_or_else(|e| e.into_inner());
    i.last = Some(LastBlock { height, source, label: source.label(), peer: peer.into(), delay_ms });
    if i.delays.len() == WINDOW {
        i.delays.pop_front();
    }
    i.delays.push_back((source, delay_ms));
}

/// `{ last, avgDelayMs, samples, bySource: { label: { count, avgDelayMs } } }`
pub fn snapshot() -> serde_json::Value {
    let i = intake().lock().unwrap_or_else(|e| e.into_inner());
    let avg = |xs: &[i64]| if xs.is_empty() { None } else { Some(xs.iter().sum::<i64>() / xs.len() as i64) };
    let all: Vec<i64> = i.delays.iter().map(|(_, d)| *d).collect();
    let mut by = serde_json::Map::new();
    for s in [Source::PullLegacy, Source::PushLegacy, Source::GossipIroh, Source::Forged] {
        let xs: Vec<i64> = i.delays.iter().filter(|(src, _)| *src == s).map(|(_, d)| *d).collect();
        if !xs.is_empty() {
            by.insert(s.label().into(), serde_json::json!({ "count": xs.len(), "avgDelayMs": avg(&xs) }));
        }
    }
    serde_json::json!({ "last": i.last, "avgDelayMs": avg(&all), "samples": all.len(), "bySource": by })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_last_block_and_averages_per_source() {
        let net = crate::config::Network::mainnet();
        // a block whose slot started 30 s ago → delay ≈ 30 000 ms
        let ts = net.now_epoch() - 30;
        record(Source::PullLegacy, "1.2.3.4", 100, ts, &net);
        record(Source::GossipIroh, "abcd", 101, ts, &net);
        let s = snapshot();
        assert_eq!(s["last"]["height"], 101);
        assert_eq!(s["last"]["label"], "gossip/iroh");
        assert_eq!(s["last"]["source"], "gossip-iroh");
        let d = s["last"]["delayMs"].as_i64().unwrap();
        assert!((29_000..32_000).contains(&d), "{d}");
        assert!(s["bySource"]["pull/legacy"]["count"].as_u64().unwrap() >= 1);
        assert!(s["samples"].as_u64().unwrap() >= 2);
    }
}
