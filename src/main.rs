//! Author: TechnoL0g
//!
//! `sth-core` CLI: `init` (node.yaml), `run` (relay node), snapshot tools, legacy sync and
//! peer diagnostics. Node assembly lives in `sth_core::node`.

use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use sth_core::config::Network;
use sth_core::crypto::{verify_block, ChainObject};
use sth_core::models::Block;
use sth_core::node::{download_bar, resolve_snapshot, run_import, NodeContext, RunFlags};
use sth_core::node_config::{NodeConfig, DEFAULT_CONFIG_FILE};
use sth_core::node_pool::RateLimitConfig;
use sth_core::p2p_legacy::PeerTable;
use sth_core::snapshot::{self, SnapshotMeta, DEFAULT_SNAPSHOT_BASE_URL};
use sth_core::storage::Storage;
use sth_core::sync::{SyncConfig, Syncer};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "sth-core", version, about = "SmartHoldem relay node (Rust)")]
struct Cli {
    /// Sled database directory (default: node.yaml `db_path`, else ./data).
    #[arg(long, global = true)]
    db_path: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show local chain height and last block id.
    Info,
    /// Verify a block JSON file (core `IBlockData` format, with transactions).
    VerifyBlock { file: PathBuf },
    /// Verify and apply a block JSON file to the local database.
    ImportBlock { file: PathBuf },
    /// Show a stored wallet.
    Wallet { address: String },
    /// Catch up with the network over the legacy REST API (temporary bootstrap; resumable).
    Sync {
        /// Comma-separated node URLs (default: node0..node5.smartholdem.io).
        #[arg(long, value_delimiter = ',')]
        nodes: Option<Vec<String>>,
        /// Blocks per request (max 100).
        #[arg(long, default_value_t = 100)]
        batch: u64,
        /// Concurrent range requests (spread over the node pool).
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        /// Max requests per second per node (legacy API limit is 5/s).
        #[arg(long, default_value_t = 4)]
        rps: u32,
        /// Max requests per node per 60 s window (legacy API limit is 300).
        #[arg(long, default_value_t = 250)]
        window_limit: usize,
        /// HTTP timeout per request, seconds.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        /// Only check chain linkage and block ids (skip signature / payload verification).
        #[arg(long)]
        skip_verify: bool,
        /// Keep following the chain after catching up.
        #[arg(long)]
        follow: bool,
        /// Hide the progress bar.
        #[arg(long)]
        quiet: bool,
        /// Bootstrap from a snapshot first: a dump folder, a .tgz, or `latest` (download from
        /// https://snapshots.smartholdem.io/). HTTP sync continues from the snapshot height.
        #[arg(long, value_name = "PATH|latest")]
        from_dump: Option<String>,
        /// Skip secp256k1 signature checks while importing a trusted dump (ids, linkage and payload hashes are still verified).
        #[arg(long)]
        fast_import: bool,
        /// Where downloaded snapshots are stored / extracted.
        #[arg(long, default_value = "./snapshots")]
        snapshot_dir: PathBuf,
    },
    /// Snapshot tools (dumps produced by `yarn sth snapshot:dump`).
    Snapshot {
        #[command(subcommand)]
        cmd: SnapshotCmd,
    },
    /// Initialise a commented `node.yaml` configuration file.
    Init {
        /// Where to write the configuration.
        #[arg(long, default_value = DEFAULT_CONFIG_FILE)]
        config: PathBuf,
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
        /// Also export the embedded network files (crypto-networks layout) into ./network.
        #[arg(long)]
        network_files: bool,
    },
    /// Run the relay node from `node.yaml` (CLI flags override the file): optional snapshot bootstrap →
    /// catch up → follow the chain + local REST API.
    Run {
        /// Configuration file (default: ./node.yaml when present, else built-in defaults).
        #[arg(long, value_name = "node.yaml")]
        config: Option<PathBuf>,
        /// Bind address of the local API (env CORE_API_HOST — keep 127.0.0.1, local bridge only).
        #[arg(long, env = "CORE_API_HOST")]
        api_host: Option<String>,
        #[arg(long, env = "CORE_API_PORT")]
        api_port: Option<u16>,
        /// Disable the REST API.
        #[arg(long)]
        no_api: bool,
        /// Legacy REST nodes (bootstrap / relay fallback).
        #[arg(long, value_delimiter = ',')]
        nodes: Option<Vec<String>>,
        #[arg(long)]
        concurrency: Option<usize>,
        #[arg(long)]
        skip_verify: bool,
        #[arg(long)]
        quiet: bool,
        /// Import a snapshot before syncing (a dump folder / .tgz / `latest`), even if the DB is not empty.
        #[arg(long, value_name = "PATH|latest")]
        from_dump: Option<String>,
        #[arg(long)]
        fast_import: bool,
        #[arg(long)]
        snapshot_dir: Option<PathBuf>,
        /// Max transactions kept in the mempool.
        #[arg(long)]
        mempool_size: Option<usize>,
        /// Follow the chain through the legacy P2P port (default from config; `--no-p2p` for REST polling).
        #[arg(long, conflicts_with = "no_p2p")]
        p2p: bool,
        #[arg(long)]
        no_p2p: bool,
        #[arg(long, env = "CORE_P2P_PORT")]
        p2p_port: Option<u16>,
        /// Legacy P2P peers as IPs; replaces peers.json + built-in seeds.
        #[arg(long, value_delimiter = ',')]
        peers: Option<Vec<String>>,
        /// Peers pulled from concurrently during catch-up.
        #[arg(long)]
        parallel_peers: Option<usize>,
        /// Reward address for future seeding / delegate rewards.
        #[arg(long)]
        reward_address: Option<String>,
    },
    /// Print (creating if needed) the iroh EndpointId of this node — share it as `p2p.iroh.bootstrap` on other nodes.
    IrohId {
        #[arg(long, default_value = "./iroh.key")]
        key_file: PathBuf,
    },
    /// Probe the legacy peer list and print the health table (latency, height, version).
    Peers {
        #[arg(long, env = "CORE_P2P_PORT", default_value_t = 4001)]
        port: u16,
        #[arg(long, value_delimiter = ',')]
        peers: Option<Vec<String>>,
    },
    /// Query a legacy node over its P2P port (getStatus + getPeers) — connectivity check.
    PeerStatus {
        host: String,
        #[arg(long, env = "CORE_P2P_PORT", default_value_t = 4001)]
        port: u16,
        /// Also fetch `--blocks N` blocks after height `--from` and verify them.
        #[arg(long, default_value_t = 0)]
        blocks: u32,
        #[arg(long, default_value_t = 0)]
        from: u64,
    },
}

