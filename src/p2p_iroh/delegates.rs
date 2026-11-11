//! Author: TechnoL0g
//! Registry of delegates known to forge on Rust nodes (learned from signed `Delegates` gossip announces).

use super::proto::DelegateAnnounce;
use crate::crypto::KeyPair;
use iroh::EndpointId;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_SKEW: u64 = 600;
/// Entries older than this are dropped from listings (announces come every 2 min).
pub const STALE_AFTER: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone)]
pub struct RustDelegateInfo {
    pub version: String,
    pub node: EndpointId,
    pub last_seen: Instant,
}

/// Why an announce for a delegate key was refused (kept for the metrics "why LEGACY" column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// The delegate signature does not match node id + timestamp.
    Signature,
    /// The announce timestamp is more than 10 min off this node's clock.
    Clock,
}

impl Rejection {
    pub fn code(self) -> &'static str {
        match self {
            Rejection::Signature => "announce-bad-signature",
            Rejection::Clock => "announce-clock-skew",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RejectedAnnounce {
    pub reason: Rejection,
    pub count: u64,
    pub last: Instant,
}

#[derive(Default)]
pub struct RustDelegates {
    map: Mutex<HashMap<String, RustDelegateInfo>>,
    rejected: Mutex<HashMap<String, RejectedAnnounce>>,
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn announce_hash(node: &EndpointId, timestamp: u64) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"sth-delegate-announce");
    h.update(node.as_bytes());
    h.update(timestamp.to_le_bytes());
    h.finalize().into()
}

/// Signed announces for every delegate key forging on `node`.
pub fn sign_announces(keys: &[KeyPair], node: &EndpointId) -> Vec<DelegateAnnounce> {
    let timestamp = now_unix();
    let hash = announce_hash(node, timestamp);
    keys.iter()
        .filter_map(|k| crate::crypto::sign_ecdsa(&hash, k.private_key()).ok().map(|signature| DelegateAnnounce {
            node_id: Some(node.to_string()),
            public_key: k.public_key_hex(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            timestamp,
            signature,
        }))
        .collect()
}

/// The forging node an announce speaks for: its own `node_id` (any number of gossip hops), else the 1-hop sender.
pub fn announce_node(a: &DelegateAnnounce, from: &EndpointId) -> Option<EndpointId> {
    match a.node_id.as_deref() {
        Some(id) => id.parse().ok(),
        None => Some(*from),
    }
}

pub fn verify_announce(a: &DelegateAnnounce, from: &EndpointId) -> bool {
    check_announce(a, from).is_ok()
}

pub fn check_announce(a: &DelegateAnnounce, from: &EndpointId) -> Result<(), Rejection> {
    if a.timestamp.abs_diff(now_unix()) > MAX_SKEW {
        return Err(Rejection::Clock);
    }
    let node = announce_node(a, from).ok_or(Rejection::Signature)?;
    match crate::crypto::verify_signature(&announce_hash(&node, a.timestamp), &a.signature, &a.public_key) {
        Ok(true) => Ok(()),
        _ => Err(Rejection::Signature),
    }
}

impl RustDelegates {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, RustDelegateInfo>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records verified announces; returns how many were accepted.
    pub fn record(&self, from: EndpointId, announces: &[DelegateAnnounce]) -> usize {
        let mut map = self.lock();
        let mut ok = 0;
        let mut rejected = self.rejected.lock().unwrap_or_else(|e| e.into_inner());
        for a in announces.iter().take(64) {
            match check_announce(a, &from) {
                Ok(()) => {
                    let node = announce_node(a, &from).unwrap_or(from);
                    map.insert(a.public_key.clone(), RustDelegateInfo { version: a.version.clone(), node, last_seen: Instant::now() });
                    rejected.remove(&a.public_key);
                    ok += 1;
                }
                // only keys we have never accepted fresh: a forged announce must not hide a working delegate
                Err(reason) if rejected.len() < 1024 => {
                    let e = rejected.entry(a.public_key.clone()).or_insert(RejectedAnnounce { reason, count: 0, last: Instant::now() });
                    e.reason = reason;
                    e.count += 1;
                    e.last = Instant::now();
                }
                Err(_) => {}
            }
        }
        ok
    }

    /// Every accepted announce, stale ones included (for "last announce N min ago").
    pub fn all(&self) -> HashMap<String, RustDelegateInfo> {
        self.lock().clone()
    }

    /// Refused announces seen within `STALE_AFTER`.
    pub fn rejected(&self) -> HashMap<String, RejectedAnnounce> {
        self.rejected.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|(_, r)| r.last.elapsed() < STALE_AFTER).map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Fresh entries only (seen within `STALE_AFTER`).
    pub fn snapshot(&self) -> HashMap<String, RustDelegateInfo> {
        self.lock().iter().filter(|(_, i)| i.last_seen.elapsed() < STALE_AFTER).map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relayed_announce_verifies_against_its_own_node_id() {
        let key = KeyPair::from_passphrase("delegate").unwrap();
        let author = iroh::SecretKey::from_bytes(&[1; 32]).public();
        let relay_hop = iroh::SecretKey::from_bytes(&[2; 32]).public();
        let mut a = sign_announces(std::slice::from_ref(&key), &author).remove(0);
        assert!(verify_announce(&a, &relay_hop), "delivered by another node (2+ gossip hops)");
        let book = RustDelegates::default();
        assert_eq!(book.record(relay_hop, std::slice::from_ref(&a)), 1);
        assert_eq!(book.snapshot()[&key.public_key_hex()].node, author, "credited to the forging node, not the hop");

        a.node_id = Some(relay_hop.to_string());
        assert!(!verify_announce(&a, &relay_hop), "a hop cannot claim the delegate for itself");
        a.node_id = None;
        assert!(verify_announce(&a, &author) && !verify_announce(&a, &relay_hop), "pre-0.25.51 announce: 1-hop sender");
        assert_eq!(check_announce(&a, &relay_hop), Err(Rejection::Signature));
        let mut old = a.clone();
        old.timestamp -= 3600;
        assert_eq!(check_announce(&old, &author), Err(Rejection::Clock));
        let book = RustDelegates::default();
        book.record(relay_hop, &[a.clone(), old.clone()]);
        assert_eq!(book.rejected()[&key.public_key_hex()].reason, Rejection::Clock);
        assert_eq!(book.rejected()[&key.public_key_hex()].count, 2);
        a.node_id = Some(author.to_string());
        book.record(relay_hop, &[a]);
        assert!(book.rejected().is_empty(), "a valid announce clears the rejection");
    }
}
