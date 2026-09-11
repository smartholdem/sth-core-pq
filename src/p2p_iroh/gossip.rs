//! Author: TechnoL0g
//!
//! Gossip loops: receive blocks (apply / gap-fill via RPC) and transactions (into the mempool),
//! publish blocks we applied from other sources and transactions our mempool accepted.

use super::proto::GossipMessage;
use super::{fetch_blocks, IrohNode};
use crate::error::{Error, Result};
use crate::mempool::Mempool;
use crate::models::Block;
use crate::storage::Storage;
use crate::sync::{apply_blocks, ChainTip};
use iroh::EndpointId;
use iroh_gossip::api::{Event, GossipSender};
use n0_future::StreamExt;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Heights / tx ids that arrived over gossip (never re-published) — bounded ring.
#[derive(Default)]
struct Seen {
    heights: HashSet<u64>,
    tx_ids: HashSet<String>,
}

impl Seen {
    fn trim(&mut self) {
        if self.heights.len() > 4_096 {
            self.heights.clear();
        }
        if self.tx_ids.len() > 16_384 {
            self.tx_ids.clear();
        }
    }
}

fn tip_of(storage: &Storage) -> Result<ChainTip> {
    Ok(match storage.get_last_block()? {
        Some(b) => ChainTip { height: b.height, id: b.id },
        None => ChainTip { height: 0, id: None },
    })
}

async fn apply(storage: &Arc<Storage>, blocks: Vec<Block>, tip: ChainTip, verify: bool) -> Result<ChainTip> {
    let st = storage.clone();
    let net = storage.network().clone();
    tokio::task::spawn_blocking(move || apply_blocks(&st, &net, &blocks, tip, verify))
        .await
        .map_err(|e| Error::Sync(format!("apply task failed: {e}")))?
}

pub async fn start(
    node: Arc<IrohNode>,
    bootstrap: Vec<EndpointId>,
    storage: Arc<Storage>,
    mempool: Arc<Mempool>,
    verify: bool,
    legacy_peers: Option<Arc<crate::p2p_legacy::PeerTable>>,
) -> Result<()> {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let bootstrap_for_watchdog = bootstrap.clone();
    let blocks_topic = node.gossip.subscribe(node.topics.blocks, bootstrap.clone()).await.map_err(|e| Error::Sync(format!("gossip subscribe: {e}")))?;
    let txs_topic = node.gossip.subscribe(node.topics.transactions, bootstrap).await.map_err(|e| Error::Sync(format!("gossip subscribe: {e}")))?;
    let (block_sender, mut block_receiver) = blocks_topic.split();
    let (tx_sender, mut tx_receiver) = txs_topic.split();

    // blocks intake
    {
        let (node, storage, seen) = (node.clone(), storage.clone(), seen.clone());
        let legacy_table_blocks = legacy_peers.clone();
        tokio::spawn(async move {
            while let Some(event) = block_receiver.next().await {
                match event {
                    Ok(Event::Received(msg)) => {
                        let block = match GossipMessage::decode(&msg.content) {
                            Some(GossipMessage::Block { block }) => block,
                            Some(GossipMessage::Peers { peers: hints, gateway }) => {
                                on_peers(&node, legacy_table_blocks.as_deref(), msg.delivered_from, &hints, &gateway);
                                continue;
                            }
                            _ => continue,
                        };
                        node.peers.seen(msg.delivered_from, Some(block.height));
                        if let Err(e) = on_block(&node, &storage, &seen, msg.delivered_from, block, verify).await {
                            tracing::debug!(error = %e, "gossip block not applied");
                        }
                    }
                    Ok(Event::NeighborUp(id)) => {
                        node.peers.set_neighbor(id, super::peers::TOPIC_BLOCKS, true);
                        tracing::info!(peer = %id.fmt_short(), "iroh neighbor up (blocks)");
                    }
                    Ok(Event::NeighborDown(id)) => {
                        node.peers.set_neighbor(id, super::peers::TOPIC_BLOCKS, false);
                        tracing::info!(peer = %id.fmt_short(), "iroh neighbor down (blocks)");
                    }
                    Ok(Event::Lagged) => tracing::warn!("iroh block gossip lagged"),
                    Err(e) => {
                        tracing::warn!(error = %e, "iroh block gossip stream ended");
                        break;
                    }
                }
            }
        });
    }
    // transactions + peer hints intake
    {
        let (node, mempool, seen, legacy_table) = (node.clone(), mempool.clone(), seen.clone(), legacy_peers.clone());
        tokio::spawn(async move {
            while let Some(event) = tx_receiver.next().await {
                match event {
                    Ok(Event::Received(msg)) => {
                        let decoded = GossipMessage::decode(&msg.content);
                        if let Some(GossipMessage::Peers { peers: hints, gateway }) = &decoded {
                            on_peers(&node, legacy_table.as_deref(), msg.delivered_from, hints, gateway);
                            continue;
                        }
                        let Some(GossipMessage::Transactions { transactions }) = decoded else { continue };
                        node.peers.seen(msg.delivered_from, None);
                        {
                            let mut s = seen.lock().unwrap_or_else(|e| e.into_inner());
                            s.tx_ids.extend(transactions.iter().filter_map(|t| t.id.clone()));
                            s.trim();
                        }
                        let (resp, _) = mempool.add_many(transactions).await;
                        tracing::debug!(accepted = resp.accept.len(), invalid = resp.invalid.len(), "gossip transactions processed");
                    }
                    Ok(Event::NeighborUp(id)) => {
                        node.peers.set_neighbor(id, super::peers::TOPIC_TXS, true);
                        tracing::info!(peer = %id.fmt_short(), "iroh neighbor up (transactions)");
                    }
                    Ok(Event::NeighborDown(id)) => {
                        node.peers.set_neighbor(id, super::peers::TOPIC_TXS, false);
                        tracing::info!(peer = %id.fmt_short(), "iroh neighbor down (transactions)");
                    }
                    Ok(Event::Lagged) => tracing::warn!("iroh transaction gossip lagged"),
                    Err(e) => {
                        tracing::warn!(error = %e, "iroh transaction gossip stream ended");
                        break;
                    }
                }
            }
        });
    }
    // publishers
    if let Some(table) = legacy_peers {
        tokio::spawn(publish_peers([block_sender.clone(), tx_sender.clone()], table, node.gateway.clone()));
    }
    tokio::spawn(rejoin_watchdog(node.clone(), [block_sender.clone(), tx_sender.clone()], bootstrap_for_watchdog));
    tokio::spawn(publish_transactions(tx_sender, mempool.subscribe(), seen.clone()));
    tokio::spawn(publish_blocks(block_sender, storage, node.peers.clone(), seen));
    Ok(())
}

