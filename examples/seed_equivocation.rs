//! Author: TechnoL0g
//! Dev helper: record a double vote of the first newnet delegate into a stopped node's database (dashboard demo).
//! Usage: cargo run --example seed_equivocation -- <net dir>
use sth_core::crypto::KeyPair;
use sth_core::node_config::NodeConfig;
use sth_core::p2p_iroh::finality::{sign_vote, FinalityTracker};
use sth_core::storage::Storage;
use std::sync::Arc;

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("net dir"));
    let cfg = NodeConfig::load(&dir.join("node.yaml")).unwrap();
    let network = cfg.load_network().unwrap();
    let storage = Arc::new(Storage::open(dir.join(cfg.db_path.trim_start_matches("./")), network).unwrap());
    let delegates: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("delegates.json")).unwrap()).unwrap();
    let key = KeyPair::from_passphrase(delegates["delegates"][0]["passphrase"].as_str().unwrap()).unwrap();
    let tip = storage.get_last_block().unwrap().unwrap();
    let id = tip.id.clone().unwrap();
    let tracker = FinalityTracker::new(storage.clone());
    let out = tracker.record(&[sign_vote(&key, tip.height, &id).unwrap(), sign_vote(&key, tip.height, &"a".repeat(64)).unwrap()]);
    println!("proofs recorded: {} at height {}", out.proofs.len(), tip.height);
}
