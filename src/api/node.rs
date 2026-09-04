//! Author: TechnoL0g
//!
//! Node / network resources: blockchain, node status, configuration (+crypto), fees, transaction
//! types & schemas, peers (legacy shape), `/api/node/peers` health dashboard, entities (empty).

use super::render::human_time;
use super::{last_height, not_found, paginate, slice_page, ApiResult, Params, Shared};
use crate::config::{FEE_NAMES, TOTAL_SUPPLY};
use crate::error::Error;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde_json::{json, Map, Value};
use std::time::Instant;

const SCHEMAS_JSON: &str = include_str!("schemas.json");

/// Legacy root: `GET /` → `{ "data": "Hello World!" }` (netfory-provider health probe).
pub async fn hello() -> ApiResult {
    Ok(Json(json!({ "data": "Hello World!" })))
}

pub async fn blockchain(State(st): State<Shared>) -> ApiResult {
    let last = st.storage.get_last_block()?.ok_or_else(|| not_found("Block"))?;
    Ok(Json(json!({ "data": { "block": { "height": last.height, "id": last.id }, "supply": TOTAL_SUPPLY.to_string() } })))
}

pub async fn status(State(st): State<Shared>) -> ApiResult {
    let h = last_height(&st)?;
    let network_height = st.peer_table.as_ref().map(|t| t.best_height()).unwrap_or(h).max(h);
    Ok(Json(json!({ "data": {
        "synced": network_height.saturating_sub(h) <= 1,
        "now": h,
        "blocksCount": network_height as i64 - h as i64,
        "timestamp": st.network.now_epoch(),
    } })))
}

pub async fn syncing(State(st): State<Shared>) -> ApiResult {
    let last = st.storage.get_last_block()?;
    let h = last.as_ref().map(|b| b.height).unwrap_or(0);
    let network_height = st.peer_table.as_ref().map(|t| t.best_height()).unwrap_or(h).max(h);
    Ok(Json(json!({ "data": {
        "syncing": network_height.saturating_sub(h) > 1,
        "blocks": network_height as i64 - h as i64,
        "height": h,
        "id": last.and_then(|b| b.id),
    } })))
}

pub async fn configuration(State(st): State<Shared>) -> ApiResult {
    let h = last_height(&st)?;
    let m = st.network.milestone(h.max(1));
    let fees: Map<String, Value> = m.fees.static_fees.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    let mut ports = Map::new();
    ports.insert("@smartholdem/core-api".into(), json!(4003));
    if st.peer_table.is_some() {
        ports.insert("@smartholdem/core-p2p".into(), json!(st.peer_table.as_ref().map(|t| t.port()).unwrap_or(4001)));
    }
    Ok(Json(json!({ "data": {
        "core": { "version": env!("CARGO_PKG_VERSION"), "implementation": "sth-core-rust" },
        "nethash": st.network.nethash,
        "slip44": st.network.slip44,
        "wif": st.network.wif,
        "token": st.network.token,
        "symbol": st.network.symbol,
        "explorer": st.network.explorer,
        "version": st.network.pubkey_hash,
        "ports": Value::Object(ports),
        "constants": {
            "height": m.height, "reward": m.reward.to_string(), "activeDelegates": m.active_delegates, "blocktime": m.blocktime,
            "block": { "version": m.block_version(), "idFullSha256": m.id_full_sha256(), "maxTransactions": m.max_transactions(), "maxPayload": m.max_payload() },
            "epoch": human_time(st.network.epoch_unix),
            "fees": { "staticFees": Value::Object(fees) },
            "vendorFieldLength": m.vendor_field_length, "multiPaymentLimit": m.multi_payment_limit, "htlcEnabled": m.htlc_enabled,
            "blockBurnAddress": m.block_burn_address, "aip11": m.aip11, "aip36": m.aip36, "aip37": m.aip37,
        },
        "transactionPool": {
            "dynamicFees": { "enabled": false },
            "maxTransactionsInPool": st.mempool.max_size(), "maxTransactionsPerSender": 150, "maxTransactionsPerRequest": 40,
            "maxTransactionAge": 2700, "maxTransactionBytes": 2_000_000,
        },
    } })))
}

/// `network.json` + `exceptions.json` + `milestones.json` + `genesisBlock.json` — exactly the crypto-networks files.
pub async fn configuration_crypto(State(st): State<Shared>) -> ApiResult {
    let genesis = crate::genesis::genesis_json(st.network.genesis_gz())?;
    Ok(Json(json!({ "data": {
        "network": *st.network.raw_network,
        "exceptions": *st.network.raw_exceptions,
        "milestones": *st.network.raw_milestones,
        "genesisBlock": genesis,
    } })))
}

