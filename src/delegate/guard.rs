//! Author: TechnoL0g
//!
//! Double-forge guard (`delegate.guard`, optional module, on by default).
//!
//! A delegate passphrase that is loaded on two nodes at once (typically a legacy node and a PQ node during the
//! legacy → PQ transition) produces two different blocks for the same slot. Both branches are valid, the network
//! splits and only a snapshot/rollback brings it back. This module never touches consensus; it only observes:
//!
//! 1. **Detection** — a block signed by one of OUR delegate keys that reached us from the network (we did not forge
//!    it) means the key is active on another node; a second block at the same height signed by the same generator
//!    (any delegate) is a double forge. Both are logged at ERROR and pushed through `alerts` (Telegram / webhook).
//! 2. **Yield** (`mode: yield`, default) — while foreign blocks by our key keep arriving (last `rounds` rounds), the
//!    forger skips that key's slots instead of double-forging. The other node keeps producing the blocks, the chain
//!    stays whole; once it stops (the operator removed the key there) forging resumes automatically.
//!    `mode: alert` only reports.
//!
//! Everything is best effort and lock-free for the hot path apart from a small mutex; a disabled guard is a no-op.

use crate::intake::Source;
use crate::models::Block;
use crate::node_config::{DoubleForgeGuardConfig, GuardMode};
use crate::storage::Storage;
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

/// A block signed by one of our keys that another node forged.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignBlock {
    pub height: u64,
    pub id: String,
    pub generator: String,
    pub username: String,
    pub peer: String,
    pub source: Source,
    pub at_unix: i64,
}

/// A valid block by another delegate at a height we already have — the network forked there.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkSeen {
    pub height: u64,
    pub ours: String,
    pub our_generator: String,
    pub theirs: String,
    pub their_generator: String,
    pub peer: String,
    pub at_unix: i64,
}


/// Two different blocks at one height signed by the same generator.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompetingBlock {
    pub height: u64,
    pub ours: String,
    pub theirs: String,
    pub generator: String,
    pub username: String,
    pub peer: String,
    pub own_key: bool,
    pub at_unix: i64,
}

#[derive(Default)]
struct State {
    foreign: VecDeque<ForeignBlock>,
    competing: VecDeque<CompetingBlock>,
    /// Valid blocks by ANOTHER delegate at a height we already have (network fork at that height).
    forks: VecDeque<ForkSeen>,
    /// Ids of the blocks this node forged: a re-pulled own block (after an orphan rollback) is not "foreign".
    own_forged: VecDeque<String>,
    yielded_slots: u64,
}

const KEEP: usize = 50;
const OWN_IDS: usize = 256;

pub struct Guard {
    cfg: DoubleForgeGuardConfig,
    own_keys: HashSet<String>,
    storage: Option<Arc<Storage>>,
    state: Mutex<State>,
}

static GUARD: OnceLock<Guard> = OnceLock::new();

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// Install once at node start. `own_keys` = public keys of the delegates forging on THIS node (empty for a relay:
/// competing-block detection still works, the yield logic has nothing to protect).
pub fn init(cfg: &DoubleForgeGuardConfig, own_keys: Vec<String>, storage: Arc<Storage>) {
    let _ = GUARD.set(Guard::new(cfg.clone(), own_keys, Some(storage)));
    if let Some(g) = GUARD.get() {
        if g.cfg.enabled {
            tracing::info!(mode = ?g.cfg.mode, rounds = g.cfg.rounds, keys = g.own_keys.len(), "double-forge guard enabled");
        } else {
            tracing::warn!("double-forge guard DISABLED (delegate.guard.enabled: false) — a key loaded on two nodes will split the network unnoticed");
        }
    }
}

fn guard() -> Option<&'static Guard> {
    GUARD.get().filter(|g| g.cfg.enabled)
}

/// A block entered the chain live from the network or was forged here (called by every intake path).
pub fn on_accepted(source: Source, peer: &str, block: &Block, network: &crate::config::Network) {
    if let Some(g) = guard() {
        g.on_accepted(source, peer, block, network);
    }
}

/// Same as [`on_accepted`] for a catch-up batch (legacy pull).
pub fn on_accepted_batch(source: Source, peer: &str, blocks: &[Block], network: &crate::config::Network) {
    if let Some(g) = guard() {
        for b in blocks {
            g.on_accepted(source, peer, b, network);
        }
    }
}

