//! Author: TechnoL0g
//!
//! Node runtime assembled from `NodeConfig`: storage → optional snapshot bootstrap → mempool →
//! local REST API → block intake (legacy P2P with peer table, or REST fallback).
//! `NodeContext` is the shared handle future modules (delegate/forger, iroh) attach to.

use crate::api::{self, AppState};
use crate::config::Network;
use crate::error::{Error, Result};
use crate::mempool::Mempool;
use crate::node_config::NodeConfig;
use crate::p2p_iroh::IrohNode;
use crate::p2p_legacy::{self, P2pOptions, PeerTable};
use crate::snapshot::{self, ImportOptions, ImportReport, SnapshotMeta, DEFAULT_SNAPSHOT_BASE_URL};
use crate::storage::Storage;
use crate::sync::{progress_style, SyncConfig, Syncer};
use indicatif::{ProgressBar, ProgressStyle};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct NodeContext {
    pub config: NodeConfig,
    pub network: Network,
    pub storage: Arc<Storage>,
    pub mempool: Arc<Mempool>,
    pub peers: Arc<PeerTable>,
    /// Destination of future relay / delegate rewards (from `rewards.*`).
    pub reward_address: Option<String>,
    /// Web 4.0 layer, present when `p2p.iroh.enabled`.
    pub iroh: Option<Arc<IrohNode>>,
    pub forging: Option<Arc<std::sync::Mutex<crate::delegate::ForgingStatus>>>,
    /// Resolved delegate passphrases (empty → relay only). Resolved in `open()` so a broken
    /// `delegate:` section is reported immediately instead of after the peer discovery.
    pub delegate_secrets: Vec<String>,
}

/// Non-fatal: a misconfigured `delegate:` section demotes the node to relay-only with a loud error.
fn resolve_delegate_secrets(cfg: &NodeConfig) -> Vec<String> {
    let has_any = cfg.delegate.secrets.iter().any(|s| !s.trim().is_empty()) || !cfg.delegate.secrets_file.trim().is_empty();
    if !cfg.delegate.enabled {
        if has_any {
            tracing::warn!("Delegate module DISABLED although secrets are configured — set delegate.enabled: true in node.yaml to forge");
        } else {
            tracing::info!("Delegate module disabled (relay only). To forge add to node.yaml:  delegate: {{ enabled: true, secrets: [\"<passphrase>\"] }}");
        }
        return Vec::new();
    }
    match crate::delegate::load_secrets(&cfg.delegate.secrets, &cfg.delegate.secrets_file) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            tracing::error!(
                "delegate.enabled is true but no passphrase is configured — forging is OFF, the node runs as a relay only.\n\
                 Fix node.yaml:\n\
                 delegate:\n\
                 \x20 enabled: true\n\
                 \x20 secrets:\n\
                 \x20   - \"<12-word delegate passphrase>\"\n\
                 or point delegate.secrets_file to a legacy delegates.json ({{ \"secrets\": [\"...\"] }})"
            );
            Vec::new()
        }
        Err(e) => {
            tracing::error!(error = %e, file = cfg.delegate.secrets_file.trim(), "cannot load delegate secrets — forging is OFF, the node runs as a relay only");
            Vec::new()
        }
    }
}

/// Runtime switches that are not part of the persisted configuration.
#[derive(Debug, Clone, Default)]
pub struct RunFlags {
    pub quiet: bool,
    /// Import `sync.bootstrap_snapshot` even when the database is not empty (`--from-dump`).
    pub force_bootstrap: bool,
}

impl NodeContext {
    pub async fn open(config: NodeConfig) -> Result<Self> {
        config.validate()?;
        let network = config.load_network()?;
        let storage = Arc::new(Storage::open_with_cache(Path::new(&config.db_path), network.clone(), config.db_cache_mb)?);
        storage.ensure_vote_index()?;
        let reward_address = config.reward_address()?;
        if let Some(a) = &reward_address {
            tracing::info!(address = a, "rewards.reward_address set (reserved for future relay rewards, unused for now)");
        }
        let delegate_secrets = resolve_delegate_secrets(&config);
        let mut seeds: Vec<String> = config.p2p.legacy_peers.clone();
        if config.p2p.legacy_enabled && config.p2p.use_peer_list {
            seeds.extend(p2p_legacy::fetch_peer_list(config.p2p.legacy_port).await);
        } else if seeds.is_empty() && network.name == "mainnet" {
            seeds.extend(p2p_legacy::P2P_SEEDS.iter().map(|s| s.to_string()));
        }
        let peers = Arc::new(PeerTable::new(config.p2p.legacy_port, seeds));
        let mempool = Arc::new(if config.p2p.legacy_enabled {
            Mempool::with_p2p(storage.clone(), config.sync.rest_nodes.clone(), peers.clone(), config.p2p.relay_fanout, config.mempool.max_size)
        } else {
            Mempool::new(storage.clone(), config.sync.rest_nodes.clone(), config.mempool.max_size)
        }
        .with_max_bytes(config.mempool.max_bytes)
        .with_dynamic_fees(config.mempool.dynamic_fees.clone()));
        Ok(Self { config, network, storage, mempool, peers, reward_address, iroh: None, forging: None, delegate_secrets })
    }

