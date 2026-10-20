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
    let txs_topic = node.gossip.subscribe(node.topics.transactions, bootstrap.clone()).await.map_err(|e| Error::Sync(format!("gossip subscribe: {e}")))?;
    let finality_topic = node.gossip.subscribe(node.topics.finality, bootstrap.clone()).await.map_err(|e| Error::Sync(format!("gossip subscribe: {e}")))?;
    // sthpq address book (optional module): its own topic so nodes without the module never see the messages
    if let Some(book) = node.addrbook.get().cloned() {
        let addrs_topic = node.gossip.subscribe(node.topics.addrs, bootstrap).await.map_err(|e| Error::Sync(format!("gossip subscribe: {e}")))?;
        tokio::spawn(super::addrbook::run(node.clone(), book, node.secret.clone(), addrs_topic, legacy_peers.clone()));
    }
    let (block_sender, mut block_receiver) = blocks_topic.split();
    let (tx_sender, mut tx_receiver) = txs_topic.split();
    let (fin_sender, mut fin_receiver) = finality_topic.split();
    // finality votes + equivocation proofs intake (SHIP-35)
    {
        let node = node.clone();
        let sender = fin_sender.clone();
        tokio::spawn(async move {
            while let Some(event) = fin_receiver.next().await {
                match event {
                    Ok(Event::Received(msg)) => match GossipMessage::decode(&msg.content) {
                        Some(GossipMessage::Finality { votes }) => {
                            node.peers.seen(msg.delivered_from, None);
                            let out = node.finality.record(&votes);
                            tracing::debug!(peer = %msg.delivered_from.fmt_short(), votes = votes.len(), ok = out.accepted, "finality votes received");
                            for proof in out.proofs {
                                if let Err(e) = sender.broadcast(GossipMessage::Equivocation { proof }.encode().into()).await {
                                    tracing::debug!(error = %e, "equivocation proof broadcast failed");
                                }
                            }
                        }
                        Some(GossipMessage::Equivocation { proof }) => match node.finality.adopt_proof(&proof) {
                            Ok(true) => {
                                // relay once so the whole network learns about the slashable delegate
                                let _ = sender.broadcast(GossipMessage::Equivocation { proof }.encode().into()).await;
                            }
                            Ok(false) => {}
                            Err(e) => tracing::debug!(peer = %msg.delivered_from.fmt_short(), error = %e, "equivocation proof rejected"),
                        },
                        _ => {}
                    },
                    Ok(Event::NeighborUp(id)) => node.peers.set_neighbor(id, super::peers::TOPIC_FINALITY, true),
                    Ok(Event::NeighborDown(id)) => node.peers.set_neighbor(id, super::peers::TOPIC_FINALITY, false),
                    Ok(Event::Lagged) => tracing::warn!("iroh finality gossip lagged"),
                    Err(e) => {
                        tracing::warn!(error = %e, "iroh finality gossip stream ended");
                        break;
                    }
                }
            }
        });
    }

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
                            Some(GossipMessage::Delegates { delegates }) => {
                                on_delegates(&node, msg.delivered_from, &delegates);
                                continue;
                            }
                            _ => continue,
                        };
                        node.peers.seen(msg.delivered_from, Some(block.height));
                        if let Err(e) = on_block(&node, &storage, &seen, msg.delivered_from, block, verify).await {
                            match e {
                                Error::StaleTip(msg) => tracing::debug!(%msg, "gossip block already applied by another path"),
                                e => tracing::debug!(error = %e, "gossip block not applied"),
                            }
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
                        if let Some(GossipMessage::Delegates { delegates }) = &decoded {
                            on_delegates(&node, msg.delivered_from, delegates);
                            continue;
                        }
                        let Some(GossipMessage::Transactions { transactions }) = decoded else { continue };
                        node.peers.seen(msg.delivered_from, None);
                        {
                            let mut s = seen.lock().unwrap_or_else(|e| e.into_inner());
                            s.tx_ids.extend(transactions.iter().filter_map(|t| t.id.clone()));
                            s.trim();
                        }
                        let (resp, _) = mempool.add_many_from(transactions, &crate::antispam::TxOrigin::IrohPeer(msg.delivered_from.to_string())).await;
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
    if let Some(table) = legacy_peers.clone() {
        tokio::spawn(publish_peers([block_sender.clone(), tx_sender.clone()], table, node.gateway.clone()));
    }
    tokio::spawn(rejoin_watchdog(node.clone(), [block_sender.clone(), tx_sender.clone(), fin_sender.clone()], bootstrap_for_watchdog, legacy_peers.clone()));
    tokio::spawn(publish_finality(node.clone(), fin_sender, storage.clone()));
    tokio::spawn(sync_finality(node.clone(), storage.clone()));
    tokio::spawn(publish_delegates(node.clone(), [block_sender.clone(), tx_sender.clone()]));
    tokio::spawn(publish_transactions(tx_sender.clone(), mempool.subscribe(), Some(seen.clone())));
    tokio::spawn(publish_transactions(tx_sender, mempool.subscribe_rebroadcast(), None));
    tokio::spawn(publish_blocks(block_sender, storage, node.peers.clone(), seen));
    Ok(())
}

