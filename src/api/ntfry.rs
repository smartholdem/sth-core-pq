//! Author: TechnoL0g
//!
//! sth-core extensions for the Web 4.0 (netfory / iroh) layer and the operator metrics page:
//! `GET /api/ntfry/peers` (iroh peers by EndpointId only — no IP addresses), `GET /api/ntfry/metrics`
//! (one JSON snapshot of the node for dashboards) and the self-contained HTML page served at `/`
//! when `api.page_metrics` is on (or on its own port through `api.metrics_listen`).

use super::render::block_json;
use super::{last_height, node, paginate, slice_page, ApiResult, Params, Shared};
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Instant;

const PAGE: &str = include_str!("metrics.html");

/// `/` — the metrics page when enabled on this listener, otherwise the legacy `Hello World!`.
pub async fn root(State(st): State<Shared>) -> Response {
    if st.page_metrics {
        Html(PAGE).into_response()
    } else {
        node::hello().await.into_response()
    }
}

pub async fn page() -> Html<&'static str> {
    Html(PAGE)
}

fn iroh_peer_json(p: &crate::p2p_iroh::IrohPeerInfo, now: Instant) -> Value {
    json!({
        "nodeId": p.id.to_string(), "height": p.height, "latency": p.latency_ms,
        "neighbor": p.neighbor, "gateway": p.gateway.is_some(), "messages": p.messages, "failures": p.failures,
        "lastSeen": now.saturating_duration_since(p.last_seen).as_secs(),
        "state": if p.neighbor { "neighbor" } else { "known" },
    })
}

fn iroh_meta(st: &Shared) -> Value {
    match &st.iroh {
        Some(iroh) => {
            let peers = iroh.peers.snapshot();
            json!({
                "enabled": true, "nodeId": iroh.id().to_string(),
                "gateway": iroh.gateway,
                "relays": iroh.relay_urls,
                "homeRelays": iroh.connected_relays(),
                "neighbors": peers.iter().filter(|p| p.neighbor).count(), "known": peers.len(),
                "gateways": peers.iter().filter(|p| p.gateway.is_some()).count(),
                "bestHeight": iroh.peers.best_height(),
            })
        }
        None => json!({ "enabled": false }),
    }
}

/// Web 4.0 peers by iroh EndpointId (never IPs), neighbours first.
pub async fn peers(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let now = Instant::now();
    let items: Vec<Value> = st.iroh.as_ref().map(|i| i.peers.snapshot().iter().map(|p| iroh_peer_json(p, now)).collect()).unwrap_or_default();
    let (total, page) = slice_page(items, &q);
    let mut out = paginate("/ntfry/peers", &q, total, page);
    let meta = iroh_meta(&st);
    for (k, v) in meta.as_object().into_iter().flatten() {
        out["meta"][k] = v.clone();
    }
    Ok(Json(out))
}

