//! Author: TechnoL0g
//!
//! `sth-core audit-all` against a RUNNING node: the sled directory is locked by the node, so the CLI asks the node to
//! audit itself (`POST /api/ntfry/audit`, loopback only) and polls the job — a forging delegate never stops.

use crate::error::{Error, Result};
use crate::node_config::NodeConfig;
use crate::storage::ChainAudit;
use serde_json::Value;
use std::time::Duration;

/// Base URL of the node's own REST API (`--api`, else `api.host:api.port` when the API is enabled).
pub fn api_url(cfg: &NodeConfig, override_url: Option<String>) -> Option<String> {
    if let Some(u) = override_url {
        return Some(u.trim_end_matches('/').to_string());
    }
    if !cfg.api.enabled {
        return None;
    }
    let host = if cfg.api.host == "0.0.0.0" || cfg.api.host.is_empty() { "127.0.0.1" } else { cfg.api.host.as_str() };
    Some(format!("http://{host}:{}", cfg.api.port))
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(15)).no_proxy().user_agent("sth-core-audit").build()?)
}

/// Is a node answering at `url`?
pub async fn reachable(url: &str) -> bool {
    match client() {
        Ok(c) => c.get(format!("{url}/api/ntfry/metrics")).send().await.is_ok_and(|r| r.status().is_success()),
        Err(_) => false,
    }
}

/// Start the live audit and follow it to the end. Prints like the offline command; returns the exit code
/// (0 = consistent / repaired, 2 = drift found without `--fix`).
pub async fn run(url: &str, fix: bool, json: bool) -> Result<i32> {
    let c = client()?;
    let started = std::time::Instant::now();
    let r = c.post(format!("{url}/api/ntfry/audit?fix={fix}")).send().await?;
    if !r.status().is_success() {
        let status = r.status();
        let body: Value = r.json().await.unwrap_or(Value::Null);
        return Err(Error::Sync(format!("{url}: HTTP {status} — {}", body["message"].as_str().unwrap_or("node refused the audit"))));
    }
    if !json {
        eprintln!("node is running → live audit through {url} (forging is not interrupted)");
    }
    let mut last_scanned = 0u64;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let v: Value = c.get(format!("{url}/api/ntfry/audit")).send().await?.error_for_status()?.json().await?;
        let job = &v["data"];
        let scanned = job["scanned"].as_u64().unwrap_or(0);
        if job["running"].as_bool().unwrap_or(false) {
            if !json && scanned != last_scanned {
                eprintln!("  scanned {} of {} blocks…", crate::delegate::forger::group(scanned), crate::delegate::forger::group(job["tip"].as_u64().unwrap_or(0)));
                last_scanned = scanned;
            }
            continue;
        }
        if let Some(e) = job["error"].as_str() {
            return Err(Error::Sync(format!("live audit failed: {e}")));
        }
        let report: ChainAudit = serde_json::from_value(job["result"].clone())?;
        let repaired = job["repaired"].as_u64().unwrap_or(0);
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!(
                "height {} · wallets {} · compared {} · skipped {} · drifted {} · {:.1}s (live)",
                crate::delegate::forger::group(report.height),
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
        if report.drifted.is_empty() {
            if !json {
                println!("OK: every wallet matches the chain history");
            }
            return Ok(0);
        }
        if fix {
            if !json {
                println!("repaired {repaired} wallet(s), vote index rebuilt");
            }
            return Ok(0);
        }
        if !json {
            println!("rerun with --fix to overwrite the drifted states (works on the running node too)");
        }
        return Ok(2);
    }
}
