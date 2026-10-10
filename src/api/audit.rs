//! Author: TechnoL0g
//!
//! Live state audit on a RUNNING node — `sth-core audit-all [--fix]` uses it when the sled directory is locked by the
//! node (a forging delegate must not stop for minutes). Operator action: loopback only, never through a reverse proxy.
//! `POST /api/ntfry/audit?fix=true` starts the job (409 while one runs), `GET /api/ntfry/audit` reports progress / result.

use super::{ApiError, ApiResult, Shared};
use crate::storage::{AuditSummary, ChainAudit};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// State of the (single) audit job, served by `GET /api/ntfry/audit`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditJob {
    pub running: bool,
    pub fix: bool,
    pub started_unix: i64,
    /// Highest block booked so far (progress; updated every 10 000 blocks).
    pub scanned: u64,
    /// Tip at start (the scan walks up to it, the tail is booked under the apply lock).
    pub tip: u64,
    pub duration_secs: f64,
    pub repaired: usize,
    pub result: Option<ChainAudit>,
    pub error: Option<String>,
}

pub type SharedJob = Arc<Mutex<AuditJob>>;

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn local_only(peer: &Option<ConnectInfo<SocketAddr>>, headers: &HeaderMap) -> Result<(), ApiError> {
    let local = peer.as_ref().is_some_and(|p| p.0.ip().is_loopback());
    let proxied = headers.contains_key("x-forwarded-for") || headers.contains_key("x-real-ip") || headers.contains_key("forwarded");
    if !local || proxied {
        return Err(ApiError(StatusCode::FORBIDDEN, "state audit is allowed only from 127.0.0.1 on the node itself (sth-core audit-all)".into()));
    }
    Ok(())
}

pub async fn status(State(st): State<Shared>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap) -> ApiResult {
    local_only(&peer, &headers)?;
    let job = st.audit_job.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(Json(json!({ "data": job })))
}

pub async fn start(State(st): State<Shared>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    local_only(&peer, &headers)?;
    let fix = q.get("fix").is_some_and(|v| v == "true" || v == "1");
    let tip = st.storage.get_last_height()?;
    {
        let mut job = st.audit_job.lock().unwrap_or_else(|e| e.into_inner());
        if job.running {
            return Err(ApiError(StatusCode::CONFLICT, format!("an audit is already running (scanned {} of {})", job.scanned, job.tip)));
        }
        *job = AuditJob { running: true, fix, started_unix: now_unix(), tip, ..Default::default() };
    }
    let storage = st.storage.clone();
    let job = st.audit_job.clone();
    let progress = job.clone();
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let outcome = storage.audit_all_live(fix, |h| progress.lock().unwrap_or_else(|e| e.into_inner()).scanned = h);
        let elapsed = started.elapsed().as_secs_f64();
        let mut j = job.lock().unwrap_or_else(|e| e.into_inner());
        j.running = false;
        j.duration_secs = elapsed;
        match outcome {
            Ok((report, repaired)) => {
                j.scanned = report.height;
                j.repaired = repaired;
                if let Err(e) = storage.save_audit_summary(&AuditSummary::from_audit(&report, fix, elapsed)).and_then(|_| storage.flush().map(|_| ())) {
                    tracing::warn!(error = %e, "live audit: saving the summary failed");
                }
                if report.drifted.is_empty() {
                    tracing::info!(height = report.height, wallets = report.wallets, secs = format!("{elapsed:.1}"), "live audit: every wallet matches the chain history");
                } else {
                    let msg = format!(
                        "audit-all (live) at height {}: {} of {} wallets drifted{} — {}",
                        report.height,
                        report.drifted.len(),
                        report.wallets,
                        if fix { " (repaired)" } else { "" },
                        report.drifted.iter().take(5).map(|a| a.address.as_str()).collect::<Vec<_>>().join(", ")
                    );
                    tracing::warn!("{msg}");
                    crate::alerts::notify("audit", msg, true);
                }
                j.result = Some(report);
            }
            Err(e) => {
                tracing::warn!(error = %e, "live audit failed");
                j.error = Some(e.to_string());
            }
        }
    });
    Ok(Json(json!({ "data": { "started": true, "fix": fix, "tip": tip } })))
}