#[derive(Subcommand)]
enum SnapshotCmd {
    /// Download the newest `<start>-<end>.tgz` and extract it.
    Download {
        #[arg(long, default_value = DEFAULT_SNAPSHOT_BASE_URL)]
        base_url: String,
        #[arg(long, default_value = "./snapshots")]
        out: PathBuf,
    },
    /// Import a dump (folder or .tgz) into the local database (resumable).
    Import {
        path: PathBuf,
        /// Skip secp256k1 signature checks (trusted dump).
        #[arg(long)]
        fast_import: bool,
        #[arg(long)]
        quiet: bool,
    },
    /// Show meta.json of a dump.
    Info { path: PathBuf },
}

fn read_block(file: &PathBuf) -> anyhow::Result<Block> {
    let raw = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&raw)?;
    // Accept both a bare block and an API envelope `{ "data": {...} }`.
    let inner = value.get("data").cloned().unwrap_or(value);
    Ok(serde_json::from_value(inner)?)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = Cli::parse();
    let network = Network::mainnet();
    let db_path: PathBuf = cli.db_path.clone().unwrap_or_else(|| PathBuf::from("./data"));

    match cli.command {
        Command::Info => {
            let storage = Storage::open(&db_path, network.clone())?;
            let last = storage.get_last_block()?;
            println!("network:      {} (pubKeyHash {})", network.name, network.pubkey_hash);
            println!("db path:      {}", db_path.display());
            match last {
                Some(b) => {
                    println!("height:       {}", b.height);
                    println!("last block:   {}", b.id.unwrap_or_default());
                    println!("timestamp:    {} (unix {})", b.timestamp, network.epoch_to_unix(b.timestamp));
                }
                None => println!("height:       0 (empty)"),
            }
        }
        Command::VerifyBlock { file } => {
            let block = read_block(&file)?;
            let computed = block.get_id()?;
            let v = verify_block(&block, &network);
            println!("height:       {}", block.height);
            println!("id:           {}", block.id.clone().unwrap_or_default());
            println!("computed id:  {computed}");
            println!("signature:    {}", if block.verify_signature()? { "valid" } else { "INVALID" });
            println!("verified:     {}", v.verified);
            for e in &v.errors {
                println!("  - {e}");
            }
            if !v.verified {
                return Err(anyhow!("block verification failed"));
            }
        }
        Command::ImportBlock { file } => {
            let block = read_block(&file)?;
            let v = verify_block(&block, &network);
            if !v.verified {
                return Err(anyhow!("block verification failed: {:?}", v.errors));
            }
            let storage = Storage::open(&db_path, network)?;
            storage.apply_block(&block)?;
            storage.flush()?;
            tracing::info!(height = block.height, "block imported");
        }
        Command::Wallet { address } => {
            let storage = Storage::open(&db_path, network)?;
            match storage.get_wallet(&address)? {
                Some(w) => println!("{}", serde_json::to_string_pretty(&w)?),
                None => println!("wallet {address} not found"),
            }
        }
        Command::Init { config, force, network_files } => {
            if force && config.exists() {
                std::fs::remove_file(&config).with_context(|| format!("removing {}", config.display()))?;
            }
            NodeConfig::write_default(&config)?;
            println!("configuration written to {}", config.display());
            if network_files {
                let dir = PathBuf::from("network");
                NodeConfig::export_network_files(&dir)?;
                println!("network files written to {} (set network_dir: ./network in {} to use them)", dir.display(), config.display());
            }
            println!("edit rewards.reward_address / reward_passphrase, then start with: sth-core run --config {}", config.display());
        }
        Command::Run {
            config, api_host, api_port, no_api, nodes, concurrency, skip_verify, quiet, from_dump, fast_import, snapshot_dir,
            mempool_size, p2p, no_p2p, p2p_port, peers, parallel_peers, reward_address,
        } => {
            let mut cfg = NodeConfig::load_or_default(config.as_deref())?;
            if let Some(p) = &cli.db_path {
                cfg.db_path = p.display().to_string();
            }
            if let Some(h) = api_host {
                cfg.api.host = h;
            }
            if let Some(p) = api_port {
                cfg.api.port = p;
            }
            if no_api {
                cfg.api.enabled = false;
            }
            if let Some(n) = nodes {
                cfg.sync.rest_nodes = n.into_iter().map(|n| n.trim_end_matches('/').to_string()).collect();
            }
            if let Some(c) = concurrency {
                cfg.sync.concurrency = c;
            }
            if skip_verify {
                cfg.sync.verify_blocks = false;
            }
            if let Some(d) = &from_dump {
                cfg.sync.bootstrap_snapshot = d.clone();
            }
            if fast_import {
                cfg.sync.fast_import = true;
            }
            if let Some(d) = snapshot_dir {
                cfg.sync.snapshot_dir = d.display().to_string();
            }
            if let Some(m) = mempool_size {
                cfg.mempool.max_size = m;
            }
            if p2p {
                cfg.p2p.legacy_enabled = true;
            }
            if no_p2p {
                cfg.p2p.legacy_enabled = false;
            }
            if let Some(p) = p2p_port {
                cfg.p2p.legacy_port = p;
            }
            if let Some(list) = peers {
                cfg.p2p.legacy_peers = list;
                cfg.p2p.use_peer_list = false;
            }
            if let Some(n) = parallel_peers {
                cfg.p2p.parallel_peers = n;
            }
            if let Some(a) = reward_address {
                cfg.rewards.reward_address = a;
            }
            let node = NodeContext::open(cfg).await?;
            node.run(RunFlags { quiet, force_bootstrap: from_dump.is_some() }).await?;
        }
        Command::IrohId { key_file } => {
            let secret = sth_core::p2p_iroh::load_or_create_secret(&key_file)?;
            println!("{}", secret.public());
        }
        Command::Peers { port, peers } => {
            let seeds = match peers {
                Some(p) => p,
                None => sth_core::p2p_legacy::fetch_peer_list(port).await,
            };
            let table = PeerTable::new(port, seeds);
            let alive = table.refresh(16, Duration::from_secs(10)).await;
            println!("{:<18} {:>9} {:>8} {:<8} {}", "peer", "height", "latency", "version", "state");
            for p in table.snapshot() {
                let state = if p.successes == 0 { "unreachable" } else { "ok" };
                let latency = if p.successes == 0 { "-".to_string() } else { format!("{} ms", p.latency_ms) };
                println!("{:<18} {:>9} {:>8} {:<8} {}", p.ip, p.height, latency, p.version, state);
            }
            println!("alive: {alive} / {}  best height: {}", table.len(), table.best_height());
        }
        Command::PeerStatus { host, port, blocks, from } => {
            let mut peer = sth_core::p2p_legacy::LegacyPeer::connect(&host, port, Duration::from_secs(15)).await?;
            let status = peer.get_status().await?;
            let state = status.state.unwrap_or_default();
            let config = status.config.unwrap_or_default();
            println!("peer:         {host}:{port}");
            println!("core version: {}", config.version);
            println!("network:      {} nethash {}", config.network.as_ref().map(|n| n.name.as_str()).unwrap_or("?"), config.network.as_ref().map(|n| n.nethash.as_str()).unwrap_or("?"));
            println!("height:       {}", state.height);
            println!("last block:   {}", state.header.map(|h| h.id).unwrap_or_default());
            let peers = peer.get_peers().await?;
            println!("peers:        {}", peers.len());
            for p in peers.iter().take(10) {
                println!("  - {}:{}", p.ip, p.port);
            }
            if blocks > 0 {
                let got = peer.get_blocks(from, blocks).await?;
                println!("blocks:       {} received after height {from}", got.len());
                for b in &got {
                    let v = verify_block(b, &network);
                    println!("  - {} {} txs={} verified={}{}", b.height, b.id.clone().unwrap_or_default(), b.transactions.len(), v.verified, if v.errors.is_empty() { String::new() } else { format!(" {:?}", v.errors) });
                }
            }
        }
        Command::Snapshot { cmd } => match cmd {
            SnapshotCmd::Download { base_url, out } => {
                let pb = download_bar()?;
                let archive = snapshot::download_latest(&base_url, &out, &pb).await?;
                pb.finish_and_clear();
                let dir = tokio::task::spawn_blocking(move || snapshot::extract_tgz(&archive, &out)).await??;
                let meta = SnapshotMeta::read(&dir)?;
                tracing::info!(dir = %dir.display(), end_height = meta.blocks.end, "snapshot ready");
                println!("{}", dir.display());
            }
            SnapshotCmd::Import { path, fast_import, quiet } => {
                let dir = tokio::task::spawn_blocking(move || snapshot::locate_snapshot_dir(&path)).await??;
                let storage = Arc::new(Storage::open(&db_path, network)?);
                let report = run_import(storage, dir, fast_import, quiet).await?;
                if report.interrupted {
                    return Err(anyhow!("import interrupted"));
                }
            }
            SnapshotCmd::Info { path } => {
                let dir = snapshot::locate_snapshot_dir(&path)?;
                let meta = SnapshotMeta::read(&dir)?;
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                    "dir": dir.display().to_string(),
                    "network": meta.network,
                    "codec": meta.codec,
                    "compressed": !meta.skip_compression,
                    "blocks": { "count": meta.blocks.count, "start": meta.blocks.start, "end": meta.blocks.end },
                    "transactions": meta.transactions.count,
                    "rounds": meta.rounds.count,
                    "coreVersion": meta.package_version,
                }))?);
            }
        },
        Command::Sync {
            nodes,
            batch,
            concurrency,
            rps,
            window_limit,
            timeout,
            skip_verify,
            follow,
            quiet,
            from_dump,
            fast_import,
            snapshot_dir,
        } => {
            let mut cfg = SyncConfig {
                batch_size: batch,
                concurrency,
                request_timeout: Duration::from_secs(timeout),
                verify: !skip_verify,
                follow,
                quiet,
                rate_limit: RateLimitConfig { per_node_rps: rps, window_limit, ..RateLimitConfig::default() },
                ..SyncConfig::default()
            };
            if let Some(nodes) = nodes {
                cfg.nodes = nodes.into_iter().map(|n| n.trim_end_matches('/').to_string()).collect();
            }
            let storage = Arc::new(Storage::open(&db_path, network)?);
            if let Some(source) = from_dump {
                let dir = resolve_snapshot(&source, &snapshot_dir).await?;
                let report = run_import(storage.clone(), dir, fast_import, quiet).await?;
                if report.interrupted {
                    return Err(anyhow!("import interrupted — rerun `sth-core sync --from-dump ...` to resume"));
                }
            }
            let syncer = Arc::new(Syncer::new(storage, cfg)?);
            let report = syncer.run().await?;
            let secs = report.elapsed.as_secs_f64().max(0.001);
            tracing::info!(
                start = report.start_height,
                end = report.end_height,
                network = report.network_height,
                applied = report.blocks_applied,
                elapsed = format!("{:.1}s", secs),
                rate = format!("{:.1} blocks/sec", report.blocks_applied as f64 / secs),
                interrupted = report.interrupted,
                "sync {}",
                if report.interrupted { "interrupted — run `sth-core sync` again to resume" } else { "complete" }
            );
        }
    }
    Ok(())
}
