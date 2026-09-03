//! Author: TechnoL0g
//!
//! Forging loop: every half second work out the current slot, the forging order of the round and
//! whether one of our delegates owns the slot; if so build, sign, apply and broadcast the block.
//! Forks are handled by the follow loop (rollback + resync), so the forger only ever extends the tip.

use super::block_builder::{forge_block, select_transactions};
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
    /// iroh layer whose peers take part in the quorum next to the legacy ones.
    pub iroh: Option<Arc<crate::p2p_iroh::IrohNode>>,
}

impl Default for ForgerOptions {
    fn default() -> Self {
        Self { broadcast_fanout: 6, max_lag: 1, quorum_share: 0.5, iroh: None }
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
    storage: Arc<Storage>,
    mempool: Arc<Mempool>,
    peers: Arc<PeerTable>,
    opts: ForgerOptions,
}

impl Forger {
    pub fn new(secrets: &[String], storage: Arc<Storage>, mempool: Arc<Mempool>, peers: Arc<PeerTable>, opts: ForgerOptions) -> Result<Self> {
        let keys = secrets.iter().map(|s| KeyPair::from_passphrase(s)).collect::<Result<Vec<_>>>()?;
        Ok(Self { status: Default::default(), keys, storage, mempool, peers, opts })
    }

    pub fn public_keys(&self) -> Vec<String> {
        self.keys.iter().map(|k| k.public_key_hex()).collect()
    }

    /// Ask the best legacy peers for their tip (height + block id) concurrently.
    async fn network_view(&self, count: usize) -> Vec<PeerView> {
        let port = self.peers.port();
        let ips = self.peers.best(count);
        let views = futures::future::join_all(ips.into_iter().map(|ip| async move {
            let res = tokio::time::timeout(Duration::from_secs(4), async {
                let mut peer = LegacyPeer::connect(&ip, port, Duration::from_secs(4)).await?;
                peer.get_status().await
            })
            .await;
            match res {
                Ok(Ok(status)) => status.state.map(|st| PeerView {
                    ip,
                    height: st.height as u64,
                    id: st.header.map(|h| h.id).unwrap_or_default(),
                }),
                _ => None,
            }
        }))
        .await;
        let mut views: Vec<PeerView> = views.into_iter().flatten().collect();
        if let Some(node) = &self.opts.iroh {
            let ids: Vec<_> = node.peers.snapshot().into_iter().filter(|p| p.failures < 3).map(|p| p.id).take(count).collect();
            let iroh_views = futures::future::join_all(ids.into_iter().map(|id| async move {
                match tokio::time::timeout(Duration::from_secs(4), crate::p2p_iroh::fetch_status(&node.endpoint, &node.peers, id)).await {
                    Ok(Ok((height, block_id))) => Some(PeerView { ip: format!("iroh:{}", id.fmt_short()), height, id: block_id.unwrap_or_default() }),
                    _ => None,
                }
            }))
            .await;
            views.extend(iroh_views.into_iter().flatten());
        }
        views
    }

    /// Wait (until `deadline`) for the local tip to match the network majority; returns the tip to build on.
    async fn wait_for_quorum(&self, deadline: Instant) -> std::result::Result<crate::models::Block, String> {
        loop {
            let tip = self.storage.get_last_block().ok().flatten().ok_or("empty database")?;
            let views = self.network_view(8).await;
            if views.is_empty() {
                return Err("no legacy peer answered getStatus".into());
            }
            for v in views.iter().filter(|v| !v.ip.starts_with("iroh:")) {
                self.peers.record_success(&v.ip, Duration::from_millis(500), Some(v.height));
            }
            let max_h = views.iter().map(|v| v.height).max().unwrap_or(0);
            let same: usize = views.iter().filter(|v| v.height == tip.height && Some(v.id.as_str()) == tip.id.as_deref()).count();
            let needed = ((views.len() as f64 * self.opts.quorum_share.clamp(0.0, 1.0)).ceil() as usize).max(1);
            if max_h <= tip.height && same >= needed {
                return Ok(tip);
            }
            if Instant::now() >= deadline {
                let heights: Vec<String> = views.iter().map(|v| format!("{}={}", v.ip, v.height)).collect();
                return Err(format!(
                    "local tip {} ({}) vs peers [{}]: {} of {} peers agree with us",
                    tip.height,
                    tip.id.as_deref().map(|i| &i[..8]).unwrap_or("?"),
                    heights.join(", "),
                    same,
                    views.len()
                ));
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    fn record_skip(&self, slot: u64, delegate: &str, reason: String) {
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
            statuses.push(DelegateStatus { username: name.clone(), public_key: pk.clone(), address: address.clone(), rank, active: rank.is_some_and(|r| r <= n) });
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
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        let mut ticks = 0u64;
        loop {
            tick.tick().await;
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
        let order = forging_order(&self.storage, info.round, n)?;
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

        let pool = self.mempool.all().await;
        let txs = select_transactions(&self.storage, network, pool, next_height)?;
        let block = forge_block(network, keys, &tip, slot_start(slot, blocktime), txs)?;
        let chain_tip = ChainTip { height: tip.height, id: tip.id.clone() };
        let st = self.storage.clone();
        let net = network.clone();
        let b = block.clone();
        tokio::task::spawn_blocking(move || apply_blocks(&st, &net, &[b], chain_tip, true))
            .await
            .map_err(|e| crate::error::Error::Sync(format!("apply task failed: {e}")))??;
        tracing::info!(
            "Forged new block {} by delegate {} ({}) at height {} with {} transactions (fees {})",
            block.id.as_deref().unwrap_or(""),
            name,
            keys.public_key_hex(),
            group(block.height),
            block.transactions.len(),
            block.total_fee
        );
        self.mempool.prune_confirmed().await;
        {
            let mut st = self.status.lock().unwrap_or_else(|e| e.into_inner());
            st.forged_count += 1;
            st.last_forged = Some(ForgedInfo { height: block.height, id: block.id.clone().unwrap_or_default(), transactions: block.transactions.len(), at_unix: now_unix(), accepted_by: 0 });
        }
        let status = self.status.clone();

        let height = block.height;
        let serialized = serialize_block_with_transactions(&block, network)?;
        let peers = self.peers.best(self.opts.broadcast_fanout);
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
                        Ok(r) => {
                            tracing::debug!(peer = ip, accepted = r.status, height = r.height, "postBlock");
                            r.status
                        }
                        Err(e) => {
                            tracing::debug!(peer = ip, error = %e, "postBlock failed");
                            table.record_failure(ip);
                            false
                        }
                    }
                }
            }))
            .await;
            let accepted = results.iter().filter(|ok| **ok).count();
            tracing::info!("Broadcasting block {} to {} peers ({accepted} accepted)", group(height), results.len());
            if let Some(last) = status.lock().unwrap_or_else(|e| e.into_inner()).last_forged.as_mut() {
                if last.height == height {
                    last.accepted_by = accepted;
                }
            }
        });
        Ok(())
    }
}
