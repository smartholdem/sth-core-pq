//! Author: TechnoL0g
//!
//! Node configuration file (`node.yaml`, same style as the netfory headless seeder).
//! Every module (api, sync, p2p, rewards, mempool) has its own section so new modules — e.g. the
//! future delegate/forger module — plug in by adding a section without touching the others.

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
    /// sled page cache in MiB (default 64). Lower it (16–32) on VPS with ≤1 GB RAM; sled may use ~2× this value.
    pub db_cache_mb: u32,
    /// Count the wallets for the startup banner (a full `w:` key scan, ~1–3 s on a mainnet database with a cold page
    /// cache). Informational only — the state itself lives in sled and is never regenerated at start (unlike the
    /// legacy "State Generation" steps); registered delegates are always counted (cheap `wu:` index).
    pub startup_scan: bool,
    pub api: ApiConfig,
    pub sync: SyncSection,
    pub p2p: P2pConfig,
    pub rewards: RewardsConfig,
    pub mempool: MempoolConfig,
    pub delegate: DelegateConfig,
    pub alerts: AlertsConfig,
}

/// Operator alerts (Telegram / JSON webhook) for CHAIN STALLED, fork rollbacks and audit drift. Off unless a target is set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AlertsConfig {
    pub enabled: bool,
    /// Bot token from @BotFather; the bot must be a member of the chat.
    pub telegram_bot_token: String,
    /// Chat / channel id (e.g. "-1001234567890"); get it from @userinfobot or the getUpdates API.
    pub telegram_chat_id: String,
    /// Any HTTPS endpoint receiving `POST {kind, text, at}` JSON (Slack/Discord bridges, n8n, own bot).
    pub webhook_url: String,
    /// Minimum seconds between two alerts of the same kind (recovery messages ignore it).
    pub cooldown_secs: u64,
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self { enabled: true, telegram_bot_token: String::new(), telegram_chat_id: String::new(), webhook_url: String::new(), cooldown_secs: 600 }
    }
}

impl AlertsConfig {
    pub fn active(&self) -> bool {
        self.enabled && ((!self.telegram_bot_token.is_empty() && !self.telegram_chat_id.is_empty()) || !self.webhook_url.is_empty())
    }
}

/// `secrets: "one passphrase"` or `secrets: [..]` — operators copy the legacy delegates.json shape in both forms.
fn string_or_seq<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        One(String),
        Many(Vec<String>),
    }
    Ok(match V::deserialize(d)? {
        V::One(s) => vec![s],
        V::Many(v) => v,
    })
}

/// Forging module (off unless secrets are configured). Mirrors the legacy `delegates.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DelegateConfig {
    pub enabled: bool,
    /// Delegate passphrases (same meaning as `secrets` in the legacy delegates.json). A single string is accepted too,
    /// and an entry ending in `.json` is read as a legacy delegates file (same as `secrets_file`).
    #[serde(deserialize_with = "string_or_seq")]
    pub secrets: Vec<String>,
    /// Optional path to a legacy `delegates.json`; its `secrets` are merged with the list above.
    pub secrets_file: String,
    /// Quantum Shield stage C: passphrases of the delegates' ML-DSA-44 keys (the ones registered with `sth-cli pq-register`);
    /// matched to a delegate by its on-chain `pq_key`. `STH_DELEGATE_PQ_PASSPHRASE` is appended. Needed once `pq.blocks` is on.
    pub pq_secrets: Vec<String>,
    /// Legacy peers that receive every forged block via `postBlock`.
    pub broadcast_fanout: usize,
    /// Share (0.0–1.0) of responding peers (legacy + iroh) that must report our exact tip before forging.
    /// `0.0` = private / single-node network (`init newnet`): the node forges even when no peer answers.
    pub quorum_share: f64,
    /// Never forge with fewer than this many peers agreeing on our tip (default 3) — an isolated node
    /// must skip its slot instead of building a private fork.
    pub min_quorum_peers: usize,
    /// Announce over iroh gossip (signed by the delegate key) that these delegates forge on a Rust node,
    /// so every metrics page can show "N of 21 delegates on Rust". Public keys are public anyway.
    pub announce: bool,
    /// Double-forge guard: a delegate key loaded on two nodes at once is detected and (mode yield) its slots are skipped
    /// here so the network is not split. See `delegate::guard`.
    pub guard: DoubleForgeGuardConfig,
}