fn static_fee_map(st: &Shared) -> Result<Map<String, Value>, Error> {
    let h = st.storage.get_last_height()?;
    Ok(st.network.static_fees(h).into_iter().map(|(name, _, fee)| (name.to_string(), json!(fee.to_string()))).collect())
}

pub async fn transaction_fees(State(st): State<Shared>) -> ApiResult {
    use crate::models::entity;
    let mut data = json!({ "1": Value::Object(static_fee_map(&st)?) });
    if st.network.milestone(last_height(&st)? + 1).aip36 {
        data["2"] = json!({ "entityRegistration": entity::FEE_REGISTER.to_string(), "entityUpdate": entity::FEE_UPDATE.to_string(), "entityResignation": entity::FEE_RESIGN.to_string() });
    }
    Ok(Json(json!({ "data": data })))
}

/// Fee statistics over the last `days` (1–30, default 7) from the chain (`avg/max/min/sum` per type).
pub async fn fees(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let days = q.parse::<u32>("days").unwrap_or(7).clamp(1, 30);
    let tip = last_height(&st)?;
    let since = st.network.now_epoch().saturating_sub(days * 86_400);
    let from = st.storage.height_at_or_after(since)?;
    let mut stats: std::collections::BTreeMap<(u32, u16), (u64, u64, u64, u64)> = Default::default();
    let mut h = from;
    while h <= tip {
        for b in st.storage.get_blocks_from(h, 1_000)? {
            for tx in &b.transactions {
                let e = stats.entry((tx.type_group, tx.type_)).or_insert((0, 0, u64::MAX, 0));
                e.0 += 1;
                e.1 = e.1.max(tx.fee);
                e.2 = e.2.min(tx.fee);
                e.3 += tx.fee;
            }
        }
        h += 1_000;
    }
    let mut data: Map<String, Value> = Map::new();
    for ((group, type_), (count, max, min, sum)) in stats {
        let name = FEE_NAMES.iter().find(|(_, t)| *t == type_).map(|(n, _)| n.to_string()).unwrap_or_else(|| type_.to_string());
        let entry = json!({ "avg": (sum / count.max(1)).to_string(), "max": max.to_string(), "min": min.to_string(), "sum": sum.to_string() });
        data.entry(group.to_string()).or_insert_with(|| Value::Object(Map::new()))[name] = entry;
    }
    Ok(Json(json!({ "meta": { "days": days }, "data": Value::Object(data) })))
}

pub async fn transaction_types(State(st): State<Shared>) -> ApiResult {
    let core: Map<String, Value> = [
        ("Transfer", 0), ("SecondSignature", 1), ("DelegateRegistration", 2), ("Vote", 3), ("MultiSignature", 4), ("Ipfs", 5),
        ("MultiPayment", 6), ("DelegateResignation", 7), ("HtlcLock", 8), ("HtlcClaim", 9), ("HtlcRefund", 10),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), json!(v)))
    .collect();
    // like legacy: business/bridgechain until aip36, Entity after it
    let aip36 = st.network.milestone(last_height(&st)? + 1).aip36;
    let magistrate: Map<String, Value> = if aip36 {
        [("Entity", json!(6))].into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    } else {
        [
            ("BusinessRegistration", 0), ("BusinessResignation", 1), ("BusinessUpdate", 2),
            ("BridgechainRegistration", 3), ("BridgechainResignation", 4), ("BridgechainUpdate", 5),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), json!(v)))
        .collect()
    };
    Ok(Json(json!({ "data": { "1": Value::Object(core), "2": Value::Object(magistrate) } })))
}

pub async fn transaction_schemas() -> ApiResult {
    let schemas: Value = serde_json::from_str(SCHEMAS_JSON).map_err(Error::Json)?;
    Ok(Json(json!({ "data": schemas })))
}

fn legacy_peer(ip: &str, port: u16, version: &str, height: u64, latency: u64) -> Value {
    json!({
        "ip": ip, "port": port,
        "ports": { "@smartholdem/core-api": 4003, "@smartholdem/core-webhooks": -1 },
        "version": version, "height": height, "latency": latency,
        "plugins": { "@smartholdem/core-api": { "port": 4003, "enabled": true, "estimateTotalCount": true } },
    })
}