/// Apply a gossiped block at tip+1, or gap-fill from the sender when it is ahead.
async fn on_block(node: &IrohNode, storage: &Arc<Storage>, seen: &Mutex<Seen>, from: EndpointId, block: Block, verify: bool) -> Result<()> {
    let tip = tip_of(storage)?;
    if block.height <= tip.height {
        // same height, other id → double-forge guard (optional module) checks the generator
        if block.height + 1 >= tip.height {
            if let Some(ours) = storage.get_block_by_height(block.height)? {
                if ours.id != block.id {
                    crate::delegate::guard::on_competing(crate::intake::Source::GossipIroh, &from.fmt_short().to_string(), &ours, &block);
                    crate::intake::prefer_earlier_slot(storage, &ours, &block, &from.fmt_short().to_string()).await;
                }
            }
        }
        return Ok(());
    }
    if block.height == tip.height + 1 {
        let (h, ts, gen) = (block.height, block.timestamp, block.generator_public_key.clone());
        let header = Block { transactions: Vec::new(), ..block.clone() };
        let tip = apply(storage, vec![block], tip, verify).await?;
        crate::intake::record(crate::intake::Source::GossipIroh, from.fmt_short().to_string(), h, ts, &gen, storage.network());
        crate::delegate::guard::on_accepted(crate::intake::Source::GossipIroh, &from.fmt_short().to_string(), &header, storage.network());
        let mut s = seen.lock().unwrap_or_else(|e| e.into_inner());
        s.heights.insert(h);
        s.trim();
        tracing::info!(height = tip.height, peer = %from.fmt_short(), "block applied from iroh gossip");
        return Ok(());
    }
    // behind: pull the gap from the announcing peer (bounded so a bogus announcement cannot stall us)
    let tip = fill_gap_from(node, storage, from, block.height, verify).await?;
    tracing::info!(height = tip.height, peer = %from.fmt_short(), "gap filled from iroh peer");
    let mut s = seen.lock().unwrap_or_else(|e| e.into_inner());
    s.heights.extend(tip.height.saturating_sub(64)..=tip.height);
    s.trim();
    Ok(())
}

/// Pull blocks from `from` until the local tip reaches `target` (≤ 8 RPC rounds); also used by the stall watchdog.
pub async fn fill_gap_from(node: &IrohNode, storage: &Arc<Storage>, from: EndpointId, target: u64, verify: bool) -> Result<ChainTip> {
    let mut tip = tip_of(storage)?;
    let mut rounds = 0;
    while tip.height < target && rounds < 8 {
        let batch = fetch_blocks(&node.endpoint, &node.peers, from, tip.height, super::rpc::MAX_BLOCKS).await?;
        if batch.is_empty() {
            break;
        }
        tip = apply(storage, batch, tip, verify).await?;
        rounds += 1;
    }
    Ok(tip)
}

