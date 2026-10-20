//! Author: TechnoL0g
//!
//! `sth-core doctor` — one health verdict for operators: node API + metrics feed (sync, stall, peers, last audit),
//! then the legacy acceptance check (`peers --accepted`) when the node has a public legacy address.

use crate::node_config::NodeConfig;
use serde_json::Value;
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::Ok => "[ OK ]",
            Level::Warn => "[WARN]",
            Level::Fail => "[FAIL]",
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Check {
    pub level: Level,
    pub name: &'static str,
    pub detail: String,
}

fn check(level: Level, name: &'static str, detail: impl Into<String>) -> Check {
    Check { level, name, detail: detail.into() }
}

fn dur(s: i64) -> String {
    let s = s.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h {}m", s / 3600, s % 3600 / 60)
    } else {
        format!("{}d {}h", s / 86_400, s % 86_400 / 3600)
    }
}

/// Evaluate the `/api/ntfry/metrics` payload (`data` object). Pure — unit-tested with fixtures.
pub fn evaluate(m: &Value, now_unix: i64) -> Vec<Check> {
    let mut out = Vec::new();
    let node = &m["node"];
    let chain = &m["chain"];
    out.push(check(Level::Ok, "node", format!("{} v{} · uptime {}", node["implementation"].as_str().unwrap_or("?"), node["version"].as_str().unwrap_or("?"), dur(node["uptime"].as_i64().unwrap_or(0)))));

    let bt = chain["blockTime"].as_i64().unwrap_or(8).max(1);
    let behind = chain["behind"].as_u64().unwrap_or(0);
    let height = chain["height"].as_u64().unwrap_or(0);
    let stall = &chain["stall"];
    if stall["lastReason"]["kind"] == "legacy-split" && stall["lastReason"]["height"].as_u64().is_some_and(|h| height.saturating_sub(h) < 200) {
        out.push(check(Level::Fail, "network", format!("LEGACY NETWORK SPLIT: {} — legacy nodes reject our branch; roll them back below the fork point and resync from PQ peers (or return the PQ nodes to the legacy branch)", stall["lastReason"]["detail"].as_str().unwrap_or(""))));
    }
    if stall["stalled"] == true {
        let why = stall["lastReason"].as_object().map(|r| format!(" — cause: {} {} ({})", r["kind"].as_str().unwrap_or("?"), r["peer"].as_str().unwrap_or(""), r["detail"].as_str().unwrap_or(""))).unwrap_or_else(|| " — see the node log (CHAIN STALLED)".into());
        out.push(check(Level::Fail, "chain", format!("STALLED: tip {height} unchanged for {} while {} peer(s) report {}{why}", dur(stall["seconds"].as_i64().unwrap_or(0)), stall["peersAhead"], stall["networkHeight"])));
    } else if behind > 100 {
        out.push(check(Level::Fail, "chain", format!("{behind} blocks behind the network (height {height} vs {})", chain["networkHeight"])));
    } else if behind > 10 {
        out.push(check(Level::Warn, "chain", format!("{behind} blocks behind (catching up?)")));
    } else {
        out.push(check(Level::Ok, "chain", format!("height {height} · {} behind", behind)));
    }
    let age = chain["lastBlockAge"].as_i64().unwrap_or(0);
    out.push(check(if age > 10 * bt { Level::Fail } else if age > 3 * bt { Level::Warn } else { Level::Ok }, "last block", format!("{} ago (slot {bt}s)", dur(age))));

    // system clock: NTP probe (every 10 min) + the earliest block arrival (a block before its slot start = our clock lags)
    let clock = &m["clock"];
    let skew = m["intake"]["clockSkewHintMs"].as_i64().unwrap_or(0);
    match clock["offsetMs"].as_i64() {
        Some(ms) if ms.abs() > crate::ntp::DRIFT_ALERT_MS => out.push(check(Level::Fail, "clock", format!("system clock {}ms {} NTP ({}) — enable time sync (chrony / timedatectl set-ntp true)", ms.abs(), if ms > 0 { "ahead of" } else { "behind" }, clock["server"].as_str().unwrap_or("?")))),
        Some(ms) if skew > 500 => out.push(check(Level::Warn, "clock", format!("NTP offset {ms}ms but blocks arrive up to {skew}ms before their slot — a forger's clock runs ahead (not ours)"))),
        Some(ms) => out.push(check(Level::Ok, "clock", format!("{ms}ms from NTP ({})", clock["server"].as_str().unwrap_or("?")))),
        None if skew > 500 => out.push(check(Level::Warn, "clock", format!("NTP unreachable ({}) and blocks arrive up to {skew}ms before their slot start — system clock probably behind, enable time sync", clock["error"].as_str().unwrap_or("not probed yet")))),
        None => out.push(check(if clock["error"].is_string() { Level::Warn } else { Level::Ok }, "clock", clock["error"].as_str().map(|e| format!("NTP probe failed: {e}")).unwrap_or_else(|| "NTP not probed yet".into()))),
    }

    let legacy = &m["legacy"];
    if legacy["enabled"] == true {
        let alive = legacy["alive"].as_u64().unwrap_or(0);
        let parked = legacy["parked"].as_u64().unwrap_or(0);
        let known = legacy["known"].as_u64().unwrap_or(0);
        out.push(check(if alive == 0 { Level::Fail } else if alive < 3 { Level::Warn } else { Level::Ok }, "legacy peers", format!("{alive}/{known} alive · {parked} parked · best {}", legacy["bestHeight"])));
    } else {
        out.push(check(Level::Warn, "legacy peers", "legacy P2P disabled (p2p.legacy_enabled: false)"));
    }
    let ntfry = &m["ntfry"];
    if ntfry["enabled"] == true {
        let n = ntfry["neighbors"].as_u64().unwrap_or(0);
        let relays = ntfry["homeRelays"].as_array().map(|a| a.len()).unwrap_or(0);
        out.push(check(if n == 0 { Level::Warn } else { Level::Ok }, "iroh peers", format!("{n} neighbours · {} known · {} gateways · {relays} home relay(s)", ntfry["known"], ntfry["gateways"])));
        let n0_home: Vec<&str> = ntfry["homeRelays"].as_array().map(|a| a.iter().filter_map(|u| u.as_str()).filter(|u| crate::p2p_iroh::relay_guard::is_n0_relay(u)).collect()).unwrap_or_default();
        if !n0_home.is_empty() {
            out.push(check(Level::Warn, "iroh relay", format!("home relay is a public n0 relay ({}) — rate-limited, wallets may time out; set p2p.iroh.relay_n0: false", n0_home.join(", "))));
        }
    } else {
        out.push(check(Level::Warn, "iroh peers", "iroh disabled (p2p.iroh.enabled: false) — finality votes and wallet stream unavailable"));
    }

    let forging = &m["forging"];
    if forging["enabled"] == true {
        let skipped = forging["skipped"].as_array().map(|a| a.len()).unwrap_or(0);
        let delegates: Vec<String> = forging["delegates"].as_array().map(|a| a.iter().map(|d| format!("{}{}", d["username"].as_str().unwrap_or("?"), d["rank"].as_u64().map(|r| format!(" #{r}")).unwrap_or_default())).collect()).unwrap_or_default();
        out.push(check(if skipped > 0 { Level::Warn } else { Level::Ok }, "forging", format!("{} · {} block(s) forged · {skipped} skipped slot(s) in the recent list", delegates.join(", "), forging["forgedCount"])));
        // SHIP-35: a forging node must sign votes for every tip; a stale own vote = the finality publisher is stuck
        let fin = &m["finality"];
        if ntfry["enabled"] == true && (fin["enabled"] == true || fin["finalizedHeight"].as_u64().unwrap_or(0) > 0) {
            let lag = fin["lag"].as_u64().unwrap_or(0);
            let failed = fin["broadcastFailed"].as_u64().unwrap_or(0);
            match fin["lastOwnVoteAge"].as_u64() {
                Some(age) if age as i64 <= 3 * bt => out.push(check(if lag > 21 { Level::Warn } else { Level::Ok }, "finality", format!("finalized {} · lag {lag} · {}/{} votes at tip · own votes signed {}s ago{}", fin["finalizedHeight"], fin["tipVotes"], fin["quorum"], age, if failed > 0 { format!(" · {failed} gossip broadcasts FAILED ({})", fin["lastError"].as_str().unwrap_or("")) } else { String::new() }))),
                Some(age) => out.push(check(Level::Fail, "finality", format!("own votes last signed {} ago while blocks keep arriving — finality publisher stuck (lag {lag}); {failed} broadcasts failed: {}", dur(age as i64), fin["lastError"].as_str().unwrap_or("-")))),
                None => out.push(check(Level::Warn, "finality", format!("no own votes signed since start (lag {lag}) — none of the loaded keys is in the active set?"))),
            }
        }
    }

    match &node["lastAudit"] {
        Value::Null => out.push(check(Level::Warn, "state audit", "never run — schedule `sth-core audit-all` nightly (node stopped)")),
        a => {
            let age = now_unix - a["atUnix"].as_i64().unwrap_or(0);
            let drifted = a["drifted"].as_u64().unwrap_or(0);
            let fixed = a["fixed"] == true;
            let level = if drifted > 0 && !fixed { Level::Fail } else if age > 2 * 86_400 { Level::Warn } else { Level::Ok };
            out.push(check(level, "state audit", format!("{} ago at height {} · {} wallets · {drifted} drifted{}{}", dur(age), a["height"], a["wallets"], if drifted > 0 { if fixed { " (repaired)" } else { " — run audit-all --fix" } } else { "" }, if age > 2 * 86_400 { " · older than 2 days" } else { "" })));
        }
    }
    let mp = &m["mempool"];
    let count = mp["count"].as_u64().unwrap_or(0);
    let max = mp["max"].as_u64().unwrap_or(1).max(1);
    out.push(check(if count * 10 > max * 9 { Level::Warn } else { Level::Ok }, "mempool", format!("{count} / {max} transactions")));
    out
}