/// One snapshot with everything the metrics page shows.
/// Active delegates (top 21 by votes) with the implementation they run: `rust` = proven by a signed announce,
/// `legacy` = inferred (their recent blocks reached us only through legacy peers that are not Rust gateways), else `unknown`.
fn delegates_json(st: &Shared) -> Result<Value, crate::error::Error> {
    let tip = st.storage.get_last_height()?;
    let ms = st.network.milestone(tip.max(1));
    let active = ms.active_delegates as usize;
    let round = crate::delegate::round::round_info(tip + 1, ms.active_delegates as u64).round;
    let slashing = ms.finality.slashing;
    let min_version = ms.min_core_version.as_str();
    let known = st.iroh.as_ref().map(|i| i.rust_delegates.snapshot()).unwrap_or_default();
    // legacy-protocol addresses that are actually Rust nodes (gateways announced over iroh + this node)
    let mut rust_ws: std::collections::HashSet<String> = st
        .iroh
        .as_ref()
        .map(|i| i.peers.gateways().into_iter().filter_map(|g| g.split(':').next().map(str::to_string)).collect())
        .unwrap_or_default();
    if let Some(gw) = st.iroh.as_ref().and_then(|i| i.gateway.clone()).and_then(|g| g.split(':').next().map(str::to_string)) {
        rust_ws.insert(gw);
    }
    let seen = crate::intake::legacy_evidence(&rust_ws);
    let mut list = Vec::new();
    for (rank, (w, votes)) in st.storage.delegate_ranking()?.into_iter().filter(|(w, _)| !w.resigned).take(active).enumerate() {
        let pk = w.public_key.clone().unwrap_or_default();
        let info = known.get(&pk);
        let pq_key = w.pq_key.is_some();
        let proofs = st.storage.equivocation_history(&pk)?;
        let latest_ban = proofs.iter().map(|p| p.banned_until_round).max();
        let (observed, legacy_only) = seen.get(&pk).copied().unwrap_or((0, false));
        let implementation = if info.is_some() {
            "rust"
        } else if observed >= 2 && legacy_only {
            "legacy"
        } else {
            "unknown"
        };
        list.push(json!({
            "rank": rank + 1,
            "username": w.username,
            "publicKey": pk,
            "votes": votes.to_string(),
            "implementation": implementation,
            "inferred": implementation == "legacy",
            "observed": observed,
            "version": info.map(|i| i.version.clone()),
            "outdated": info.is_some_and(|i| version_below(&i.version, min_version)),
            "node": info.map(|i| i.node.fmt_short().to_string()),
            "lastSeen": info.map(|i| i.last_seen.elapsed().as_secs()),
            "pqKey": pq_key,
            "doubleVotes": proofs.len(),
            "bannedUntilRound": latest_ban,
            "banned": slashing && latest_ban.is_some_and(|r| r > round),
        }));
    }
    let count = |k: &str| list.iter().filter(|d| d["implementation"] == k).count();
    let outdated = list.iter().filter(|d| d["outdated"] == true).count();
    let pq_blocks_at = st.network.pq_blocks_activation_height();
    let grace_end = pq_blocks_at.map(|h| h + st.network.milestone(h).pq.blocks_grace);
    Ok(json!({ "active": active, "rust": count("rust"), "legacy": count("legacy"), "unknown": count("unknown"), "outdated": outdated, "minCoreVersion": min_version,
        "pqKeys": list.iter().filter(|d| d["pqKey"] == true).count(),
        "pqBlocks": { "active": ms.pq.blocks, "activation": pq_blocks_at, "graceEnd": grace_end, "blocksLeft": grace_end.map(|g| g.saturating_sub(tip)) },
        "slashing": { "enabled": slashing, "rounds": ms.finality.slashing_rounds(), "currentRound": round },
        "equivocations": equivocations_json(st, tip)?,
        "list": list }))
}

/// `a < b` for dotted numeric versions ("0.9.10" < "0.14.0"); non-numeric parts count as 0.
pub fn version_below(a: &str, b: &str) -> bool {
    if b.is_empty() {
        return false;
    }
    let parse = |v: &str| -> Vec<u64> { v.trim_start_matches('v').split(['.', '-', '+']).take(3).map(|p| p.parse().unwrap_or(0)).collect() };
    let (mut x, mut y) = (parse(a), parse(b));
    x.resize(3, 0);
    y.resize(3, 0);
    x < y
}

