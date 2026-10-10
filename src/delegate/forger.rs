//! Author: TechnoL0g
//!
//! Forging loop: every half second work out the current slot, the forging order of the round and
//! whether one of our delegates owns the slot; if so build, sign, apply and broadcast the block.
//! Forks are handled by the follow loop (rollback + resync), so the forger only ever extends the tip.

use super::block_builder::{forge_block_with, select_transactions};
use super::round::{forging_order, round_info, slot_number, slot_start};
use crate::crypto::{serialize_block_with_transactions, KeyPair};
use crate::error::Result;
use crate::mempool::Mempool;
use crate::p2p_legacy::{LegacyPeer, PeerTable};
use crate::storage::Storage;
use crate::sync::{apply_blocks, ChainTip};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One legacy peer's view of the chain tip.
struct PeerView {
    ip: String,
    height: u64,
    id: String,
}

#[derive(Clone)]
pub struct ForgerOptions {
    /// Legacy peers that receive `postBlock` for every forged block.
    pub broadcast_fanout: usize,
    /// Do not forge while more than this many blocks behind the best known peer height.
    pub max_lag: u64,
    /// Share (0.0–1.0) of responding peers that must report our exact tip before we forge.
    pub quorum_share: f64,
    /// Absolute minimum of peers that must agree with our tip (guards against forging alone while
    /// isolated: one or two of our own iroh nodes echoing our tip is not a network).
    pub min_quorum_peers: usize,
    /// iroh layer whose peers take part in the quorum next to the legacy ones.
    pub iroh: Option<Arc<crate::p2p_iroh::IrohNode>>,
}

impl Default for ForgerOptions {
    fn default() -> Self {
        Self { broadcast_fanout: 6, max_lag: 1, quorum_share: 0.5, min_quorum_peers: 3, iroh: None }
    }
}