    async fn start_iroh(&mut self) -> Result<()> {
        let cfg = &self.config.p2p.iroh;
        if !cfg.enabled {
            return Ok(());
        }
        let secret = crate::p2p_iroh::load_or_create_secret(Path::new(&cfg.secret_key_file))?;
        let bootstrap = cfg.bootstrap.iter().map(|s| crate::p2p_iroh::parse_endpoint_id(s)).collect::<Result<Vec<_>>>()?;
        let gateway = Some(self.config.p2p.legacy_public_addr.trim().to_string()).filter(|g| !g.is_empty() && !self.config.p2p.legacy_listen.trim().is_empty());
        let relay = cfg.relay.then(|| crate::p2p_iroh::RelaySetup { n0: cfg.relay_n0, extra: cfg.relays.clone() });
        let node = IrohNode::spawn(secret, bootstrap, cfg.serve_blocks, relay, self.storage.clone(), self.mempool.clone(), self.config.sync.verify_blocks, Some(self.peers.clone()), gateway).await?;
        self.iroh = Some(node);
        Ok(())
    }

    /// Full node lifecycle; returns when the intake loop stops or on Ctrl+C.
    pub async fn run(mut self, flags: RunFlags) -> Result<()> {
        tokio::spawn(crate::ntp::check_clock("pool.ntp.org"));
        tokio::spawn(crate::mem::watchdog(std::time::Duration::from_secs(600)));
        self.start_iroh().await?;
        let cfg = self.config.clone();
        let source = cfg.sync.bootstrap_snapshot.trim();
        if !source.is_empty() && (flags.force_bootstrap || self.storage.get_last_height()? == 0) {
            let dir = resolve_snapshot(source, Path::new(&cfg.sync.snapshot_dir)).await?;
            let report = run_import(self.storage.clone(), dir, cfg.sync.fast_import, false, flags.quiet).await?;
            if report.interrupted {
                return Err(Error::Sync("import interrupted".into()));
            }
        }
        {
            let st = self.storage.clone();
            tokio::task::spawn_blocking(move || crate::genesis::ensure_genesis(&st, st.network()))
                .await
                .map_err(|e| Error::Sync(format!("genesis task failed: {e}")))??;
        }
        if !self.delegate_secrets.is_empty() {
            let mut pq_secrets = cfg.delegate.pq_secrets.clone();
            if let Ok(v) = std::env::var("STH_DELEGATE_PQ_PASSPHRASE") {
                pq_secrets.push(v);
            }
            let forger = Arc::new(crate::delegate::Forger::new(
                &self.delegate_secrets,
                &pq_secrets,
                self.storage.clone(),
                self.mempool.clone(),
                self.peers.clone(),
                crate::delegate::ForgerOptions {
                    broadcast_fanout: cfg.delegate.broadcast_fanout,
                    quorum_share: cfg.delegate.quorum_share,
                    min_quorum_peers: cfg.delegate.min_quorum_peers,
                    iroh: self.iroh.clone(),
                    ..Default::default()
                },
            )?);
            forger.log_loaded();
            if let Some(iroh) = &self.iroh {
                iroh.set_forging_keys(forger.keys(), cfg.delegate.announce);
            }
            self.forging = Some(forger.status.clone());
            tokio::spawn(forger.run());
        }
        if cfg.api.enabled {
            let addr: SocketAddr = format!("{}:{}", cfg.api.host, cfg.api.port)
                .parse()
                .map_err(|e| Error::Config(format!("invalid api bind address: {e}")))?;
            let mut state = AppState::new(self.storage.clone(), self.mempool.clone(), cfg.sync.rest_nodes.clone());
            if cfg.p2p.legacy_enabled {
                state = state.with_peer_table(self.peers.clone());
            }
            if let Some(iroh) = &self.iroh {
                state = state.with_iroh(iroh.clone());
            }
            if let Some(f) = &self.forging {
                state = state.with_forging(f.clone());
            }
            let metrics_addr: Option<SocketAddr> = match cfg.api.metrics_listen.trim() {
                "" => None,
                s => Some(s.parse().map_err(|e| Error::Config(format!("invalid api.metrics_listen: {e}")))?),
            };
            let state = Arc::new(state.with_metrics_page(cfg.api.page_metrics && metrics_addr.is_none()));
            if cfg.api.page_metrics {
                match metrics_addr {
                    Some(maddr) => {
                        let st = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = api::serve_metrics(st, maddr).await {
                                tracing::error!(error = %e, "metrics page stopped");
                            }
                        });
                    }
                    None => tracing::info!(url = format!("http://{addr}/"), "metrics page enabled on the API port"),
                }
            }
            tokio::spawn(async move {
                if let Err(e) = api::serve(state, addr).await {
                    tracing::error!(error = %e, "REST API stopped");
                }
            });
        }
        if !cfg.p2p.legacy_listen.trim().is_empty() {
            let addr: SocketAddr = cfg.p2p.legacy_listen.trim().parse().map_err(|e| Error::Config(format!("invalid p2p.legacy_listen: {e}")))?;
            let server = crate::p2p_legacy::LegacyServer::new(self.storage.clone(), self.mempool.clone(), self.peers.clone(), cfg.sync.verify_blocks);
            tokio::spawn(async move {
                if let Err(e) = server.serve(addr).await {
                    tracing::error!(error = %e, "legacy P2P server stopped");
                }
            });
        }
        crate::delegate::spawn_round_tracker(self.storage.clone());
        let pool = self.mempool.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(8));
            loop {
                tick.tick().await;
                pool.prune_confirmed().await;
            }
        });
        if cfg.p2p.legacy_enabled {
            let opts = P2pOptions {
                verify: cfg.sync.verify_blocks,
                parallel: cfg.p2p.parallel_peers,
                quiet: flags.quiet,
                refresh_interval: Duration::from_secs(cfg.p2p.refresh_secs.max(10)),
                iroh: self.iroh.clone(),
                ..P2pOptions::default()
            };
            tracing::info!(port = cfg.p2p.legacy_port, peers = self.peers.len(), parallel = opts.parallel, "relay node running over legacy P2P (Ctrl+C to stop)");
            tokio::select! {
                r = p2p_legacy::run_follow(self.storage.clone(), self.peers.clone(), opts) => { r?; }
                _ = tokio::signal::ctrl_c() => { tracing::info!("SIGINT received, stopping"); }
            }
            if let Some(iroh) = &self.iroh {
                iroh.shutdown().await;
            }
            self.storage.flush()?;
            tracing::info!(height = self.storage.get_last_height()?, "relay node stopped");
        } else {
            let sync_cfg = SyncConfig {
                nodes: cfg.sync.rest_nodes.iter().map(|n| n.trim_end_matches('/').to_string()).collect(),
                concurrency: cfg.sync.concurrency,
                verify: cfg.sync.verify_blocks,
                follow: true,
                quiet: flags.quiet,
                ..SyncConfig::default()
            };
            let syncer = Arc::new(Syncer::new(self.storage.clone(), sync_cfg)?);
            tracing::info!("relay node running: catching up over REST, then following the chain (Ctrl+C to stop)");
            let report = syncer.run().await?;
            tracing::info!(height = report.end_height, applied = report.blocks_applied, "relay node stopped");
        }
        Ok(())
    }
}