/// Open sale orders (sobjV2 market), newest registration id order of the index, `page`/`limit` ≤ 100.
pub async fn market(State(st): State<Shared>, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let limit = q.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(100).clamp(1, 100);
    let page = q.get("page").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).max(1);
    let type_filter = q.get("type").and_then(|v| v.parse::<u8>().ok());
    let (orders, total) = st.storage.market_orders((page - 1) * limit, limit)?;
    let tip = last_height(&st)?;
    let items: Vec<Value> = orders
        .into_iter()
        .filter(|(_, _, rec)| type_filter.is_none_or(|t| rec.type_ == t))
        .map(|(id, owner, rec)| {
            let token = st.storage.token_state(&id).ok().flatten();
            json!({
                "id": id, "type": rec.type_, "subType": rec.sub_type, "name": rec.data.name, "ntfryData": rec.data.ntfry_data,
                "owner": owner, "price": rec.price.unwrap_or(0).to_string(),
                "token": token.map(|t| json!({ "symbol": t.symbol, "decimals": t.decimals, "supply": t.supply.to_string(), "supplyCap": t.supply_cap.to_string(),
                    "logoUrl": t.meta.as_ref().filter(|m| m.logo.is_some()).map(|_| format!("/api/tokens/{id}/logo")), "name": t.meta.as_ref().map(|m| m.name.clone()) })),
            })
        })
        .collect();
    let ms = st.network.milestone(tip + 1);
    Ok(Json(json!({ "meta": { "page": page, "limit": limit, "totalCount": total, "pageCount": total.div_ceil(limit).max(1) },
        "data": { "active": ms.sobj_v2, "fees": { "sell": crate::models::sobj::FEE_SELL, "buy": crate::models::sobj::FEE_BUY }, "orders": items } })))
}

/// `GET /api/ntfry/finality` — SHIP-35 state: highest certificate, lag, votes at the tip, recent heights.
pub async fn finality(State(st): State<Shared>) -> ApiResult {
    Ok(Json(json!({ "data": finality_json(&st)? })))
}

fn finality_json(st: &Shared) -> Result<Value, crate::error::Error> {
    let tip = st.storage.get_last_height()?;
    let ms = st.network.milestone(tip.max(1));
    Ok(match &st.iroh {
        Some(i) => {
            let mut v = serde_json::to_value(i.finality.snapshot()).map_err(crate::error::Error::Json)?;
            v["enabled"] = json!(true);
            v["latestCertificate"] = json!(st.storage.latest_finality_cert()?.map(|c| json!({ "height": c.height, "blockId": c.block_id, "votes": c.votes.len(), "delegates": c.votes.keys().collect::<Vec<_>>() })));
            v["slashing"] = json!({ "enabled": ms.finality.slashing, "rounds": ms.finality.slashing_rounds() });
            v["equivocations"] = equivocations_json(st, tip)?;
            v
        }
        None => {
            let cert = st.storage.latest_finality_cert()?;
            json!({ "enabled": false, "finalizedHeight": cert.as_ref().map(|c| c.height).unwrap_or(0), "finalizedId": cert.map(|c| c.block_id), "lag": tip, "quorum": ms.finality_quorum(), "activeDelegates": ms.active_delegates, "hard": ms.finality.active, "tipVotes": 0,
                "slashing": { "enabled": ms.finality.slashing, "rounds": ms.finality.slashing_rounds() }, "equivocations": equivocations_json(st, tip)? })
        }
    })
}

/// Proven double votes (SHIP-35): who, at which height, and whether the exclusion is in force right now.
fn equivocations_json(st: &Shared, tip: u64) -> Result<Value, crate::error::Error> {
    let ms = st.network.milestone(tip.max(1));
    let round = crate::delegate::round::round_info(tip + 1, ms.active_delegates as u64).round;
    let mut out = Vec::new();
    for p in st.storage.equivocations()? {
        let username = st.storage.find_wallet(&p.public_key)?.and_then(|w| w.username);
        out.push(json!({ "publicKey": p.public_key, "username": username, "height": p.height, "blockIds": p.block_ids, "detectedHeight": p.detected_height,
            "bannedUntilRound": p.banned_until_round, "banned": ms.finality.slashing && p.banned_until_round > round, "roundsLeft": p.banned_until_round.saturating_sub(round) }));
    }
    Ok(Value::Array(out))
}

