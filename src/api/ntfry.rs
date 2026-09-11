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
        "mempool": { "count": st.mempool.len().await, "max": st.mempool.max_size() },
        "intake": crate::intake::snapshot(),
        "legacy": legacy,
        "ntfry": iroh,
        "forging": forging,
        "blocks": blocks,
    } })))
}

/// Router for the dedicated metrics port: the page plus the two `ntfry` endpoints only.
pub fn metrics_router(state: Shared) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/api/ntfry/peers", get(peers))
        .route("/api/ntfry/metrics", get(metrics))
        .fallback(|| async { super::not_found_plain() })
        .with_state(state)
}