/// Apply a gossiped block at tip+1, or gap-fill from the sender when it is ahead.
async fn on_block(node: &IrohNode, storage: &Arc<Storage>, seen: &Mutex<Seen>, from: EndpointId, block: Block, verify: bool) -> Result<()> {
    let mut tip = tip_of(storage)?;
    if block.height <= tip.height {
        return Ok(());
    }
    if block.height == tip.height + 1 {
        let (h, ts) = (block.height, block.timestamp);
        tip = apply(storage, vec![block], tip, verify).await?;
        crate::intake::record(crate::intake::Source::GossipIroh, from.fmt_short().to_string(), h, ts, storage.network());
        let mut s = seen.lock().unwrap_or_else(|e| e.into_inner());
        s.heights.insert(h);
        s.trim();
        tracing::info!(height = tip.height, peer = %from.fmt_short(), "block applied from iroh gossip");
        return Ok(());
    }
    // behind: pull the gap from the announcing peer (bounded so a bogus announcement cannot stall us)
    let mut rounds = 0;
    while tip.height < block.height && rounds < 8 {
        let batch = fetch_blocks(&node.endpoint, &node.peers, from, tip.height, super::rpc::MAX_BLOCKS).await?;
        if batch.is_empty() {
            break;
        }
        let heights: Vec<u64> = batch.iter().map(|b| b.height).collect();
        tip = apply(storage, batch, tip, verify).await?;
        let mut s = seen.lock().unwrap_or_else(|e| e.into_inner());
        s.heights.extend(heights);
        s.trim();
        rounds += 1;
    }
    tracing::info!(height = tip.height, peer = %from.fmt_short(), "gap filled from iroh peer");
    Ok(())
}

