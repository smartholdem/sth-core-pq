//! Author: TechnoL0g
//!
//! Live mainnet probe (ignored by default): download the range around a reported fork from a legacy peer and run it
//! through our decoder + fork detection. `cargo test --test live_fork_probe -- --ignored --nocapture`.

use sth_core::p2p_legacy::LegacyPeer;
use std::time::Duration;

#[tokio::test]
#[ignore]
async fn decode_range_around_fork() {
    let from: u64 = std::env::var("FROM").ok().and_then(|v| v.parse().ok()).unwrap_or(11_891_755);
    let ip = std::env::var("PEER").unwrap_or_else(|_| "78.47.194.10".to_string());
    let mut peer = LegacyPeer::connect(&ip, 4001, Duration::from_secs(10)).await.expect("connect");
    let status = peer.get_status().await.expect("status");
    println!("peer {ip} height {}", status.state.as_ref().map(|s| s.height).unwrap_or(0));
    let mut peer = LegacyPeer::connect(&ip, 4001, Duration::from_secs(20)).await.expect("connect");
    match peer.get_blocks(from, 400).await {
        Ok(blocks) => {
            println!("got {} blocks {}..{}", blocks.len(), blocks.first().map(|b| b.height).unwrap_or(0), blocks.last().map(|b| b.height).unwrap_or(0));
            for b in blocks.iter().filter(|b| !b.transactions.is_empty()) {
                println!("  {} txs={} prev={} gen={}", b.height, b.transactions.len(), &b.previous_block[..10], &b.generator_public_key[..10]);
            }
            println!("first previous_block = {}", blocks[0].previous_block);
        }
        Err(e) => println!("GET BLOCKS FAILED: {e}"),
    }
}