/// `GET /api/ntfry/delegates` — progress of the Rust rollout across the active delegate set.
pub async fn delegates(State(st): State<Shared>) -> ApiResult {
    Ok(Json(json!({ "data": delegates_json(&st)? })))
}

pub async fn metrics(State(st): State<Shared>) -> ApiResult {
    let now = Instant::now();
    let tip = last_height(&st)?;
    let last = st.storage.get_last_block()?;
    let ms = st.network.milestone(tip.max(1));
    let recent = st.storage.get_blocks(0, 12)?;
    let blocks: Vec<Value> = recent.iter().map(|b| block_json(&st, b, false, tip)).collect::<Result<_, _>>()?;
    let now_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let last_unix = last.as_ref().map(|b| st.network.epoch_to_unix(b.timestamp));
    let blocks_per_min = match (recent.first(), recent.last()) {
        (Some(a), Some(b)) if a.height > b.height && a.timestamp > b.timestamp => {
            Some((a.height - b.height) as f64 * 60.0 / (a.timestamp - b.timestamp) as f64)
        }
        _ => None,
    };
    let legacy = match &st.peer_table {
        Some(t) => {
            let snap = t.snapshot();
            json!({
                "enabled": true, "port": t.port(), "alive": t.alive(), "known": snap.len(),
                "banned": snap.iter().filter(|p| p.is_banned(now)).count(),
                "parked": snap.iter().filter(|p| p.is_blocks_parked(now)).count(),
                "bestHeight": t.best_height(),
                "peers": snap.iter().take(40).map(|p| json!({
                    "ip": p.ip, "version": p.version, "height": p.height, "latency": p.latency_ms,
                    "blocksLatency": p.blocks_latency_ms, "blocksLimit": p.blocks_limit(),
                    "state": if p.is_banned(now) { "banned" } else if p.is_blocks_parked(now) { "parked" } else if p.successes == 0 { "unknown" } else { "ok" },
                })).collect::<Vec<_>>(),
            })
        }
        None => json!({ "enabled": false }),
    };
    let mut iroh = iroh_meta(&st);
    if let Some(i) = &st.iroh {
        iroh["peers"] = Value::Array(i.peers.snapshot().iter().take(40).map(|p| iroh_peer_json(p, now)).collect());
    }
    let legacy_best = st.peer_table.as_ref().map(|t| t.best_height()).unwrap_or(0);
    let iroh_best = st.iroh.as_ref().map(|i| i.peers.best_height()).unwrap_or(0);
    let network_height = legacy_best.max(iroh_best).max(tip);
    let df = st.mempool.dynamic_fees();
    let forging = match &st.forging {
        Some(s) => {
            let snap = s.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let mut v = serde_json::to_value(snap).map_err(crate::error::Error::Json)?;
            v["enabled"] = json!(true);
            v
        }
        None => json!({ "enabled": false }),
    };
    Ok(Json(json!({ "data": {
        "node": {
            "version": env!("CARGO_PKG_VERSION"), "implementation": "sth-core-rust",
            "uptime": st.started.elapsed().as_secs(), "nethash": st.network.nethash, "now": now_unix,
            "rssBytes": crate::mem::rss_bytes(),
            "pqCommitments": st.storage.pq_commitment_count(), "pqActive": st.storage.pq_key_count(), "pqStage": if ms.pq.blocks { "C" } else if st.network.milestone(tip + 1).pq.active { "B" } else { "A" },
            "pqBlocks": ms.pq.blocks, "lastBlockVersion": last.as_ref().map(|b| b.version),
        },
        "chain": {
            "height": tip, "id": last.as_ref().and_then(|b| b.id.clone()),
            "lastBlockAge": last_unix.map(|u| (now_unix - u).max(0)),
            "networkHeight": network_height, "behind": network_height.saturating_sub(tip),
            "syncing": network_height.saturating_sub(tip) > 1, "blocksPerMin": blocks_per_min,
            "blockTime": ms.blocktime,
            "multiPaymentLimit": ms.multi_payment_limit,
            "fees": { "transfer": ms.static_fee("transfer"), "multiPayment": ms.static_fee("multiPayment") },
        },
        "finality": finality_json(&st)?,
        "mempool": { "count": st.mempool.len().await, "max": st.mempool.max_size(), "bytes": st.mempool.bytes(), "maxBytes": st.mempool.max_bytes(),
            "dynamicFees": { "enabled": df.enabled, "minFeePool": df.min_fee_pool, "minFeeBroadcast": df.min_fee_broadcast, "addonBytes": df.addon_bytes, "minTransferFee": st.mempool.min_transfer_fee(), "transferBytes": crate::mempool::Mempool::TRANSFER_WIRE_BYTES, "lowFeeRejected": st.mempool.low_fee_rejected() } },
        "intake": crate::intake::snapshot(),
        "legacy": legacy,
        "ntfry": iroh,
        "forging": forging,
        "blocks": blocks,
        "delegates": delegates_json(&st)?,
    } })))
}

