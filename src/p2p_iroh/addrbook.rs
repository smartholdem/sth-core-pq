//! Author: TechnoL0g
//!
//! sthpq address book (`p2p.iroh.discovery`, optional module, on by default) — how PQ nodes find each other
//! WITHOUT the n0 DNS service and without relays for the public-IP backbone.
//!
//! Every node signs a small record with its iroh key (`SignedAddr`: EndpointId, public `ip:port` list, home relay,
//! `seq` = signing time) and shares it three ways:
//! 1. gossip topic `sth/<nethash>/addrs` — own record on start and every `announce_secs`, plus up to
//!    `RELAY_RECORDS` fresh records of other nodes (Bitcoin-style `addr` relay, so newcomers learn peers that are not
//!    in their bootstrap list);
//! 2. iroh RPC `GetAddrs` (pull from known peers on start / periodically);
//! 3. legacy port 4001 route `p2p.pq.getAddrs` — PQ nodes already see each other's IPs in the legacy peer table
//!    (`CORE_PQ` plugin flag), so the very first iroh contact needs neither n0 nor a relay.
//!
//! Records are verified (Ed25519 by the EndpointId, nethash-bound, ±10 min / expiry window), fed into the endpoint's
//! `MemoryLookup` so iroh dials the direct addresses, and persisted in sled (`ab:<id>`). Liveness follows Bitcoin's
//! addrman rules: entries that never answered after 3 attempts, or whose last success is older than `expire_days`,
//! are evicted; a crawler re-probes stale entries with `GetStatus`. Observation/discovery only — consensus untouched.

use super::proto::{Request, Response};
use crate::error::{Error, Result};
use crate::node_config::DiscoveryConfig;
use crate::storage::Storage;
use iroh::{EndpointAddr, EndpointId, PublicKey, SecretKey, Signature};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const DOMAIN: &[u8] = b"sthpq-addr-v1";
/// Records of other nodes attached to our own announcement.
pub const RELAY_RECORDS: usize = 16;
/// `GetAddrs` / `p2p.pq.getAddrs` answer size cap.
pub const MAX_RECORDS: usize = 100;
const MAX_ADDRS: usize = 8;
const FUTURE_SKEW: u64 = 600;
/// A record is "fresh" (shared with others) when the node answered within this many seconds.
const FRESH_SECS: u64 = 3 * 86_400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedAddr {
    pub v: u8,
    /// EndpointId (Ed25519 public key) hex.
    pub id: String,
    /// Public direct addresses `ip:port` (UDP/QUIC), at most `MAX_ADDRS`.
    pub addrs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    /// Unix seconds at signing; the newest wins (BEP-44 style).
    pub seq: u64,
    pub version: String,
    /// Ed25519 hex over `sha256(DOMAIN || nethash || id || seq LE || addrs joined ',' || relay)`.
    pub signature: String,
}

impl SignedAddr {
    fn message(nethash: &str, id: &str, seq: u64, addrs: &[String], relay: Option<&str>) -> [u8; 32] {
        let mut m = DOMAIN.to_vec();
        m.extend_from_slice(nethash.as_bytes());
        m.extend_from_slice(id.as_bytes());
        m.extend_from_slice(&seq.to_le_bytes());
        m.extend_from_slice(addrs.join(",").as_bytes());
        m.extend_from_slice(relay.unwrap_or("").as_bytes());
        crate::crypto::sha256(&m)
    }

    pub fn sign(secret: &SecretKey, nethash: &str, mut addrs: Vec<String>, relay: Option<String>, seq: u64) -> Self {
        addrs.sort();
        addrs.dedup();
        addrs.truncate(MAX_ADDRS);
        let id = secret.public().to_string();
        let sig = secret.sign(&Self::message(nethash, &id, seq, &addrs, relay.as_deref()));
        Self { v: 1, id, addrs, relay, seq, version: env!("CARGO_PKG_VERSION").to_string(), signature: hex::encode(sig.to_bytes()) }
    }

