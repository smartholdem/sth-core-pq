//! Author: TechnoL0g
//!
//! SHIP-35 BFT finality gadget: active delegates vote for `(height, blockId)` over the `finality` gossip topic; ≥ quorum
//! (⌊2n/3⌋ + 1 = 15 of 21) distinct votes form a certificate stored as `fc:<height>`. Soft mode counts and shows finality;
//! milestone `finality.active` (hard mode) forbids rolling back below the highest certificate. Legacy nodes never see votes.

use super::proto::FinalityVote;
use crate::crypto::KeyPair;
use crate::storage::{EquivocationProof, FinalityCert, Storage};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

/// Votes older than this many blocks below the tip are dropped.
const KEEP_BEHIND: u64 = 120;
/// Votes may run ahead of our tip by this much (the block is still on its way).
const AHEAD: u64 = 21;

pub fn vote_hash(height: u64, block_id: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"sth-finality-v1");
    h.update(height.to_le_bytes());
    h.update(hex::decode(block_id).unwrap_or_else(|_| block_id.as_bytes().to_vec()));
    h.finalize().into()
}

pub fn sign_vote(keys: &KeyPair, height: u64, block_id: &str) -> Option<FinalityVote> {
    let signature = crate::crypto::sign_schnorr_legacy(&vote_hash(height, block_id), keys.private_key()).ok()?;
    Some(FinalityVote { height, block_id: block_id.to_string(), public_key: keys.public_key_hex(), signature })
}

pub fn verify_vote(v: &FinalityVote) -> bool {
    v.block_id.len() == 64
        && hex::decode(&v.signature).ok().is_some_and(|sig| crate::crypto::verify_schnorr_legacy(&vote_hash(v.height, &v.block_id), &sig, &v.public_key).unwrap_or(false))
}

#[derive(Default)]
struct Inner {
    /// height → blockId → delegate public key → signature
    votes: BTreeMap<u64, HashMap<String, BTreeMap<String, String>>>,
    /// Our own votes (equivocation guard): height → blockId.
    own: BTreeMap<u64, String>,
    /// Votes received / rejected since start (for the metrics page).
    received: u64,
    rejected: u64,
}

pub struct RecordOutcome {
    pub accepted: usize,
    pub proofs: Vec<EquivocationProof>,
}

/// Snapshot for `/api/ntfry/finality` and the metrics page.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinalitySnapshot {
    pub finalized_height: u64,
    pub finalized_id: Option<String>,
    pub lag: u64,
    pub quorum: u32,
    pub active_delegates: u32,
    pub hard: bool,
    pub tip_votes: usize,
    pub received: u64,
    pub rejected: u64,
    pub own_votes: usize,
    pub heights: Vec<HeightVotes>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeightVotes {
    pub height: u64,
    pub block_id: String,
    pub votes: usize,
    pub certified: bool,
}

pub struct FinalityTracker {
    storage: Arc<Storage>,
    inner: Mutex<Inner>,
}

