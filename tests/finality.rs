//! Author: TechnoL0g
//!
//! SHIP-35 finality gadget: vote format, tracker (quorum, active-set filter, equivocation guard), certificate in sled,
//! soft-mode rollback (warns, drops the certificate) vs hard-mode rollback (refused).

use sth_core::crypto::KeyPair;
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::models::Block;
use sth_core::newnet::{generate, NewNetOptions};
use sth_core::node_config::NodeConfig;
use sth_core::p2p_iroh::finality::{sign_vote, verify_vote, FinalityTracker};
use sth_core::storage::Storage;
use sth_core::sync::{apply_blocks, ChainTip};
use std::sync::Arc;

struct Net {
    storage: Arc<Storage>,
    network: sth_core::config::Network,
    delegates: Vec<KeyPair>,
    tip: Block,
}

fn newnet(seed: &str, hard: bool) -> Net {
    let dir = tempfile::tempdir().unwrap();
    let opts = NewNetOptions { ticker: "FIN".into(), title: "FinNet".into(), delegates: 3, pubkey_hash: 30, p2p_port: 4102, api_port: 4104, metrics_port: 4989, seed: seed.into(), tokens_at: None, pq_at: None, pq_blocks_at: None, finality_hard: hard, treasury_supply: 1_000_000 * 100_000_000, delegate_stake: 1000 * 100_000_000 };
    let n = generate(&opts, dir.path()).unwrap();
    let network = NodeConfig::load(&dir.path().join("node.yaml")).unwrap().load_network().unwrap();
    let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    storage.set_undo_enabled(true);
    let delegates = n.delegate_passphrases.iter().map(|p| KeyPair::from_passphrase(p).unwrap()).collect();
    let tip = storage.get_last_block().unwrap().unwrap();
    Net { storage, network, delegates, tip }
}

impl Net {
    fn forge(&mut self, by: usize) -> Block {
        let slot = self.tip.height + 1;
        let b = forge_block(&self.network, &self.delegates[by], &self.tip, slot_start(slot, 8), vec![]).unwrap();
        apply_blocks(&self.storage, &self.network, &[b.clone()], ChainTip { height: self.tip.height, id: self.tip.id.clone() }, true).unwrap();
        self.tip = b.clone();
        b
    }
}

#[test]
fn vote_signature_round_trip_and_tamper_detection() {
    let k = KeyPair::from_passphrase("voter").unwrap();
    let id = "a".repeat(64);
    let v = sign_vote(&k, 42, &id).unwrap();
    assert!(verify_vote(&v));
    let mut other = v.clone();
    other.height = 43;
    assert!(!verify_vote(&other), "height is part of the signed message");
    let mut other = v.clone();
    other.block_id = "b".repeat(64);
    assert!(!verify_vote(&other));
    let mut other = v.clone();
    other.public_key = KeyPair::from_passphrase("impostor").unwrap().public_key_hex();
    assert!(!verify_vote(&other));
}

