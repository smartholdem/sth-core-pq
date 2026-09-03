//! Author: TechnoL0g
//!
//! Legacy inter-node protocol (CORE_P2P_PORT 4001) split into small modules:
//! `proto` (protobuf messages), `client` (nes framing + `LegacyPeer`), `health` (peer table:
//! latency / height / failures), `follow` (parallel catch-up + live follow) and `relay`
//! (mempool → `postTransactions` fan-out).

mod client;
pub mod follow;
pub mod health;
mod proto;
pub mod relay;
pub mod server;

pub use client::{decode_blocks, LegacyPeer};
pub use follow::{catch_up, run as run_follow, P2pOptions};
pub use health::{PeerStats, PeerTable};
pub use proto::*;
pub use relay::broadcast;
pub use server::LegacyServer;

use std::time::Duration;

pub const DEFAULT_P2P_PORT: u16 = 4001;

/// Published peer list of the mainnet (maintained by the SmartHoldem team).
pub const PEERS_URL: &str = "https://raw.githubusercontent.com/smartholdem/data/main/mainnet/peers.json";

/// Seed peers of the legacy network. The P2P port must be addressed by IP — the
/// `nodeN.smartholdem.io` hostnames sit behind a reverse proxy that answers 403 on 4001.
pub const P2P_SEEDS: &[&str] = &[
    "138.199.164.235",
    "138.199.149.214",
    "116.202.32.250",
    "188.245.166.98",
    "188.245.206.222",
    "136.243.144.114",
    "91.99.119.119",
    "159.69.188.60",
    "78.47.194.10",
    "157.180.114.125",
    "95.217.132.244",
];
/// Version advertised in `headers.version` (peers reject unknown majors).
pub const PEER_VERSION: &str = "3.8.2";
/// Server-side hard limit of `p2p.blocks.getBlocks`.
pub const MAX_BLOCKS_PER_REQUEST: u32 = 400;

/// `error sending request` alone hides the cause; append the whole `source()` chain (DNS, timeout, TLS…).
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        let part = s.to_string();
        if !msg.contains(&part) {
            msg.push_str(": ");
            msg.push_str(&part);
        }
        src = s.source();
    }
    msg
}

/// Fetch `peers.json` (`[{ "ip", "port" }]`) and return `ip` entries for `port`; falls back to `P2P_SEEDS`.
/// Two attempts; network failures are not fatal — the built-in seeds are always enough to start.
pub async fn fetch_peer_list(port: u16) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Entry {
        ip: String,
        port: u16,
    }
    let seeds = || P2P_SEEDS.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut last_err = String::new();
    for attempt in 1..=2u8 {
        let fetched = async {
            let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
            client.get(PEERS_URL).send().await?.error_for_status()?.json::<Vec<Entry>>().await
        }
        .await;
        match fetched {
            Ok(list) if !list.is_empty() => {
                let mut ips: Vec<String> = list.into_iter().filter(|e| e.port == port).map(|e| e.ip).collect();
                for seed in P2P_SEEDS {
                    if !ips.iter().any(|ip| ip == seed) {
                        ips.push(seed.to_string());
                    }
                }
                tracing::info!(count = ips.len(), url = PEERS_URL, "peer list loaded");
                return ips;
            }
            Ok(_) => {
                tracing::warn!(url = PEERS_URL, seeds = P2P_SEEDS.len(), "peers.json is empty, using built-in seed peers");
                return seeds();
            }
            Err(e) => {
                last_err = error_chain(&e);
                if attempt < 2 {
                    tracing::debug!(error = %last_err, attempt, "peers.json fetch failed, retrying");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
    tracing::warn!(
        error = %last_err,
        url = PEERS_URL,
        seeds = P2P_SEEDS.len(),
        "cannot fetch peers.json (no internet / DNS / GitHub unreachable) — using built-in seed peers, more are discovered via getPeers"
    );
    seeds()
}