pub fn verdict(checks: &[Check]) -> Level {
    checks.iter().map(|c| c.level).max().unwrap_or(Level::Ok)
}

pub fn verdict_label(level: Level) -> &'static str {
    match level {
        Level::Ok => "HEALTHY",
        Level::Warn => "DEGRADED",
        Level::Fail => "UNHEALTHY",
    }
}

/// `{ verdict, level, checks[] }` for `/api/ntfry/metrics.doctor` (the in-process part of `sth-core doctor`).
pub fn summary(metrics: &Value, now_unix: i64) -> Value {
    let checks = evaluate(metrics, now_unix);
    let level = verdict(&checks);
    serde_json::json!({ "verdict": verdict_label(level), "level": level, "checks": checks })
}

/// Run the full diagnosis against a running node. Returns the process exit code (0 ok, 1 warnings, 2 failures).
pub async fn run(cfg: &NodeConfig, public_ip: Option<String>, api_url: Option<String>) -> i32 {
    let mut checks = Vec::new();
    let host = if cfg.api.host == "0.0.0.0" || cfg.api.host.is_empty() { "127.0.0.1" } else { cfg.api.host.as_str() };
    // candidates: --api, the REST API, then the metrics listener (serves /api/ntfry/* too)
    let mut urls: Vec<String> = Vec::new();
    if let Some(u) = api_url {
        urls.push(u.trim_end_matches('/').to_string());
    } else {
        if cfg.api.enabled {
            urls.push(format!("http://{host}:{}", cfg.api.port));
        }
        if let Some((h, p)) = cfg.api.metrics_listen.trim().rsplit_once(':') {
            let h = if h == "0.0.0.0" || h.is_empty() { "127.0.0.1" } else { h };
            urls.push(format!("http://{h}:{p}"));
        }
    }
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(10)).no_proxy().user_agent("sth-core-doctor").build() {
        Ok(c) => c,
        Err(e) => {
            checks.push(check(Level::Fail, "node api", format!("http client: {e}")));
            return finish(checks);
        }
    };
    let now_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let mut failures = Vec::new();
    let mut got = None;
    for url in &urls {
        match client.get(format!("{url}/api/ntfry/metrics")).send().await {
            Ok(r) if r.status().is_success() => match r.json::<Value>().await {
                Ok(v) => {
                    got = Some((url.clone(), v));
                    break;
                }
                Err(e) => failures.push(format!("{url}: bad JSON ({e})")),
            },
            Ok(r) => failures.push(format!("{url}: HTTP {}", r.status())),
            Err(e) => failures.push(format!("{url}: {}", root_cause(&e))),
        }
    }
    match got {
        Some((url, v)) => {
            checks.push(check(Level::Ok, "node api", format!("{url} answers")));
            checks.extend(evaluate(&v["data"], now_unix));
        }
        None => checks.push(check(Level::Fail, "node api", format!("{} — is the node running (`pgrep -f 'sth-core run'`)? api.enabled / api.host in node.yaml? pass --api http://host:port", failures.join("; ")))),
    }

    let me = public_ip.or_else(|| cfg.p2p.legacy_public_addr.rsplit_once(':').map(|(ip, _)| ip.to_string())).filter(|s| !s.trim().is_empty());
    match me {
        Some(me) if cfg.p2p.legacy_enabled => {
            let port = cfg.p2p.legacy_port;
            let seeds = if cfg.network == "mainnet" { crate::p2p_legacy::fetch_peer_list(port).await } else { cfg.p2p.legacy_peers.clone() };
            let table = crate::p2p_legacy::PeerTable::new(port, seeds);
            let alive = table.refresh(16, Duration::from_secs(10)).await;
            let r = crate::p2p_legacy::acceptance_check(&table, &me, Duration::from_secs(10)).await;
            let pct = if r.asked > 0 { r.listed * 100 / r.asked } else { 0 };
            let level = if alive == 0 { Level::Warn } else if r.listed == 0 { Level::Fail } else if pct < 50 { Level::Warn } else { Level::Ok };
            checks.push(check(level, "legacy acceptance", if alive == 0 { "no legacy peer reachable from here".to_string() } else { format!("{me}:{port} listed by {} / {} peers ({pct}%){}", r.listed, r.asked, if r.listed == 0 { " — port open? legacy_public_addr correct? node ≥ 0.23.0?" } else { "" }) }));
        }
        _ => checks.push(check(Level::Warn, "legacy acceptance", "skipped: set p2p.legacy_public_addr (gateway) or pass --public-ip to check whether legacy nodes accept this node")),
    }
    finish(checks)
}