async fn publish_transactions(sender: GossipSender, mut events: tokio::sync::broadcast::Receiver<Vec<crate::models::Transaction>>, seen: Arc<Mutex<Seen>>) {
    loop {
        match events.recv().await {
            Ok(batch) => {
                let fresh: Vec<_> = {
                    let s = seen.lock().unwrap_or_else(|e| e.into_inner());
                    batch.into_iter().filter(|t| t.id.as_ref().map_or(true, |id| !s.tx_ids.contains(id))).collect()
                };
                if fresh.is_empty() {
                    continue;
                }
                let count = fresh.len();
                if let Err(e) = sender.broadcast(GossipMessage::Transactions { transactions: fresh }.encode().into()).await {
                    tracing::debug!(error = %e, "gossip tx broadcast failed");
                } else {
                    tracing::info!(count, "transactions published to iroh gossip");
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return,
        }
    }
}

/// Every 5 minutes share our healthiest legacy peers so freshly started Rust nodes skip the probing phase.
/// `Peers` announcement: gateway address + legacy peer hints from a Rust peer.
fn on_peers(
    node: &super::IrohNode,
    legacy_table: Option<&crate::p2p_legacy::PeerTable>,
    from: EndpointId,
    hints: &[super::proto::PeerHint],
    gateway: &Option<String>,
) {
    node.peers.seen(from, None);
    node.peers.set_gateway(from, gateway.clone());
    let Some(table) = legacy_table else { return };
    if let Some(gw) = gateway {
        if let Some((ip, port)) = gw.rsplit_once(':') {
            if port.parse::<u16>().ok() == Some(table.port()) && table.add(ip) {
                tracing::info!(gateway = %gw, from = %from.fmt_short(), "gateway node announced over iroh");
            }
        }
    }
    let mut added = 0;
    for h in hints.iter().filter(|h| h.port == table.port()).take(64) {
        if table.add(&h.ip) {
            table.record_success(&h.ip, Duration::from_millis(h.latency_ms.max(1)), Some(h.height));
            table.set_version(&h.ip, &h.version);
            added += 1;
        }
    }
    if added > 0 {
        tracing::info!(added, from = %from.fmt_short(), "legacy peers learned from iroh gossip");
    }
}

/// A topic whose neighbours all dropped never re-bootstraps by itself: re-join the configured
/// bootstrap peers (and any peer we still talk to over RPC) whenever a topic has no neighbour.
async fn rejoin_watchdog(node: Arc<super::IrohNode>, senders: [GossipSender; 2], bootstrap: Vec<EndpointId>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.tick().await;
    loop {
        tick.tick().await;
        for (topic, sender) in senders.iter().enumerate() {
            if node.peers.neighbors_on(topic) > 0 {
                continue;
            }
            let mut targets: Vec<EndpointId> = bootstrap.clone();
            targets.extend(node.peers.snapshot().into_iter().filter(|p| p.failures < 3).map(|p| p.id));
            targets.sort();
            targets.dedup();
            if targets.is_empty() {
                continue;
            }
            let name = if topic == super::peers::TOPIC_BLOCKS { "blocks" } else { "transactions" };
            match sender.join_peers(targets.clone()).await {
                Ok(()) => tracing::info!(topic = name, peers = targets.len(), "no gossip neighbours, re-joining"),
                Err(e) => tracing::warn!(topic = name, error = %e, "gossip re-join failed"),
            }
        }
    }
}

/// Legacy peer hints + our gateway address, on both topics (a node may be joined on only one of them).
async fn publish_peers(senders: [GossipSender; 2], table: Arc<crate::p2p_legacy::PeerTable>, gateway: Option<String>) {
    let mut tick = tokio::time::interval(Duration::from_secs(if gateway.is_some() { 120 } else { 300 }));
    tokio::time::sleep(Duration::from_secs(15)).await; // let the gossip swarm form, then announce right away
    loop {
        tick.tick().await;
        let peers: Vec<super::proto::PeerHint> = table
            .snapshot()
            .into_iter()
            .filter(|p| p.successes > 0 && p.failures == 0)
            .take(32)
            .map(|p| super::proto::PeerHint { ip: p.ip, port: table.port(), height: p.height, latency_ms: p.latency_ms, version: p.version })
            .collect();
        if peers.is_empty() && gateway.is_none() {
            continue;
        }
        let count = peers.len();
        let payload = GossipMessage::Peers { peers, gateway: gateway.clone() }.encode();
        for sender in &senders {
            match sender.broadcast(payload.clone().into()).await {
                Ok(()) => tracing::info!(count, gateway = gateway.as_deref().unwrap_or("-"), "legacy peer reputation shared over iroh gossip"),
                Err(e) => tracing::debug!(error = %e, "peer hint broadcast failed"),
            }
        }
    }
}

/// Publish blocks that advanced our tip from non-gossip sources (legacy port, own forging).
async fn publish_blocks(sender: GossipSender, storage: Arc<Storage>, peers: Arc<super::IrohPeers>, seen: Arc<Mutex<Seen>>) {
    let mut last = storage.get_last_height().unwrap_or(0);
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tick.tick().await;
        let tip = storage.get_last_height().unwrap_or(last);
        if tip <= last {
            continue;
        }
        // during catch-up we are far behind: publishing history is pointless, just follow the tip
        if tip - last > 10 || peers.is_empty() {
            last = tip;
            continue;
        }
        for h in last + 1..=tip {
            let from_gossip = seen.lock().unwrap_or_else(|e| e.into_inner()).heights.contains(&h);
            if from_gossip {
                continue;
            }
            if let Ok(Some(block)) = storage.get_block_by_height(h) {
                if let Err(e) = sender.broadcast(GossipMessage::Block { block }.encode().into()).await {
                    tracing::debug!(error = %e, height = h, "gossip block broadcast failed");
                }
            }
        }
        last = tip;
    }
}
