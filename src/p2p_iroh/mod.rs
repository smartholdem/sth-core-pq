//! Author: TechnoL0g
//!
//! Web 4.0 P2P layer on iroh (QUIC, NAT traversal, relay fallback): gossip topics for blocks and
//! transactions plus a tiny request/response protocol (`GetStatus`, `GetBlocks`) on our own ALPN.
//! Runs next to the legacy port 4001 until the network has fully migrated to sth-core nodes.

pub mod delegates;
pub mod finality;
mod gossip;
mod peers;
pub mod proto;
mod rpc;

pub use peers::{IrohPeerInfo, IrohPeers};
pub use proto::{GossipMessage, Request, Response};
pub use rpc::{fetch_blocks, fetch_finality, fetch_status, RpcHandler, ALPN};

use crate::error::{Error, Result};
use crate::mempool::Mempool;
use crate::storage::Storage;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointId, SecretKey};
use iroh_gossip::{Gossip, TopicId};
use std::path::Path;
use std::sync::Arc;

pub struct IrohNode {
    pub endpoint: Endpoint,
    pub router: Router,
    pub gossip: Gossip,
    pub peers: Arc<IrohPeers>,
    pub topics: Topics,
    /// Manually pinned peer addresses (LAN peers, tests) — consulted before DNS / relay discovery.
    pub lookup: iroh::address_lookup::MemoryLookup,
    /// Our own public legacy address announced to peers (gateway nodes).
    pub gateway: Option<String>,
    /// Relay servers configured at bind time (empty = relays disabled).
    pub relay_urls: Vec<String>,
    /// Delegate keys forging on this node (set by the forger) — announced over gossip so the network can count Rust delegates.
    pub forging_keys: std::sync::Mutex<Vec<crate::crypto::KeyPair>>,
    /// Publish signed `Delegates` announces (node.yaml `delegate.announce`); finality votes are sent regardless.
    pub announce: std::sync::atomic::AtomicBool,
    /// Delegates proven to forge on Rust nodes (ours + announced by peers).
    pub rust_delegates: delegates::RustDelegates,
    /// SHIP-35 finality votes / certificates.
    pub finality: Arc<finality::FinalityTracker>,
}

#[derive(Debug, Clone, Copy)]
pub struct Topics {
    pub blocks: TopicId,
    pub transactions: TopicId,
    pub finality: TopicId,
}

impl Topics {
    /// Topics are bound to the network by nethash so testnets never mix with mainnet.
    pub fn for_network(nethash: &str) -> Self {
        let id = |kind: &str| TopicId::from_bytes(crate::crypto::sha256(format!("sth/{nethash}/{kind}").as_bytes()));
        Self { blocks: id("blocks"), transactions: id("transactions"), finality: id("finality") }
    }
}

/// Load the node secret key from `path`, creating a fresh one when the file does not exist.
pub fn load_or_create_secret(path: &Path) -> Result<SecretKey> {
    if path.exists() {
        let hex_str = std::fs::read_to_string(path).map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        let bytes = hex::decode(hex_str.trim())?;
        let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| Error::Config(format!("{}: expected 32-byte key", path.display())))?;
        return Ok(SecretKey::from_bytes(&arr));
    }
    let key = SecretKey::generate();
    std::fs::write(path, hex::encode(key.to_bytes())).map_err(|e| Error::Config(format!("cannot write {}: {e}", path.display())))?;
    tracing::info!(file = %path.display(), "iroh secret key generated");
    Ok(key)
}

pub fn parse_endpoint_id(s: &str) -> Result<EndpointId> {
    s.trim().parse::<EndpointId>().map_err(|e| Error::Config(format!("invalid iroh EndpointId '{s}': {e}")))
}

/// NETFORY n1 relay servers (iroh-relay), always included when relays are enabled.
pub const N1_RELAYS: &[&str] = &["https://relay-fsn7.sth.cx", "https://relay-ru1.sth.cx"];

/// Which relay servers the endpoint may use. `None` at spawn = relays disabled.
#[derive(Debug, Clone, Default)]
pub struct RelaySetup {
    /// Add the public n0 relays (use1/usw1/euc1/aps1.relay.n0.iroh.link).
    pub n0: bool,
    /// Extra relay URLs from node.yaml (`p2p.iroh.relays`).
    pub extra: Vec<String>,
}

impl RelaySetup {
    /// n1 + optional n0 + extra, deduplicated; invalid URLs are reported and skipped.
    pub fn relay_map(&self) -> iroh::RelayMap {
        let mut urls: Vec<iroh::RelayUrl> = Vec::new();
        if self.n0 {
            urls.extend(iroh::defaults::prod::default_relay_map().urls::<Vec<_>>());
        }
        for raw in N1_RELAYS.iter().map(|s| s.to_string()).chain(self.extra.iter().cloned()) {
            match raw.trim().parse::<iroh::RelayUrl>() {
                Ok(u) if !urls.contains(&u) => urls.push(u),
                Ok(_) => {}
                Err(e) => tracing::warn!(url = raw, error = %e, "ignoring invalid relay url"),
            }
        }
        iroh::RelayMap::from_iter(urls)
    }
}