fn peer_items(st: &Shared) -> Result<Vec<Value>, Error> {
    let h = st.storage.get_last_height()?;
    Ok(match &st.peer_table {
        Some(table) => table
            .snapshot()
            .into_iter()
            .filter(|p| p.successes > 0)
            .map(|p| legacy_peer(&p.ip, table.port(), &p.version, p.height, p.latency_ms))
            .collect(),
        None => st
            .peers
            .iter()
            .map(|p| legacy_peer(p.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/'), 4001, "3.8.2", h, 0))
            .collect(),
    })
}

pub async fn peers(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let mut items = peer_items(&st)?;
    if let Some(v) = q.get("version") {
        items.retain(|p| p["version"].as_str() == Some(v));
    }
    if let Some((field, asc)) = q.order() {
        items.sort_by(|a, b| {
            let (x, y) = (a[field].as_u64().unwrap_or(0), b[field].as_u64().unwrap_or(0));
            if asc { x.cmp(&y) } else { y.cmp(&x) }
        });
    }
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/peers", &q, total, page)))
}

pub async fn peer_by_ip(State(st): State<Shared>, Path(ip): Path<String>) -> ApiResult {
    let item = peer_items(&st)?.into_iter().find(|p| p["ip"].as_str() == Some(ip.as_str())).ok_or_else(|| not_found("Peer"))?;
    Ok(Json(json!({ "data": item })))
}

/// sth-core extension: live health table of every known peer (latency history, failures, ban state).
pub async fn peer_dashboard(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let now = Instant::now();
    let local = last_height(&st)?;
    let mut items: Vec<Value> = Vec::new();
    let mut meta: Map<String, Value> = Map::new();
    if let Some(iroh) = &st.iroh {
        meta.insert("irohEndpointId".into(), json!(iroh.id().to_string()));
        meta.insert("irohPeers".into(), json!(iroh.peers.len()));
        meta.insert("gateways".into(), json!(iroh.peers.gateways()));
        for p in iroh.peers.snapshot() {
            items.push(json!({
                "id": p.id.to_string(), "source": "iroh", "height": p.height, "latency": p.latency_ms, "gateway": p.gateway,
                "neighbor": p.neighbor, "messages": p.messages, "failures": p.failures,
                "lastSeen": now.saturating_duration_since(p.last_seen).as_secs(),
                "state": if p.neighbor { "neighbor" } else { "known" },
            }));
        }
    }
    let Some(table) = &st.peer_table else {
        let (total, page) = slice_page(items, &q);
        let mut out = paginate("/node/peers", &q, total, page);
        for (k, v) in meta {
            out["meta"][k] = v;
        }
        return Ok(Json(out));
    };
    let best = table.best_height();
    let legacy: Vec<Value> = table
        .snapshot()
        .into_iter()
        .map(|p| {
            let banned = p.is_banned(now);
            json!({
                "ip": p.ip, "port": table.port(), "source": "legacy", "version": p.version,
                "height": p.height, "lag": best.saturating_sub(p.height),
                "latency": p.latency_ms, "latencyHistory": p.latency_history,
                "score": p.score(best),
                "blocksLatency": p.blocks_latency_ms, "blocksLimit": p.blocks_limit(), "blocksFailures": p.blocks_failures,
                "blocksParkedFor": p.blocks_parked_until.filter(|t| *t > now).map(|t| t.saturating_duration_since(now).as_secs()),
                "successes": p.successes, "failures": p.failures, "totalFailures": p.total_failures,
                "lastSeen": p.last_ok.map(|t| now.saturating_duration_since(t).as_secs()),
                "state": if banned { "banned" } else if p.successes == 0 { "unknown" } else { "ok" },
                "bannedFor": p.banned_until.filter(|_| banned).map(|t| t.saturating_duration_since(now).as_secs()),
            })
        })
        .collect();
    items.extend(legacy);
    let (total, page) = slice_page(items, &q);
    let mut out = paginate("/node/peers", &q, total, page);
    for (k, v) in meta {
        out["meta"][k] = v;
    }
    out["meta"]["alive"] = json!(table.alive());
    out["meta"]["known"] = json!(table.len());
    out["meta"]["bestHeight"] = json!(best);
    out["meta"]["localHeight"] = json!(local);
    Ok(Json(out))
}

/// sth-core extension: delegate module state (delegates, next slot, last forged block, skipped slots).
pub async fn forging(State(st): State<Shared>) -> ApiResult {
    let Some(status) = &st.forging else {
        return Ok(Json(json!({ "data": { "enabled": false } })));
    };
    let snapshot = status.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut data = serde_json::to_value(snapshot).map_err(Error::Json)?;
    data["enabled"] = json!(true);
    data["height"] = json!(last_height(&st)?);
    Ok(Json(json!({ "data": data })))
}