impl Default for DelegateConfig {
    fn default() -> Self {
        Self { enabled: false, secrets: Vec::new(), secrets_file: String::new(), pq_secrets: Vec::new(), broadcast_fanout: 6, quorum_share: 0.5, min_quorum_peers: 3, announce: true, guard: DoubleForgeGuardConfig::default() }
    }
}

/// What the double-forge guard does when a block signed by one of OUR keys arrives from the network.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GuardMode {
    /// Log + alert only; the forger keeps its slots (operator must act).
    Alert,
    /// Log + alert and skip this key's slots while foreign blocks keep coming (default).
    Yield,
}

/// `delegate.guard` — optional module, on by default (never changes consensus, only whether THIS node forges).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DoubleForgeGuardConfig {
    pub enabled: bool,
    pub mode: GuardMode,
    /// Yield window: a foreign block by our key within the last N rounds keeps the key's slots skipped (default 2).
    pub rounds: u64,
}

impl Default for DoubleForgeGuardConfig {
    fn default() -> Self {
        Self { enabled: true, mode: GuardMode::Yield, rounds: 2 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ApiConfig {
    pub enabled: bool,
    /// Keep 127.0.0.1 — the API is a local bridge for netfory-provider, never public.
    pub host: String,
    pub port: u16,
    /// Operator metrics page (HTML): at `http://host:port/` or on `metrics_listen` when set.
    pub page_metrics: bool,
    /// Separate bind address for the metrics page, e.g. "0.0.0.0:4888" (serves `/` + `/api/ntfry/*` only). Empty = main API.
    pub metrics_listen: String,
    /// Browser origins allowed by CORS (both API and metrics ports). Empty = CORS off; `["*"]` = any origin.
    pub cors_origins: Vec<String>,
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
    /// Use relay servers for NAT traversal (turn off for LAN-only setups). Relays used = NETFORY n1
    /// (built in, `N1_RELAYS`) + public n0 (`relay_n0`) + `relays`.
    pub relay: bool,
    /// Also use the public n0 relay servers (use1/usw1/euc1/aps1.relay.n0.iroh.link). Off by default since 0.25.19:
    /// they are rate-limited ("not for production" per n0) and win the latency race against n1, then drop wallet handshakes.
    pub relay_n0: bool,
    /// Extra relay URLs (`https://relay.example.org`), e.g. your own iroh-relay.
    pub relays: Vec<String>,
    /// Relay guard module: removes a non-protected relay from the map after repeated handshake timeouts through it.
    pub relay_guard: RelayGuardConfig,
    /// sthpq address book: n0-free peer discovery (signed dial records over gossip / RPC / legacy port 4001).
    pub discovery: DiscoveryConfig,
    /// SHIP-44: REST-over-iroh (`netfory/api/1`) and push subscriptions (`sth/events/1`) for wallets.
    pub wallet_api: WalletApiConfig,
    /// SHIP-44 browser bridge: WebSocket ⇄ iroh for web wallets (any node may run it; Service Nodes typically do).
    pub ws_bridge: WsBridgeConfig,
}

/// Mainnet PQ nodes every new node dials first (`sth-core init` writes them into node.yaml; edit for other networks).
pub const MAINNET_IROH_BOOTSTRAP: [&str; 3] = [
    "747a93f8d56d361ba01a56265b1f10e746d7ab1962ee01189c43fae15481d4a0",
    "d8c32e15974a4197f16d61b7cc03e4be4338cde2deca6310589584adbc465bf6",
    "9906e9f29cf841b8699b65ed2c27c27db90feb2ffdfa223f25925901ef79ae3f",
];

impl Default for IrohConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            secret_key_file: "./iroh.key".into(),
            bootstrap: MAINNET_IROH_BOOTSTRAP.iter().map(|s| s.to_string()).collect(),
            serve_blocks: true,
            relay: true,
            relay_n0: false,
            relays: Vec::new(),
            relay_guard: RelayGuardConfig::default(),
            discovery: DiscoveryConfig::default(),
            wallet_api: WalletApiConfig::default(),
            ws_bridge: WsBridgeConfig::default(),
        }
    }
}