impl From<bool> for RelaySetup {
    fn from(n0: bool) -> Self {
        Self { n0, extra: Vec::new() }
    }
}

impl IrohNode {
    /// Bind the endpoint, register the RPC handler + gossip, join both topics and start the
    /// gossip intake / publish loops.
    pub async fn spawn(
        secret: SecretKey,
        bootstrap: Vec<EndpointId>,
        serve_blocks: bool,
        relay: Option<RelaySetup>,
        storage: Arc<Storage>,
        mempool: Arc<Mempool>,
        verify: bool,
        legacy_peers: Option<Arc<crate::p2p_legacy::PeerTable>>,
        gateway: Option<String>,
    ) -> Result<Arc<Self>> {
        let lookup = iroh::address_lookup::MemoryLookup::new();
        let (builder, relay_urls) = match &relay {
            Some(setup) => {
                let map = setup.relay_map();
                let urls: Vec<String> = map.urls::<Vec<iroh::RelayUrl>>().iter().map(|u| u.to_string()).collect();
                (Endpoint::builder(presets::N0).relay_mode(iroh::RelayMode::Custom(map)), urls)
            }
            None => (Endpoint::builder(presets::N0DisableRelay), Vec::new()),
        };
        let endpoint = builder
            .secret_key(secret)
            .address_lookup(lookup.clone())
            .bind()
            .await
            .map_err(|e| Error::Sync(format!("iroh bind: {e}")))?;
        let peers = Arc::new(IrohPeers::default());
        let gossip = Gossip::builder().spawn(endpoint.clone());
        let mut builder = Router::builder(endpoint.clone()).accept(iroh_gossip::ALPN, gossip.clone());
        if serve_blocks {
            builder = builder.accept(ALPN, RpcHandler::new(storage.clone(), peers.clone()));
        }
        let router = builder.spawn();
        let topics = Topics::for_network(&storage.network().nethash);
        tracing::info!(endpoint_id = %endpoint.id(), bootstrap = bootstrap.len(), "iroh endpoint online (share this EndpointId with peers)");
        if relay_urls.is_empty() {
            tracing::info!("iroh relays disabled (direct connections only)");
        } else {
            tracing::info!(relays = relay_urls.join(", "), "iroh relays");
        }
        for id in &bootstrap {
            peers.seen(*id, None);
        }
        let node = Arc::new(Self { endpoint, router, gossip, peers, topics, lookup, gateway, relay_urls, forging_keys: Default::default(), announce: std::sync::atomic::AtomicBool::new(true), rust_delegates: Default::default(), finality: Arc::new(finality::FinalityTracker::new(storage.clone())) });
        gossip::start(node.clone(), bootstrap, storage, mempool, verify, legacy_peers).await?;
        Ok(node)
    }

    /// `GetStatus` every known peer concurrently (heights + latency for the download scheduler).
    pub async fn refresh_peers(&self) -> usize {
        let ids: Vec<EndpointId> = self.peers.snapshot().into_iter().map(|p| p.id).filter(|id| *id != self.id()).collect();
        let results = futures::future::join_all(ids.into_iter().map(|id| async move {
            tokio::time::timeout(std::time::Duration::from_secs(15), rpc::fetch_status(&self.endpoint, &self.peers, id)).await.ok().and_then(|r| r.ok())
        }))
        .await;
        let alive = results.iter().filter(|r| r.is_some()).count();
        if !self.peers.is_empty() {
            tracing::info!(alive, known = self.peers.len(), best_height = self.peers.best_height(), "iroh peers refreshed");
        }
        alive
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Registers the delegate keys forging here: they vote for finality (SHIP-35) and, with `announce`, are announced to
    /// the network every 2 min (and counted locally at once).
    pub fn set_forging_keys(&self, keys: Vec<crate::crypto::KeyPair>, announce: bool) {
        self.announce.store(announce, std::sync::atomic::Ordering::Relaxed);
        if announce {
            let me = self.endpoint.id();
            let announces = delegates::sign_announces(&keys, &me);
            self.rust_delegates.record(me, &announces);
        }
        *self.forging_keys.lock().unwrap_or_else(|e| e.into_inner()) = keys;
    }

    /// Home relay URLs currently connected (empty = relays off or not connected yet).
    pub fn connected_relays(&self) -> Vec<String> {
        use iroh::Watcher;
        self.endpoint.home_relay_status().get().iter().filter(|s| s.is_connected()).map(|s| s.url().to_string()).collect()
    }

    /// Pin a peer with known direct addresses (no discovery needed).
    pub fn add_peer_addr(&self, addr: iroh::EndpointAddr) {
        self.peers.seen(addr.id, None);
        self.lookup.add_endpoint_info(addr);
    }

    pub async fn shutdown(&self) {
        if let Err(e) = self.router.shutdown().await {
            tracing::warn!(error = %e, "iroh router shutdown");
        }
    }
}