/// `seen = Some` skips transactions that came in over gossip (fresh admissions); `None` re-publishes everything (re-broadcast
/// of pending transactions the network has not forged).
async fn publish_transactions(sender: GossipSender, mut events: tokio::sync::broadcast::Receiver<Vec<crate::models::Transaction>>, seen: Option<Arc<Mutex<Seen>>>) {
    loop {
        match events.recv().await {
            Ok(batch) => {
                let fresh: Vec<_> = match &seen {
                    Some(seen) => {
                        let s = seen.lock().unwrap_or_else(|e| e.into_inner());
                        batch.into_iter().filter(|t| t.id.as_ref().map_or(true, |id| !s.tx_ids.contains(id))).collect()
                    }
                    None => batch,
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
            table.set_version(&h.ip, &h.version, false);
            added += 1;
        }
    }
    if added > 0 {
        tracing::info!(added, from = %from.fmt_short(), "legacy peers learned from iroh gossip");
    }
}

/// A topic whose neighbours all dropped never re-bootstraps by itself: re-join the configured
/// bootstrap peers (and any peer we still talk to over RPC) whenever a topic has no neighbour.
/// Seconds without any connected home relay before relay-down mode kicks in.
const RELAY_DOWN_AFTER: Duration = Duration::from_secs(60);

async fn rejoin_watchdog(node: Arc<super::IrohNode>, senders: [GossipSender; 3], bootstrap: Vec<EndpointId>, legacy: Option<Arc<crate::p2p_legacy::PeerTable>>) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    tick.tick().await;
    let mut relay_down_since: Option<std::time::Instant> = None;
    let mut relay_down_mode = false;
    loop {
        tick.tick().await;
        // relay-down mode: every configured relay unreachable → the network must keep working on direct addresses.
        // Pull fresh sthpq records over the legacy TCP port (peers.json seeds, no relay / DNS / DHT involved), pin
        // them with direct addresses only, and re-join gossip with those peers whenever neighbours run low.
        let relays_configured = !node.relay_urls.is_empty();
        let connected = node.connected_relays();
        if relays_configured && connected.is_empty() {
            let since = *relay_down_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= RELAY_DOWN_AFTER {
                if !relay_down_mode {
                    relay_down_mode = true;
                    let msg = format!("ALL RELAYS DOWN for {}s ({}) — running on direct addresses: pulling sthpq records over the legacy port and re-joining gossip with pinned peers", since.elapsed().as_secs(), node.relay_urls.join(", "));
                    tracing::warn!("{msg}");
                    crate::alerts::notify("relay-down", msg, false);
                }
                let mut pinned = 0usize;
                if let Some(book) = node.addrbook.get() {
                    if let Some(table) = &legacy {
                        super::addrbook::pull_over_legacy(book, table, 6).await;
                    }
                    for rec in book.fresh(super::addrbook::MAX_RECORDS) {
                        if let Some(addr) = super::addrbook::direct_only_addr(&rec).filter(|a| a.id != node.id()) {
                            node.add_peer_addr(addr);
                            pinned += 1;
                        }
                    }
                }
                let low = (0..3).filter(|t| node.peers.neighbors_on(*t) < 2).count();
                tracing::info!(pinned, topics_low = low, "relay-down mode: direct dial info refreshed");
                if low > 0 {
                    let targets: Vec<EndpointId> = node.peers.snapshot().into_iter().filter(|p| p.failures < 3 && p.id != node.id()).map(|p| p.id).collect();
                    for (topic, sender) in senders.iter().enumerate() {
                        if node.peers.neighbors_on(topic) < 2 && !targets.is_empty() {
                            if let Err(e) = sender.join_peers(targets.clone()).await {
                                tracing::debug!(topic, error = %e, "relay-down re-join failed");
                            }
                        }
                    }
                }
            }
        } else {
            if relay_down_mode {
                let msg = format!("relay back: {} connected after {}s on direct addresses", connected.join(", "), relay_down_since.map(|t| t.elapsed().as_secs()).unwrap_or(0));
                tracing::info!("{msg}");
                crate::alerts::notify("relay-down", msg, true);
            }
            relay_down_since = None;
            relay_down_mode = false;
        }
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
            let name = ["blocks", "transactions", "finality"][topic];
            match sender.join_peers(targets.clone()).await {
                Ok(()) => tracing::info!(topic = name, peers = targets.len(), "no gossip neighbours, re-joining"),
                Err(e) => tracing::warn!(topic = name, error = %e, "gossip re-join failed"),
            }
        }
    }
}

fn on_delegates(node: &super::IrohNode, from: EndpointId, announces: &[super::proto::DelegateAnnounce]) {
    let ok = node.rust_delegates.record(from, announces);
    if ok > 0 {
        node.peers.seen(from, None);
        tracing::debug!(peer = %from.fmt_short(), delegates = ok, "rust delegate announce received");
    } else if !announces.is_empty() {
        tracing::debug!(peer = %from.fmt_short(), "rust delegate announce rejected (bad signature or clock)");
    }
}