/// `p2p.iroh.discovery` — sthpq address book module (see `p2p_iroh::addrbook`). `bootstrap` entries may carry a
/// direct address as `<EndpointId>@<ip>:<udp port>` so the first contact needs neither n0 DNS nor a relay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DiscoveryConfig {
    pub enabled: bool,
    /// Fixed UDP port for the iroh endpoint (0 = random, changes on every start). Gateways should pin it (e.g. 4004).
    pub listen_port: u16,
    /// Public `ip:port` of this node announced in its record (empty = only the addresses iroh discovers itself).
    pub external_addr: String,
    /// How often the own record (+ fresh peers) is gossiped.
    pub announce_secs: u64,
    /// A peer silent for this many days is forgotten (Bitcoin addrman style).
    pub expire_days: u64,
    pub max_entries: usize,
    /// Keep the n0 DNS service (`dns.iroh.link`) as publish/resolve fallback. `false` = preset `Minimal`: the node
    /// never talks to n0 (address book + Mainline DHT only).
    pub n0_dns: bool,
    /// Publish/resolve pkarr records in the BitTorrent Mainline DHT (BEP-44, same scheme as NETFORY clients).
    pub mainline: bool,
    /// Extra DHT bootstrap nodes (`host:port`) added to the public routers, e.g. NETFORY seeders.
    pub mainline_bootstrap: Vec<String>,
    /// UDP port of the DHT socket (0 = random).
    pub mainline_port: u16,
    /// Run the DHT socket in server mode (answers other nodes' queries) — turns a gateway into a DHT bootstrap node for
    /// the network (`mainline_bootstrap: ["<ip>:<mainline_port>"]` on the others). Needs `mainline_port` open (UDP).
    pub mainline_server: bool,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self { enabled: true, listen_port: 0, external_addr: String::new(), announce_secs: 1800, expire_days: 30, max_entries: 2000, n0_dns: true, mainline: true, mainline_bootstrap: Vec::new(), mainline_port: 0, mainline_server: false }
    }
}

/// `p2p.iroh.relay_guard` — optional module (see `p2p_iroh::relay_guard`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RelayGuardConfig {
    pub enabled: bool,
    /// Handshake timeouts through one relay within `window_secs` that get it removed from the relay map.
    pub max_failures: usize,
    pub window_secs: u64,
}

impl Default for RelayGuardConfig {
    fn default() -> Self {
        Self { enabled: true, max_failures: 5, window_secs: 600 }
    }
}

/// `ws://<listen>/ws/events[/<EndpointId>]` and `/ws/api[/<EndpointId>]` — one WebSocket message = one protocol frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WsBridgeConfig {
    pub enabled: bool,
    /// Bind address; put nginx/TLS in front for `wss://` (see docs/nginx).
    pub listen: String,
    /// EndpointId used when the URL names no node; empty = this node.
    pub default_node: String,
}

impl Default for WsBridgeConfig {
    fn default() -> Self {
        Self { enabled: false, listen: "0.0.0.0:4005".into(), default_node: String::new() }
    }
}

/// SHIP-44 wallet protocols. Defaults are sized for a 1–2 GB RAM node: an idle QUIC connection costs ~60–100 KB,
/// so 2000 connections stay under ~200 MB next to the sled cache. 4 GB → 5000, 8 GB → 10000.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WalletApiConfig {
    pub enabled: bool,
    /// Simultaneous wallet connections (requests + event sessions); 0 = unlimited.
    pub max_connections: usize,
    /// Addresses one event session may subscribe to.
    pub max_addresses: usize,
    /// API requests per second per EndpointId (burst 2×); 0 = unlimited.
    pub rate_limit_rps: u32,
    /// An event session without any command (`ping`) for this long is closed.
    pub idle_timeout_secs: u64,
    /// Print the node's `api://<EndpointId>/sth-core` URL as a QR code in the startup banner (mobile wallets pair by scanning).
    pub startup_qr: bool,
}