#[test]
fn quorum_of_active_delegates_certifies_and_stores_fc_record() {
    let mut n = newnet("fin-quorum", false);
    let b2 = n.forge(0);
    let id = b2.id.clone().unwrap();
    assert_eq!(n.network.milestone(2).finality_quorum(), 3, "3 delegates → ⌊2·3/3⌋+1 = 3");
    let tracker = FinalityTracker::new(n.storage.clone());

    // two of our own keys vote: below quorum, nothing certified
    let own = tracker.own_votes(&n.delegates[..2], 2, &id);
    assert_eq!(own.len(), 2);
    assert!(tracker.finalized().is_none());
    assert_eq!(tracker.snapshot().tip_votes, 2);

    // a stranger's vote is rejected (valid signature, not in the active set)
    let stranger = sign_vote(&KeyPair::from_passphrase("stranger").unwrap(), 2, &id).unwrap();
    assert_eq!(tracker.record(&[stranger]).accepted, 0);
    // a vote for another block id at the same height does not count towards ours
    let other = sign_vote(&n.delegates[2], 2, &"c".repeat(64)).unwrap();
    assert_eq!(tracker.record(&[other]).accepted, 1);
    assert!(tracker.finalized().is_none());

    // the third active delegate votes for our block → certificate — and, having voted for "cc…" too, an equivocation proof
    let third = sign_vote(&n.delegates[2], 2, &id).unwrap();
    let out = tracker.record(&[third.clone()]);
    assert_eq!(out.accepted, 1);
    assert_eq!(out.proofs.len(), 1);
    assert_eq!(out.proofs[0].public_key, third.public_key);
    assert!(n.storage.equivocation(&third.public_key).unwrap().is_some());
    assert!(!n.network.milestone(2).finality.slashing, "soft network: recorded, not enforced");
    assert_eq!(n.storage.active_delegates(3).unwrap().len(), 3, "no exclusion without finality.slashing");
    assert_eq!(tracker.finalized(), Some((2, id.clone())));
    let cert = n.storage.finality_cert(2).unwrap().unwrap();
    assert_eq!(cert.votes.len(), 3);
    assert!(cert.votes.contains_key(&third.public_key));
    assert_eq!(n.storage.latest_finality_cert().unwrap().unwrap().height, 2);
    let snap = tracker.snapshot();
    assert_eq!((snap.finalized_height, snap.lag, snap.quorum), (2, 0, 3));

    // a fresh tracker (restart) sees the certificate from sled
    assert!(FinalityTracker::new(n.storage.clone()).is_final(2));
}

#[test]
fn equivocation_guard_never_signs_two_ids_at_one_height() {
    let mut n = newnet("fin-equiv", false);
    let b2 = n.forge(1);
    let tracker = FinalityTracker::new(n.storage.clone());
    assert_eq!(tracker.own_votes(&n.delegates, 2, b2.id.as_deref().unwrap()).len(), 3);
    assert!(tracker.own_votes(&n.delegates, 2, &"d".repeat(64)).is_empty(), "second id at the same height must be refused");
    // same id again is fine (re-broadcast)
    assert_eq!(tracker.own_votes(&n.delegates, 2, b2.id.as_deref().unwrap()).len(), 3);
}

#[test]
fn soft_mode_rollback_warns_and_drops_certificate_hard_mode_refuses() {
    let mut soft = newnet("fin-soft", false);
    let b2 = soft.forge(0);
    let tracker = FinalityTracker::new(soft.storage.clone());
    tracker.own_votes(&soft.delegates, 2, b2.id.as_deref().unwrap());
    assert!(tracker.is_final(2));
    assert_eq!(soft.storage.rollback_last_block().unwrap(), 1, "soft mode: rollback allowed");
    assert!(soft.storage.latest_finality_cert().unwrap().is_none(), "certificate of the orphaned block is dropped");

    let mut hard = newnet("fin-hard", true);
    assert!(hard.network.milestone(1).finality.active);
    let b2 = hard.forge(0);
    let b3 = hard.forge(1);
    let tracker = FinalityTracker::new(hard.storage.clone());
    tracker.own_votes(&hard.delegates, 2, b2.id.as_deref().unwrap());
    // block 3 is not certified → may be rolled back; block 2 is final → refused
    assert_eq!(hard.storage.rollback_last_block().unwrap(), 2);
    let err = hard.storage.rollback_last_block().unwrap_err().to_string();
    assert!(err.contains("FinalityViolation"), "{err}");
    assert_eq!(hard.storage.get_last_height().unwrap(), 2);
    assert_ne!(b3.id, hard.storage.get_last_block().unwrap().unwrap().id);
}