/// Observable forging state for `/api/node/forging`.
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgingStatus {
    pub delegates: Vec<DelegateStatus>,
    pub next_slot: Option<SlotInfo>,
    pub last_forged: Option<ForgedInfo>,
    pub skipped: std::collections::VecDeque<SkippedSlot>,
    pub forged_count: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegateStatus {
    pub username: String,
    pub public_key: String,
    pub address: String,
    pub rank: Option<usize>,
    pub active: bool,
    /// Stage C readiness: PQ key registered on chain (`pq-register`) and its passphrase configured (`delegate.pq_secrets`).
    pub pq_registered: bool,
    pub pq_secret: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotInfo {
    pub slot: u64,
    pub delegate: String,
    pub starts_at_unix: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgedInfo {
    pub height: u64,
    pub id: String,
    pub transactions: usize,
    pub at_unix: i64,
    pub accepted_by: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedSlot {
    pub slot: u64,
    pub delegate: String,
    pub reason: String,
    pub at_unix: i64,
}

/// `11708910` → `11,708,910` (legacy console style).
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub struct Forger {
    pub status: Arc<std::sync::Mutex<ForgingStatus>>,
    keys: Vec<KeyPair>,
    /// ML-DSA-44 keys for hybrid block signatures (matched to a delegate through its on-chain `pq_key`).
    pq_keys: Vec<crate::crypto::pq::PqKeyPair>,
    storage: Arc<Storage>,
    mempool: Arc<Mempool>,
    peers: Arc<PeerTable>,
    opts: ForgerOptions,
}

impl Forger {
    pub fn new(secrets: &[String], pq_secrets: &[String], storage: Arc<Storage>, mempool: Arc<Mempool>, peers: Arc<PeerTable>, opts: ForgerOptions) -> Result<Self> {
        let keys = secrets.iter().map(|s| KeyPair::from_passphrase(s)).collect::<Result<Vec<_>>>()?;
        let pq_keys = pq_secrets.iter().filter(|s| !s.trim().is_empty()).map(|s| crate::crypto::pq::PqKeyPair::from_passphrase(s)).collect();
        Ok(Self { status: Default::default(), keys, pq_keys, storage, mempool, peers, opts })
    }

    /// The configured PQ key that matches the delegate's registered `pq_key`, if any.
    fn pq_key_for(&self, public_key: &str) -> Option<&crate::crypto::pq::PqKeyPair> {
        let registered = self.storage.find_wallet(public_key).ok().flatten().and_then(|w| w.pq_key)?;
        self.pq_keys.iter().find(|k| k.public_key_hex() == registered.public_key)
    }

    pub fn keys(&self) -> Vec<KeyPair> {
        self.keys.clone()
    }

    pub fn public_keys(&self) -> Vec<String> {
        self.keys.iter().map(|k| k.public_key_hex()).collect()
    }

    /// Ask the best legacy peers and the iroh neighbours for their tip (height + block id) concurrently. Returns as soon
    /// as `enough(&views)` holds — one unreachable peer must not cost the full 4 s timeout — or when every probe answered.
    async fn network_view(&self, count: usize, enough: impl Fn(&[PeerView], usize) -> bool) -> Vec<PeerView> {
        use futures::StreamExt;
        let port = self.peers.port();
        let mut probes: futures::stream::FuturesUnordered<futures::future::BoxFuture<'_, Option<PeerView>>> = futures::stream::FuturesUnordered::new();
        for ip in self.peers.best(count) {
            probes.push(Box::pin(async move {
                let res = tokio::time::timeout(Duration::from_secs(4), async {
                    let mut peer = LegacyPeer::connect(&ip, port, Duration::from_secs(4)).await?;
                    peer.get_status().await
                })
                .await;
                match res {
                    Ok(Ok(status)) => status.state.map(|st| PeerView { ip, height: st.height as u64, id: st.header.map(|h| h.id).unwrap_or_default() }),
                    _ => None,
                }
            }));
        }
        if let Some(node) = &self.opts.iroh {
            for id in node.peers.snapshot().into_iter().filter(|p| p.failures < 3).map(|p| p.id).take(count) {
                probes.push(Box::pin(async move {
                    match tokio::time::timeout(Duration::from_secs(4), crate::p2p_iroh::fetch_status(&node.endpoint, &node.peers, id)).await {
                        Ok(Ok((height, block_id))) => Some(PeerView { ip: format!("iroh:{}", id.fmt_short()), height, id: block_id.unwrap_or_default() }),
                        _ => None,
                    }
                }));
            }
        }
        let asked = probes.len();
        let mut views: Vec<PeerView> = Vec::new();
        while let Some(v) = probes.next().await {
            if let Some(v) = v {
                views.push(v);
                if enough(&views, asked) {
                    break;
                }
            }
        }
        views
    }

    /// Wait (until `deadline`) for the local tip to match the network majority; returns the tip to build on.
    async fn wait_for_quorum(&self, deadline: Instant) -> std::result::Result<crate::models::Block, String> {
        loop {
            let tip = self.storage.get_last_block().ok().flatten().ok_or("empty database")?;
            // early exit: enough peers (counted against everyone we asked, so silent peers never relax the bar) agree
            // with our tip and nobody is ahead — the slot is used within ~0.5 s instead of ~4–5 s
            let share = self.opts.quorum_share.clamp(0.0, 1.0);
            let min_peers = self.opts.min_quorum_peers.max(1);
            let (tip_h, tip_id) = (tip.height, tip.id.clone().unwrap_or_default());
            let views = self
                .network_view(8, |views, asked| {
                    let needed = ((asked as f64 * share).ceil() as usize).max(min_peers);
                    let same = views.iter().filter(|v| v.height == tip_h && v.id == tip_id).count();
                    same >= needed && views.iter().all(|v| v.height <= tip_h)
                })
                .await;
            if views.is_empty() {
                // quorum_share 0 = private / single-node network (`init newnet`): forge without peers
                if self.opts.quorum_share <= 0.0 {
                    return Ok(tip);
                }
                return Err("no legacy peer answered getStatus".into());
            }
            for v in views.iter().filter(|v| !v.ip.starts_with("iroh:")) {
                self.peers.record_success(&v.ip, Duration::from_millis(500), Some(v.height));
            }
            let max_h = views.iter().map(|v| v.height).max().unwrap_or(0);
            let same: usize = views.iter().filter(|v| v.height == tip.height && Some(v.id.as_str()) == tip.id.as_deref()).count();
            let needed = ((views.len() as f64 * self.opts.quorum_share.clamp(0.0, 1.0)).ceil() as usize).max(self.opts.min_quorum_peers.max(1));
            if max_h <= tip.height && same >= needed {
                return Ok(tip);
            }
            if Instant::now() >= deadline {
                let heights: Vec<String> = views.iter().map(|v| format!("{}={}", v.ip, v.height)).collect();
                if views.len() < self.opts.min_quorum_peers {
                    return Err(format!(
                        "only {} peer(s) reachable [{}], need at least {} agreeing — refusing to forge in isolation",
                        views.len(),
                        heights.join(", "),
                        self.opts.min_quorum_peers
                    ));
                }
                return Err(format!(
                    "local tip {} ({}) vs peers [{}]: {} of {} peers agree with us (need {})",
                    tip.height,
                    tip.id.as_deref().and_then(|i| i.get(..8)).unwrap_or("?"),
                    heights.join(", "),
                    same,
                    views.len(),
                    needed
                ));
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    fn record_skip(&self, slot: u64, delegate: &str, reason: String) {
        crate::console::event(crate::console::Kind::Skipped, &format!("slot {slot}  {delegate}"), &reason);
        let mut st = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if st.skipped.len() >= 20 {
            st.skipped.pop_front();
        }
        st.skipped.push_back(SkippedSlot { slot, delegate: delegate.to_string(), reason, at_unix: now_unix() });
    }

    /// Delegate username for a public key (from the wallet state), `<unregistered>` otherwise.
    fn name_of(&self, public_key: &str) -> String {
        self.storage
            .find_wallet(public_key)
            .ok()
            .flatten()
            .and_then(|w| w.username)
            .unwrap_or_else(|| "<unregistered>".to_string())
    }

    /// Legacy-style summary: `Loaded 1 active delegate: axai (03518a…)`, preceded by one identity
    /// line per configured key (username / address / rank taken from the local chain state).
    pub fn log_loaded(&self) {
        let height = self.storage.get_last_height().unwrap_or(0);
        let n = self.storage.network().milestone(height.max(1)).active_delegates as usize;
        let ranking = self.storage.delegate_ranking().unwrap_or_default();
        let mut active = Vec::new();
        let mut statuses = Vec::new();
        for k in &self.keys {
            let pk = k.public_key_hex();
            let address = k.address(self.storage.network().pubkey_hash).unwrap_or_default();
            let rank = ranking.iter().position(|(w, _)| w.public_key.as_deref() == Some(pk.as_str())).map(|i| i + 1);
            let name = self.name_of(&pk);
            let pq_registered = self.storage.find_wallet(&pk).ok().flatten().is_some_and(|w| w.pq_key.is_some());
            let pq_secret = self.pq_key_for(&pk).is_some();
            if self.storage.network().pq_blocks_activation_height().is_some() && !pq_secret {
                tracing::warn!("Delegate {name}: no matching PQ key for hybrid block signatures (pq.blocks) — register one with `sth-cli pq-register` and add its passphrase to delegate.pq_secrets");
            }
            statuses.push(DelegateStatus { username: name.clone(), public_key: pk.clone(), address: address.clone(), rank, active: rank.is_some_and(|r| r <= n), pq_registered, pq_secret });
            match rank {
                Some(r) if r <= n => {
                    tracing::info!("Delegate {name}: address {address}, rank {r} of {n} (active), public key {pk}");
                    active.push(format!("{name} ({pk})"));
                }
                Some(r) => tracing::warn!("Delegate {name}: address {address}, rank {r} of {n} — registered but NOT in the active set, it will not forge"),
                None if height == 0 => tracing::warn!("Delegate address {address} ({pk}): chain database is empty — the username will be resolved after sync"),
                None => tracing::warn!("Delegate address {address} ({pk}) is not a registered delegate at local height {} — it will not forge (still syncing?)", group(height)),
            }
        }
        self.status.lock().unwrap_or_else(|e| e.into_inner()).delegates = statuses;
        if active.is_empty() {
            tracing::warn!("Loaded 0 active delegates");
        } else {
            tracing::info!("Loaded {} active delegate{}: {}", active.len(), if active.len() == 1 { "" } else { "s" }, active.join(", "));
        }
    }

    fn has_unresolved_delegate(&self) -> bool {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).delegates.iter().any(|d| d.rank.is_none())
    }

    pub async fn run(self: Arc<Self>) {
        let network = self.storage.network().clone();
        let mut last_slot = 0u64;
        let mut announced_slot = 0u64;
        tracing::info!("Forger Manager started.");
        let mut ticks = 0u64;
        loop {
            // slots start on whole epoch seconds: wake 25 ms after each second boundary (and once mid-second) instead of
            // on a free-running 500 ms interval that noticed a new slot up to 0.5 s late
            let sub = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_millis()).unwrap_or(0);
            let wait = if sub < 25 { 25 - sub } else if sub < 525 { 525 - sub } else { 1025 - sub };
            tokio::time::sleep(Duration::from_millis(u64::from(wait))).await;
            ticks += 1;
            // keys unknown at start (empty / syncing DB): re-read name & rank from the chain every 30 s
            if ticks % 60 == 0 && self.has_unresolved_delegate() {
                self.log_loaded();
            }
            if let Err(e) = self.tick(&network, &mut last_slot, &mut announced_slot).await {
                tracing::warn!(error = %e, "forger tick failed");
            }
        }
    }

    async fn tick(&self, network: &crate::config::Network, last_slot: &mut u64, announced_slot: &mut u64) -> Result<()> {
        let Some(tip) = self.storage.get_last_block()? else { return Ok(()) };
        let m = network.milestone(tip.height + 1);
        let (blocktime, n) = (m.blocktime, m.active_delegates as usize);
        let now = network.now_epoch();
        let slot = slot_number(now, blocktime);
        let next_height = tip.height + 1;
        let info = round_info(next_height, n as u64);
        let Some(order) = forging_order(&self.storage, info.round, n)? else {
            if *last_slot != slot {
                *last_slot = slot;
                tracing::error!(
                    "Skipping slot {slot}: no active-delegates snapshot for round {} yet (it is written when block {} is applied) — forging from live balances would take other delegates' slots",
                    group(info.round),
                    group(info.round_height.saturating_sub(1))
                );
            }
            return Ok(());
        };
        if order.is_empty() {
            return Ok(());
        }
        // announce (once) when the upcoming slot belongs to one of our delegates
        let next_pk = &order[((slot + 1) % n as u64) as usize].public_key;
        if *announced_slot != slot + 1 {
            if let Some(k) = self.keys.iter().find(|k| k.public_key_hex() == *next_pk) {
                let name = self.name_of(next_pk);
                tracing::info!("Next forging delegate {} ({}) is active on this node.", name, k.public_key_hex());
                self.status.lock().unwrap_or_else(|e| e.into_inner()).next_slot =
                    Some(SlotInfo { slot: slot + 1, delegate: name, starts_at_unix: network.epoch_to_unix(slot_start(slot + 1, blocktime)) });
                *announced_slot = slot + 1;
            }
        }
        if slot == *last_slot || slot_number(tip.timestamp, blocktime) >= slot {
            return Ok(());
        }
        let expected = &order[(slot % n as u64) as usize].public_key;
        let Some(keys) = self.keys.iter().find(|k| k.public_key_hex() == *expected) else { return Ok(()) };
        *last_slot = slot;
        let name = self.name_of(expected);
        // double-forge guard (optional module): the key was seen forging on another node → give the slot up
        if let Some(reason) = super::guard::yield_reason(expected, next_height, n as u64) {
            tracing::error!("Skipping slot {slot} for delegate {name}: {reason}");
            self.record_skip(slot, &name, reason);
            return Ok(());
        }
        tracing::info!("Slot {slot} belongs to delegate {name} on this node — checking network quorum");
        // the block must extend the tip the network agrees on; wait for the previous slot's block if needed
        let slot_end = slot_start(slot + 1, blocktime);
        let remaining = (slot_end as i64 - network.now_epoch() as i64).max(1) as u64;
        let deadline = Instant::now() + Duration::from_secs(remaining.saturating_sub(2).max(1));
        let tip = match self.wait_for_quorum(deadline).await {
            Ok(tip) => tip,
            Err(reason) => {
                tracing::warn!("Skipping slot {slot} for delegate {name}: {reason}");
                self.record_skip(slot, &name, reason);
                return Ok(());
            }
        };
        if slot_number(tip.timestamp, blocktime) >= slot {
            let reason = format!("a block for this slot already exists (height {})", tip.height);
            tracing::warn!("Skipping slot {slot} for delegate {name}: {reason}");
            self.record_skip(slot, &name, reason);
            return Ok(());
        }

        // stage C: hybrid signature needs our PQ key matching the delegate's on-chain pq_key; without it v0 is only legal in the grace window
        let (v1_ok, v0_ok) = network.block_versions_allowed(next_height);
        let pq = if v1_ok { self.pq_key_for(expected) } else { None };
        if v1_ok && pq.is_none() {
            if v0_ok {
                tracing::warn!("Delegate {name} forges a version-0 block: no PQ key for pq.blocks (grace window still open) — register one before it closes");
            } else {
                let reason = "BlockPqKeyMissingError: pq.blocks requires a registered PQ key + delegate.pq_secrets".to_string();
                tracing::error!("Skipping slot {slot} for delegate {name}: {reason}");
                self.record_skip(slot, &name, reason);
                return Ok(());
            }
        }
        let pool = self.mempool.all().await;
        let txs = select_transactions(&self.storage, network, pool, next_height)?;
        let block = forge_block_with(network, keys, pq, &tip, slot_start(slot, blocktime), txs)?;
        let chain_tip = ChainTip { height: tip.height, id: tip.id.clone() };
        let st = self.storage.clone();
        let net = network.clone();
        let b = block.clone();
        tokio::task::spawn_blocking(move || apply_blocks(&st, &net, &[b], chain_tip, true))
            .await
            .map_err(|e| crate::error::Error::Sync(format!("apply task failed: {e}")))??;
        crate::intake::record(crate::intake::Source::Forged, name.clone(), block.height, block.timestamp, &block.generator_public_key, network);
        super::guard::own_forged(block.id.as_deref().unwrap_or_default());
        tracing::info!(
            "Forged new block {} by delegate {} ({}) at height {} with {} transactions (fees {}){}",
            block.id.as_deref().unwrap_or(""),
            name,
            keys.public_key_hex(),
            group(block.height),
            block.transactions.len(),
            block.total_fee,
            if block.pq_signature.is_some() { " · v1 hybrid secp256k1 + ML-DSA-44" } else { "" }
        );
        crate::console::event(
            crate::console::Kind::Forged,
            &format!("#{}  {name}", group(block.height)),
            &format!(
                "{} tx · fees {} · reward {} · id {}…{}",
                block.transactions.len(),
                sth(block.total_fee),
                sth(block.reward),
                block.id.as_deref().map(|i| &i[..i.len().min(10)]).unwrap_or(""),
                if block.pq_signature.is_some() { " · ML-DSA-44" } else { "" }
            ),
        );
        self.mempool.prune_confirmed().await;
        {
            let mut st = self.status.lock().unwrap_or_else(|e| e.into_inner());
            st.forged_count += 1;
            st.last_forged = Some(ForgedInfo { height: block.height, id: block.id.clone().unwrap_or_default(), transactions: block.transactions.len(), at_unix: now_unix(), accepted_by: 0 });
        }
        let status = self.status.clone();

        let height = block.height;
        let tx_count = block.transactions.len();
        let serialized = serialize_block_with_transactions(&block, network)?;
        // a forged block goes to every healthy legacy peer — a missed slot costs more than a few sockets
        let peers = self.peers.best(self.opts.broadcast_fanout.max(self.peers.alive()));
        let table = self.peers.clone();
        let port = table.port();
        tokio::spawn(async move {
            let results = futures::future::join_all(peers.iter().map(|ip| {
                let bytes = serialized.clone();
                let table = table.clone();
                async move {
                    let res = async {
                        let mut peer = LegacyPeer::connect(ip, port, Duration::from_secs(10)).await?;
                        peer.post_block(bytes).await
                    }
                    .await;
                    match res {
                        Ok(r) if r.status => {
                            tracing::debug!(peer = ip, height = r.height, "postBlock accepted");
                            true
                        }
                        Ok(r) => {
                            // legacy answers { status: false, height: <its tip> }: a peer behind us cannot chain the block
                            let peer_height = u64::from(r.height);
                            if peer_height + 1 < height {
                                tracing::info!(peer = ip, peer_height, height, behind = height - peer_height, "postBlock not accepted: the legacy peer is behind the network and cannot chain this block yet (peer problem, not ours)");
                            } else if peer_height >= height {
                                // status:false + its tip already at our height = the block reached it first via another relay
                                // (a PQ peer got it over iroh gossip and pushed it) — delivered, count it as accepted
                                tracing::debug!(peer = ip, peer_height, height, "postBlock: the legacy peer already has this height (delivered by another relay first)");
                                return true;
                            } else if tx_count == 0 {
                                tracing::warn!(peer = ip, peer_height = r.height, height, "postBlock REJECTED by legacy peer at the right height for an empty block — check the peer's log (slot/generator check or block version)");
                            } else {
                                tracing::warn!(peer = ip, peer_height = r.height, height, "postBlock REJECTED by legacy peer (invalid transaction for the network? check `sth-core audit-wallet <sender>`)");
                            }
                            false
                        }
                        Err(e) => {
                            tracing::warn!(peer = ip, error = %e, height, "postBlock REJECTED / failed — the peer's reason is in `error`");
                            table.record_failure(ip);
                            false
                        }
                    }
                }
            }))
            .await;
            let accepted = results.iter().filter(|ok| **ok).count();
            if accepted == 0 && !results.is_empty() {
                tracing::error!("Broadcasting block {} to {} peers: NONE accepted — the network will skip this slot and the block will be rolled back", group(height), results.len());
                crate::console::event(crate::console::Kind::Rejected, &format!("#{}", group(height)), &format!("none of {} legacy peers accepted the block — it will be rolled back", results.len()));
            } else {
                tracing::info!("Broadcasting block {} to {} peers ({accepted} accepted)", group(height), results.len());
                crate::console::event(crate::console::Kind::Relayed, &format!("#{}", group(height)), &format!("{accepted}/{} legacy peers accepted", results.len()));
            }
            if let Some(last) = status.lock().unwrap_or_else(|e| e.into_inner()).last_forged.as_mut() {
                if last.height == height {
                    last.accepted_by = accepted;
                }
            }
        });
        self.spawn_orphan_watchdog(block.height, block.id.clone().unwrap_or_default(), network.milestone(block.height).blocktime);
        Ok(())
    }

    /// Two slots after forging, ask the healthy legacy peers which block they have at our height. If most of them serve
    /// another block, our block was orphaned (rejected or late): roll it back right away so the follow loop resyncs
    /// instead of sitting on a private fork — the legacy `forkBlock` behaviour.
    fn spawn_orphan_watchdog(&self, height: u64, id: String, blocktime: u32) {
        let (storage, table) = (self.storage.clone(), self.peers.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(u64::from(blocktime) * 2 + 2)).await;
            if storage.get_block_by_height(height).ok().flatten().and_then(|b| b.id) != Some(id.clone()) {
                return; // already replaced or rolled back
            }
            let port = table.port();
            let checks = futures::future::join_all(table.best(5).into_iter().map(|ip| async move {
                let res = async {
                    let mut peer = LegacyPeer::connect(&ip, port, Duration::from_secs(8)).await?;
                    let blocks = peer.get_blocks(height - 1, 1).await?;
                    Ok::<_, crate::error::Error>(blocks.into_iter().next().and_then(|b| b.id))
                }
                .await;
                (ip, res)
            }))
            .await;
            let mut same = 0usize;
            let mut other: Vec<(String, String)> = Vec::new();
            for (ip, res) in checks {
                match res {
                    Ok(Some(their)) if their == id => same += 1,
                    Ok(Some(their)) => other.push((ip, their)),
                    _ => {}
                }
            }
            if other.len() >= 2 && other.len() > same {
                tracing::warn!(height, ours = &id[..16], theirs = &other[0].1[..16.min(other[0].1.len())], peers = ?other.iter().map(|(ip, _)| ip.as_str()).collect::<Vec<_>>(), "our forged block was orphaned by the network — rolling it back");
                let st = storage.clone();
                match tokio::task::spawn_blocking(move || st.rollback_to(height - 1)).await {
                    Ok(Ok(h)) => tracing::warn!(to = h, "orphaned block rolled back; resyncing from peers"),
                    Ok(Err(e)) => tracing::error!(error = %e, "orphan rollback failed"),
                    Err(e) => tracing::error!(error = %e, "orphan rollback task failed"),
                }
            } else if same > 0 {
                tracing::debug!(height, same, other = other.len(), "forged block confirmed by peers");
            }
        });
    }
}

fn sth(v: u64) -> String {
    let s = format!("{}.{:08}", v / 100_000_000, v % 100_000_000);
    format!("{} STH", s.trim_end_matches('0').trim_end_matches('.'))
}