impl Default for WalletApiConfig {
    fn default() -> Self {
        Self { enabled: true, max_connections: 2000, max_addresses: 64, rate_limit_rps: 20, idle_timeout_secs: 300, startup_qr: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RewardsConfig {
    /// Reserved for future relay / gateway rewards (block distribution, snapshots) — NOT used yet and NOT the
    /// delegate's reward: block rewards always go to the forging delegate's own wallet (`delegate.secrets`).
    pub reward_address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MempoolConfig {
    pub max_size: usize,
    /// Byte budget of the pool (wire bytes of pending transactions). Keeps 2.5 KB Quantum Shield (v3) transactions from
    /// crowding out ordinary 160-byte ones when the count limit alone would still allow it.
    pub max_bytes: usize,
    /// Fee policy of THIS node's pool — a per-node setting, not a consensus rule (blocks are never rejected for it).
    pub dynamic_fees: DynamicFeesConfig,
    /// Legacy `transactionPool.maxTransactionAge`: a pending transaction the network has not forged within this many
    /// blocks is dropped (2700 ≈ 6 h). 0 = keep forever (not recommended: stuck nonces block the sender).
    pub max_age_blocks: u64,
    /// Re-relay pending transactions to legacy peers / iroh gossip every this many blocks (0 = only once, on arrival).
    pub rebroadcast_blocks: u64,
    /// Do the legacy (Core 3.8.2) peers run `dynamicFees`? `false` = their pools take exactly the static fee, so a cheaper
    /// transaction is forged only by PQ delegates — the fee advisor (`Mempool::fee_hint`) warns wallets about it.
    pub legacy_dynamic_fees: bool,
}

/// Legacy `transactionPool.dynamicFees`: minimum fee = `(addon_bytes[type] + wire bytes) × satoshi_per_byte`.
/// Off = a core transaction must carry exactly the static fee of its type (legacy behaviour with dynamic fees disabled).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DynamicFeesConfig {
    pub enabled: bool,
    /// Smartoshi per byte a transaction must pay to enter the pool.
    pub min_fee_pool: u64,
    /// Smartoshi per byte a transaction must pay to be relayed (accepted but kept local below it).
    pub min_fee_broadcast: u64,
    /// Virtual extra bytes per core type name (`transfer`, `vote`, `delegateRegistration`, ...).
    pub addon_bytes: std::collections::BTreeMap<String, u64>,
}

impl DynamicFeesConfig {
    /// Minimum fee for a transaction of `type_name` weighing `bytes` at `satoshi_per_byte`.
    pub fn min_fee(&self, type_name: &str, bytes: usize, satoshi_per_byte: u64) -> u64 {
        // type 5 is the Netfory pointer; `ipfs` is the legacy JSON key wallets know, `ntfry` is accepted in node.yaml
        let addon = self.addon_bytes.get(type_name).or_else(|| (type_name == "ipfs").then(|| self.addon_bytes.get("ntfry")).flatten()).copied().unwrap_or(0);
        addon.saturating_add(bytes as u64).saturating_mul(satoshi_per_byte.max(1))
    }
}

impl Default for DynamicFeesConfig {
    fn default() -> Self {
        let addon_bytes = [
            ("transfer", 100), ("secondSignature", 250), ("delegateRegistration", 400_000), ("vote", 100), ("multiSignature", 500),
            ("ipfs", 250), ("multiPayment", 500), ("delegateResignation", 100), ("htlcLock", 100), ("htlcClaim", 0), ("htlcRefund", 0),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        Self { enabled: true, min_fee_pool: 3_000, min_fee_broadcast: 3_000, addon_bytes }
    }
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            network: "mainnet".into(),
            network_dir: String::new(),
            db_path: "./data".into(),
            db_cache_mb: 64,
            startup_scan: true,
            api: ApiConfig::default(),
            sync: SyncSection::default(),
            p2p: P2pConfig::default(),
            rewards: RewardsConfig::default(),
            mempool: MempoolConfig::default(),
            delegate: DelegateConfig::default(),
            alerts: AlertsConfig::default(),
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self { enabled: true, host: "127.0.0.1".into(), port: 4003, page_metrics: false, metrics_listen: String::new(), cors_origins: Vec::new() }
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
            relay_fanout: 20,
            refresh_secs: 60,
            iroh: IrohConfig::default(),
        }
    }
}

impl Default for RewardsConfig {
    fn default() -> Self {
        Self { reward_address: String::new() }
    }
}

impl Default for MempoolConfig {
    fn default() -> Self {
        Self { max_size: 5_000, max_bytes: 8 * 1024 * 1024, dynamic_fees: DynamicFeesConfig::default(), max_age_blocks: 2700, rebroadcast_blocks: 30, legacy_dynamic_fees: false }
    }
}

impl NodeConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        let mut cfg = Self::parse(&raw).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        // network files live next to node.yaml (init newnet writes `network_dir: .`) — works from any cwd
        let dir = cfg.network_dir.trim();
        if !dir.is_empty() && Path::new(dir).is_relative() {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                cfg.network_dir = parent.join(dir).display().to_string();
            }
        }
        Ok(cfg)
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
        if self.network != "mainnet" && self.network_dir.trim().is_empty() {
            return Err(Error::Config(format!("network '{}' needs network_dir with its files (see `sth-core init newnet`)", self.network)));
        }
        if !(0.0..=1.0).contains(&self.delegate.quorum_share) {
            return Err(Error::Config("delegate.quorum_share must be between 0.0 and 1.0".into()));
        }
        if self.delegate.min_quorum_peers == 0 {
            return Err(Error::Config("delegate.min_quorum_peers must be >= 1".into()));
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
             # db_cache_mb: sled page cache (MiB). 64 is fine for 2 GB+ RAM; use 16–32 on a 1 GB VPS (process RSS ≈ 2–3× this).\n\
             # startup_scan: count the wallets for the startup banner (full key scan, ~1–3 s on mainnet). Informational only —\n\
             #               the state is kept in sled incrementally; integrity is checked by `sth-core audit-all`, not at start.\n\
             # api:     local REST bridge for wallets / explorers / netfory-provider (keep host 127.0.0.1).\n\
             #          page_metrics: true serves the operator dashboard at http://host:port/ — or on its own address\n\
             #          with metrics_listen: \"0.0.0.0:4888\" (page + /api/ntfry/* only, safe to expose).\n\
             #          cors_origins: [] = CORS off (default); [\"http://localhost:3000\"] allows a browser app, [\"*\"] = any origin.\n\
             # sync:    legacy REST bootstrap nodes; bootstrap_snapshot is used only while the database is empty\n\
             #          (`latest` downloads from https://snapshots.smartholdem.io/, a path imports a local dump, empty skips).\n\
             # p2p:     legacy_enabled follows the chain through port 4001 (IP peers) with parallel_peers ranges in flight;\n\
             #          relay_fanout peers receive every accepted transaction.\n\
             #          GATEWAY node = legacy_listen: \"0.0.0.0:4001\" (inbound legacy server: old nodes and netfory-provider\n\
             #          ws:// clients pull blocks from us) + legacy_public_addr: \"<public IP of THIS server>:4001\" (announced to\n\
             #          Rust peers over iroh; they show us with gateway: true in /api/ntfry/peers). Both must be set.\n\
             #          p2p.iroh enables the Web 4.0 layer\n\
             #          (iroh endpoint + gossip topics for blocks / transactions, GetBlocks RPC); bootstrap = EndpointIds of peers.\n\
             #          Enabled by default with the mainnet PQ bootstrap nodes; iroh.key is created on first start (keep it — it is\n\
             #          the node's EndpointId / api:// address). For another network replace bootstrap or set enabled: false.\n\
             #          Relays (NAT traversal): relay: true uses the NETFORY n1 relays (relay-fsn7.sth.cx, relay-ru1.sth.cx) built in,\n\
             #          relay_n0: false (default) keeps the rate-limited public n0 relays out — they win the latency race and then drop\n\
             #          wallet handshakes; relays: [\"https://…\"] adds your own iroh-relay servers. relay_guard (enabled: true) removes a\n\
             #          non-n1 relay from the map after max_failures (5) handshake timeouts within window_secs (600).\n\
             #          discovery (sthpq address book, enabled: true): nodes gossip signed dial records (EndpointId + public ip:port +\n\
             #          relay) and pull them over RPC / legacy port 4001, so peers are found without n0 DNS. Dead peers are forgotten\n\
             #          after expire_days (30). Gateways: listen_port: 4004 (fixed UDP, open it) + external_addr: \"<public ip>:4004\";\n\
             #          bootstrap entries may pin a direct address: \"<EndpointId>@<ip>:4004\". mainline: true publishes/resolves the\n\
             #          record in the BitTorrent Mainline DHT (BEP-44, as NETFORY clients do; mainline_bootstrap adds your seeders);\n\
             #          n0_dns: true keeps dns.iroh.link as a fallback — set false once the DHT is confirmed to drop n0 entirely.\n\
             # rewards: RESERVED, not used yet — future relay/gateway rewards (block & snapshot distribution).\n\
             #          Not the delegate's reward: block rewards always go to the forging delegate's wallet.\n\
             # delegate: forging module. enabled: true + secrets: [passphrase] (or secrets_file: ./delegates.json, legacy format).\n\
             #          min_quorum_peers (3): the slot is skipped unless that many peers confirm our tip — never forge while isolated.\n\
             #          announce (true): tell the network (signed) that these delegates run on Rust — counted on the metrics page.\n\
             #          Forging happens only when this delegate is in the active top-21 of the round; fees go to its wallet.\n\
             #          pq_secrets: [passphrase] = ML-DSA-44 key(s) registered on chain with `sth-cli pq-register` (Quantum Shield\n\
             #          stage C): from milestone pq.blocks every block is version 1 with a hybrid secp256k1 + ML-DSA signature;\n\
             #          a delegate without its PQ key skips the slot once the grace window (pq.blocksGrace) has closed.\n\
             #          Finality (SHIP-35): the delegate keys also sign finality votes over iroh gossip — nothing to configure.\n\
             #          guard: double-forge guard (enabled: true). A block signed by one of OUR keys that arrives from the network\n\
             #          means the passphrase is loaded on another node (legacy + PQ during the transition): ERROR + alert, and with\n\
             #          mode: yield (default) this node skips that key's slots for `rounds` (2) rounds instead of splitting the\n\
             #          network; mode: alert only reports. Two different blocks at one height by one generator are reported too.\n\
             # mempool: max_size / max_bytes = pool limits. dynamic_fees = fee policy of THIS node only (not consensus, same as the\n\
             #          legacy transactionPool.dynamicFees): enabled: true accepts a core transaction when\n\
             #          fee >= (addon_bytes[type] + wire bytes) × min_fee_pool and relays it from min_fee_broadcast (a transfer\n\
             #          costs ≈ 0.007 STH); enabled: false demands exactly the static fee of the milestone (1 STH per transfer).\n\
             #          legacy_dynamic_fees: false = the legacy Core 3.8.2 peers take only the static fee → the fee advisor marks\n\
             #          cheaper transactions (feeHint in POST /api/transactions, wallet stream mempoolTx, metrics) as PQ-only.\n\
             # alerts:  operator notifications (CHAIN STALLED / recovered, fork rollbacks, audit-all drift): telegram_bot_token +\n\
             #          telegram_chat_id and/or webhook_url (POST JSON {{kind,text,at}}); cooldown_secs between repeats of one kind.\n\
             {body}"
        )
    }

    /// Reserved reward address (relay / gateway rewards, not implemented yet).
    pub fn reward_address(&self) -> Result<Option<String>> {
        Ok((!self.rewards.reward_address.is_empty()).then(|| self.rewards.reward_address.clone()))
    }
}