#[test]
fn certificates_from_peers_are_verified_before_adoption() {
    use sth_core::storage::FinalityCert;
    let mut n = newnet("fin-adopt", false);
    let b2 = n.forge(0);
    let b3 = n.forge(1);
    let id2 = b2.id.clone().unwrap();
    let id3 = b3.id.clone().unwrap();
    let tracker = FinalityTracker::new(n.storage.clone());
    let votes_for = |height: u64, id: &str, keys: &[KeyPair]| keys.iter().map(|k| { let v = sign_vote(k, height, id).unwrap(); (v.public_key, v.signature) }).collect::<std::collections::BTreeMap<_, _>>();

    // below quorum
    let err = tracker.adopt_certificate(&FinalityCert { height: 2, block_id: id2.clone(), votes: votes_for(2, &id2, &n.delegates[..2]) }).unwrap_err();
    assert!(err.contains("quorum"), "{err}");
    // a stranger padding the vote count
    let mut padded = votes_for(2, &id2, &n.delegates[..2]);
    padded.extend(votes_for(2, &id2, &[KeyPair::from_passphrase("stranger").unwrap()]));
    assert!(tracker.adopt_certificate(&FinalityCert { height: 2, block_id: id2.clone(), votes: padded }).unwrap_err().contains("not an active delegate"));
    // signatures for another id
    assert!(tracker.adopt_certificate(&FinalityCert { height: 2, block_id: id2.clone(), votes: votes_for(2, &"e".repeat(64), &n.delegates) }).unwrap_err().contains("invalid vote signature"));
    // a valid certificate for a block we do not have
    let other = "f".repeat(64);
    assert!(tracker.adopt_certificate(&FinalityCert { height: 2, block_id: other.clone(), votes: votes_for(2, &other, &n.delegates) }).unwrap_err().contains("not in our chain"));
    // valid → stored; an older one afterwards is ignored (Ok(false))
    assert!(tracker.adopt_certificate(&FinalityCert { height: 3, block_id: id3.clone(), votes: votes_for(3, &id3, &n.delegates) }).unwrap());
    assert_eq!(tracker.finalized(), Some((3, id3)));
    assert!(!tracker.adopt_certificate(&FinalityCert { height: 2, block_id: id2.clone(), votes: votes_for(2, &id2, &n.delegates) }).unwrap());
    assert_eq!(n.storage.latest_finality_cert().unwrap().unwrap().height, 3);
}

#[test]
fn proven_double_vote_excludes_the_delegate_for_slashing_rounds_in_hard_mode() {
    use sth_core::storage::EquivocationProof;
    let mut n = newnet("fin-slash", true);
    assert!(n.network.milestone(1).finality.slashing);
    assert_eq!(n.network.milestone(1).finality.slashing_rounds(), 30);
    let b2 = n.forge(0);
    let id = b2.id.clone().unwrap();
    let tracker = FinalityTracker::new(n.storage.clone());
    let cheater = n.delegates[2].clone();
    assert_eq!(n.storage.active_delegates(3).unwrap().len(), 3);

    // two votes from one key for two ids at height 2 → proof, delegate leaves the active set for 30 rounds
    let a = sign_vote(&cheater, 2, &id).unwrap();
    let b = sign_vote(&cheater, 2, &"a".repeat(64)).unwrap();
    let out = tracker.record(&[a.clone(), b.clone()]);
    assert_eq!(out.proofs.len(), 1);
    let proof = &out.proofs[0];
    assert_eq!(proof.block_ids, [id.clone(), "a".repeat(64)]);
    assert_eq!(proof.signatures, [a.signature.clone(), b.signature.clone()]);
    let round_now = sth_core::delegate::round::round_info(3, 3).round;
    assert_eq!(proof.banned_until_round, round_now + 30);
    let active = n.storage.active_delegates(3).unwrap();
    assert_eq!(active.len(), 2, "only three delegates exist, the cheater is skipped");
    assert!(active.iter().all(|d| d.public_key != cheater.public_key_hex()));
    // round snapshots (what forging_order uses) inherit the exclusion
    let order = sth_core::delegate::round::forging_order(&n.storage, round_now, 3).unwrap();
    assert!(order.iter().all(|d| d.public_key != cheater.public_key_hex()));

    // a proof from the network is re-verified: forged signature / non-delegate / identical ids are refused
    let mut fake = proof.clone();
    fake.signatures[1] = a.signature.clone();
    let err = tracker.adopt_proof(&fake).unwrap_err();
    assert!(err.contains("does not verify"), "{err}");
    let mut same = proof.clone();
    same.block_ids[1] = id.clone();
    assert!(tracker.adopt_proof(&same).unwrap_err().contains("same block id"));
    let stranger = KeyPair::from_passphrase("nobody").unwrap();
    let s1 = sign_vote(&stranger, 2, &id).unwrap();
    let s2 = sign_vote(&stranger, 2, &"b".repeat(64)).unwrap();
    let foreign = EquivocationProof { height: 2, public_key: stranger.public_key_hex(), block_ids: [id.clone(), "b".repeat(64)], signatures: [s1.signature, s2.signature], detected_height: 2, banned_until_round: 10_000 };
    assert!(tracker.adopt_proof(&foreign).unwrap_err().contains("not an active delegate"));
    // the same proof again brings nothing new; an inflated ban from a peer is clamped to our own round + 30
    assert!(!tracker.adopt_proof(proof).unwrap());
    let mut inflated = proof.clone();
    inflated.height = 3;
    let a3 = sign_vote(&cheater, 3, &"c".repeat(64)).unwrap();
    let b3 = sign_vote(&cheater, 3, &"d".repeat(64)).unwrap();
    inflated.block_ids = ["c".repeat(64), "d".repeat(64)];
    inflated.signatures = [a3.signature, b3.signature];
    inflated.banned_until_round = 1_000_000;
    assert!(tracker.adopt_proof(&inflated).unwrap());
    assert_eq!(n.storage.equivocation(&cheater.public_key_hex()).unwrap().unwrap().banned_until_round, round_now + 30);
    assert_eq!(n.storage.equivocation_history(&cheater.public_key_hex()).unwrap().iter().map(|p| p.height).collect::<Vec<_>>(), vec![2, 3], "history keeps every height");
    assert_eq!(n.storage.banned_delegates(round_now, 3).unwrap(), vec![cheater.public_key_hex()], "one entry per delegate");
}

