//! Author: TechnoL0g
//!
//! Transaction relay: fan mempool transactions out to the best legacy peers via
//! `p2p.transactions.postTransactions` (port 4001) so they reach the forging delegates.

use super::{LegacyPeer, PeerTable};
use std::time::{Duration, Instant};

/// Send `serialized` transactions to up to `fanout` healthy peers concurrently.
/// Returns the number of peers that accepted the batch.
pub async fn broadcast(table: &PeerTable, serialized: Vec<Vec<u8>>, fanout: usize, timeout: Duration) -> usize {
    if serialized.is_empty() {
        return 0;
    }
    let peers = table.best(fanout.max(1));
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
                Ok(()) => {
                    table.record_success(ip, started.elapsed(), None);
                    true
                }
                Err(e) => {
                    tracing::debug!(peer = ip, error = %e, "postTransactions failed");
                    table.record_failure(ip);
                    false
                }
            }
        }
    }))
    .await;
    let delivered = results.into_iter().filter(|ok| *ok).count();
    tracing::info!(delivered, of = peers.len(), count = serialized.len(), "transactions relayed over legacy P2P");
    delivered
}
