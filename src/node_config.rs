//! Author: TechnoL0g
//!
//! Node configuration file (`node.yaml`, same style as the netfory headless seeder).
//! Every module (api, sync, p2p, rewards, mempool) has its own section so new modules - e.g. the
//! future delegate/forger module - plug in by adding a section without touching the others.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DEFAULT_CONFIG_FILE: &str = "node.yaml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NodeConfig {
    pub network: String,
    /// Directory with `network.json`, `milestones.json`, `exceptions.json`, `genesisBlock.json[.gz]`
    /// (crypto-networks layout). Empty = embedded mainnet files. Lets a future milestone (e.g. forging
    /// parameters from block N) be rolled out by editing JSON only.
    pub network_dir: String,
    pub db_path: String,
    pub api: ApiConfig,
    pub sync: SyncSection,
    pub p2p: P2pConfig,
    pub rewards: RewardsConfig,
    pub mempool: MempoolConfig,
    pub delegate: DelegateConfig,
}

/// Forging module (off unless secrets are configured). Mirrors the legacy `delegates.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DelegateConfig {
    pub enabled: bool,
    /// Delegate passphrases (same meaning as `secrets` in the legacy delegates.json).
    pub secrets: Vec<String>,
    /// Optional path to a legacy `delegates.json`; its `secrets` are merged with the list above.
    pub secrets_file: String,
    /// Legacy peers that receive every forged block via `postBlock`.
    pub broadcast_fanout: usize,
    /// Share (0.0–1.0) of responding peers (legacy + iroh) that must report our exact tip before forging.
    pub quorum_share: f64,
}

