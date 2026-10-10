//! Author: TechnoL0g
//!
//! Web 4.0 P2P layer on iroh (QUIC, NAT traversal, relay fallback): gossip topics for blocks and
//! transactions plus a tiny request/response protocol (`GetStatus`, `GetBlocks`) on our own ALPN.
//! Runs next to the legacy port 4001 until the network has fully migrated to sth-core nodes.

pub mod addrbook;
pub mod delegates;
pub mod finality;
mod gossip;
pub use gossip::fill_gap_from;
mod peers;
pub mod proto;
pub mod relay_guard;
pub mod rpc;
pub mod wallet_api;

/// iroh-gossip message size limit of this node (receive + send).
pub const GOSSIP_MAX_MESSAGE_SIZE: usize = 64 * 1024;
pub use gossip::FINALITY_VOTES_PER_MESSAGE;
pub mod ws_bridge;

pub use peers::{IrohPeerInfo, IrohPeers};
pub use proto::{GossipMessage, Request, Response};
pub use rpc::{fetch_blocks, fetch_finality, fetch_status, RpcHandler, ALPN};
pub use wallet_api::WalletApi;

use crate::error::{Error, Result};
use crate::mempool::Mempool;
use crate::storage::Storage;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
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
    /// SHIP-44 wallet protocols (`netfory/api/*`, `sth/events/1`); inert until `wallet_api.attach(...)`.
    pub wallet_api: Arc<WalletApi>,
    /// Nethash the node serves (binds address-book records to the network).
    pub nethash: String,
    /// Node key (signs sthpq address records; the same key the endpoint runs on).
    pub secret: SecretKey,
    /// sthpq address book (`p2p.iroh.discovery`), set when the module is enabled.
    pub addrbook: std::sync::OnceLock<Arc<addrbook::AddrBook>>,
}

/// `p2p.iroh.discovery` runtime options: the config + bootstrap entries given as `id@ip:port`.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryOpts {
    pub cfg: crate::node_config::DiscoveryConfig,
    pub pinned: Vec<EndpointAddr>,
}

#[derive(Debug, Clone, Copy)]
pub struct Topics {
    pub blocks: TopicId,
    pub transactions: TopicId,
    pub finality: TopicId,
    /// sthpq address book announcements.
    pub addrs: TopicId,
}

impl Topics {
    /// Topics are bound to the network by nethash so testnets never mix with mainnet.
    pub fn for_network(nethash: &str) -> Self {
        let id = |kind: &str| TopicId::from_bytes(crate::crypto::sha256(format!("sth/{nethash}/{kind}").as_bytes()));
        Self { blocks: id("blocks"), transactions: id("transactions"), finality: id("finality"), addrs: id("addrs") }
    }
}

/// Mainline DHT address lookup (BEP-44 mutable item keyed by the node's Ed25519 key, pkarr SignedPacket value —
/// identical to what NETFORY clients publish/resolve). Only relay URLs and PUBLIC IPs are published so a node behind
/// NAT never leaks its LAN addresses; the endpoint's secret key signs the packets (set by `into_address_lookup`).
fn mainline_lookup(cfg: &crate::node_config::DiscoveryConfig, secret: &SecretKey) -> Result<iroh_mainline_address_lookup::DhtAddressLookup> {
    let mut dht = n0_mainline::Dht::builder();
    if !cfg.mainline_bootstrap.is_empty() {
        dht.extra_bootstrap(&cfg.mainline_bootstrap);
    }
    if cfg.mainline_port > 0 {
        dht.port(cfg.mainline_port);
    }
    if cfg.mainline_server {
        dht.server_mode();
    }
    let public_only = iroh::address_lookup::AddrFilter::new(|addrs| {
        std::borrow::Cow::Owned(
            addrs
                .iter()
                .filter(|a| match a {
                    iroh::TransportAddr::Relay(_) => true,
                    iroh::TransportAddr::Ip(sa) => addrbook::is_public(sa.ip()),
                    _ => false,
                })
                .cloned()
                .collect(),
        )
    });
    tracing::info!(extra_bootstrap = cfg.mainline_bootstrap.len(), port = cfg.mainline_port, server = cfg.mainline_server, "Mainline DHT address lookup enabled (pkarr / BEP-44)");
    iroh_mainline_address_lookup::DhtAddressLookup::builder()
        .dht_builder(dht)
        .addr_filter(public_only)
        .secret_key(secret.clone())
        .build()
        .map_err(|e| Error::Config(format!("Mainline DHT: {e}")))
}

/// Health of the Mainline DHT as seen by this node (`ntfry.discovery.dht`).
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DhtHealth {
    /// `probing` | `ok` | `unreachable`
    pub status: String,
    pub last_ok_unix: u64,
    pub last_probe_unix: u64,
    pub failures: u32,
}

static DHT_HEALTH: std::sync::Mutex<Option<DhtHealth>> = std::sync::Mutex::new(None);

