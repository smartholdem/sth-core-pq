//! Author: TechnoL0g
//!
//! Web 4.0 P2P layer on iroh (QUIC, NAT traversal, relay fallback): gossip topics for blocks and
//! transactions plus a tiny request/response protocol (`GetStatus`, `GetBlocks`) on our own ALPN.
//! Runs next to the legacy port 4001 until the network has fully migrated to sth-core nodes.

mod gossip;
mod peers;
pub mod proto;
mod rpc;

pub use peers::{IrohPeerInfo, IrohPeers};
pub use proto::{GossipMessage, Request, Response};
pub use rpc::{fetch_blocks, fetch_status, RpcHandler, ALPN};

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
}

#[derive(Debug, Clone, Copy)]
pub struct Topics {
    pub blocks: TopicId,
    pub transactions: TopicId,
}

impl Topics {
    /// Topics are bound to the network by nethash so testnets never mix with mainnet.
    pub fn for_network(nethash: &str) -> Self {
        let id = |kind: &str| TopicId::from_bytes(crate::crypto::sha256(format!("sth/{nethash}/{kind}").as_bytes()));
        Self { blocks: id("blocks"), transactions: id("transactions") }
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

impl IrohNode {
    /// Bind the endpoint, register the RPC handler + gossip, join both topics and start the
    /// gossip intake / publish loops.
    pub async fn spawn(
        secret: SecretKey,
        bootstrap: Vec<EndpointId>,
        serve_blocks: bool,
        relay: bool,
        storage: Arc<Storage>,
        mempool: Arc<Mempool>,
        verify: bool,
        legacy_peers: Option<Arc<crate::p2p_legacy::PeerTable>>,
        gateway: Option<String>,
    ) -> Result<Arc<Self>> {
        let lookup = iroh::address_lookup::MemoryLookup::new();
        let builder = if relay { Endpoint::builder(presets::N0) } else { Endpoint::builder(presets::N0DisableRelay) };
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
        for id in &bootstrap {
            peers.seen(*id, None);
        }
        let node = Arc::new(Self { endpoint, router, gossip, peers, topics, lookup, gateway });
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
