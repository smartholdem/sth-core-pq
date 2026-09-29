//! Author: TechnoL0g
//!
//! Transaction relay: fan mempool transactions out to the best legacy peers via
//! `p2p.transactions.postTransactions` (port 4001) so they reach the forging delegates.

use super::{LegacyPeer, PeerTable};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Who received our relayed transactions: legacy peers vs other PQ Nodes (metrics page, mempool card).
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayCoverage {
    pub at_unix: i64,
    pub count: usize,
    pub legacy_delivered: usize,
    pub legacy_total: usize,
    pub pq_delivered: usize,
    pub pq_total: usize,
    /// Peers whose pool accepted at least one of the transactions (delivered ≠ accepted: a legacy pool may refuse a
    /// low fee while the block-level rules would still take it).
    pub legacy_accepted: usize,
    pub pq_accepted: usize,
    /// Alive peers by kind at relay time — shows whether legacy delegates were reachable at all.
    pub alive_legacy: usize,
    pub alive_pq: usize,
    pub relays: u64,
}

fn coverage_slot() -> &'static Mutex<RelayCoverage> {
    static C: OnceLock<Mutex<RelayCoverage>> = OnceLock::new();
    C.get_or_init(Default::default)
}

pub fn relay_coverage() -> RelayCoverage {
    coverage_slot().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Legacy core broadcasts every accepted transaction to `maxPeersBroadcast` = 20 peers; fewer (the old default 3, picked
/// by latency) tends to reach only the nearby PQ Nodes, so legacy delegates never saw the transaction in their pool and
/// only PQ delegates forged blocks with transactions (mainnet 2026-09-19).
pub const MIN_RELAY_FANOUT: usize = 20;

/// Send `serialized` transactions to up to `fanout` (≥ `MIN_RELAY_FANOUT`) healthy peers concurrently.
/// Returns the number of peers that accepted the batch.
pub async fn broadcast(table: &PeerTable, serialized: Vec<Vec<u8>>, fanout: usize, timeout: Duration) -> usize {
    if serialized.is_empty() {
        return 0;
    }
    let peers = table.best(fanout.max(MIN_RELAY_FANOUT));
    if peers.is_empty() {
        tracing::warn!("no healthy legacy peers for transaction relay");
        return 0;
    }
    let results = futures::future::join_all(peers.iter().map(|ip| {
        let txs = serialized.clone();
        async move {
            let started = Instant::now();
            let sent = async {
                let mut peer = LegacyPeer::connect(ip, table.port(), timeout).await?;
                peer.post_transactions(txs).await
            }
            .await;
            match sent {
                Ok(accepted) => {
                    table.record_success(ip, started.elapsed(), None);
                    (true, accepted > 0)
                }
                Err(e) => {
                    tracing::debug!(peer = ip, error = %e, "postTransactions failed");
                    table.record_failure(ip);
                    (false, false)
                }
            }
        }
    }))
    .await;
    let mut cov = RelayCoverage { count: serialized.len(), ..Default::default() };
    cov.at_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    (cov.alive_legacy, cov.alive_pq) = table.alive_by_kind();
    for (ip, (ok, accepted)) in peers.iter().zip(results.iter()) {
        if table.is_pq(ip) {
            cov.pq_total += 1;
            cov.pq_delivered += usize::from(*ok);
            cov.pq_accepted += usize::from(*accepted);
        } else {
            cov.legacy_total += 1;
            cov.legacy_delivered += usize::from(*ok);
            cov.legacy_accepted += usize::from(*accepted);
        }
    }
    let delivered = cov.legacy_delivered + cov.pq_delivered;
    tracing::info!(delivered, of = peers.len(), legacy = format!("{}/{} accepted {}", cov.legacy_delivered, cov.legacy_total, cov.legacy_accepted), pq = format!("{}/{} accepted {}", cov.pq_delivered, cov.pq_total, cov.pq_accepted), count = serialized.len(), peers = %peers.join(","), "transactions relayed over legacy P2P");
    if cov.legacy_delivered == 0 && cov.alive_legacy > 0 {
        tracing::warn!(alive_legacy = cov.alive_legacy, "relayed transactions reached no legacy peer — legacy delegates will not see them");
    } else if cov.legacy_delivered > 0 && cov.legacy_accepted == 0 {
        tracing::warn!(delivered = cov.legacy_delivered, "legacy pools ACCEPTED NONE of the relayed transactions — fee below the legacy pool minimum (static fee unless the peer runs dynamic fees)? only PQ delegates will forge them");
    }
    {
        let mut c = coverage_slot().lock().unwrap_or_else(|e| e.into_inner());
        cov.relays = c.relays + 1;
        *c = cov;
    }
    delivered
}
