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

#[derive(Default)]
pub struct RustDelegates {
    map: Mutex<HashMap<String, RustDelegateInfo>>,
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
            public_key: k.public_key_hex(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            timestamp,
            signature,
        }))
        .collect()
}

pub fn verify_announce(a: &DelegateAnnounce, from: &EndpointId) -> bool {
    if a.timestamp.abs_diff(now_unix()) > MAX_SKEW {
        return false;
    }
    crate::crypto::verify_signature(&announce_hash(from, a.timestamp), &a.signature, &a.public_key).unwrap_or(false)
}

impl RustDelegates {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, RustDelegateInfo>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records verified announces; returns how many were accepted.
    pub fn record(&self, from: EndpointId, announces: &[DelegateAnnounce]) -> usize {
        let mut map = self.lock();
        let mut ok = 0;
        for a in announces.iter().take(64) {
            if verify_announce(a, &from) {
                map.insert(a.public_key.clone(), RustDelegateInfo { version: a.version.clone(), node: from, last_seen: Instant::now() });
                ok += 1;
            }
        }
        ok
    }

    /// Fresh entries only (seen within `STALE_AFTER`).
    pub fn snapshot(&self) -> HashMap<String, RustDelegateInfo> {
        self.lock().iter().filter(|(_, i)| i.last_seen.elapsed() < STALE_AFTER).map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}