pub fn dht_health() -> Option<DhtHealth> {
    DHT_HEALTH.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Consecutive failed probes before the `dht-unreachable` alert: a fresh record takes minutes to spread through the
/// DHT, so the first probes after a (re)start fail on perfectly reachable hosts.
const DHT_ALERT_AFTER_FAILURES: u32 = 3;

/// Resolve our own record from the DHT (first after 90 s, then 5 min / 15 min while failing, every 30 min once ok):
/// success proves bootstrap + publish work. Failures produce ONE clear WARN each and the alert only after
/// `DHT_ALERT_AFTER_FAILURES` in a row, instead of n0_mainline's error every 2 s (muted in main.rs).
async fn dht_health_loop(lookup: iroh_mainline_address_lookup::DhtAddressLookup, me: EndpointId, cfg: crate::node_config::DiscoveryConfig) {
    use futures::StreamExt;
    use iroh::address_lookup::AddressLookup;
    *DHT_HEALTH.lock().unwrap_or_else(|e| e.into_inner()) = Some(DhtHealth { status: "probing".into(), ..Default::default() });
    let mut wait = std::time::Duration::from_secs(90);
    loop {
        tokio::time::sleep(wait).await;
        let found = match lookup.resolve(me) {
            Some(mut stream) => tokio::time::timeout(std::time::Duration::from_secs(45), async { while let Some(item) = stream.next().await { if item.is_ok() { return true; } } false }).await.unwrap_or(false),
            None => false,
        };
        let now = addrbook::now_unix();
        let mut h = DHT_HEALTH.lock().unwrap_or_else(|e| e.into_inner());
        let st = h.get_or_insert_with(Default::default);
        st.last_probe_unix = now;
        if found {
            if st.status != "ok" {
                tracing::info!("Mainline DHT reachable: own record resolved (bootstrap + publish OK)");
            }
            st.status = "ok".into();
            st.last_ok_unix = now;
            st.failures = 0;
            wait = std::time::Duration::from_secs(1800);
        } else {
            st.failures += 1;
            st.status = if st.failures >= DHT_ALERT_AFTER_FAILURES { "unreachable".into() } else { "probing".into() };
            wait = std::time::Duration::from_secs(if st.failures < DHT_ALERT_AFTER_FAILURES { 300 } else { 900 });
            let (alert, failures) = (st.failures >= DHT_ALERT_AFTER_FAILURES, st.failures);
            let msg = format!(
                "Mainline DHT unreachable: own record not found after {} probe(s) — outbound UDP to the DHT bootstrap routers is blocked or throttled (ISP filtering of BitTorrent traffic is common). Discovery keeps working through the sthpq address book{}; add reachable seeders in discovery.mainline_bootstrap (e.g. a gateway with mainline_server: true) or set discovery.mainline: false",
                st.failures,
                if cfg.n0_dns { " and n0 DNS" } else { "" }
            );
            drop(h);
            if alert {
                tracing::warn!("{msg}");
                crate::alerts::notify("dht-unreachable", msg, false);
            } else {
                tracing::info!("Mainline DHT: own record not found yet (probe {failures} of {DHT_ALERT_AFTER_FAILURES} before the alert; a fresh record needs minutes to spread)");
            }
        }
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
    /// Optional relay guard module (drops a misbehaving non-protected relay from the map at runtime).
    pub guard: crate::node_config::RelayGuardConfig,
}

impl RelaySetup {
    /// Relays that the guard never removes: n1 + node.yaml `relays`.
    pub fn protected(&self) -> Vec<String> {
        N1_RELAYS.iter().map(|s| s.to_string()).chain(self.extra.iter().cloned()).collect()
    }

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
        Self { n0, extra: Vec::new(), guard: Default::default() }
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
        Self::spawn_with(secret, bootstrap, serve_blocks, relay, storage, mempool, verify, legacy_peers, gateway, None).await
    }

    /// [`IrohNode::spawn`] + the sthpq address book module (`p2p.iroh.discovery`) and pinned bootstrap addresses.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_with(
        secret: SecretKey,
        bootstrap: Vec<EndpointId>,
        serve_blocks: bool,
        relay: Option<RelaySetup>,
        storage: Arc<Storage>,
        mempool: Arc<Mempool>,
        verify: bool,
        legacy_peers: Option<Arc<crate::p2p_legacy::PeerTable>>,
        gateway: Option<String>,
        discovery: Option<DiscoveryOpts>,
    ) -> Result<Arc<Self>> {
        let lookup = iroh::address_lookup::MemoryLookup::new();
        // n0 DNS (`presets::N0*`) stays a fallback unless discovery.n0_dns is off → `Minimal` = no n0 contact at all
        let n0_dns = discovery.as_ref().map_or(true, |d| !d.cfg.enabled || d.cfg.n0_dns);
        let (mut builder, relay_urls) = match (&relay, n0_dns) {
            (Some(setup), true) => {
                let map = setup.relay_map();
                let urls: Vec<String> = map.urls::<Vec<iroh::RelayUrl>>().iter().map(|u| u.to_string()).collect();
                (Endpoint::builder(presets::N0).relay_mode(iroh::RelayMode::Custom(map)), urls)
            }
            (None, true) => (Endpoint::builder(presets::N0DisableRelay), Vec::new()),
            (Some(setup), false) => {
                let map = setup.relay_map();
                let urls: Vec<String> = map.urls::<Vec<iroh::RelayUrl>>().iter().map(|u| u.to_string()).collect();
                (Endpoint::builder(presets::Minimal).relay_mode(iroh::RelayMode::Custom(map)), urls)
            }
            (None, false) => (Endpoint::builder(presets::Minimal).relay_mode(iroh::RelayMode::Disabled), Vec::new()),
        };
        if !n0_dns {
            tracing::info!("n0 DNS address lookup OFF (discovery.n0_dns: false) — peers are found via the sthpq address book and the Mainline DHT only");
        }
        let mut dht_lookup = None;
        if let Some(d) = &discovery {
            if d.cfg.enabled && d.cfg.mainline {
                let lookup = mainline_lookup(&d.cfg, &secret)?;
                builder = builder.address_lookup(lookup.clone());
                dht_lookup = Some(lookup);
            }
        }
        if let Some(d) = &discovery {
            // a fixed UDP port + announced public address = stable direct dial info for the address book
            if d.cfg.listen_port > 0 {
                let bind = std::net::SocketAddr::from(([0, 0, 0, 0], d.cfg.listen_port));
                builder = builder.bind_addr(bind).map_err(|e| Error::Config(format!("p2p.iroh.discovery.listen_port: {e}")))?;
                tracing::info!(port = d.cfg.listen_port, "iroh listens on a fixed UDP port (open it in the firewall)");
            }
            if !d.cfg.external_addr.trim().is_empty() {
                let ext: std::net::SocketAddr = d.cfg.external_addr.trim().parse().map_err(|e| Error::Config(format!("p2p.iroh.discovery.external_addr `{}`: {e}", d.cfg.external_addr)))?;
                if !addrbook::is_public(ext.ip()) {
                    return Err(Error::Config(format!("p2p.iroh.discovery.external_addr {ext} is not a public address")));
                }
                builder = builder.external_addr(ext);
            }
        }
        let wallet_api = WalletApi::new(secret.clone());
        let endpoint = builder
            .secret_key(secret.clone())
            .address_lookup(lookup.clone())
            .bind()
            .await
            .map_err(|e| Error::Sync(format!("iroh bind: {e}")))?;
        let peers = Arc::new(IrohPeers::default());
        // blocks with transactions and vote batches exceed the 4 KiB iroh-gossip default; peers on older builds still
        // receive our ≤ 4 KiB messages (votes are chunked), we accept up to 64 KiB from newer ones
        let gossip = Gossip::builder().max_message_size(GOSSIP_MAX_MESSAGE_SIZE).spawn(endpoint.clone());
        let mut builder = Router::builder(endpoint.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(wallet_api::ALPN_API_V0, wallet_api::ApiHandler::new(wallet_api.clone(), false))
            .accept(wallet_api::ALPN_API_V1, wallet_api::ApiHandler::new(wallet_api.clone(), true))
            .accept(wallet_api::ALPN_API, wallet_api::ApiHandler::new(wallet_api.clone(), true))
            .accept(wallet_api::ALPN_EVENTS, wallet_api::EventsHandler::new(wallet_api.clone()));
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
        if let Some(setup) = &relay {
            if setup.n0 {
                tracing::warn!("p2p.iroh.relay_n0: true — the public n0 relays are rate-limited and not meant for production; if one wins the latency race wallets time out through it (relay guard removes it after repeated failures). Prefer relay_n0: false (NETFORY n1 only)");
            }
            relay_guard::init(&setup.guard, &setup.protected(), endpoint.clone());
        }
        for id in &bootstrap {
            peers.seen(*id, None);
        }
        let node = Arc::new(Self { endpoint, router, gossip, peers, topics, lookup, gateway, relay_urls, forging_keys: Default::default(), announce: std::sync::atomic::AtomicBool::new(true), rust_delegates: Default::default(), finality: Arc::new(finality::FinalityTracker::new(storage.clone())), wallet_api, nethash: storage.network().nethash.clone(), secret, addrbook: std::sync::OnceLock::new() });
        if let Some(d) = discovery {
            for addr in d.pinned {
                tracing::info!(peer = %addr.id.fmt_short(), addrs = ?addr.ip_addrs().collect::<Vec<_>>(), "bootstrap peer pinned with a direct address");
                node.add_peer_addr(addr);
            }
            if let Some(book) = addrbook::init(&d.cfg, &node.nethash, node.id(), storage.clone(), node.lookup.clone()) {
                let _ = node.addrbook.set(book);
            }
            if let Some(lookup) = dht_lookup {
                tokio::spawn(dht_health_loop(lookup, node.id(), d.cfg.clone()));
            }
        }
        gossip::start(node.clone(), bootstrap, storage, mempool, verify, legacy_peers).await?;
        Ok(node)
    }

    /// Nethash of the chain this node serves (address-book records are bound to it).
    pub fn storage_nethash(&self) -> String {
        self.nethash.clone()
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