impl Default for DelegateConfig {
    fn default() -> Self {
        Self { enabled: false, secrets: Vec::new(), secrets_file: String::new(), broadcast_fanout: 6, quorum_share: 0.5 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ApiConfig {
    pub enabled: bool,
    /// Keep 127.0.0.1 - the API is a local bridge for netfory-provider, never public.
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SyncSection {
    /// Legacy REST nodes used for bootstrap / relay fallback.
    pub rest_nodes: Vec<String>,
    pub verify_blocks: bool,
    pub concurrency: usize,
    /// Snapshot used when the database is empty: a dump path, `latest`, or empty to skip.
    pub bootstrap_snapshot: String,
    pub fast_import: bool,
    pub snapshot_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct P2pConfig {
    /// Follow the chain over the legacy P2P port (IP peers) instead of REST polling.
    pub legacy_enabled: bool,
    pub legacy_port: u16,
    /// Extra seed IPs (merged with peers.json + built-in seeds unless `use_peer_list` is false).
    pub legacy_peers: Vec<String>,
    /// Load the published peers.json at start.
    pub use_peer_list: bool,
    /// Inbound legacy P2P server, e.g. "0.0.0.0:4001" (gateway nodes with a public IP; also what a
    /// netfory-provider `local_ws_url: ws://127.0.0.1:4001` endpoint proxies). Empty = disabled.
    pub legacy_listen: String,
    /// Public address of our legacy port announced to Rust peers over iroh (`ip:4001`), e.g. for gateway nodes.
    pub legacy_public_addr: String,
    /// Peers pulled from concurrently during catch-up (400 blocks each).
    pub parallel_peers: usize,
    /// Peers a new transaction is relayed to.
    pub relay_fanout: usize,
    /// Seconds between peer-table health probes.
    pub refresh_secs: u64,
    /// Web 4.0 layer: iroh endpoint + gossip (runs next to the legacy port until the network migrates).
    pub iroh: IrohConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct IrohConfig {
    pub enabled: bool,
    /// File holding the 32-byte Ed25519 secret key (hex). Created on first start; the EndpointId is derived from it.
    pub secret_key_file: String,
    /// EndpointIds (z-base-32 / hex) of nodes to join the gossip swarm through.
    pub bootstrap: Vec<String>,
    /// Serve `GetStatus` / `GetBlocks` to other iroh nodes.
    pub serve_blocks: bool,
    /// Use the public n0 relay servers for NAT traversal (turn off for LAN-only setups).
    pub relay: bool,
}

impl Default for IrohConfig {
    fn default() -> Self {
        Self { enabled: false, secret_key_file: "./iroh.key".into(), bootstrap: Vec::new(), serve_blocks: true, relay: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RewardsConfig {
    /// Where future rewards (relay / delegate module) should go. Provide EITHER an address ...
    pub reward_address: String,
    /// ... OR a passphrase to derive the address from (SmartNet wallet scheme).
    pub reward_passphrase: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MempoolConfig {
    pub max_size: usize,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            network: "mainnet".into(),
            network_dir: String::new(),
            db_path: "./data".into(),
            api: ApiConfig::default(),
            sync: SyncSection::default(),
            p2p: P2pConfig::default(),
            rewards: RewardsConfig::default(),
            mempool: MempoolConfig::default(),
            delegate: DelegateConfig::default(),
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self { enabled: true, host: "127.0.0.1".into(), port: 4003 }
    }
}

impl Default for SyncSection {
    fn default() -> Self {
        Self {
            rest_nodes: crate::sync::NODES.iter().map(|s| s.to_string()).collect(),
            verify_blocks: true,
            concurrency: 8,
            bootstrap_snapshot: "latest".into(),
            fast_import: true,
            snapshot_dir: "./snapshots".into(),
        }
    }
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            legacy_enabled: true,
            legacy_port: crate::p2p_legacy::DEFAULT_P2P_PORT,
            legacy_peers: Vec::new(),
            use_peer_list: true,
            legacy_listen: String::new(),
            legacy_public_addr: String::new(),
            parallel_peers: 4,
            relay_fanout: 3,
            refresh_secs: 60,
            iroh: IrohConfig::default(),
        }
    }
}

impl Default for RewardsConfig {
    fn default() -> Self {
        Self { reward_address: String::new(), reward_passphrase: String::new() }
    }
}

impl Default for MempoolConfig {
    fn default() -> Self {
        Self { max_size: 5_000 }
    }
}

impl NodeConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        Self::parse(&raw).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
    }

    pub fn parse(yaml: &str) -> Result<Self> {
        let cfg: Self = serde_yaml::from_str(yaml).map_err(|e| Error::Config(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// `node.yaml` in the working directory when present, built-in defaults otherwise.
    pub fn load_or_default(explicit: Option<&Path>) -> Result<Self> {
        match explicit {
            Some(p) => Self::load(p),
            None => {
                let p = Path::new(DEFAULT_CONFIG_FILE);
                if p.exists() {
                    tracing::info!(file = DEFAULT_CONFIG_FILE, "using configuration file");
                    Self::load(p)
                } else {
                    Ok(Self::default())
                }
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.network != "mainnet" {
            return Err(Error::Config(format!("only mainnet is supported (got {})", self.network)));
        }
        if !(0.0..=1.0).contains(&self.delegate.quorum_share) {
            return Err(Error::Config("delegate.quorum_share must be between 0.0 and 1.0".into()));
        }
        if self.p2p.parallel_peers == 0 {
            return Err(Error::Config("p2p.parallel_peers must be >= 1".into()));
        }
        if !self.rewards.reward_address.is_empty() && !crate::crypto::validate_address(&self.rewards.reward_address, self.network_byte()) {
            return Err(Error::Config(format!("rewards.reward_address {} is not a valid address", self.rewards.reward_address)));
        }
        Ok(())
    }

    pub fn network_byte(&self) -> u8 {
        self.load_network().map(|n| n.pubkey_hash).unwrap_or(63)
    }

    /// Network files from `network_dir`, or the embedded mainnet set.
    pub fn load_network(&self) -> Result<crate::config::Network> {
        if self.network_dir.trim().is_empty() {
            Ok(crate::config::Network::mainnet())
        } else {
            crate::config::Network::from_dir(Path::new(self.network_dir.trim()))
        }
    }

    /// Write the embedded network files into `dir` (crypto-networks layout) for local editing.
    pub fn export_network_files(dir: &Path) -> Result<()> {
        use crate::config::{MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ, MAINNET_MILESTONES_JSON, MAINNET_NETWORK_JSON};
        std::fs::create_dir_all(dir).map_err(|e| Error::Config(format!("cannot create {}: {e}", dir.display())))?;
        let write = |name: &str, bytes: &[u8]| -> Result<()> {
            let p = dir.join(name);
            std::fs::write(&p, bytes).map_err(|e| Error::Config(format!("cannot write {}: {e}", p.display())))
        };
        write("network.json", MAINNET_NETWORK_JSON.as_bytes())?;
        write("milestones.json", MAINNET_MILESTONES_JSON.as_bytes())?;
        write("exceptions.json", MAINNET_EXCEPTIONS_JSON.as_bytes())?;
        write("genesisBlock.json.gz", MAINNET_GENESIS_GZ)
    }

    /// Write the commented default config (no overwrite).
    pub fn write_default(path: &Path) -> Result<()> {
        if path.exists() {
            return Err(Error::Config(format!("{} already exists", path.display())));
        }
        std::fs::write(path, Self::default_yaml()).map_err(|e| Error::Config(format!("cannot write {}: {e}", path.display())))
    }

    pub fn default_yaml() -> String {
        let body = serde_yaml::to_string(&Self::default()).unwrap_or_default();
        format!(
            "# sth-core relay node configuration (generated by `sth-core init`)\n\
             # Sections map 1:1 to modules: api, sync, p2p, rewards, mempool, delegate.\n\
             #\n\
             # network_dir: optional folder with network.json / milestones.json / exceptions.json / genesisBlock.json[.gz]\n\
             #          (crypto-networks layout, `sth-core init --network-files` exports the embedded ones).\n\
             # api:     local REST bridge for wallets / explorers / netfory-provider (keep host 127.0.0.1).\n\
             # sync:    legacy REST bootstrap nodes; bootstrap_snapshot is used only while the database is empty\n\
             #          (`latest` downloads from https://snapshots.smartholdem.io/, a path imports a local dump, empty skips).\n\
             # p2p:     legacy_enabled follows the chain through port 4001 (IP peers) with parallel_peers ranges in flight;\n\
             #          relay_fanout peers receive every accepted transaction. legacy_listen: 0.0.0.0:4001 turns on the inbound\n\
             #          legacy server (gateway node) - old nodes and netfory-provider ws:// clients can pull blocks from us.\n\
             #          p2p.iroh enables the Web 4.0 layer\n\
             #          (iroh endpoint + gossip topics for blocks / transactions, GetBlocks RPC); bootstrap = EndpointIds of peers.\n\
             # rewards: where future seeding / delegate rewards go. Provide EITHER reward_address ...\n\
             #          OR reward_passphrase (the address is derived from it; the passphrase never leaves this machine).\n\
             # delegate: forging module. enabled: true + secrets: [passphrase] (or secrets_file: ./delegates.json, legacy format).\n\
             #          Forging happens only when this delegate is in the active top-21 of the round; fees go to its wallet.\n\
             {body}"
        )
    }

    /// Reward address: explicit address wins, otherwise derived from the passphrase.
    pub fn reward_address(&self) -> Result<Option<String>> {
        if !self.rewards.reward_address.is_empty() {
            return Ok(Some(self.rewards.reward_address.clone()));
        }
        if !self.rewards.reward_passphrase.is_empty() {
            return crate::crypto::KeyPair::from_passphrase(&self.rewards.reward_passphrase)?.address(self.network_byte()).map(Some);
        }
        Ok(None)
    }
}