    /// Signature + shape check; returns the EndpointId. `now` is used for the ±skew / expiry window.
    pub fn verify(&self, nethash: &str, now: u64, expire_secs: u64) -> Result<EndpointId> {
        if self.v != 1 || self.addrs.len() > MAX_ADDRS {
            return Err(Error::Sync("sthpq addr: unsupported version or too many addresses".into()));
        }
        if self.seq > now + FUTURE_SKEW || self.seq + expire_secs < now {
            return Err(Error::Sync(format!("sthpq addr: seq {} outside the accepted window", self.seq)));
        }
        if self.addrs.iter().any(|a| a.parse::<SocketAddr>().map_or(true, |s| !is_public(s.ip()))) {
            return Err(Error::Sync("sthpq addr: non-public or malformed address".into()));
        }
        let id: PublicKey = self.id.parse().map_err(|e| Error::Sync(format!("sthpq addr: bad id: {e}")))?;
        let sig = hex::decode(&self.signature).map_err(|e| Error::Sync(format!("sthpq addr: bad signature hex: {e}")))?;
        let sig: [u8; 64] = sig.try_into().map_err(|_| Error::Sync("sthpq addr: signature must be 64 bytes".into()))?;
        id.verify(&Self::message(nethash, &self.id, self.seq, &self.addrs, self.relay.as_deref()), &Signature::from_bytes(&sig))
            .map_err(|_| Error::Sync("sthpq addr: invalid signature".into()))?;
        Ok(id)
    }

    /// Dial info for iroh (direct addresses + relay).
    pub fn endpoint_addr(&self, id: EndpointId) -> EndpointAddr {
        let mut addr = EndpointAddr::new(id);
        for a in self.addrs.iter().filter_map(|a| a.parse::<SocketAddr>().ok()) {
            addr = addr.with_ip_addr(a);
        }
        if let Some(url) = self.relay.as_deref().and_then(|u| u.parse().ok()) {
            addr = addr.with_relay_url(url);
        }
        addr
    }
}

