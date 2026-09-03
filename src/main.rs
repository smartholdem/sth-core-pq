//! Author: TechnoL0g
//!
//! `sth-core` CLI: local state inspection, block verify / import and legacy HTTP sync.
//! `run` (API + P2P) arrives in later phases.

use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use sth_core::api::{self, AppState};
use sth_core::config::Network;
use sth_core::mempool::Mempool;
use sth_core::crypto::{verify_block, ChainObject};
use sth_core::models::Block;
use indicatif::{ProgressBar, ProgressStyle};
use sth_core::node_pool::RateLimitConfig;
use sth_core::snapshot::{self, ImportOptions, ImportReport, SnapshotMeta, DEFAULT_SNAPSHOT_BASE_URL};
use sth_core::storage::Storage;
use sth_core::sync::{progress_style, SyncConfig, Syncer};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "sth-core", version, about = "SmartHoldem relay node (Rust)")]
struct Cli {
    /// Sled database directory.
    #[arg(long, global = true, default_value = "./data")]
    db_path: PathBuf,
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
    /// Run the relay node: optional snapshot bootstrap → catch up → follow the chain + local REST API.
    Run {
        #[arg(long, default_value = "mainnet")]
        network: String,
        /// Bind address of the local API (env CORE_API_HOST, default 127.0.0.1 — local bridge only).
        #[arg(long, env = "CORE_API_HOST", default_value = "127.0.0.1")]
        api_host: String,
        #[arg(long, env = "CORE_API_PORT", default_value_t = 4003)]
        api_port: u16,
        /// Disable the REST API.
        #[arg(long)]
        no_api: bool,
        #[arg(long, value_delimiter = ',')]
        nodes: Option<Vec<String>>,
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        #[arg(long)]
        skip_verify: bool,
        #[arg(long)]
        quiet: bool,
        #[arg(long, value_name = "PATH|latest")]
        from_dump: Option<String>,
        #[arg(long)]
        fast_import: bool,
        #[arg(long, default_value = "./snapshots")]
        snapshot_dir: PathBuf,
        /// Max transactions kept in the mempool.
        #[arg(long, default_value_t = 5_000)]
        mempool_size: usize,
        /// After catching up, follow the chain through the legacy P2P port instead of polling the REST API.
        #[arg(long)]
        p2p: bool,
        #[arg(long, env = "CORE_P2P_PORT", default_value_t = 4001)]
        p2p_port: u16,
        /// Legacy P2P seed peers as IPs (default: built-in seed list; hostnames answer 403 on 4001).
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

fn import_bar(quiet: bool) -> anyhow::Result<ProgressBar> {
    let pb = if quiet { ProgressBar::hidden() } else { ProgressBar::new(0) };
    pb.set_style(progress_style("Importing")?);
    Ok(pb)
}

fn download_bar() -> anyhow::Result<ProgressBar> {
    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::with_template("Downloading: [{bar:40}] {bytes} / {total_bytes} ({bytes_per_sec}, ETA: {eta})")?
            .progress_chars("=> "),
    );
    Ok(pb)
}

/// Resolve `latest` / .tgz / folder into an extracted snapshot directory.
async fn resolve_snapshot(source: &str, snapshot_dir: &Path) -> anyhow::Result<PathBuf> {
    let archive_or_dir = if source == "latest" {
        let pb = download_bar()?;
        let archive = snapshot::download_latest(DEFAULT_SNAPSHOT_BASE_URL, snapshot_dir, &pb).await?;
        pb.finish_and_clear();
        archive
    } else {
        PathBuf::from(source)
    };
    let dir = tokio::task::spawn_blocking(move || snapshot::locate_snapshot_dir(&archive_or_dir)).await??;
    Ok(dir)
}

/// Run the blocking import on the blocking pool with Ctrl+C support.
async fn run_import(storage: Arc<Storage>, dir: PathBuf, fast: bool, quiet: bool) -> anyhow::Result<ImportReport> {
    let meta = SnapshotMeta::read(&dir)?;
    tracing::info!(
        folder = meta.folder,
        blocks = meta.blocks.count,
        transactions = meta.transactions.count,
        end_height = meta.blocks.end,
        compressed = !meta.skip_compression,
        fast,
        "importing snapshot"
    );
    let pb = import_bar(quiet)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_flag = cancel.clone();
    let ctrl_c = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_flag.store(true, Ordering::Relaxed);
        }
    });
    let pb_worker = pb.clone();
    let report = tokio::task::spawn_blocking(move || {
        snapshot::import_snapshot(&storage, &dir, ImportOptions { fast }, &pb_worker, &cancel)
    })
    .await??;
    ctrl_c.abort();
    pb.finish_and_clear();
    let secs = report.elapsed.as_secs_f64().max(0.001);
    tracing::info!(
        from = report.start_height,
        to = report.end_height,
        imported = report.imported,
        skipped = report.skipped,
        snapshot_end = report.snapshot_end,
        elapsed = format!("{:.1}s", secs),
        rate = format!("{:.0} blocks/sec", report.imported as f64 / secs),
        "snapshot import {}",
        if report.interrupted { "interrupted — rerun to resume" } else { "complete" }
    );
    Ok(report)
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

    match cli.command {
        Command::Info => {
            let storage = Storage::open(&cli.db_path, network.clone())?;
            let last = storage.get_last_block()?;
            println!("network:      {} (pubKeyHash {})", network.name, network.pubkey_hash);
            println!("db path:      {}", cli.db_path.display());
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
            let storage = Storage::open(&cli.db_path, network)?;
            storage.apply_block(&block)?;
            storage.flush()?;
            tracing::info!(height = block.height, "block imported");
        }
        Command::Wallet { address } => {
            let storage = Storage::open(&cli.db_path, network)?;
            match storage.get_wallet(&address)? {
                Some(w) => println!("{}", serde_json::to_string_pretty(&w)?),
                None => println!("wallet {address} not found"),
            }
        }
        Command::Run { network: net_name, api_host, api_port, no_api, nodes, concurrency, skip_verify, quiet, from_dump, fast_import, snapshot_dir, mempool_size, p2p, p2p_port, peers } => {
            if net_name != "mainnet" {
                return Err(anyhow!("only mainnet is supported (got {net_name})"));
            }
            let mut cfg = SyncConfig { concurrency, verify: !skip_verify, follow: !p2p, quiet, ..SyncConfig::default() };
            if let Some(nodes) = nodes {
                cfg.nodes = nodes.into_iter().map(|n| n.trim_end_matches('/').to_string()).collect();
            }
            let storage = Arc::new(Storage::open(&cli.db_path, network)?);
            if let Some(source) = from_dump {
                let dir = resolve_snapshot(&source, &snapshot_dir).await?;
                let report = run_import(storage.clone(), dir, fast_import, quiet).await?;
                if report.interrupted {
                    return Err(anyhow!("import interrupted"));
                }
            }
            let mempool = Arc::new(Mempool::new(storage.clone(), cfg.nodes.clone(), mempool_size));
            if !no_api {
                let addr: SocketAddr = format!("{api_host}:{api_port}").parse().context("invalid api bind address")?;
                let state = Arc::new(AppState::new(storage.clone(), mempool.clone(), cfg.nodes.clone()));
                tokio::spawn(async move {
                    if let Err(e) = api::serve(state, addr).await {
                        tracing::error!(error = %e, "REST API stopped");
                    }
                });
            }
            let pool_for_prune = mempool.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(8));
                loop {
                    tick.tick().await;
                    pool_for_prune.prune_confirmed().await;
                }
            });
            let hosts: Vec<String> = match peers {
                Some(p) => p,
                None => sth_core::p2p_legacy::fetch_peer_list(p2p_port).await,
            };
            if p2p {
                // Legacy P2P: 400 blocks per call, no REST rate limit — used for catch-up and live follow.
                tracing::info!(port = p2p_port, "relay node running over legacy P2P (Ctrl+C to stop)");
                tokio::select! {
                    r = sth_core::p2p_legacy::follow(storage.clone(), hosts, p2p_port, !skip_verify) => { r?; }
                    _ = tokio::signal::ctrl_c() => { tracing::info!("SIGINT received, stopping"); }
                }
                storage.flush()?;
                tracing::info!(height = storage.get_last_height()?, "relay node stopped");
            } else {
                let syncer = Arc::new(Syncer::new(storage, cfg)?);
                tracing::info!("relay node running: catching up over REST, then following the chain (Ctrl+C to stop)");
                let report = syncer.run().await?;
                tracing::info!(height = report.end_height, applied = report.blocks_applied, "relay node stopped");
            }
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
                let storage = Arc::new(Storage::open(&cli.db_path, network)?);
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
            let storage = Arc::new(Storage::open(&cli.db_path, network)?);
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
