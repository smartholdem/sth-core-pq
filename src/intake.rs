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

/// Where a live block first came from (used to infer which core a delegate runs).
#[derive(Debug, Clone)]
pub struct Origin {
    pub height: u64,
    pub source: Source,
    pub peer: String,
    pub generator: String,
}

#[derive(Default)]
struct Intake {
    last: Option<LastBlock>,
    delays: VecDeque<(Source, i64)>,
    origins: VecDeque<Origin>,
}

const WINDOW: usize = 100;
/// 10 rounds of 21 blocks — every active delegate is observed several times.
const ORIGINS: usize = 210;

fn intake() -> &'static Mutex<Intake> {
    static I: OnceLock<Mutex<Intake>> = OnceLock::new();
    I.get_or_init(Default::default)
}

/// Record a block that just entered the chain live (catch-up batches are not counted).
pub fn record(source: Source, peer: impl Into<String>, height: u64, block_epoch: u32, generator: &str, network: &crate::config::Network) {
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let delay_ms = now_ms - network.epoch_to_unix(block_epoch) * 1000;
    let peer = peer.into();
    let mut i = intake().lock().unwrap_or_else(|e| e.into_inner());
    i.last = Some(LastBlock { height, source, label: source.label(), peer: peer.clone(), delay_ms });
    if i.delays.len() == WINDOW {
        i.delays.pop_front();
    }
    i.delays.push_back((source, delay_ms));
    if i.origins.len() == ORIGINS {
        i.origins.pop_front();
    }
    i.origins.push_back(Origin { height, source, peer, generator: generator.to_string() });
}

/// Live block origins, oldest first (up to 10 rounds).
pub fn origins() -> Vec<Origin> {
    intake().lock().unwrap_or_else(|e| e.into_inner()).origins.iter().cloned().collect()
}

/// Per generator: (blocks observed, all of them came through legacy peers that are not Rust gateways).
/// `rust_ws` = legacy-protocol IPs known to be Rust nodes (gateways announced over iroh, this node).
pub fn legacy_evidence(rust_ws: &std::collections::HashSet<String>) -> std::collections::HashMap<String, (u32, bool)> {
    let mut seen: std::collections::HashMap<String, (u32, bool)> = Default::default();
    for o in origins() {
        let via_legacy = matches!(o.source, Source::PullLegacy | Source::PushLegacy) && !rust_ws.contains(&o.peer);
        let e = seen.entry(o.generator).or_insert((0, true));
        e.0 += 1;
        e.1 &= via_legacy;
    }
    seen
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

/// Why the chain stopped receiving blocks (`note_stall_reason`): the last diagnosis made by the follow loop or the
/// stall watchdog, shown on the metrics page as "last stall cause".
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StallReason {
    /// `lagging-peer` | `fork` | `pull-broken` | `iroh-pull-failed` | `no-legacy-peers` | `rollback-failed`
    pub kind: &'static str,
    /// Legacy IP or iroh id the diagnosis points at (empty when none).
    pub peer: String,
    pub detail: String,
    /// Local tip at the time.
    pub height: u64,
    pub at_unix: i64,
}

/// The previous stall, kept after recovery so operators see what happened while they were away.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastStall {
    pub height: u64,
    pub seconds: u64,
    pub network_height: u64,
    pub recovered_height: u64,
    pub ended_at_unix: i64,
}

/// Tip stuck while peers are ahead (`spawn_stall_watchdog`); shown on the metrics page and in `/api/ntfry/metrics`.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stall {
    pub stalled: bool,
    /// Local tip that has not moved.
    pub height: u64,
    /// Seconds since the tip last moved.
    pub seconds: u64,
    /// Highest height reported by peers.
    pub network_height: u64,
    pub peers_ahead: usize,
    /// How many times the watchdog fired since start.
    pub alerts: u64,
    pub last_reason: Option<StallReason>,
    pub last_stall: Option<LastStall>,
}

fn stall_slot() -> &'static Mutex<Stall> {
    static S: OnceLock<Mutex<Stall>> = OnceLock::new();
    S.get_or_init(Default::default)
}