impl FinalityTracker {
    pub fn new(storage: Arc<Storage>) -> Self {
        Self { storage, inner: Mutex::new(Inner::default()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Active delegate set (public keys) of the round that contains `height`.
    fn active_set(&self, height: u64) -> Vec<String> {
        let n = self.storage.network().milestone(height.max(1)).active_delegates as usize;
        let round = crate::delegate::round::round_info(height, n as u64).round;
        crate::delegate::round::forging_order(&self.storage, round, n).map(|l| l.into_iter().map(|d| d.public_key).collect()).unwrap_or_default()
    }

    /// Verify and record votes from the network. Returns the number accepted and the equivocation proofs discovered
    /// (a key that already voted for another id at that height) — the caller broadcasts them.
    pub fn record(&self, votes: &[FinalityVote]) -> RecordOutcome {
        let tip = self.storage.get_last_height().unwrap_or(0);
        let mut ok = 0;
        let mut proofs = Vec::new();
        let mut set_cache: HashMap<u64, Vec<String>> = HashMap::new();
        for v in votes.iter().take(64) {
            let in_range = v.height + KEEP_BEHIND >= tip && v.height <= tip + AHEAD;
            let active = in_range && {
                let n = self.storage.network().milestone(v.height.max(1)).active_delegates as u64;
                let round = crate::delegate::round::round_info(v.height, n).round;
                set_cache.entry(round).or_insert_with(|| self.active_set(v.height)).contains(&v.public_key)
            };
            if !active || !verify_vote(v) {
                self.lock().rejected += 1;
                continue;
            }
            let conflict = {
                let mut i = self.lock();
                i.received += 1;
                let at_height = i.votes.entry(v.height).or_default();
                let conflict = at_height.iter().find(|(id, sigs)| **id != v.block_id && sigs.contains_key(&v.public_key)).map(|(id, sigs)| (id.clone(), sigs[&v.public_key].clone()));
                at_height.entry(v.block_id.clone()).or_default().insert(v.public_key.clone(), v.signature.clone());
                conflict
            };
            if let Some((other_id, other_sig)) = conflict {
                let proof = self.make_proof(v, &other_id, &other_sig, tip);
                if self.adopt_proof(&proof).unwrap_or(false) {
                    proofs.push(proof);
                }
            }
            ok += 1;
            self.try_certify(v.height, &v.block_id);
        }
        self.prune(tip);
        RecordOutcome { accepted: ok, proofs }
    }

    fn make_proof(&self, v: &FinalityVote, other_id: &str, other_sig: &str, tip: u64) -> EquivocationProof {
        let ms = self.storage.network().milestone(tip.max(1));
        let round = crate::delegate::round::round_info(tip + 1, ms.active_delegates as u64).round;
        EquivocationProof {
            height: v.height,
            public_key: v.public_key.clone(),
            block_ids: [other_id.to_string(), v.block_id.clone()],
            signatures: [other_sig.to_string(), v.signature.clone()],
            detected_height: tip,
            banned_until_round: round + ms.finality.slashing_rounds(),
        }
    }

    /// Delegates of the round of `height` as ranked by votes — the saved round snapshot, else the ranking without slashing
    /// (a key already banned must still be a valid subject of a proof).
    fn ranked_set(&self, height: u64) -> Vec<String> {
        let n = self.storage.network().milestone(height.max(1)).active_delegates as usize;
        let round = crate::delegate::round::round_info(height, n as u64).round;
        let list = match self.storage.get_round(round) {
            Ok(Some(l)) => l,
            _ => self.storage.active_delegates_ranked(n).unwrap_or_default(),
        };
        list.into_iter().map(|d| d.public_key).collect()
    }

    /// Check a proof from the network or our own detection: two valid votes of one active delegate for two ids at one height.
    pub fn verify_proof(&self, p: &EquivocationProof) -> std::result::Result<(), String> {
        if p.block_ids[0] == p.block_ids[1] {
            return Err("same block id twice".into());
        }
        if !self.ranked_set(p.height).contains(&p.public_key) {
            return Err(format!("{} is not an active delegate of that round", p.public_key));
        }
        for k in 0..2 {
            let v = FinalityVote { height: p.height, block_id: p.block_ids[k].clone(), public_key: p.public_key.clone(), signature: p.signatures[k].clone() };
            if !verify_vote(&v) {
                return Err(format!("vote {k} does not verify"));
            }
        }
        Ok(())
    }

    /// Store a verified proof (history per delegate, one record per height). Ok(true) = new information.
    pub fn adopt_proof(&self, p: &EquivocationProof) -> std::result::Result<bool, String> {
        self.verify_proof(p)?;
        let tip = self.storage.get_last_height().map_err(|e| e.to_string())?;
        let ms = self.storage.network().milestone(tip.max(1));
        let round = crate::delegate::round::round_info(tip + 1, ms.active_delegates as u64).round;
        // the ban is measured from OUR view of the current round, never trusting the sender's number
        let mut p = p.clone();
        p.banned_until_round = p.banned_until_round.min(round + ms.finality.slashing_rounds());
        if let Some(have) = self.storage.equivocation(&p.public_key).map_err(|e| e.to_string())? {
            if have.height >= p.height {
                return Ok(false);
            }
        }
        self.storage.put_equivocation(&p).map_err(|e| e.to_string())?;
        tracing::warn!(delegate = %p.public_key, height = p.height, banned_until_round = p.banned_until_round, slashing = ms.finality.slashing, "EQUIVOCATION proven: delegate voted for two blocks at one height");
        Ok(true)
    }

    /// Sign votes for `block` with every one of our keys that is in the active set — never twice for one height.
    pub fn own_votes(&self, keys: &[KeyPair], height: u64, block_id: &str) -> Vec<FinalityVote> {
        if keys.is_empty() {
            return Vec::new();
        }
        let set = self.active_set(height);
        let mut out = Vec::new();
        let mut i = self.lock();
        if i.own.get(&height).is_some_and(|id| id != block_id) {
            tracing::warn!(height, "finality: refusing to vote for a second block at this height (equivocation guard)");
            return out;
        }
        for k in keys.iter().filter(|k| set.contains(&k.public_key_hex())) {
            if let Some(v) = sign_vote(k, height, block_id) {
                i.votes.entry(height).or_default().entry(block_id.to_string()).or_default().insert(v.public_key.clone(), v.signature.clone());
                out.push(v);
            }
        }
        if !out.is_empty() {
            i.own.insert(height, block_id.to_string());
        }
        drop(i);
        self.try_certify(height, block_id);
        out
    }

    /// Store a certificate once quorum is reached and the block is in our chain under that id.
    fn try_certify(&self, height: u64, block_id: &str) {
        let quorum = self.storage.network().milestone(height.max(1)).finality_quorum() as usize;
        if self.finalized().is_some_and(|(h, _)| h >= height) {
            return;
        }
        let sigs = {
            let i = self.lock();
            match i.votes.get(&height).and_then(|m| m.get(block_id)) {
                Some(s) if s.len() >= quorum => s.clone(),
                _ => return,
            }
        };
        let ours = self.storage.get_block_by_height(height).ok().flatten().and_then(|b| b.id);
        if ours.as_deref() != Some(block_id) {
            return;
        }
        let cert = FinalityCert { height, block_id: block_id.to_string(), votes: sigs.into_iter().collect() };
        if let Err(e) = self.storage.put_finality_cert(&cert) {
            tracing::warn!(error = %e, height, "finality certificate not stored");
            return;
        }
        tracing::info!(height, votes = cert.votes.len(), quorum, "block FINAL (SHIP-35 certificate stored)");
    }

    /// Adopt a certificate received over RPC: every vote must verify, come from the active set of that round and there must be
    /// ≥ quorum of them; the block must be in our chain under that id and the certificate must beat the one we hold.
    pub fn adopt_certificate(&self, cert: &FinalityCert) -> std::result::Result<bool, String> {
        let quorum = self.storage.network().milestone(cert.height.max(1)).finality_quorum() as usize;
        if cert.votes.len() < quorum {
            return Err(format!("certificate has {} votes, quorum is {quorum}", cert.votes.len()));
        }
        let set = self.active_set(cert.height);
        for (pk, sig) in &cert.votes {
            if !set.contains(pk) {
                return Err(format!("vote by {pk} — not an active delegate of that round"));
            }
            if !verify_vote(&FinalityVote { height: cert.height, block_id: cert.block_id.clone(), public_key: pk.clone(), signature: sig.clone() }) {
                return Err(format!("invalid vote signature by {pk}"));
            }
        }
        let ours = self.storage.get_block_by_height(cert.height).map_err(|e| e.to_string())?.and_then(|b| b.id);
        if ours.as_deref() != Some(cert.block_id.as_str()) {
            return Err(format!("block {} is not in our chain under id {}", cert.height, cert.block_id));
        }
        if self.finalized().is_some_and(|(h, _)| h >= cert.height) {
            return Ok(false);
        }
        self.storage.put_finality_cert(cert).map_err(|e| e.to_string())?;
        tracing::info!(height = cert.height, votes = cert.votes.len(), "finality certificate adopted from a peer (GetFinality)");
        Ok(true)
    }

    fn prune(&self, tip: u64) {
        let mut i = self.lock();
        let cut = tip.saturating_sub(KEEP_BEHIND);
        i.votes = i.votes.split_off(&cut);
        i.own = i.own.split_off(&cut);
    }

    /// Highest certificate in sled (survives restarts; removed by a soft-mode rollback).
    pub fn finalized(&self) -> Option<(u64, String)> {
        self.storage.latest_finality_cert().ok().flatten().map(|c| (c.height, c.block_id))
    }

    pub fn is_final(&self, height: u64) -> bool {
        self.finalized().is_some_and(|(h, _)| h >= height)
    }

    pub fn snapshot(&self) -> FinalitySnapshot {
        let tip = self.storage.get_last_height().unwrap_or(0);
        let ms = self.storage.network().milestone(tip.max(1));
        let tip_id = self.storage.get_last_block().ok().flatten().and_then(|b| b.id);
        let (finalized_height, finalized_id) = self.finalized().map(|(h, id)| (h, Some(id))).unwrap_or((0, None));
        let i = self.lock();
        let tip_votes = tip_id.as_ref().and_then(|id| i.votes.get(&tip).and_then(|m| m.get(id))).map(|s| s.len()).unwrap_or(0);
        let heights = i
            .votes
            .iter()
            .rev()
            .take(8)
            .flat_map(|(h, m)| m.iter().map(move |(id, s)| HeightVotes { height: *h, block_id: id.clone(), votes: s.len(), certified: finalized_height >= *h && finalized_height > 0 }))
            .collect();
        FinalitySnapshot {
            finalized_height,
            finalized_id,
            lag: tip.saturating_sub(finalized_height),
            quorum: ms.finality_quorum(),
            active_delegates: ms.active_delegates,
            hard: ms.finality.active,
            tip_votes,
            received: i.received,
            rejected: i.rejected,
            own_votes: i.own.len(),
            heights,
        }
    }
}