/// Router for the dedicated metrics port: the page plus the two `ntfry` endpoints only.
/// Token explorer feed for the metrics page: every token with its manifest + the latest issues (TokenInit order).
pub async fn tokens(State(st): State<Shared>) -> ApiResult {
    let tip = last_height(&st)?;
    let ms = st.network.milestone(tip + 1);
    let mut list: Vec<(String, crate::storage::TokenState)> = st.storage.all_tokens()?;
    list.sort_by(|a, b| b.1.init_height.cmp(&a.1.init_height).then_with(|| a.1.symbol.cmp(&b.1.symbol)));
    let items: Vec<Value> = list
        .iter()
        .map(|(id, t)| {
            let mut v = super::tokens::token_json(&st, id, t);
            let ts = st.storage.get_block_by_height(t.init_height).ok().flatten().map(|b| st.network.epoch_to_unix(b.timestamp));
            v["initUnix"] = json!(ts);
            v["confirmations"] = json!(tip.saturating_sub(t.init_height));
            v
        })
        .collect();
    let with_logo = items.iter().filter(|v| v["meta"]["logoUrl"].is_string()).count();
    Ok(Json(json!({ "data": {
        "active": ms.tokens,
        "count": items.len(),
        "withManifest": items.iter().filter(|v| !v["meta"].is_null()).count(),
        "withLogo": with_logo,
        "fees": { "init": ms.token_fees.init, "meta": ms.token_fees.meta, "transfer": ms.token_fees.transfer, "transferPerRecipient": ms.token_fees.transfer_per_recipient, "mint": ms.token_fees.mint, "burn": ms.token_fees.burn },
        "maxRecipients": ms.token_transfer_max_recipients,
        "maxLogoBytes": crate::models::token::META_MAX_LOGO,
        "tokens": items,
    } })))
}

pub fn metrics_router(state: Shared) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/api/ntfry/peers", get(peers))
        .route("/api/ntfry/delegates", get(delegates))
        .route("/api/ntfry/metrics", get(metrics))
        .route("/api/ntfry/finality", get(finality))
        .route("/api/ntfry/tokens", get(tokens))
        .route("/api/ntfry/market", get(market))
        .route("/api/tokens/:key/logo", get(super::tokens::logo))
        .fallback(|| async { super::not_found_plain() })
        .with_state(state)
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_compare() {
        use super::version_below;
        assert!(version_below("0.10.0", "0.14.0"));
        assert!(version_below("0.9.10", "0.14.0"));
        assert!(version_below("0.14.0-rc1", "0.14.1"));
        assert!(!version_below("0.14.0", "0.14.0"));
        assert!(!version_below("1.0.0", "0.14.0"));
        assert!(!version_below("0.10.0", ""), "no requirement");
        assert!(version_below("v0.13.1", "0.14.0"));
    }
}