pub fn stall() -> Stall {
    stall_slot().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Record why blocks stopped arriving (follow loop / watchdog diagnosis). `peer` = legacy IP or iroh id, may be empty.
pub fn note_stall_reason(kind: &'static str, peer: impl Into<String>, detail: impl Into<String>, height: u64) {
    let mut s = stall_slot().lock().unwrap_or_else(|e| e.into_inner());
    s.last_reason = Some(StallReason { kind, peer: peer.into(), detail: detail.into(), height, at_unix: unix_now() });
}

/// The stall ended: remember it (`lastStall`) for the metrics page.
pub fn note_stall_recovered(height: u64, seconds: u64, network_height: u64, recovered_height: u64) {
    let mut s = stall_slot().lock().unwrap_or_else(|e| e.into_inner());
    s.last_stall = Some(LastStall { height, seconds, network_height, recovered_height, ended_at_unix: unix_now() });
}

/// Slots the tip may stand still (while peers are ahead) before the watchdog fires.
pub const STALL_SLOTS: u64 = 3;

/// Legacy peers this many blocks (two rounds) behind us while we keep forging = they are on another branch.
pub const SPLIT_BLOCKS: u64 = 42;
/// How long the picture must persist before the split alarm fires.
pub const SPLIT_AFTER_SECS: u64 = 300;

/// Network-split detector: ≥ 3 answering legacy peers, ≥ 80 % of them (and their best) more than `SPLIT_BLOCKS` behind
/// our tip → `Some((peers_behind, their_best_height))`. The opposite of a stall: *we* are ahead of the legacy network,
/// which means the legacy nodes reject our branch (fork recovery on their side failed).
pub fn legacy_split(tip: u64, legacy_heights: &[u64]) -> Option<(usize, u64)> {
    let answering: Vec<u64> = legacy_heights.iter().copied().filter(|h| *h > 0).collect();
    if answering.len() < 3 {
        return None;
    }
    let behind = answering.iter().filter(|h| **h + SPLIT_BLOCKS < tip).count();
    let best = answering.iter().copied().max().unwrap_or(0);
    (behind * 5 >= answering.len() * 4 && best + SPLIT_BLOCKS < tip).then_some((behind, best))
}

/// One watchdog step: `tip` = local height, `ahead` = (peers ahead of it, their best height). Returns `true` when the
/// alert fires (tip unchanged for ≥ `STALL_SLOTS` slots with peers ahead) — the caller logs and lifts peer parking.
pub fn stall_check(tip: u64, ahead: (usize, u64), blocktime: u64, now: std::time::Instant, last_move: &mut (u64, std::time::Instant)) -> bool {
    if tip != last_move.0 {
        *last_move = (tip, now);
    }
    let seconds = now.duration_since(last_move.1).as_secs();
    let stalled = ahead.0 > 0 && seconds >= STALL_SLOTS * blocktime.max(1);
    let mut s = stall_slot().lock().unwrap_or_else(|e| e.into_inner());
    let fired = stalled && !s.stalled;
    if stalled != s.stalled || stalled {
        s.stalled = stalled;
        s.height = tip;
        s.seconds = seconds;
        s.network_height = ahead.1;
        s.peers_ahead = ahead.0;
        s.alerts += u64::from(fired);
    } else {
        s.seconds = 0;
    }
    fired
}

/// `(our tip id, the network's id at the same height, the legacy peer that answered)`: `getBlocks(tip)` returns block
/// tip+1 whose `previousBlock` is the network's block at our height.
async fn branch_check(storage: &crate::storage::Storage, peers: &crate::p2p_legacy::PeerTable) -> Option<(String, String, String)> {
    let tip = storage.get_last_block().ok().flatten()?;
    let ours = tip.id.clone()?;
    for ip in peers.best_for_blocks(3) {
        let Ok(mut peer) = crate::p2p_legacy::LegacyPeer::connect(&ip, peers.port(), std::time::Duration::from_secs(10)).await else { continue };
        let Ok(blocks) = peer.get_blocks(tip.height, 1).await else { continue };
        if let Some(next) = blocks.iter().find(|b| b.height == tip.height + 1) {
            return Some((ours, next.previous_block.clone(), ip));
        }
    }
    None
}

/// Every blocktime: if the tip has not moved for `STALL_SLOTS` slots while legacy / iroh peers report a higher height,
/// log ERROR (once per stall) and lift the block-source parking so the follow loop can pull again. When the stall persists
/// one more slot, pull the gap directly from the iroh peers that are ahead (`fill_gap_from`).
pub fn spawn_stall_watchdog(storage: std::sync::Arc<crate::storage::Storage>, peers: std::sync::Arc<crate::p2p_legacy::PeerTable>, iroh: Option<std::sync::Arc<crate::p2p_iroh::IrohNode>>, verify: bool) {
    tokio::spawn(async move {
        let mut last_move = (0u64, std::time::Instant::now());
        let mut last_error = None::<std::time::Instant>;
        let mut was_stalled = false;
        let mut stalled_since = None::<(u64, std::time::Instant)>;
        let mut fork_depth = 1u64;
        let mut split_since = None::<std::time::Instant>;
        let mut split_alerted = None::<std::time::Instant>;
        loop {
            let tip = storage.get_last_height().unwrap_or(0);
            let blocktime = u64::from(storage.network().milestone(tip.max(1)).blocktime);
            tokio::time::sleep(std::time::Duration::from_secs(blocktime.max(1))).await;
            let now = std::time::Instant::now();
            let legacy_ahead = peers.snapshot().iter().filter(|p| !p.is_banned(now) && p.height > tip).count();
            let iroh_ahead = iroh.as_ref().map(|n| n.peers.snapshot().iter().filter(|p| p.height > tip).count()).unwrap_or(0);
            let best = peers.best_height().max(iroh.as_ref().map(|n| n.peers.best_height()).unwrap_or(0));
            let ahead = (legacy_ahead + iroh_ahead, best);
            let fired = stall_check(tip, ahead, blocktime, now, &mut last_move);
            let s = stall();
            // the mirror image of a stall: every legacy peer is far behind us and refuses our blocks — network split
            let legacy_heights: Vec<u64> = peers.snapshot().iter().filter(|p| !p.is_banned(now)).map(|p| p.height).collect();
            match legacy_split(tip, &legacy_heights) {
                Some((behind, best)) => {
                    let since = *split_since.get_or_insert(now);
                    if now.duration_since(since).as_secs() >= SPLIT_AFTER_SECS && split_alerted.map_or(true, |t: std::time::Instant| now.duration_since(t).as_secs() >= 1800) {
                        split_alerted = Some(now);
                        let msg = format!(
                            "LEGACY NETWORK SPLIT: {behind}/{} legacy peers are stuck at ≤ {} while this node forges at {} ({} blocks ahead for {}m) — the legacy nodes reject our branch; they need a rollback below the fork point (`snapshot:rollback`) and a resync from PQ peers, or the PQ nodes must return to the legacy branch",
                            legacy_heights.iter().filter(|h| **h > 0).count(), crate::delegate::forger::group(best), crate::delegate::forger::group(tip), crate::delegate::forger::group(tip - best), now.duration_since(since).as_secs() / 60
                        );
                        tracing::error!("{msg}");
                        note_stall_reason("legacy-split", "", format!("{behind} legacy peers at ≤ {} while we are at {}", crate::delegate::forger::group(best), crate::delegate::forger::group(tip)), tip);
                        crate::alerts::notify("split", msg, false);
                    }
                }
                None => split_since = None,
            }
            // repeat the ERROR every 10 slots while the stall lasts, so operators' log alerts keep firing
            if s.stalled && (fired || last_error.is_none_or(|t| now.duration_since(t).as_secs() >= 10 * blocktime)) {
                let unparked = peers.unpark_blocks();
                let msg = format!(
                    "CHAIN STALLED: tip {} unchanged for {}s while {} peer(s) report up to {} ({} behind) — block-source parking lifted for {} peer(s); check legacy/iroh connectivity",
                    crate::delegate::forger::group(tip),
                    s.seconds,
                    s.peers_ahead,
                    crate::delegate::forger::group(s.network_height),
                    crate::delegate::forger::group(s.network_height.saturating_sub(tip)),
                    unparked
                );
                tracing::error!("{msg}");
                crate::alerts::notify("stall", msg, fired);
                last_error = Some(now);
            } else if !s.stalled {
                last_error = None;
            }
            // auto-recovery: the follow loop had a full slot after unparking; if the tip is still stuck, pull from iroh peers ahead
            if s.stalled && !fired {
                // 1) is our tip block even on the network's branch? (two PQ Nodes feeding each other an orphan block look
                //    exactly like this: same height, same id, every legacy peer ahead)
                match branch_check(&storage, &peers).await {
                    Some((ours, theirs, ip)) if ours != theirs => {
                        let target = tip.saturating_sub(fork_depth).max(1);
                        let msg = format!(
                            "stall recovery: local tip {} ({}) is NOT on the network branch (network has {}) — rolling back to {} so the follow loop can resync",
                            crate::delegate::forger::group(tip), &ours[..ours.len().min(10)], &theirs[..theirs.len().min(10)], crate::delegate::forger::group(target)
                        );
                        tracing::warn!("{msg}");
                        note_stall_reason("fork", &ip, format!("our block {} at {} is not on the network branch (peer {ip} has {})", &ours[..ours.len().min(10)], crate::delegate::forger::group(tip), &theirs[..theirs.len().min(10)]), tip);
                        let st = storage.clone();
                        match tokio::task::spawn_blocking(move || st.rollback_to(target)).await {
                            Ok(Ok(h)) => {
                                crate::alerts::notify("fork", format!("{msg} (rolled back to {})", crate::delegate::forger::group(h)), false);
                                fork_depth = (fork_depth * 3).min(crate::storage::UNDO_DEPTH);
                                continue;
                            }
                            Ok(Err(e)) => {
                                let m = format!("stall recovery: rollback to {} FAILED ({e}) — undo records missing or SHIP-35 hard finality; delete the database and resync from snapshot", crate::delegate::forger::group(target));
                                tracing::error!("{m}");
                                note_stall_reason("rollback-failed", &ip, format!("rollback to {} failed: {e}", crate::delegate::forger::group(target)), tip);
                                crate::alerts::notify("resync", m, false);
                            }
                            Err(e) => tracing::error!(error = %e, "stall recovery: rollback task failed"),
                        }
                    }
                    Some((_, _, ip)) => {
                        tracing::warn!(tip = crate::delegate::forger::group(tip), "stall recovery: tip is on the network branch but no block arrives — pull path broken, trying iroh peers");
                        note_stall_reason("pull-broken", &ip, format!("tip {} is on the network branch (confirmed by {ip}) but the follow loop delivered no block for {}s", crate::delegate::forger::group(tip), s.seconds), tip);
                    }
                    None => {
                        tracing::debug!("stall recovery: no legacy peer answered the branch check");
                        note_stall_reason("no-legacy-peers", "", format!("no legacy peer answered getBlocks({}) — legacy connectivity lost", crate::delegate::forger::group(tip)), tip);
                    }
                }
                // 2) pull directly from iroh peers ahead
                if let Some(node) = &iroh {
                    let mut ahead: Vec<_> = node.peers.snapshot().into_iter().filter(|p| p.height > tip && p.failures < 3).collect();
                    ahead.sort_by_key(|p| p.latency_ms.unwrap_or(u64::MAX));
                    for p in ahead.into_iter().take(3) {
                        match crate::p2p_iroh::fill_gap_from(node, &storage, p.id, p.height, verify).await {
                            Ok(t) if t.height > tip => {
                                tracing::warn!(from = tip, to = t.height, peer = %p.id.fmt_short(), "stall recovery: gap pulled from iroh peer");
                                break;
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::debug!(peer = %p.id.fmt_short(), error = %e, "stall recovery: iroh pull failed");
                                note_stall_reason("iroh-pull-failed", p.id.fmt_short().to_string(), format!("iroh peer at {} did not deliver the gap: {e}", crate::delegate::forger::group(p.height)), tip);
                            }
                        }
                    }
                }
            }
            if s.stalled && stalled_since.is_none() {
                stalled_since = Some((tip, now));
            }
            if was_stalled && !s.stalled {
                fork_depth = 1;
                let (h0, t0) = stalled_since.take().unwrap_or((tip, now));
                let secs = now.duration_since(t0).as_secs();
                let msg = format!("chain recovered: tip {} → {} after {}s", crate::delegate::forger::group(h0), crate::delegate::forger::group(tip), secs);
                tracing::warn!("{msg}");
                note_stall_recovered(h0, secs, best, tip);
                crate::alerts::notify("stall", msg, true);
            }
            was_stalled = s.stalled;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_last_block_and_averages_per_source() {
        let net = crate::config::Network::mainnet();
        // a block whose slot started 30 s ago → delay ≈ 30 000 ms
        let ts = net.now_epoch() - 30;
        record(Source::PullLegacy, "1.2.3.4", 100, ts, "02aa", &net);
        record(Source::GossipIroh, "abcd", 101, ts, "02bb", &net);
        let o = origins();
        assert!(o.len() >= 2);
        let last = o.last().unwrap();
        assert_eq!((last.height, last.generator.as_str(), last.peer.as_str()), (101, "02bb", "abcd"));
        record(Source::PullLegacy, "5.6.7.8", 102, ts, "02aa", &net);
        record(Source::PushLegacy, "9.9.9.9", 103, ts, "02cc", &net); // 9.9.9.9 is a Rust gateway
        let rust_ws: std::collections::HashSet<String> = ["9.9.9.9".to_string()].into();
        let ev = legacy_evidence(&rust_ws);
        assert_eq!(ev["02aa"], (2, true), "two blocks, both from plain legacy peers → legacy evidence");
        assert_eq!(ev["02bb"], (1, false), "arrived over iroh gossip → not legacy");
        assert_eq!(ev["02cc"], (1, false), "arrived from a Rust gateway over the legacy protocol → not legacy");
        let s = snapshot();
        assert_eq!(s["last"]["height"], 103);
        assert_eq!(s["last"]["label"], "push/legacy");
        let d = s["last"]["delayMs"].as_i64().unwrap();
        assert!((29_000..32_000).contains(&d), "{d}");
        assert!(s["bySource"]["pull/legacy"]["count"].as_u64().unwrap() >= 1);
        assert!(s["samples"].as_u64().unwrap() >= 2);
    }

    /// The stall state is a process-wide static: tests touching it run one at a time.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static L: Mutex<()> = Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn stall_watchdog_fires_once_after_three_slots_with_peers_ahead() {
        use std::time::{Duration, Instant};
        let _g = serial();
        let a0 = stall().alerts;
        let t0 = Instant::now();
        let mut lm = (0u64, t0);
        assert!(!stall_check(100, (5, 110), 8, t0, &mut lm), "first sample only records the tip");
        assert!(!stall_check(100, (5, 110), 8, t0 + Duration::from_secs(16), &mut lm), "2 slots: not yet");
        assert!(stall_check(100, (5, 110), 8, t0 + Duration::from_secs(24), &mut lm), "3 slots with peers ahead → fires");
        let s = stall();
        assert!(s.stalled && s.height == 100 && s.peers_ahead == 5 && s.network_height == 110 && s.seconds == 24, "{s:?}");
        assert!(!stall_check(100, (5, 111), 8, t0 + Duration::from_secs(32), &mut lm), "still stalled, alert not repeated");
        assert_eq!(stall().alerts, a0 + 1);
        // nobody ahead (we are the tip of a quiet network) → not a stall even if the tip stands still
        assert!(!stall_check(100, (0, 100), 8, t0 + Duration::from_secs(100), &mut lm));
        assert!(!stall().stalled);
        // tip moves → cleared
        assert!(!stall_check(101, (5, 111), 8, t0 + Duration::from_secs(101), &mut lm));
        assert!(!stall().stalled && stall().seconds == 0);
    }

    #[test]
    fn legacy_split_fires_only_when_the_legacy_majority_is_far_behind() {
        // the user's incident: 15 legacy peers at 11,926,268 while we forge at 11,927,024
        let heights = vec![11_926_268u64; 15];
        assert_eq!(legacy_split(11_927_024, &heights), Some((15, 11_926_268)));
        // normal operation: peers at or around our tip
        assert_eq!(legacy_split(11_927_024, &[11_927_024, 11_927_023, 11_927_024, 11_927_022]), None);
        // one lagging peer is not a split (< 80 %)
        assert_eq!(legacy_split(11_927_024, &[11_927_024, 11_927_024, 11_927_024, 11_926_000]), None);
        // exactly two rounds behind is still tolerated, one more block is not
        assert_eq!(legacy_split(1_000, &[958, 958, 958]), None);
        assert_eq!(legacy_split(1_000, &[957, 957, 957]), Some((3, 957)));
        // too few answering peers → no verdict (peers with height 0 never answered)
        assert_eq!(legacy_split(1_000, &[1, 1, 0, 0]), None);
        // 4 of 5 behind, but the best one is with us → not a split (we are on the network branch)
        assert_eq!(legacy_split(1_000, &[1, 1, 1, 1, 1_000]), None);
    }

    #[test]
    fn stall_reason_and_last_stall_survive_watchdog_updates() {
        use std::time::{Duration, Instant};
        let _g = serial();
        note_stall_reason("lagging-peer", "10.0.0.7", "followed peer stuck at 100 while the network is at 103", 100);
        let t0 = Instant::now();
        let mut lm = (0u64, t0);
        stall_check(100, (5, 103), 8, t0, &mut lm);
        assert!(stall_check(100, (5, 103), 8, t0 + Duration::from_secs(24), &mut lm));
        let s = stall();
        let r = s.last_reason.as_ref().expect("reason kept across stall_check");
        assert_eq!((r.kind, r.peer.as_str(), r.height), ("lagging-peer", "10.0.0.7", 100));
        assert!(r.at_unix > 1_700_000_000);
        note_stall_recovered(100, 32, 104, 104);
        assert!(!stall_check(104, (0, 104), 8, t0 + Duration::from_secs(33), &mut lm));
        let s = stall();
        let l = s.last_stall.as_ref().unwrap();
        assert_eq!((l.height, l.seconds, l.network_height, l.recovered_height), (100, 32, 104, 104));
        assert_eq!(s.last_reason.as_ref().unwrap().kind, "lagging-peer", "the cause stays until the next diagnosis");
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["lastReason"]["kind"], "lagging-peer");
        assert_eq!(v["lastStall"]["recoveredHeight"], 104);
    }
}