/// Signed "these delegates forge on this Rust node" announces, every 2 min on both topics.
async fn publish_delegates(node: Arc<super::IrohNode>, senders: [GossipSender; 2]) {
    let mut tick = tokio::time::interval(Duration::from_secs(120));
    tokio::time::sleep(Duration::from_secs(20)).await;
    loop {
        tick.tick().await;
        let keys = node.forging_keys.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if keys.is_empty() || !node.announce.load(std::sync::atomic::Ordering::Relaxed) {
            continue;
        }
        let me = node.endpoint.id();
        let delegates = super::delegates::sign_announces(&keys, &me);
        node.rust_delegates.record(me, &delegates);
        let payload = GossipMessage::Delegates { delegates }.encode();
        for sender in &senders {
            if let Err(e) = sender.broadcast(payload.clone().into()).await {
                tracing::debug!(error = %e, "delegate announce broadcast failed");
            }
        }
        tracing::debug!(count = keys.len(), "rust delegate announce sent");
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

/// Votes per `Finality` gossip message (~2.6 KiB of JSON), safe for peers with the 4 KiB default message limit.
pub const FINALITY_VOTES_PER_MESSAGE: usize = 8;

/// SHIP-35: vote for every new tip with our active delegate keys (any source: forged, gossip, legacy port); the vote is
/// re-sent once a slot later while the block is not yet certified, so late neighbours still collect it.
async fn publish_finality(node: Arc<super::IrohNode>, sender: GossipSender, storage: Arc<Storage>) {
    // event-driven like `publish_blocks`: the vote leaves within milliseconds of the block being applied; the 500 ms tick
    // remains for the one-slot-later resend and as a safety net
    let mut events = storage.subscribe_chain();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut voted: Option<(u64, String, std::time::Instant, bool)> = None;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            ev = events.recv() => {
                if matches!(ev, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                    return;
                }
            }
        }
        let Ok(Some(tip)) = storage.get_last_block() else { continue };
        let Some(id) = tip.id.clone() else { continue };
        let keys = node.forging_keys.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if keys.is_empty() {
            continue;
        }
        let blocktime = storage.network().milestone(tip.height).blocktime as u64;
        let resend = matches!(&voted, Some((h, i, at, false)) if *h == tip.height && *i == id && at.elapsed() >= Duration::from_secs(blocktime) && !node.finality.is_final(*h));
        let fresh = !matches!(&voted, Some((h, i, _, _)) if *h == tip.height && *i == id);
        if !fresh && !resend {
            continue;
        }
        let votes = node.finality.own_votes(&keys, tip.height, &id);
        if votes.is_empty() {
            voted = Some((tip.height, id, std::time::Instant::now(), true));
            continue;
        }
        // ≤ FINALITY_VOTES_PER_MESSAGE votes per gossip message: a JSON vote is ~320 bytes and every peer must accept the
        // message under its own max_message_size (4 KiB on nodes before 0.25.12); the broadcast never blocks the loop
        let mut sent = 0;
        for chunk in votes.chunks(FINALITY_VOTES_PER_MESSAGE) {
            match tokio::time::timeout(Duration::from_secs(5), sender.broadcast(GossipMessage::Finality { votes: chunk.to_vec() }.encode().into())).await {
                Ok(Ok(())) => sent += chunk.len(),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, height = tip.height, votes = chunk.len(), "finality vote broadcast failed — own votes are counted locally, peers miss them");
                    node.finality.note_broadcast_failed(e.to_string());
                }
                Err(_) => {
                    tracing::warn!(height = tip.height, "finality vote broadcast timed out (gossip topic stuck?) — own votes are counted locally");
                    node.finality.note_broadcast_failed("broadcast timed out after 5s");
                }
            }
        }
        if sent > 0 {
            tracing::info!(height = tip.height, votes = sent, resend, "finality votes published");
        }
        voted = Some((tip.height, id, std::time::Instant::now(), resend));
    }
}

/// SHIP-35 catch-up: while our highest certificate lags the tip by more than two slots (fresh node, missed votes), ask a few
/// healthy iroh peers `GetFinality` and adopt the best verified certificate. Once at start, then every minute.
async fn sync_finality(node: Arc<super::IrohNode>, storage: Arc<Storage>) {
    tokio::time::sleep(Duration::from_secs(10)).await;
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        let tip = storage.get_last_height().unwrap_or(0);
        let ours = node.finality.finalized().map(|(h, _)| h).unwrap_or(0);
        if tip <= ours + 2 {
            continue;
        }
        let peers: Vec<EndpointId> = node.peers.snapshot().into_iter().filter(|p| p.failures < 3 && p.id != node.id()).map(|p| p.id).take(3).collect();
        for id in peers {
            let Ok(Ok(Some(cert))) = tokio::time::timeout(Duration::from_secs(10), super::fetch_finality(&node.endpoint, &node.peers, id)).await else { continue };
            if cert.height <= ours {
                continue;
            }
            match node.finality.adopt_certificate(&cert) {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => tracing::debug!(peer = %id.fmt_short(), error = %e, "finality certificate from peer rejected"),
            }
        }
    }
}

/// Publish blocks that advanced our tip from non-gossip sources (legacy port, own forging).
async fn publish_blocks(sender: GossipSender, storage: Arc<Storage>, peers: Arc<super::IrohPeers>, seen: Arc<Mutex<Seen>>) {
    let mut last = storage.get_last_height().unwrap_or(0);
    // event-driven: a block applied here (forged, legacy push/pull) is on the wire within milliseconds instead of
    // waiting for the next 2 s poll (the poll stays as a safety net)
    let mut events = storage.subscribe_chain();
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            ev = events.recv() => {
                match ev {
                    Ok(crate::storage::ChainEvent::Applied(_)) => {}
                    Ok(crate::storage::ChainEvent::RolledBack { height }) => { last = last.min(height); continue; }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        }
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