/// A block at a height we already have, with another id: double forge when the generator is the same.
pub fn on_competing(source: Source, peer: &str, ours: &Block, theirs: &Block) {
    if let Some(g) = guard() {
        g.on_competing(source, peer, ours, theirs);
    }
}

/// Remember a block forged by this node.
pub fn own_forged(id: &str) {
    if let Some(g) = GUARD.get() {
        g.own_forged(id);
    }
}

/// `Some(reason)` when the forger must skip the slot of `public_key` at `next_height` (mode yield + fresh foreign block).
pub fn yield_reason(public_key: &str, next_height: u64, active_delegates: u64) -> Option<String> {
    let g = guard()?;
    let reason = g.yield_reason(public_key, next_height, active_delegates)?;
    g.state.lock().unwrap_or_else(|e| e.into_inner()).yielded_slots += 1;
    crate::alerts::notify("double-forge-yield", format!("slot skipped at height {}: {reason}", crate::delegate::forger::group(next_height)), false);
    Some(reason)
}

/// `{ enabled, mode, rounds, yieldedSlots, yielding: [pk], foreign: [...], competing: [...] }` for the metrics page.
pub fn snapshot(next_height: u64, active_delegates: u64) -> serde_json::Value {
    match GUARD.get() {
        Some(g) => g.snapshot(next_height, active_delegates),
        None => serde_json::json!({ "enabled": false }),
    }
}

impl Guard {
    pub fn new(cfg: DoubleForgeGuardConfig, own_keys: Vec<String>, storage: Option<Arc<Storage>>) -> Self {
        Self { cfg, own_keys: own_keys.into_iter().collect(), storage, state: Mutex::new(State::default()) }
    }

    fn name_of(&self, public_key: &str) -> String {
        self.storage
            .as_ref()
            .and_then(|s| s.find_wallet(public_key).ok().flatten())
            .and_then(|w| w.username)
            .unwrap_or_else(|| short(public_key).to_string())
    }

    /// Only blocks forged within the yield window count — a catch-up of old blocks must not trip the guard.
    fn is_fresh(&self, block: &Block, network: &crate::config::Network) -> bool {
        let m = network.milestone(block.height);
        let window = self.cfg.rounds.max(1) * u64::from(m.active_delegates) * u64::from(m.blocktime);
        let age = i64::from(network.now_epoch()) - i64::from(block.timestamp);
        age <= window as i64
    }