pub fn import_bar(quiet: bool) -> Result<ProgressBar> {
    let pb = if quiet { ProgressBar::hidden() } else { ProgressBar::new(0) };
    pb.set_style(progress_style("Importing")?);
    Ok(pb)
}

pub fn download_bar() -> Result<ProgressBar> {
    let pb = ProgressBar::new(0);
    let style = ProgressStyle::with_template("Downloading: [{bar:40}] {bytes} / {total_bytes} ({bytes_per_sec}, ETA: {eta})")
        .map_err(|e| Error::Sync(format!("progress template: {e}")))?;
    pb.set_style(style.progress_chars("=> "));
    Ok(pb)
}

/// Resolve `latest` / .tgz / folder into an extracted snapshot directory.
pub async fn resolve_snapshot(source: &str, snapshot_dir: &Path) -> Result<PathBuf> {
    let archive_or_dir = if source == "latest" {
        let pb = download_bar()?;
        let archive = snapshot::download_latest(DEFAULT_SNAPSHOT_BASE_URL, snapshot_dir, &pb).await?;
        pb.finish_and_clear();
        archive
    } else {
        PathBuf::from(source)
    };
    tokio::task::spawn_blocking(move || snapshot::locate_snapshot_dir(&archive_or_dir))
        .await
        .map_err(|e| Error::Sync(format!("snapshot task failed: {e}")))?
}

/// Run the blocking import on the blocking pool with Ctrl+C support.
pub async fn run_import(storage: Arc<Storage>, dir: PathBuf, fast: bool, strict: bool, quiet: bool) -> Result<ImportReport> {
    let meta = SnapshotMeta::read(&dir)?;
    tracing::info!(
        folder = meta.folder,
        blocks = meta.blocks.count,
        transactions = meta.transactions.count,
        end_height = meta.blocks.end,
        compressed = !meta.skip_compression,
        fast,
        strict,
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
        snapshot::import_snapshot(&storage, &dir, ImportOptions { fast, strict }, &pb_worker, &cancel)
    })
    .await
    .map_err(|e| Error::Sync(format!("import task failed: {e}")))??;
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
