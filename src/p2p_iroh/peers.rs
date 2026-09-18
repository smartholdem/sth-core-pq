//! Author: TechnoL0g
//!
//! Peer table of the iroh layer (feeds `/api/node/peers` with `source: "iroh"`).

use iroh::EndpointId;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct IrohPeerInfo {
    pub id: EndpointId,
    pub height: u64,
    pub latency_ms: Option<u64>,
    pub last_seen: Instant,
    pub messages: u64,
    pub failures: u64,
    /// Gossip neighbour on at least one topic (blocks / transactions).
    pub neighbor: bool,
    pub neighbor_topics: [bool; 3],
    /// Public `ip:4001` announced by a gateway node.
    pub gateway: Option<String>,
}

/// Gossip topic index for [`IrohPeers::set_neighbor`].
pub const TOPIC_BLOCKS: usize = 0;
pub const TOPIC_TXS: usize = 1;
pub const TOPIC_FINALITY: usize = 2;

#[derive(Default)]
pub struct IrohPeers {
    peers: Mutex<HashMap<EndpointId, IrohPeerInfo>>,
}

impl IrohPeers {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<EndpointId, IrohPeerInfo>> {
        self.peers.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn entry(map: &mut HashMap<EndpointId, IrohPeerInfo>, id: EndpointId) -> &mut IrohPeerInfo {
        map.entry(id).or_insert_with(|| IrohPeerInfo {
            id,
            height: 0,
            latency_ms: None,
            last_seen: Instant::now(),
            messages: 0,
            failures: 0,
            neighbor: false,
            neighbor_topics: [false; 3],
            gateway: None,
        })
    }

    pub fn seen(&self, id: EndpointId, height: Option<u64>) {
        let mut map = self.lock();
        let p = Self::entry(&mut map, id);
        p.last_seen = Instant::now();
        p.messages += 1;
        if let Some(h) = height {
            p.height = p.height.max(h);
        }
    }

    /// Neighbour state per topic; a peer stays a neighbour while any topic still has it.
    pub fn set_neighbor(&self, id: EndpointId, topic: usize, up: bool) {
        let mut map = self.lock();
        let p = Self::entry(&mut map, id);
        p.neighbor_topics[topic] = up;
        p.neighbor = p.neighbor_topics.iter().any(|t| *t);
    }

    /// Number of gossip neighbours on `topic`.
    pub fn neighbors_on(&self, topic: usize) -> usize {
        self.lock().values().filter(|p| p.neighbor_topics[topic]).count()
    }

    pub fn record_rpc(&self, id: EndpointId, latency_ms: Option<u64>, height: Option<u64>) {
        let mut map = self.lock();
        let p = Self::entry(&mut map, id);
        p.last_seen = Instant::now();
        match latency_ms {
            Some(ms) => p.latency_ms = Some(p.latency_ms.map_or(ms, |old| (old * 3 + ms) / 4)),
            None => p.failures += 1,
        }
        if let Some(h) = height {
            p.height = p.height.max(h);
        }
    }

    pub fn set_gateway(&self, id: EndpointId, addr: Option<String>) {
        let mut map = self.lock();
        Self::entry(&mut map, id).gateway = addr;
    }

    /// Known gateway addresses (`ip:port`) announced over gossip.
    pub fn gateways(&self) -> Vec<String> {
        let mut v: Vec<String> = self.lock().values().filter_map(|p| p.gateway.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn snapshot(&self) -> Vec<IrohPeerInfo> {
        let mut v: Vec<IrohPeerInfo> = self.lock().values().cloned().collect();
        v.sort_by_key(|p| (!p.neighbor, std::cmp::Reverse(p.height)));
        v
    }

    pub fn best_height(&self) -> u64 {
        self.lock().values().map(|p| p.height).max().unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}