fn root_cause(e: &reqwest::Error) -> String {
    let mut cur: &dyn std::error::Error = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

fn finish(checks: Vec<Check>) -> i32 {
    for c in &checks {
        println!("{} {:<18} {}", c.level, c.name, c.detail);
    }
    let v = verdict(&checks);
    println!();
    println!("verdict: {}{}", verdict_label(v), match v { Level::Ok => "", Level::Warn => " (warnings above)", Level::Fail => " (failures above)" });
    match v {
        Level::Ok => 0,
        Level::Warn => 1,
        Level::Fail => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Value {
        json!({
            "node": { "implementation": "sth-core-pq", "version": "0.23.0", "uptime": 5000, "lastAudit": { "atUnix": 1_000_000, "height": 100, "wallets": 10, "drifted": 0, "fixed": false } },
            "chain": { "height": 100, "networkHeight": 100, "behind": 0, "blockTime": 8, "lastBlockAge": 5, "stall": { "stalled": false } },
            "legacy": { "enabled": true, "alive": 12, "known": 16, "parked": 0, "bestHeight": 100 },
            "ntfry": { "enabled": true, "neighbors": 4, "known": 6, "gateways": 2, "homeRelays": ["https://relay-fsn7.sth.cx/"] },
            "forging": { "enabled": false },
            "mempool": { "count": 3, "max": 5000 }
        })
    }

    #[test]
    fn healthy_node_is_ok() {
        let c = evaluate(&base(), 1_000_100);
        assert_eq!(verdict(&c), Level::Ok, "{c:?}");
    }

    #[test]
    fn stall_and_unrepaired_drift_fail_old_audit_warns() {
        let mut m = base();
        m["chain"]["stall"] = json!({ "stalled": true, "seconds": 40, "peersAhead": 5, "networkHeight": 110 });
        let c = evaluate(&m, 1_000_100);
        assert_eq!(c.iter().find(|c| c.name == "chain").unwrap().level, Level::Fail);
        let mut m = base();
        m["node"]["lastAudit"]["drifted"] = json!(3);
        let c = evaluate(&m, 1_000_100);
        assert_eq!(c.iter().find(|c| c.name == "state audit").unwrap().level, Level::Fail);
        m["node"]["lastAudit"]["fixed"] = json!(true);
        assert_eq!(evaluate(&m, 1_000_100).iter().find(|c| c.name == "state audit").unwrap().level, Level::Ok);
        assert_eq!(evaluate(&base(), 1_000_000 + 3 * 86_400).iter().find(|c| c.name == "state audit").unwrap().level, Level::Warn);
        let mut m = base();
        m["node"]["lastAudit"] = Value::Null;
        m["legacy"]["alive"] = json!(0);
        let c = evaluate(&m, 1_000_100);
        assert_eq!(c.iter().find(|c| c.name == "state audit").unwrap().level, Level::Warn);
        assert_eq!(verdict(&c), Level::Fail, "no legacy peer alive");
    }
}