    pub fn own_forged(&self, id: &str) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.own_forged.len() == OWN_IDS {
            st.own_forged.pop_front();
        }
        st.own_forged.push_back(id.to_string());
    }

    pub fn on_accepted(&self, source: Source, peer: &str, block: &Block, network: &crate::config::Network) {
        if source == Source::Forged || !self.own_keys.contains(&block.generator_public_key) || !self.is_fresh(block, network) {
            return;
        }
        let id = block.id.clone().unwrap_or_default();
        let username = self.name_of(&block.generator_public_key);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.own_forged.iter().any(|own| *own == id) {
            return;
        }
        if st.foreign.iter().any(|f| f.id == id) {
            return;
        }
        if st.foreign.len() == KEEP {
            st.foreign.pop_front();
        }
        st.foreign.push_back(ForeignBlock {
            height: block.height,
            id: id.clone(),
            generator: block.generator_public_key.clone(),
            username: username.clone(),
            peer: peer.to_string(),
            source,
            at_unix: now_unix(),
        });
        drop(st);
        let action = match self.cfg.mode {
            GuardMode::Yield => "this node YIELDS that delegate's slots until the foreign blocks stop (delegate.guard.mode: yield)",
            GuardMode::Alert => "delegate.guard.mode is alert — the slots are NOT skipped, remove the key from one node now",
        };
        let msg = format!(
            "DOUBLE FORGING RISK: block {} ({}…) signed by OUR delegate {username} ({}…) arrived from {peer} via {} — the key is loaded on another node; {action}",
            crate::delegate::forger::group(block.height),
            short(&id),
            short(&block.generator_public_key),
            source.label()
        );
        tracing::error!("{msg}");
        crate::console::event(crate::console::Kind::Rejected, &format!("#{}  {username}", crate::delegate::forger::group(block.height)), "key active on another node (double-forge guard)");
        crate::alerts::notify("double-forge", msg, false);
    }

    pub fn on_competing(&self, source: Source, peer: &str, ours: &Block, theirs: &Block) {
        if ours.height != theirs.height {
            return;
        }
        let (our_id, their_id) = (ours.id.clone().unwrap_or_default(), theirs.id.clone().unwrap_or_default());
        if our_id == their_id || their_id.is_empty() {
            return;
        }
        // an unsigned / forged-by-nobody block must not raise an alarm — only a genuine second signature counts
        if !matches!(crate::crypto::verify_block_signature(theirs), Ok(true)) {
            tracing::debug!(peer, height = theirs.height, "competing block with an invalid signature ignored");
            return;
        }
        if ours.generator_public_key != theirs.generator_public_key {
            // 2026-09-29 (fork at 11,988,289): a valid block by ANOTHER delegate at our height means the network
            // forked at this height — the legacy side did not get our block within its slot. Not double forging, but
            // the operator must know within a minute, not after three hours of "peer is behind".
            self.on_same_height_fork(source, peer, ours, theirs, &our_id, &their_id);
            return;
        }
        let own_key = self.own_keys.contains(&ours.generator_public_key);
        let username = self.name_of(&ours.generator_public_key);
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.competing.iter().any(|c| c.height == ours.height && c.theirs == their_id) {
                return;
            }
            if st.competing.len() == KEEP {
                st.competing.pop_front();
            }
            st.competing.push_back(CompetingBlock {
                height: ours.height,
                ours: our_id.clone(),
                theirs: their_id.clone(),
                generator: ours.generator_public_key.clone(),
                username: username.clone(),
                peer: peer.to_string(),
                own_key,
                at_unix: now_unix(),
            });
        }
        let msg = format!(
            "DOUBLE FORGING by delegate {username} ({}…) at height {}: ours {}… vs {}… from {peer} via {}{}",
            short(&ours.generator_public_key),
            crate::delegate::forger::group(ours.height),
            short(&our_id),
            short(&their_id),
            source.label(),
            if own_key { " — this is OUR key: it is loaded on another node too" } else { "" }
        );
        tracing::error!("{msg}");
        crate::alerts::notify("double-forge", msg, false);
    }

    fn on_same_height_fork(&self, source: Source, peer: &str, ours: &Block, theirs: &Block, our_id: &str, their_id: &str) {
        let (our_name, their_name) = (self.name_of(&ours.generator_public_key), self.name_of(&theirs.generator_public_key));
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.forks.iter().any(|f| f.height == ours.height && f.theirs == their_id) {
                return;
            }
            if st.forks.len() == KEEP {
                st.forks.pop_front();
            }
            st.forks.push_back(ForkSeen { height: ours.height, ours: our_id.to_string(), our_generator: our_name.clone(), theirs: their_id.to_string(), their_generator: their_name.clone(), peer: peer.to_string(), at_unix: now_unix() });
        }
        let msg = format!(
            "SAME-HEIGHT FORK at {}: we hold {}… by {our_name} (slot ts {}), {peer} sent {}… by {their_name} (slot ts {}) via {} — the network split at this height (the other side did not receive our block within its slot). If the other side is legacy: roll it back below {} and let it resync from a PQ gateway; check clocks (chronyc tracking) on both sides",
            crate::delegate::forger::group(ours.height),
            short(our_id),
            ours.timestamp,
            short(their_id),
            theirs.timestamp,
            source.label(),
            crate::delegate::forger::group(ours.height.saturating_sub(1)),
        );
        tracing::error!("{msg}");
        crate::console::event(crate::console::Kind::Rejected, &format!("#{}  fork", crate::delegate::forger::group(ours.height)), &format!("{our_name} vs {their_name} from {peer}"));
        crate::alerts::notify("same-height-fork", msg, false);
    }


    /// Heights below this are outside the yield window for `next_height`.
    fn window_start(&self, next_height: u64, active_delegates: u64) -> u64 {
        next_height.saturating_sub(self.cfg.rounds.max(1) * active_delegates.max(1))
    }

    pub fn yield_reason(&self, public_key: &str, next_height: u64, active_delegates: u64) -> Option<String> {
        if self.cfg.mode != GuardMode::Yield {
            return None;
        }
        let from = self.window_start(next_height, active_delegates);
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let f = st.foreign.iter().rev().find(|f| f.generator == public_key && f.height >= from)?;
        Some(format!(
            "double-forge-guard: block {} ({}…) by this key arrived from {} via {} {}s ago — the key forges on another node, yielding the slot instead of splitting the network (stop the other node, or set delegate.guard.mode: alert)",
            crate::delegate::forger::group(f.height),
            short(&f.id),
            f.peer,
            f.source.label(),
            (now_unix() - f.at_unix).max(0)
        ))
    }

    pub fn snapshot(&self, next_height: u64, active_delegates: u64) -> serde_json::Value {
        let from = self.window_start(next_height, active_delegates);
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let yielding: Vec<serde_json::Value> = if self.cfg.enabled && self.cfg.mode == GuardMode::Yield {
            let mut seen = HashSet::new();
            st.foreign
                .iter()
                .rev()
                .filter(|f| f.height >= from && seen.insert(f.generator.clone()))
                .map(|f| serde_json::json!({ "publicKey": f.generator, "username": f.username, "lastHeight": f.height, "peer": f.peer }))
                .collect()
        } else {
            Vec::new()
        };
        serde_json::json!({
            "enabled": self.cfg.enabled,
            "mode": self.cfg.mode,
            "rounds": self.cfg.rounds,
            "ownKeys": self.own_keys.len(),
            "yieldedSlots": st.yielded_slots,
            "yielding": yielding,
            "foreign": st.foreign.iter().rev().take(10).collect::<Vec<_>>(),
            "competing": st.competing.iter().rev().take(10).collect::<Vec<_>>(),
            "forks": st.forks.iter().rev().take(10).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: GuardMode, rounds: u64) -> DoubleForgeGuardConfig {
        DoubleForgeGuardConfig { enabled: true, mode, rounds }
    }

    fn block(height: u64, id: &str, generator: &str, timestamp: u32) -> Block {
        Block {
            id: Some(id.into()),
            version: 0,
            timestamp,
            previous_block: String::new(),
            height,
            number_of_transactions: 0,
            total_amount: 0,
            total_fee: 0,
            reward: 0,
            payload_length: 0,
            payload_hash: String::new(),
            generator_public_key: generator.into(),
            block_signature: None,
            pq_signature: None,
            transactions: Vec::new(),
        }
    }

    const OWN: &str = "03aa";
    const OTHER: &str = "03bb";

    #[test]
    fn foreign_block_by_own_key_triggers_yield_within_window() {
        let net = crate::config::Network::mainnet();
        let g = Guard::new(cfg(GuardMode::Yield, 2), vec![OWN.into()], None);
        let now = net.now_epoch();
        g.on_accepted(Source::PullLegacy, "1.2.3.4", &block(1000, "f1", OWN, now - 8), &net);
        assert!(g.yield_reason(OWN, 1001, 21).is_some(), "fresh foreign block → yield");
        assert!(g.yield_reason(OWN, 1000 + 2 * 21, 21).is_some(), "still inside 2 rounds");
        assert!(g.yield_reason(OWN, 1000 + 2 * 21 + 1, 21).is_none(), "window passed → forging resumes");
        assert!(g.yield_reason(OTHER, 1001, 21).is_none(), "another key is unaffected");
        let s = g.snapshot(1001, 21);
        assert_eq!(s["foreign"].as_array().map(|a| a.len()), Some(1));
        assert_eq!(s["yielding"][0]["publicKey"], OWN);
    }

    #[test]
    fn alert_mode_never_yields_and_own_or_stale_blocks_are_ignored() {
        let net = crate::config::Network::mainnet();
        let now = net.now_epoch();
        let g = Guard::new(cfg(GuardMode::Alert, 2), vec![OWN.into()], None);
        g.on_accepted(Source::GossipIroh, "peer", &block(1000, "f1", OWN, now), &net);
        assert!(g.yield_reason(OWN, 1001, 21).is_none(), "alert mode only reports");
        assert_eq!(g.snapshot(1001, 21)["foreign"].as_array().map(|a| a.len()), Some(1));

        let g = Guard::new(cfg(GuardMode::Yield, 2), vec![OWN.into()], None);
        g.on_accepted(Source::Forged, "self", &block(1000, "mine", OWN, now), &net);
        g.on_accepted(Source::PullLegacy, "peer", &block(1001, "x", OTHER, now), &net);
        g.own_forged("re-pulled");
        g.on_accepted(Source::PullLegacy, "peer", &block(1002, "re-pulled", OWN, now), &net);
        // 3 rounds old (window = 2 rounds × 21 × 8 s) → catch-up, not a live double forge
        g.on_accepted(Source::PullLegacy, "peer", &block(900, "old", OWN, now - 3 * 21 * 8), &net);
        assert!(g.yield_reason(OWN, 1003, 21).is_none());
        assert_eq!(g.snapshot(1003, 21)["foreign"].as_array().map(|a| a.len()), Some(0));
    }

    #[test]
    fn competing_block_needs_same_generator_and_valid_signature() {
        let g = Guard::new(cfg(GuardMode::Yield, 2), vec![], None);
        let ours = block(500, "a", OTHER, 1);
        g.on_competing(Source::PushLegacy, "peer", &ours, &block(500, "b", OWN, 1));
        assert_eq!(g.snapshot(501, 21)["competing"].as_array().map(|a| a.len()), Some(0), "different generator = not a double forge");
        assert_eq!(g.snapshot(501, 21)["forks"].as_array().map(|a| a.len()), Some(0), "unsigned block by another delegate is not a fork either");
        g.on_competing(Source::PushLegacy, "peer", &ours, &block(500, "b", OTHER, 1));
        assert_eq!(g.snapshot(501, 21)["competing"].as_array().map(|a| a.len()), Some(0), "unsigned competing block ignored");
        g.on_competing(Source::PushLegacy, "peer", &ours, &block(500, "a", OTHER, 1));
        assert_eq!(g.snapshot(501, 21)["competing"].as_array().map(|a| a.len()), Some(0), "same id = same block");
    }

    #[test]
    fn signed_competing_block_by_same_generator_is_recorded_once() {
        let keys = crate::crypto::KeyPair::from_passphrase("guard test passphrase").unwrap();
        let pk = keys.public_key_hex();
        let sign = |mut b: Block| {
            b.previous_block = "00".repeat(32);
            b.payload_hash = "00".repeat(32);
            let hash = crate::crypto::block_signing_hash(&b).unwrap();
            b.block_signature = Some(crate::crypto::sign_ecdsa(&hash, keys.private_key()).unwrap());
            b
        };
        let ours = sign(block(500, "a", &pk, 1));
        let theirs = sign(block(500, "b", &pk, 2));
        let g = Guard::new(cfg(GuardMode::Yield, 2), vec![pk.clone()], None);
        g.on_competing(Source::GossipIroh, "peer", &ours, &theirs);
        g.on_competing(Source::PushLegacy, "peer2", &ours, &theirs);
        let s = g.snapshot(501, 21);
        assert_eq!(s["competing"].as_array().map(|a| a.len()), Some(1), "same competing block reported once");
        assert_eq!(s["competing"][0]["ownKey"], true);
        assert_eq!(s["competing"][0]["theirs"], "b");
        // a competing block does not by itself make the forger yield (only a foreign block applied to the chain does)
        assert!(g.yield_reason(&pk, 501, 21).is_none());

        // 2026-09-29: a VALID block by another delegate at our height = same-height fork → recorded once, no yield
        let other = crate::crypto::KeyPair::from_passphrase("other delegate").unwrap();
        let theirs2 = {
            let mut b = block(500, "c", &other.public_key_hex(), 9);
            b.previous_block = "00".repeat(32);
            b.payload_hash = "00".repeat(32);
            let hash = crate::crypto::block_signing_hash(&b).unwrap();
            b.block_signature = Some(crate::crypto::sign_ecdsa(&hash, other.private_key()).unwrap());
            b
        };
        g.on_competing(Source::PushLegacy, "legacy-peer", &ours, &theirs2);
        g.on_competing(Source::PullLegacy, "legacy-peer2", &ours, &theirs2);
        let s = g.snapshot(501, 21);
        assert_eq!(s["forks"].as_array().map(|a| a.len()), Some(1));
        assert_eq!(s["forks"][0]["theirs"], "c");
        assert_eq!(s["competing"].as_array().map(|a| a.len()), Some(1), "fork is not counted as double forging");
        assert!(g.yield_reason(&pk, 501, 21).is_none());
    }

    #[test]
    fn disabled_guard_is_a_noop() {
        let net = crate::config::Network::mainnet();
        let g = Guard::new(DoubleForgeGuardConfig { enabled: false, ..Default::default() }, vec![OWN.into()], None);
        g.on_accepted(Source::PullLegacy, "peer", &block(1, "f", OWN, net.now_epoch()), &net);
        // the free functions gate on `enabled`; the struct itself still records, so the snapshot shows the state
        assert_eq!(g.snapshot(2, 21)["enabled"], false);
        assert!(g.snapshot(2, 21)["yielding"].as_array().is_some_and(|a| a.is_empty()));
    }
}