#[tokio::test]
async fn dashboard_shows_double_votes_and_proof_history() {
    use sth_core::api::{serve_listener, AppState};
    use sth_core::mempool::Mempool;
    let n = tokio::task::spawn_blocking(|| {
        let mut n = newnet("fin-dash", true);
        let b2 = n.forge(0);
        let tracker = FinalityTracker::new(n.storage.clone());
        let cheater = &n.delegates[2];
        let id = b2.id.clone().unwrap();
        tracker.record(&[sign_vote(cheater, 2, &id).unwrap(), sign_vote(cheater, 2, &"a".repeat(64)).unwrap()]);
        n
    })
    .await
    .unwrap();
    let mempool = Arc::new(Mempool::new(n.storage.clone(), vec![], 10));
    let state = Arc::new(AppState::new(n.storage.clone(), mempool, vec![]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { serve_listener(state, listener).await.unwrap() });
    let d: serde_json::Value = reqwest::get(format!("{base}/api/ntfry/delegates")).await.unwrap().json().await.unwrap();
    let d = &d["data"];
    assert_eq!(d["slashing"]["enabled"], true);
    assert_eq!(d["slashing"]["rounds"], 30);
    let cheater_pk = n.delegates[2].public_key_hex();
    let row = d["list"].as_array().unwrap().iter().find(|x| x["publicKey"] == cheater_pk).expect("cheater still listed (ranked by votes)");
    assert_eq!(row["doubleVotes"], 1);
    assert_eq!(row["banned"], true);
    assert_eq!(row["bannedUntilRound"], d["slashing"]["currentRound"].as_u64().unwrap() + 30);
    let honest = d["list"].as_array().unwrap().iter().find(|x| x["publicKey"] == n.delegates[0].public_key_hex()).unwrap();
    assert_eq!((honest["doubleVotes"].as_u64(), honest["banned"].as_bool()), (Some(0), Some(false)));
    let eq = d["equivocations"].as_array().unwrap();
    assert_eq!(eq.len(), 1);
    assert_eq!(eq[0]["height"], 2);
    assert_eq!(eq[0]["banned"], true);
    assert_eq!(eq[0]["roundsLeft"], 30);
    assert_eq!(eq[0]["blockIds"].as_array().unwrap().len(), 2);
    let f: serde_json::Value = reqwest::get(format!("{base}/api/ntfry/finality")).await.unwrap().json().await.unwrap();
    assert_eq!(f["data"]["equivocations"].as_array().unwrap().len(), 1);
}
