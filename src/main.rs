//! Author: TechnoL0g
//!
//! `sth-core` CLI: `init` (node.yaml), `run` (relay node), snapshot tools, legacy sync and
//! peer diagnostics. Node assembly lives in `sth_core::node`.

use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use sth_core::crypto::{verify_block, ChainObject};
use sth_core::models::Block;
use sth_core::node::{download_bar, import_bar, resolve_snapshot, run_import, NodeContext, RunFlags};
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
    /// Configuration file (default: ./node.yaml when present, else built-in mainnet defaults). Its `network` /
    /// `network_dir` select the chain for every command (`sth-core init newnet` networks included).
    #[arg(long, global = true, value_name = "node.yaml")]
    config: Option<PathBuf>,
    /// Colours + console highlights (forged / relayed / skipped blocks): auto = when stdout is a terminal,
    /// always = also under pm2 (keeps ANSI), never = plain logs.
    #[arg(long, global = true, default_value = "auto", value_parser = ["auto", "always", "never"])]
    color: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show local chain height and last block id.
    Info,
    /// Show stored blocks at the given heights (id, generator, time) — offline, to compare with a legacy node's
    /// `/api/blocks?height=H` and find the common block before `rollback`. Also prints how far back `rollback` can go.
    Block {
        /// Heights, e.g. `--height 11926050 --height 11926100`.
        #[arg(long = "height", required = true)]
        heights: Vec<u64>,
    },
    /// Verify a block JSON file (core `IBlockData` format, with transactions).
    VerifyBlock { file: PathBuf },
    /// Verify and apply a block JSON file to the local database.
    ImportBlock { file: PathBuf },
    /// Show a stored wallet.
    Wallet { address: String },
    /// Compare a wallet's stored nonce / balance with the values recomputed from its transactions (state drift after
    /// fork rollbacks). `--fix` overwrites the stored state with the recomputed one (stop the node first).
    AuditWallet {
        address: String,
        #[arg(long)]
        fix: bool,
    },
    /// Walk the whole chain once and compare EVERY stored wallet with the recomputed nonce / balance (nightly operator
    /// check, node stopped or read-only copy). `--fix` repairs the drifted wallets, `--json` prints the report as JSON.
    AuditAll {
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        json: bool,
    },
    /// One health verdict: node API / sync / stall / peers / last audit (from /api/ntfry/metrics) + legacy acceptance.
    /// Exit code 0 healthy, 1 warnings, 2 failures — cron-friendly.
    Doctor {
        /// Public IP of this node for the legacy acceptance check (default: p2p.legacy_public_addr).
        #[arg(long)]
        public_ip: Option<String>,
        /// Node API base URL (default: http://<api.host>:<api.port>).
        #[arg(long)]
        api: Option<String>,
    },
    /// Roll the local chain back to HEIGHT (needs undo records; stop the node first). Use after a private fork
    /// when automatic recovery failed; the node re-downloads the blocks from peers on next start.
    Rollback {
        #[arg(long)]
        to: u64,
    },
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
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
        /// Also export the embedded network files (crypto-networks layout) into ./network.
        #[arg(long)]
        network_files: bool,
        #[command(subcommand)]
        newnet: Option<InitCmd>,
    },
    /// Run the relay node from `node.yaml` (CLI flags override the file): optional snapshot bootstrap →
    /// catch up → follow the chain + local REST API.
    Run {
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
    /// Build, sign and post a transaction through the local REST API (transfer, sObject, tokens).
    Tx(sth_core::cli::TxArgs),
    /// Print (creating if needed) the iroh EndpointId of this node — share it as `p2p.iroh.bootstrap` on other nodes.
    IrohId {
        #[arg(long, default_value = "./iroh.key")]
        key_file: PathBuf,
        /// Print the SHIP-44 wallet URL `api://<EndpointId>/<name>` instead of the bare id.
        #[arg(long)]
        url: bool,
        /// Provider name segment of the URL (informational — the node serves any name).
        #[arg(long, default_value = "sth-core")]
        name: String,
    },
    /// Probe the legacy peer list and print the health table (latency, height, version).
    Peers {
        #[arg(long, env = "CORE_P2P_PORT", default_value_t = 4001)]
        port: u16,
        #[arg(long, value_delimiter = ',')]
        peers: Option<Vec<String>>,
        /// Ask every alive peer for its accepted peer list (`p2p.peer.getPeers`) and report which of them list this
        /// PQ Node (its public IP from `--public-ip`, else `p2p.legacy_public_addr` in node.yaml).
        #[arg(long)]
        accepted: bool,
        #[arg(long)]
        public_ip: Option<String>,
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
enum InitCmd {
    /// Generate a private / test network (mainnet rules from block 1, tokens & PQ off): network files, signed genesis
    /// with N delegates, their passphrases and node.yaml on separate ports. Nothing touches mainnet.
    Newnet {
        #[arg(long, default_value = "./testnet")]
        out: PathBuf,
        #[arg(long, default_value = "TSH")]
        ticker: String,
        #[arg(long, default_value = "SmartHoldemTestNet")]
        title: String,
        #[arg(long, default_value_t = 21)]
        delegates: u16,
        /// Address version byte (mainnet 63 = "S…"; 30 = "D…").
        #[arg(long, default_value_t = 30)]
        pubkey_hash: u8,
        #[arg(long, default_value_t = 4002)]
        p2p_port: u16,
        #[arg(long, default_value_t = 4004)]
        api_port: u16,
        #[arg(long, default_value_t = 4889)]
        metrics_port: u16,
        /// Deterministic seed for every key (same seed → same network). Random if omitted.
        #[arg(long)]
        seed: Option<String>,
        /// Activate native tokens at this height (omit = off).
        #[arg(long)]
        tokens_at: Option<u64>,
        /// Activate Quantum Shield stage B (v3 transactions, ML-DSA-44) at this height (omit = off).
        #[arg(long)]
        pq_at: Option<u64>,
        /// Activate Quantum Shield stage C (version-1 blocks with hybrid secp256k1 + ML-DSA-44 signatures, 21-block grace) at this height.
        #[arg(long)]
        pq_blocks_at: Option<u64>,
        /// SHIP-35 hard finality (rollback below a certificate refused, equivocation slashing) from height 1. Default: soft mode.
        #[arg(long, default_value_t = false)]
        finality_hard: bool,
        /// Treasury premine in whole coins.
        #[arg(long, default_value_t = 100_000_000)]
        treasury: u64,
        /// Stake per delegate in whole coins.
        #[arg(long, default_value_t = 1_000)]
        delegate_stake: u64,
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
    /// Write a dump `<out>/1-<end>.tgz` (legacy core-snapshots layout) from the local database — stop the node first or
    /// use `--end` below the tip. Readable by `sth-core snapshot import` and by the legacy `snapshot:import`.
    Dump {
        #[arg(long, default_value = "./snapshots")]
        out: PathBuf,
        /// Last height to include (default: local tip; use a height below a fork point to share a clean chain).
        #[arg(long)]
        end: Option<u64>,
    },
    /// Import a dump (folder or .tgz) into the local database (resumable).
    Import {
        path: PathBuf,
        /// Skip secp256k1 signature checks (trusted dump).
        #[arg(long)]
        fast_import: bool,
        /// History audit: also apply the live wallet-aware rules (second signature, smart objects) to every block.
        #[arg(long, conflicts_with = "fast_import")]
        strict: bool,
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
    let cli = Cli::parse();
    // `iroh-id` is meant for `$(sth-core iroh-id)`: bare id on stdout, logs to stderr, no banner
    let quiet = matches!(cli.command, Command::IrohId { .. } | Command::AuditAll { json: true, .. });
    // iroh::protocol WARNs "Accepting incoming connection ended with error: timed out" without the remote address
    // (wallet_api::accept_with_log reports it at INFO with who knocked); noq_proto WARNs "failed closing path
    // err=LastOpenPath" when a QUIC path idles out and is the connection's only path — transport noise. n0_mainline shouts
    // ERROR every 2 s while the DHT routing table is empty and iroh_mainline_address_lookup logs every resolve at INFO —
    // the node reports DHT health itself (one WARN + alert `dht-unreachable`, `ntfry.discovery.dht`). RUST_LOG overrides.
    const QUIET_TARGETS: &str = "iroh::protocol=error,noq_proto=error,n0_mainline=off,iroh_mainline_address_lookup=warn";
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(if quiet { format!("warn,{QUIET_TARGETS}") } else { format!("info,{QUIET_TARGETS}") }));
    if quiet {
        tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
    } else {
        sth_core::console::set_color(&cli.color);
        tracing_subscriber::fmt().with_env_filter(filter).with_ansi(sth_core::console::color()).init();
    }

    if !quiet {
        println!("{}", sth_core::console::paint(sth_core::console::BLUE, &format!("core version {}", env!("CARGO_PKG_VERSION"))));
    }
    // `init` writes the config, everything else reads it: network files + db path come from node.yaml
    let base_cfg = match &cli.command {
        Command::Init { .. } => NodeConfig::default(),
        _ => NodeConfig::load_or_default(cli.config.as_deref())?,
    };
    let network = base_cfg.load_network()?;
    let db_path: PathBuf = cli.db_path.clone().unwrap_or_else(|| PathBuf::from(&base_cfg.db_path));

    match cli.command {
        Command::Block { heights } => {
            let storage = Storage::open(&db_path, network.clone())?;
            let tip = storage.get_last_height()?;
            let oldest = storage.oldest_undo_height();
            println!("tip: {}  ·  rollback possible down to: {}", tip, oldest.map(|h| (h.saturating_sub(1)).to_string()).unwrap_or_else(|| "-".into()));
            for h in heights {
                match storage.get_block_by_height(h)? {
                    Some(b) => {
                        let unix = network.epoch_to_unix(b.timestamp);
                        let t = sth_core::api::human_time(unix);
                        println!("{h}: id {}  generator {}  {t}  {} tx  previous {}", b.id.clone().unwrap_or_default(), &b.generator_public_key[..b.generator_public_key.len().min(12)], b.transactions.len(), &b.previous_block[..b.previous_block.len().min(12)]);
                    }
                    None => println!("{h}: not in the local database"),
                }
            }
        }
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
        Command::Rollback { to } => {
            let storage = Storage::open(&db_path, network)?;
            let tip = storage.get_last_height()?;
            if to >= tip {
                println!("nothing to do: tip {tip}, requested {to}");
            } else {
                let reached = storage.rollback_to(to)?;
                storage.flush()?;
                println!("rolled back {tip} -> {reached}");
            }
        }
        Command::AuditAll { fix, json } => {
            let storage = Storage::open(&db_path, network.clone())?;
            let started = std::time::Instant::now();
            let report = storage.audit_all(|h| {
                if !json {
                    eprintln!("  scanned {} blocks…", sth_core::delegate::forger::group(h));
                }
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "height {} · wallets {} · compared {} · skipped {} · drifted {} · {:.1}s",
                    sth_core::delegate::forger::group(report.height),
                    report.wallets,
                    report.checked,
                    report.skipped,
                    report.drifted.len(),
                    started.elapsed().as_secs_f64()
                );
                for a in &report.drifted {
                    println!(
                        "DRIFT {}  nonce {:?} -> {}  balance {:?} -> {}  (sent {}, received {}, forged {})",
                        a.address, a.stored_nonce, a.computed_nonce, a.stored_balance, a.computed_balance, a.sent, a.received, a.forged
                    );
                }
            }
            if !report.drifted.is_empty() {
                if fix {
                    for a in &report.drifted {
                        storage.repair_wallet(a)?;
                    }
                }
            }
            if fix {
                // the vote index (`dv:`) drifted with the balances (double apply booked votes twice too) → rebuild from wallets
                let before = storage.active_delegates(51)?;
                let voters = storage.rebuild_vote_index()?;
                let after = storage.active_delegates(51)?;
                if !json {
                    println!("vote index rebuilt from {voters} voting wallet(s)");
                    for (i, (b, a)) in before.iter().zip(after.iter()).enumerate() {
                        if b.public_key != a.public_key || b.votes != a.votes {
                            println!("  rank {:>2}: {} {} -> {} {}", i + 1, &b.public_key[..12], b.votes, &a.public_key[..12], a.votes);
                        }
                    }
                }
            }
            storage.save_audit_summary(&sth_core::storage::AuditSummary::from_audit(&report, fix, started.elapsed().as_secs_f64()))?;
            storage.flush()?;
            if !report.drifted.is_empty() {
                sth_core::alerts::init(&base_cfg.alerts, if base_cfg.p2p.legacy_public_addr.trim().is_empty() { network.name.clone() } else { base_cfg.p2p.legacy_public_addr.trim().to_string() });
                let msg = format!("audit-all at height {}: {} of {} wallets drifted{} — {}", report.height, report.drifted.len(), report.wallets, if fix { " (repaired)" } else { "" }, report.drifted.iter().take(5).map(|a| a.address.as_str()).collect::<Vec<_>>().join(", "));
                if let Some(h) = sth_core::alerts::notify("audit", msg, true) {
                    let _ = h.await;
                }
            }
            if !report.drifted.is_empty() {
                if fix {
                    if !json {
                        println!("repaired {} wallet(s)", report.drifted.len());
                    }
                } else {
                    if !json {
                        println!("rerun with --fix (node stopped) to overwrite the drifted states");
                    }
                    std::process::exit(2);
                }
            } else if !json {
                println!("OK: every wallet matches the chain history");
            }
        }
        Command::Doctor { public_ip, api } => {
            let code = sth_core::doctor::run(&base_cfg, public_ip, api).await;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Command::AuditWallet { address, fix } => {
            let storage = Storage::open(&db_path, network)?;
            let a = storage.audit_wallet(&address)?;
            println!("{}", serde_json::to_string_pretty(&a)?);
            if a.nonce_ok && a.balance_ok {
                println!("OK: stored state matches the transaction history");
            } else if fix {
                storage.repair_wallet(&a)?;
                storage.flush()?;
                println!("repaired: nonce {:?} -> {}, balance {:?} -> {}", a.stored_nonce, a.computed_nonce, a.stored_balance, a.computed_balance);
            } else {
                println!("DRIFT detected — rerun with --fix (node stopped) to overwrite the stored state");
            }
        }
        Command::Wallet { address } => {
            let storage = Storage::open(&db_path, network)?;
            match storage.get_wallet(&address)? {
                Some(w) => println!("{}", serde_json::to_string_pretty(&w)?),
                None => println!("wallet {address} not found"),
            }
        }
        Command::Init { newnet: Some(InitCmd::Newnet { out, ticker, title, delegates, pubkey_hash, p2p_port, api_port, metrics_port, seed, tokens_at, pq_at, pq_blocks_at, finality_hard, treasury, delegate_stake }), .. } => {
            let seed = seed.unwrap_or_else(|| hex::encode(&iroh::SecretKey::generate().to_bytes()[..16]));
            let opts = sth_core::newnet::NewNetOptions { ticker, title, delegates: delegates.max(1), pubkey_hash, p2p_port, api_port, metrics_port, seed: seed.clone(), tokens_at, pq_at, pq_blocks_at, finality_hard, treasury_supply: treasury * 100_000_000, delegate_stake: delegate_stake * 100_000_000 };
            let n = sth_core::newnet::generate(&opts, &out)?;
            println!("new network written to {}", out.display());
            println!("  nethash     {}", n.nethash);
            println!("  genesis id  {}", n.genesis_id);
            println!("  treasury    {}  (passphrase in delegates.json)", n.treasury_address);
            println!("  delegates   {} (passphrases in delegates.json, all forging in node.yaml)", n.delegate_passphrases.len());
            println!("  seed        {seed}");
            println!("run:  cd {} && sth-core run --config node.yaml", out.display());
        }
        Command::Init { force, network_files, newnet: None } => {
            let config = cli.config.clone().unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILE));
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
            println!("edit node.yaml (delegate.secrets to forge), then start with: sth-core run --config {}", config.display());
        }
        Command::Run {
            api_host, api_port, no_api, nodes, concurrency, skip_verify, quiet, from_dump, fast_import, snapshot_dir,
            mempool_size, p2p, no_p2p, p2p_port, peers, parallel_peers, reward_address,
        } => {
            let mut cfg = base_cfg;
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
        Command::Tx(args) => sth_core::cli::run(sth_core::cli::ChainSource::Config(base_cfg), args).await?,
        Command::IrohId { key_file, url, name } => {
            let secret = sth_core::p2p_iroh::load_or_create_secret(&key_file)?;
            if url {
                println!("api://{}/{}", secret.public(), name.trim_matches('/'));
            } else {
                println!("{}", secret.public());
            }
        }
        Command::Peers { port, peers, accepted, public_ip } => {
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
            if accepted {
                let me = match public_ip.or_else(|| base_cfg.p2p.legacy_public_addr.rsplit_once(':').map(|(ip, _)| ip.to_string())).filter(|s| !s.trim().is_empty()) {
                    Some(ip) => ip.trim().to_string(),
                    None => {
                        return Err(sth_core::error::Error::Config("public IP unknown: pass --public-ip <ip> or set p2p.legacy_public_addr in node.yaml".into()).into());
                    }
                };
                let report = sth_core::p2p_legacy::acceptance_check(&table, &me, Duration::from_secs(10)).await;
                println!();
                println!("acceptance check for {me}:{port} (asked {} alive peers for their accepted peer list)", report.asked);
                println!("{:<18} {:>9} {:<8} {}", "peer", "height", "version", "lists us");
                for (ip, height, version, lists) in &report.peers {
                    println!("{:<18} {:>9} {:<8} {}", ip, height, version, if *lists { "yes" } else { "NO" });
                }
                let pct = if report.asked > 0 { report.listed * 100 / report.asked } else { 0 };
                println!("listed by {} / {} peers ({pct}%)", report.listed, report.asked);
                if report.asked == 0 {
                    println!("no peer answered getPeers — network unreachable from here");
                } else if report.listed == 0 {
                    println!("NOT ACCEPTED: no legacy peer lists {me} — is p2p.legacy_listen open on {port}/tcp, legacy_public_addr correct, node ≥ 0.23.0 (getStatus schema)?");
                } else if pct < 50 {
                    println!("partially accepted: legacy peer verification takes a few minutes after start; check firewall / port {port} if it stays low");
                } else {
                    println!("OK: this PQ Node is an accepted legacy peer");
                }
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
            SnapshotCmd::Dump { out, end } => {
                let storage = Storage::open(&db_path, network.clone())?;
                let tip = storage.get_last_height()?;
                let pb = import_bar(quiet)?;
                pb.set_length(end.unwrap_or(tip).min(tip));
                let tgz = tokio::task::spawn_blocking(move || snapshot::dump(&storage, &out, end, Some(&pb))).await??;
                let size = std::fs::metadata(&tgz).map(|m| m.len() / 1_000_000).unwrap_or(0);
                println!("snapshot written: {} ({size} MB) — serve the folder over HTTP (nginx/python3 -m http.server) and point other nodes' sync.bootstrap_snapshot at it, or copy the .tgz and run `sth-core snapshot import <file>`", tgz.display());
            }
            SnapshotCmd::Download { base_url, out } => {
                let pb = download_bar()?;
                let archive = snapshot::download_latest(&base_url, &out, &pb).await?;
                pb.finish_and_clear();
                let dir = tokio::task::spawn_blocking(move || snapshot::extract_tgz(&archive, &out)).await??;
                let meta = SnapshotMeta::read(&dir)?;
                tracing::info!(dir = %dir.display(), end_height = meta.blocks.end, "snapshot ready");
                println!("{}", dir.display());
            }
            SnapshotCmd::Import { path, fast_import, strict, quiet } => {
                let dir = tokio::task::spawn_blocking(move || snapshot::locate_snapshot_dir(&path)).await??;
                let storage = Arc::new(Storage::open(&db_path, network)?);
                let report = run_import(storage, dir, fast_import, strict, quiet).await?;
                if strict && !report.interrupted {
                    println!("history audit OK: {} blocks passed the live rules (second signature, smart objects)", report.imported);
                }
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
                let report = run_import(storage.clone(), dir, fast_import, false, quiet).await?;
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