/// Routable from the internet: no private / loopback / link-local / unspecified ranges.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() || v4.is_documentation() || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1])),
        IpAddr::V6(v6) => {
            let s = v6.segments()[0];
            !(v6.is_loopback() || v6.is_unspecified() || (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub record: SignedAddr,
    pub first_seen: u64,
    pub last_seen: u64,
    /// 0 = never answered us.
    pub last_success: u64,
    pub last_attempt: u64,
    pub failures: u32,
    pub source: String,
}

impl Entry {
    /// Bitcoin addrman "terrible": never answered after 3 tries (and known for a day), or silent for `expire_secs`.
    pub fn is_terrible(&self, now: u64, expire_secs: u64) -> bool {
        if self.last_success == 0 {
            return self.failures >= 3 && now.saturating_sub(self.first_seen) > 86_400;
        }
        now.saturating_sub(self.last_success) > expire_secs
    }

    pub fn is_fresh(&self, now: u64) -> bool {
        self.last_success > 0 && now.saturating_sub(self.last_success) <= FRESH_SECS
    }
}

pub struct AddrBook {
    cfg: DiscoveryConfig,
    nethash: String,
    me: EndpointId,
    storage: Option<Arc<Storage>>,
    lookup: Option<iroh::address_lookup::MemoryLookup>,
    entries: Mutex<HashMap<EndpointId, Entry>>,
}

static BOOK: OnceLock<Arc<AddrBook>> = OnceLock::new();

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The node-wide address book (None = module disabled / iroh off).
pub fn global() -> Option<Arc<AddrBook>> {
    BOOK.get().cloned()
}

pub fn init(cfg: &DiscoveryConfig, nethash: &str, me: EndpointId, storage: Arc<Storage>, lookup: iroh::address_lookup::MemoryLookup) -> Option<Arc<AddrBook>> {
    if !cfg.enabled {
        tracing::info!("sthpq address book disabled (p2p.iroh.discovery.enabled: false) — peers are found through bootstrap + n0 DNS only");
        return None;
    }
    let book = Arc::new(AddrBook::new(cfg.clone(), nethash, me, Some(storage), Some(lookup)));
    let loaded = book.load();
    tracing::info!(entries = loaded, announce_secs = cfg.announce_secs, expire_days = cfg.expire_days, "sthpq address book enabled");
    let _ = BOOK.set(book.clone());
    Some(book)
}

/// `{ enabled, entries, fresh, ... }` for the metrics page.
pub fn snapshot() -> serde_json::Value {
    match BOOK.get() {
        Some(b) => b.snapshot(),
        None => serde_json::json!({ "enabled": false }),
    }
}

impl AddrBook {
    pub fn new(cfg: DiscoveryConfig, nethash: &str, me: EndpointId, storage: Option<Arc<Storage>>, lookup: Option<iroh::address_lookup::MemoryLookup>) -> Self {
        Self { cfg, nethash: nethash.to_string(), me, storage, lookup, entries: Mutex::new(HashMap::new()) }
    }

    fn expire_secs(&self) -> u64 {
        self.cfg.expire_days.max(1) * 86_400
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<EndpointId, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Restore persisted entries (terrible ones are dropped on the way in).
    fn load(&self) -> usize {
        let Some(st) = &self.storage else { return 0 };
        let now = now_unix();
        let mut n = 0;
        for raw in st.load_addr_entries() {
            let Ok(e) = serde_json::from_slice::<Entry>(&raw) else { continue };
            if e.is_terrible(now, self.expire_secs()) {
                st.remove_addr_entry(&e.record.id);
                continue;
            }
            let Ok(id) = e.record.verify(&self.nethash, now, u64::MAX / 2) else { continue };
            if let Some(l) = &self.lookup {
                l.add_endpoint_info(e.record.endpoint_addr(id));
            }
            self.lock().insert(id, e);
            n += 1;
        }
        n
    }

    fn persist(&self, e: &Entry) {
        if let (Some(st), Ok(bytes)) = (&self.storage, serde_json::to_vec(e)) {
            st.save_addr_entry(&e.record.id, &bytes);
        }
    }

    /// Verify and store a record; `Ok(true)` when it was new or newer than what we had.
    pub fn insert(&self, record: SignedAddr, source: &str) -> Result<bool> {
        let now = now_unix();
        let id = record.verify(&self.nethash, now, self.expire_secs())?;
        if id == self.me {
            return Ok(false);
        }
        if record.addrs.is_empty() && record.relay.is_none() {
            return Err(Error::Sync("sthpq addr: no addresses".into()));
        }
        let mut map = self.lock();
        let newer = match map.get_mut(&id) {
            Some(e) if e.record.seq >= record.seq => {
                e.last_seen = now;
                return Ok(false);
            }
            Some(e) => {
                e.record = record;
                e.last_seen = now;
                e.clone()
            }
            None => {
                if map.len() >= self.cfg.max_entries.max(1) {
                    return Err(Error::Sync("sthpq addr: address book full".into()));
                }
                let e = Entry { record, first_seen: now, last_seen: now, last_success: 0, last_attempt: 0, failures: 0, source: source.to_string() };
                map.insert(id, e.clone());
                e
            }
        };
        drop(map);
        if let Some(l) = &self.lookup {
            l.add_endpoint_info(newer.record.endpoint_addr(id));
        }
        self.persist(&newer);
        tracing::debug!(peer = %id.fmt_short(), addrs = ?newer.record.addrs, source, "sthpq addr record stored");
        Ok(true)
    }

    pub fn mark(&self, id: EndpointId, success: bool) {
        let now = now_unix();
        let mut map = self.lock();
        let Some(e) = map.get_mut(&id) else { return };
        e.last_attempt = now;
        if success {
            e.last_success = now;
            e.failures = 0;
        } else {
            e.failures += 1;
        }
        let e = e.clone();
        drop(map);
        self.persist(&e);
    }

    /// Drop terrible entries (returns how many).
    pub fn evict(&self) -> usize {
        let now = now_unix();
        let expire = self.expire_secs();
        let gone: Vec<(EndpointId, String)> = self.lock().iter().filter(|(_, e)| e.is_terrible(now, expire)).map(|(id, e)| (*id, e.record.id.clone())).collect();
        let mut map = self.lock();
        for (id, hex) in &gone {
            map.remove(id);
            if let Some(st) = &self.storage {
                st.remove_addr_entry(hex);
            }
        }
        if !gone.is_empty() {
            tracing::info!(evicted = gone.len(), left = map.len(), "sthpq address book: dead peers evicted");
        }
        gone.len()
    }

    /// Fresh records (answered within 3 days), newest success first — what we hand to others.
    pub fn fresh(&self, limit: usize) -> Vec<SignedAddr> {
        let now = now_unix();
        let mut v: Vec<(u64, SignedAddr)> = self.lock().values().filter(|e| e.is_fresh(now)).map(|e| (e.last_success, e.record.clone())).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        v.into_iter().take(limit.min(MAX_RECORDS)).map(|(_, r)| r).collect()
    }

    /// Entries worth a probe: never answered, or silent for over an hour, and not tried in the last 30 min.
    pub fn stale(&self, limit: usize) -> Vec<EndpointId> {
        let now = now_unix();
        let mut v: Vec<(u64, EndpointId)> = self
            .lock()
            .iter()
            .filter(|(_, e)| now.saturating_sub(e.last_success) > 3_600 && now.saturating_sub(e.last_attempt) > 1_800)
            .map(|(id, e)| (e.last_attempt, *id))
            .collect();
        v.sort();
        v.into_iter().take(limit).map(|(_, id)| id).collect()
    }

    pub fn ids(&self) -> Vec<EndpointId> {
        self.lock().keys().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn snapshot(&self) -> serde_json::Value {
        let now = now_unix();
        let map = self.lock();
        let mut entries: Vec<&Entry> = map.values().collect();
        entries.sort_by(|a, b| b.last_success.cmp(&a.last_success));
        serde_json::json!({
            "enabled": true,
            "n0Dns": self.cfg.n0_dns,
            "mainline": self.cfg.mainline,
            "mainlineServer": self.cfg.mainline_server,
            "mainlineBootstrap": self.cfg.mainline_bootstrap,
            "dht": super::dht_health(),
            "listenPort": self.cfg.listen_port,
            "externalAddr": self.cfg.external_addr,
            "entries": map.len(),
            "fresh": map.values().filter(|e| e.is_fresh(now)).count(),
            "neverAnswered": map.values().filter(|e| e.last_success == 0).count(),
            "announceSecs": self.cfg.announce_secs,
            "expireDays": self.cfg.expire_days,
            "peers": entries.iter().take(200).map(|e| serde_json::json!({
                "id": e.record.id, "addrs": e.record.addrs, "relay": e.record.relay, "version": e.record.version,
                "source": e.source, "lastSuccessAge": if e.last_success == 0 { serde_json::Value::Null } else { serde_json::json!(now.saturating_sub(e.last_success)) },
                "failures": e.failures, "seqAge": now.saturating_sub(e.record.seq),
            })).collect::<Vec<_>>(),
        })
    }
}

/// Our own record from the endpoint's current addresses (public direct addrs + home relay); `None` when there is
/// nothing routable to announce yet.
pub fn own_record(node: &super::IrohNode, secret: &SecretKey) -> Option<SignedAddr> {
    let addr = node.endpoint.addr();
    let addrs: Vec<String> = addr.ip_addrs().filter(|a| is_public(a.ip())).map(|a| a.to_string()).collect();
    let relay = addr.relay_urls().next().map(|u| u.to_string());
    if addrs.is_empty() && relay.is_none() {
        return None;
    }
    Some(SignedAddr::sign(secret, &node.storage_nethash(), addrs, relay, now_unix()))
}

/// Gossip intake on the `addrs` topic + periodic announcement + crawler. Runs for the node's lifetime.
pub async fn run(node: Arc<super::IrohNode>, book: Arc<AddrBook>, secret: SecretKey, topic: iroh_gossip::api::GossipTopic, legacy: Option<Arc<crate::p2p_legacy::PeerTable>>) {
    use futures::StreamExt;
    use iroh_gossip::api::Event;
    let (sender, mut receiver) = topic.split();
    {
        let (node, book) = (node.clone(), book.clone());
        tokio::spawn(async move {
            while let Some(event) = receiver.next().await {
                match event {
                    Ok(Event::Received(msg)) => {
                        if let Some(super::proto::GossipMessage::Addrs { records }) = super::proto::GossipMessage::decode(&msg.content) {
                            node.peers.seen(msg.delivered_from, None);
                            let mut fresh = 0;
                            for r in records.into_iter().take(RELAY_RECORDS + 1) {
                                match book.insert(r, "gossip") {
                                    Ok(true) => fresh += 1,
                                    Ok(false) => {}
                                    Err(e) => tracing::debug!(peer = %msg.delivered_from.fmt_short(), error = %e, "sthpq addr record rejected"),
                                }
                            }
                            if fresh > 0 {
                                tracing::info!(peer = %msg.delivered_from.fmt_short(), new = fresh, known = book.len(), "sthpq addr records received over gossip");
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "sthpq addrs gossip stream ended");
                        break;
                    }
                }
            }
        });
    }
    // announcer: own record + a slice of fresh others
    {
        let (node, book, sender) = (node.clone(), book.clone(), sender.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let mut tick = tokio::time::interval(Duration::from_secs(book.cfg.announce_secs.max(60)));
            loop {
                tick.tick().await;
                let Some(own) = own_record(&node, &secret) else {
                    tracing::debug!("sthpq addr: no public address or relay known yet — announcement skipped");
                    continue;
                };
                let mut records = vec![own];
                records.extend(book.fresh(RELAY_RECORDS));
                let n = records.len();
                match tokio::time::timeout(Duration::from_secs(5), sender.broadcast(super::proto::GossipMessage::Addrs { records }.encode().into())).await {
                    Ok(Ok(())) => tracing::info!(records = n, "sthpq addr announcement published"),
                    Ok(Err(e)) => tracing::debug!(error = %e, "sthpq addr announcement failed"),
                    Err(_) => tracing::debug!("sthpq addr announcement timed out"),
                }
            }
        });
    }
    // crawler: pull address lists, probe stale entries, evict the dead
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let mut tick = tokio::time::interval(Duration::from_secs(300));
        loop {
            tick.tick().await;
            // peers that talked to us recently are alive
            for p in node.peers.snapshot() {
                if p.last_seen.elapsed() < Duration::from_secs(600) && p.id != node.id() {
                    book.mark(p.id, true);
                }
            }
            let mut targets: Vec<EndpointId> = node.peers.snapshot().into_iter().filter(|p| p.neighbor && p.id != node.id()).map(|p| p.id).take(3).collect();
            targets.extend(book.stale(6));
            targets.sort();
            targets.dedup();
            for id in targets {
                match tokio::time::timeout(Duration::from_secs(10), fetch_addrs(&node.endpoint, id, MAX_RECORDS)).await {
                    Ok(Ok(records)) => {
                        book.mark(id, true);
                        let mut fresh = 0;
                        for r in records {
                            if matches!(book.insert(r, "rpc"), Ok(true)) {
                                fresh += 1;
                            }
                        }
                        if fresh > 0 {
                            tracing::info!(peer = %id.fmt_short(), new = fresh, known = book.len(), "sthpq addr records pulled");
                        }
                    }
                    Ok(Err(e)) => {
                        // an older node answers "invalid request" — reachable, just no address book
                        let reachable = e.to_string().contains("invalid request");
                        book.mark(id, reachable);
                        tracing::debug!(peer = %id.fmt_short(), error = %e, "sthpq addr pull failed");
                    }
                    Err(_) => book.mark(id, false),
                }
            }
            book.evict();
            // legacy port 4001: PQ peers (CORE_PQ plugin) hand out their records too — the n0-free first contact
            if let Some(table) = &legacy {
                pull_over_legacy(&book, table, 4).await;
            }
        }
    });
}

/// Pull sthpq records from up to `n` PQ peers over the legacy port (TCP 4001 — the table is seeded from peers.json,
/// so this works with no relay, DNS or DHT at all). Returns the number of new/updated records.
pub async fn pull_over_legacy(book: &AddrBook, table: &crate::p2p_legacy::PeerTable, n: usize) -> usize {
    let ips: Vec<String> = table.snapshot().into_iter().filter(|p| p.pq && p.successes > 0).map(|p| p.ip).take(n).collect();
    let port = table.port();
    let mut total = 0;
    for ip in ips {
        let res = tokio::time::timeout(Duration::from_secs(8), async {
            let mut peer = crate::p2p_legacy::LegacyPeer::connect(&ip, port, Duration::from_secs(5)).await?;
            peer.get_pq_addrs().await
        })
        .await;
        match res {
            Ok(Ok(records)) => {
                let fresh = records.into_iter().filter(|r| matches!(book.insert(r.clone(), "legacy"), Ok(true))).count();
                if fresh > 0 {
                    tracing::info!(peer = ip, new = fresh, known = book.len(), "sthpq addr records pulled over legacy port");
                }
                total += fresh;
            }
            Ok(Err(e)) => tracing::debug!(peer = ip, error = %e, "p2p.pq.getAddrs failed (older PQ node?)"),
            Err(_) => tracing::debug!(peer = ip, "p2p.pq.getAddrs timed out"),
        }
    }
    total
}

/// Dial info with the direct addresses ONLY (no relay URL): what to pin while every relay is down, so iroh does not
/// wait on the dead relay path. `None` when the record has no public direct address.
pub fn direct_only_addr(record: &SignedAddr) -> Option<EndpointAddr> {
    let id: EndpointId = record.id.parse().ok()?;
    let addrs: Vec<SocketAddr> = record.addrs.iter().filter_map(|a| a.parse().ok()).filter(|a: &SocketAddr| is_public(a.ip())).collect();
    if addrs.is_empty() {
        return None;
    }
    let mut addr = EndpointAddr::new(id);
    for a in addrs {
        addr = addr.with_ip_addr(a);
    }
    Some(addr)
}

/// `GetAddrs` over the sth/rpc/1 protocol.
pub async fn fetch_addrs(endpoint: &iroh::Endpoint, peer: impl Into<EndpointAddr>, limit: usize) -> Result<Vec<SignedAddr>> {
    let conn = endpoint.connect(peer, super::rpc::ALPN).await.map_err(|e| Error::Sync(format!("iroh connect: {e}")))?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| Error::Sync(format!("iroh open_bi: {e}")))?;
    send.write_all(&serde_json::to_vec(&Request::GetAddrs { limit: limit as u32 })?).await.map_err(|e| Error::Sync(format!("iroh write: {e}")))?;
    send.finish().map_err(|e| Error::Sync(format!("iroh finish: {e}")))?;
    let raw = recv.read_to_end(1024 * 1024).await.map_err(|e| Error::Sync(format!("iroh read: {e}")))?;
    conn.close(0u32.into(), b"done");
    match serde_json::from_slice::<Response>(&raw)? {
        Response::Addrs { records } => Ok(records),
        Response::Error { message } => Err(Error::Sync(format!("iroh peer error: {message}"))),
        _ => Err(Error::Sync("unexpected iroh response".into())),
    }
}

/// `id@ip:port` (pinned direct address) or plain `id` — node.yaml `bootstrap` entries.
pub fn parse_bootstrap(s: &str) -> Result<(EndpointId, Option<SocketAddr>)> {
    match s.split_once('@') {
        Some((id, addr)) => {
            let sock = addr.trim().parse::<SocketAddr>().map_err(|e| Error::Config(format!("bootstrap `{s}`: bad ip:port: {e}")))?;
            Ok((super::parse_endpoint_id(id.trim())?, Some(sock)))
        }
        None => Ok((super::parse_endpoint_id(s.trim())?, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NETHASH: &str = "abcd";

    fn cfg() -> DiscoveryConfig {
        DiscoveryConfig::default()
    }

    #[test]
    fn record_roundtrip_and_tamper_detection() {
        let sk = SecretKey::generate();
        let now = now_unix();
        let r = SignedAddr::sign(&sk, NETHASH, vec!["185.199.110.5:4004".into(), "185.199.110.5:4004".into()], Some("https://relay-fsn7.sth.cx/".into()), now);
        assert_eq!(r.addrs.len(), 1, "duplicates removed");
        assert_eq!(r.verify(NETHASH, now, 86_400).unwrap(), sk.public());
        assert!(r.verify("other-net", now, 86_400).is_err(), "nethash-bound");
        let mut t = r.clone();
        t.addrs = vec!["185.199.110.9:4004".into()];
        assert!(t.verify(NETHASH, now, 86_400).is_err(), "tampered addrs");
        let mut old = r.clone();
        old.seq = now - 100_000;
        assert!(old.verify(NETHASH, now, 86_400).is_err(), "expired seq (signature is also invalid, either way rejected)");
        let priv_addr = SignedAddr::sign(&sk, NETHASH, vec!["192.168.1.2:4004".into()], None, now);
        assert!(priv_addr.verify(NETHASH, now, 86_400).is_err(), "private ip refused");
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<SignedAddr>(&json).unwrap(), r);
    }

    #[test]
    fn book_keeps_newest_evicts_terrible_and_shares_fresh_only() {
        let me = SecretKey::generate();
        let book = AddrBook::new(cfg(), NETHASH, me.public(), None, None);
        let now = now_unix();
        let a = SecretKey::generate();
        let r1 = SignedAddr::sign(&a, NETHASH, vec!["185.199.110.5:4004".into()], None, now - 10);
        let r2 = SignedAddr::sign(&a, NETHASH, vec!["185.199.110.6:4004".into()], None, now);
        assert!(book.insert(r2.clone(), "gossip").unwrap());
        assert!(!book.insert(r1, "gossip").unwrap(), "older seq ignored");
        assert_eq!(book.len(), 1);
        let own = SignedAddr::sign(&me, NETHASH, vec!["185.199.110.7:4004".into()], None, now);
        assert!(!book.insert(own, "gossip").unwrap(), "own record ignored");
        assert!(book.fresh(10).is_empty(), "never answered → not shared");
        book.mark(a.public(), true);
        assert_eq!(book.fresh(10), vec![r2]);
        assert!(book.stale(10).is_empty(), "just answered → not stale");

        // terrible: never answered, 3 failures, known > 1 day
        let b = SecretKey::generate();
        book.insert(SignedAddr::sign(&b, NETHASH, vec!["185.199.110.8:4004".into()], None, now), "rpc").unwrap();
        for _ in 0..3 {
            book.mark(b.public(), false);
        }
        assert_eq!(book.evict(), 0, "known for less than a day → kept");
        book.lock().get_mut(&b.public()).unwrap().first_seen = now - 2 * 86_400;
        assert_eq!(book.evict(), 1);
        assert_eq!(book.len(), 1);
        let s = book.snapshot();
        assert_eq!(s["entries"], 1);
        assert_eq!(s["fresh"], 1);
    }

    #[test]
    fn expiry_rule_and_bootstrap_parsing() {
        let e = Entry { record: SignedAddr { v: 1, id: String::new(), addrs: vec![], relay: None, seq: 0, version: String::new(), signature: String::new() }, first_seen: 0, last_seen: 0, last_success: 1_000, last_attempt: 0, failures: 0, source: String::new() };
        assert!(!e.is_terrible(1_000 + 29 * 86_400, 30 * 86_400));
        assert!(e.is_terrible(1_000 + 31 * 86_400, 30 * 86_400), "silent for a month → gone");
        let sk = SecretKey::generate();
        let (id, addr) = parse_bootstrap(&format!("{}@185.199.110.5:4004", sk.public())).unwrap();
        assert_eq!((id, addr), (sk.public(), Some("185.199.110.5:4004".parse().unwrap())));
        assert_eq!(parse_bootstrap(&sk.public().to_string()).unwrap(), (sk.public(), None));
        assert!(parse_bootstrap("abc@nowhere").is_err());
        assert!(is_public("8.8.8.8".parse().unwrap()));
        assert!(!is_public("10.0.0.1".parse().unwrap()));
        assert!(!is_public("100.64.0.1".parse().unwrap()), "CGNAT range");
        assert!(!is_public("fe80::1".parse().unwrap()));
    }
}
