//! Author: TechnoL0g
//!
//! Local REST API (axum) on 127.0.0.1:4003 — drop-in replacement for the legacy
//! `@smartholdem/core-api` JSON so wallets, explorers and netfory-provider keep working.
//! Every resource is served in the legacy "transformed" shape; `?transform=false` returns raw core objects.

use crate::config::{Network, STATIC_FEES, TOTAL_SUPPLY};
use crate::error::Error;
use crate::mempool::Mempool;
use crate::models::{Block, Transaction};
use crate::storage::{Storage, WalletState};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct AppState {
    pub storage: Arc<Storage>,
    pub mempool: Arc<Mempool>,
    pub network: Network,
    pub peers: Vec<String>,
    pub started: Instant,
    delegates_cache: Mutex<Option<(u64, Vec<Value>)>>,
}

impl AppState {
    pub fn new(storage: Arc<Storage>, mempool: Arc<Mempool>, peers: Vec<String>) -> Self {
        let network = storage.network().clone();
        Self { storage, mempool, network, peers, started: Instant::now(), delegates_cache: Mutex::new(None) }
    }
}

type Shared = Arc<AppState>;
type Params = HashMap<String, String>;

/// Legacy error envelope `{ statusCode, error, message }`.
pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "statusCode": self.0.as_u16(),
            "error": self.0.canonical_reason().unwrap_or("Error"),
            "message": self.1
        });
        (self.0, Json(body)).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

fn not_found(what: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("{what} not found"))
}

type ApiResult = std::result::Result<Json<Value>, ApiError>;

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/api/blockchain", get(blockchain))
        .route("/api/node/status", get(node_status))
        .route("/api/node/syncing", get(node_syncing))
        .route("/api/node/configuration", get(node_configuration))
        .route("/api/node/fees", get(node_fees))
        .route("/api/transactions/fees", get(transaction_fees))
        .route("/api/peers", get(peers))
        .route("/api/blocks", get(blocks))
        .route("/api/blocks/first", get(block_first))
        .route("/api/blocks/last", get(block_last))
        .route("/api/blocks/:id", get(block_by_id))
        .route("/api/blocks/:id/transactions", get(block_transactions))
        .route("/api/transactions", get(transactions).post(post_transactions))
        .route("/api/transactions/unconfirmed", get(unconfirmed))
        .route("/api/transactions/unconfirmed/:id", get(unconfirmed_by_id))
        .route("/api/transactions/:id", get(transaction_by_id))
        .route("/api/wallets", get(wallets))
        .route("/api/wallets/:id", get(wallet_by_id))
        .route("/api/wallets/:id/transactions", get(wallet_transactions))
        .route("/api/wallets/:id/transactions/sent", get(wallet_transactions_sent))
        .route("/api/wallets/:id/transactions/received", get(wallet_transactions_received))
        .route("/api/delegates", get(delegates))
        .route("/api/delegates/:id", get(delegate_by_id))
        .route("/api/delegates/:id/voters", get(delegate_voters))
        .route("/api/delegates/:id/blocks", get(delegate_blocks))
        .with_state(state)
}

/// Bind and serve until the process exits.
pub async fn serve(state: Shared, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "local REST API listening");
    serve_listener(state, listener).await
}

pub async fn serve_listener(state: Shared, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

// ------------------------------------------------------------------ helpers

fn page_params(q: &Params) -> (usize, usize) {
    let limit = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(100usize).clamp(1, 100);
    let page = q.get("page").and_then(|v| v.parse().ok()).unwrap_or(1usize).max(1);
    (page, limit)
}

fn transform(q: &Params) -> bool {
    q.get("transform").map(|v| v != "false").unwrap_or(true)
}

fn paginate(path: &str, q: &Params, total: usize, data: Vec<Value>) -> Value {
    let (page, limit) = page_params(q);
    let page_count = total.div_ceil(limit).max(1);
    let link = |p: usize| -> Value {
        let mut parts: Vec<String> = q
            .iter()
            .filter(|(k, _)| k.as_str() != "page")
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        parts.sort();
        parts.push(format!("page={p}"));
        parts.insert(0, format!("limit={limit}"));
        parts.dedup();
        Value::String(format!("{path}?{}", parts.join("&")))
    };
    json!({
        "meta": {
            "totalCountIsEstimate": false,
            "count": data.len(),
            "pageCount": page_count,
            "totalCount": total,
            "next": if page < page_count { link(page + 1) } else { Value::Null },
            "previous": if page > 1 { link(page - 1) } else { Value::Null },
            "self": link(page),
            "first": link(1),
            "last": link(page_count),
        },
        "data": data
    })
}

fn slice_page<T>(items: Vec<T>, q: &Params) -> (usize, Vec<T>) {
    let (page, limit) = page_params(q);
    let total = items.len();
    (total, items.into_iter().skip((page - 1) * limit).take(limit).collect())
}

/// `1788383912` → `2026-09-02T21:18:32.000Z`
pub fn human_time(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z", secs / 3600, (secs % 3600) / 60, secs % 60)
}

fn timestamp_json(net: &Network, epoch: u32) -> Value {
    let unix = net.epoch_to_unix(epoch);
    json!({ "epoch": epoch, "unix": unix, "human": human_time(unix) })
}

fn last_height(st: &AppState) -> Result<u64, ApiError> {
    Ok(st.storage.get_last_height()?)
}

fn block_json(st: &AppState, block: &Block, raw: bool, tip: u64) -> Result<Value, ApiError> {
    if raw {
        return Ok(serde_json::to_value(block.header()).map_err(Error::Json)?);
    }
    let generator = st.storage.find_wallet(&block.generator_public_key)?;
    let address = generator
        .as_ref()
        .map(|w| w.address.clone())
        .or_else(|| crate::crypto::address_from_public_key(&block.generator_public_key, st.network.pubkey_hash).ok())
        .unwrap_or_default();
    let mut gen = Map::new();
    if let Some(u) = generator.as_ref().and_then(|w| w.username.clone()) {
        gen.insert("username".into(), Value::String(u));
    }
    gen.insert("address".into(), Value::String(address));
    gen.insert("publicKey".into(), Value::String(block.generator_public_key.clone()));
    Ok(json!({
        "id": block.id,
        "version": block.version,
        "height": block.height,
        "previous": block.previous_block,
        "forged": {
            "reward": block.reward.to_string(),
            "fee": block.total_fee.to_string(),
            "amount": block.total_amount.to_string(),
            "total": (block.reward + block.total_fee).to_string(),
        },
        "payload": { "hash": block.payload_hash, "length": block.payload_length },
        "generator": Value::Object(gen),
        "signature": block.block_signature,
        "confirmations": tip.saturating_sub(block.height),
        "transactions": block.number_of_transactions,
        "timestamp": timestamp_json(&st.network, block.timestamp),
    }))
}

fn tx_json(st: &AppState, tx: &Transaction, block_ts: Option<u32>, raw: bool, tip: u64) -> Result<Value, ApiError> {
    if raw {
        return Ok(serde_json::to_value(tx).map_err(Error::Json)?);
    }
    let sender = crate::crypto::address_from_public_key(&tx.sender_public_key, st.network.pubkey_hash).unwrap_or_default();
    let mut v = Map::new();
    v.insert("id".into(), json!(tx.id));
    if let Some(b) = &tx.block_id {
        v.insert("blockId".into(), json!(b));
    }
    v.insert("version".into(), json!(tx.version));
    v.insert("type".into(), json!(tx.type_));
    v.insert("typeGroup".into(), json!(tx.type_group));
    v.insert("amount".into(), json!(tx.amount.to_string()));
    v.insert("fee".into(), json!(tx.fee.to_string()));
    v.insert("sender".into(), json!(sender));
    v.insert("senderPublicKey".into(), json!(tx.sender_public_key));
    if let Some(r) = &tx.recipient_id {
        v.insert("recipient".into(), json!(r));
    }
    v.insert("signature".into(), json!(tx.signature));
    if let Some(s) = tx.second_signature_any() {
        v.insert("signSignature".into(), json!(s));
    }
    if let Some(s) = &tx.signatures {
        v.insert("signatures".into(), json!(s));
    }
    if let Some(vf) = &tx.vendor_field {
        v.insert("vendorField".into(), json!(vf));
    }
    if let Some(a) = &tx.asset {
        v.insert("asset".into(), serde_json::to_value(a).map_err(Error::Json)?);
    }
    match (tx.block_height, block_ts) {
        (Some(h), Some(ts)) => {
            v.insert("confirmations".into(), json!(tip.saturating_sub(h) + 1));
            v.insert("timestamp".into(), timestamp_json(&st.network, ts));
        }
        _ => {
            v.insert("confirmations".into(), json!(0));
        }
    }
    v.insert("nonce".into(), json!(tx.nonce.unwrap_or(0).to_string()));
    Ok(Value::Object(v))
}

fn wallet_json(st: &AppState, w: &WalletState, rank: Option<usize>, votes: Option<u64>) -> Value {
    let mut attrs = Map::new();
    if let Some(v) = &w.vote {
        attrs.insert("vote".into(), json!(v));
    }
    if let Some(s) = &w.second_public_key {
        attrs.insert("secondPublicKey".into(), json!(s));
    }
    if let Some(u) = &w.username {
        let mut d = Map::new();
        d.insert("username".into(), json!(u));
        d.insert("voteBalance".into(), json!(votes.unwrap_or(0).to_string()));
        d.insert("forgedFees".into(), json!(w.forged_fees.to_string()));
        d.insert("forgedRewards".into(), json!(w.forged_rewards.to_string()));
        d.insert("producedBlocks".into(), json!(w.produced_blocks));
        if let Some(r) = rank {
            d.insert("rank".into(), json!(r));
        }
        if let Some(lb) = &w.last_block {
            d.insert("lastBlock".into(), json!({ "id": lb.id, "height": lb.height, "timestamp": timestamp_json(&st.network, lb.timestamp) }));
        }
        attrs.insert("delegate".into(), Value::Object(d));
    }
    json!({
        "address": w.address,
        "publicKey": w.public_key,
        "balance": w.balance.to_string(),
        "nonce": w.nonce.to_string(),
        "attributes": Value::Object(attrs),
    })
}

/// Delegates ranked by vote weight (sum of voters' balances), cached per chain height.
fn ranked_delegates(st: &AppState) -> Result<Vec<Value>, ApiError> {
    let tip = last_height(st)?;
    if let Ok(cache) = st.delegates_cache.lock() {
        if let Some((h, list)) = cache.as_ref() {
            if *h == tip {
                return Ok(list.clone());
            }
        }
    }
    let wallets = st.storage.all_wallets()?;
    let mut votes: HashMap<String, u64> = HashMap::new();
    for w in &wallets {
        if let Some(pk) = &w.vote {
            *votes.entry(pk.clone()).or_default() += w.balance.max(0) as u64;
        }
    }
    let mut delegates: Vec<&WalletState> = wallets.iter().filter(|w| w.is_delegate()).collect();
    delegates.sort_by(|a, b| {
        let va = a.public_key.as_ref().and_then(|p| votes.get(p)).copied().unwrap_or(0);
        let vb = b.public_key.as_ref().and_then(|p| votes.get(p)).copied().unwrap_or(0);
        vb.cmp(&va).then_with(|| a.public_key.cmp(&b.public_key))
    });
    let list: Vec<Value> = delegates
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let v = w.public_key.as_ref().and_then(|p| votes.get(p)).copied().unwrap_or(0);
            let approval = (v as f64 / TOTAL_SUPPLY as f64 * 10_000.0).round() / 100.0;
            let last = w.last_block.as_ref().map(|lb| json!({ "id": lb.id, "height": lb.height, "timestamp": timestamp_json(&st.network, lb.timestamp) }));
            json!({
                "username": w.username,
                "address": w.address,
                "publicKey": w.public_key,
                "votes": v.to_string(),
                "rank": i + 1,
                "isResigned": false,
                "blocks": { "produced": w.produced_blocks, "last": last },
                "production": { "approval": approval },
                "forged": {
                    "fees": w.forged_fees.to_string(),
                    "rewards": w.forged_rewards.to_string(),
                    "total": (w.forged_fees + w.forged_rewards).to_string(),
                },
            })
        })
        .collect();
    if let Ok(mut cache) = st.delegates_cache.lock() {
        *cache = Some((tip, list.clone()));
    }
    Ok(list)
}

fn find_delegate(st: &AppState, id: &str) -> Result<Option<Value>, ApiError> {
    let list = ranked_delegates(st)?;
    Ok(list.into_iter().find(|d| {
        d["username"].as_str() == Some(id) || d["address"].as_str() == Some(id) || d["publicKey"].as_str() == Some(id)
    }))
}

// ------------------------------------------------------------------ node

async fn blockchain(State(st): State<Shared>) -> ApiResult {
    let last = st.storage.get_last_block()?.ok_or_else(|| not_found("Block"))?;
    Ok(Json(json!({ "data": { "block": { "height": last.height, "id": last.id }, "supply": TOTAL_SUPPLY.to_string() } })))
}

async fn node_status(State(st): State<Shared>) -> ApiResult {
    let h = last_height(&st)?;
    Ok(Json(json!({ "data": { "synced": true, "now": h, "blocksCount": -1, "timestamp": st.network.now_epoch() } })))
}

async fn node_syncing(State(st): State<Shared>) -> ApiResult {
    let last = st.storage.get_last_block()?;
    Ok(Json(json!({ "data": {
        "syncing": false,
        "blocks": -1,
        "height": last.as_ref().map(|b| b.height).unwrap_or(0),
        "id": last.and_then(|b| b.id),
    } })))
}

async fn node_configuration(State(st): State<Shared>) -> ApiResult {
    let h = last_height(&st)?;
    let m = st.network.milestone(h.max(1));
    Ok(Json(json!({ "data": {
        "core": { "version": env!("CARGO_PKG_VERSION"), "implementation": "sth-core-rust" },
        "nethash": st.network.nethash,
        "slip44": 1,
        "wif": st.network.wif,
        "token": st.network.token,
        "symbol": st.network.token,
        "explorer": "https://explorer.smartholdem.io",
        "version": st.network.pubkey_hash,
        "ports": { "@smartholdem/core-p2p": 4001, "@smartholdem/core-api": 4003 },
        "constants": {
            "height": m.height, "reward": m.reward, "activeDelegates": m.active_delegates, "blocktime": m.blocktime,
            "block": { "version": m.block_version, "maxTransactions": m.max_transactions, "maxPayload": m.max_payload, "idFullSha256": m.id_full_sha256 },
            "epoch": human_time(st.network.epoch_unix), "vendorFieldLength": m.vendor_field_length, "aip11": m.aip11, "aip37": m.aip37,
        },
        "transactionPool": { "dynamicFees": { "enabled": false } },
    } })))
}

fn static_fee_map() -> Map<String, Value> {
    STATIC_FEES.iter().map(|(name, _, fee)| (name.to_string(), json!(fee.to_string()))).collect()
}

async fn transaction_fees() -> ApiResult {
    Ok(Json(json!({ "data": { "1": Value::Object(static_fee_map()) } })))
}

async fn node_fees() -> ApiResult {
    let stats: Map<String, Value> = STATIC_FEES
        .iter()
        .map(|(name, _, fee)| {
            let f = fee.to_string();
            (name.to_string(), json!({ "avg": f, "max": f, "min": f, "sum": f }))
        })
        .collect();
    Ok(Json(json!({ "meta": { "days": 7 }, "data": { "1": Value::Object(stats) } })))
}

async fn peers(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let h = last_height(&st)?;
    let items: Vec<Value> = st
        .peers
        .iter()
        .map(|p| {
            let host = p.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
            json!({ "ip": host, "port": 4001, "ports": { "@smartholdem/core-api": 4003 }, "version": "3.8.2", "height": h, "latency": 0 })
        })
        .collect();
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/peers", &q, total, page)))
}

// ------------------------------------------------------------------ blocks

async fn blocks(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let tip = last_height(&st)?;
    let raw = !transform(&q);
    let (page, limit) = page_params(&q);
    if let Some(id) = q.get("id") {
        let b = st.storage.get_block_by_id(id)?;
        let data = b.map(|b| block_json(&st, &b, raw, tip)).transpose()?.into_iter().collect::<Vec<_>>();
        return Ok(Json(paginate("/blocks", &q, data.len(), data)));
    }
    if let Some(h) = q.get("height").and_then(|v| v.parse::<u64>().ok()) {
        let b = st.storage.get_block_by_height(h)?;
        let data = b.map(|b| block_json(&st, &b, raw, tip)).transpose()?.into_iter().collect::<Vec<_>>();
        return Ok(Json(paginate("/blocks", &q, data.len(), data)));
    }
    let from = q.get("height.from").and_then(|v| v.parse::<u64>().ok()).unwrap_or(1).max(1);
    let to = q.get("height.to").and_then(|v| v.parse::<u64>().ok()).unwrap_or(tip).min(tip);
    if to < from {
        return Ok(Json(paginate("/blocks", &q, 0, vec![])));
    }
    let asc = q.get("orderBy").map(|o| o.ends_with(":asc")).unwrap_or(false);
    let total = (to - from + 1) as usize;
    let offset = (page - 1) * limit;
    let blocks: Vec<Block> = if asc {
        let start = from + offset as u64;
        if start > to { vec![] } else { st.storage.get_blocks_from(start, limit.min((to - start + 1) as usize))? }
    } else {
        let end = to.saturating_sub(offset as u64);
        if end < from || offset as u64 > to { vec![] } else {
            let start = end.saturating_sub(limit as u64 - 1).max(from);
            let mut v = st.storage.get_blocks_from(start, (end - start + 1) as usize)?;
            v.reverse();
            v
        }
    };
    let data = blocks.iter().map(|b| block_json(&st, b, raw, tip)).collect::<Result<Vec<_>, _>>()?;
    Ok(Json(paginate("/blocks", &q, total, data)))
}

fn resolve_block(st: &AppState, id: &str) -> Result<Block, ApiError> {
    let block = if id.len() == 64 {
        st.storage.get_block_by_id(id)?
    } else {
        id.parse::<u64>().ok().map(|h| st.storage.get_block_by_height(h)).transpose()?.flatten()
    };
    block.ok_or_else(|| not_found("Block"))
}

async fn block_by_id(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let b = resolve_block(&st, &id)?;
    Ok(Json(json!({ "data": block_json(&st, &b, !transform(&q), last_height(&st)?)? })))
}

async fn block_first(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let b = st.storage.get_block_by_height(1)?.ok_or_else(|| not_found("Block"))?;
    Ok(Json(json!({ "data": block_json(&st, &b, !transform(&q), last_height(&st)?)? })))
}

async fn block_last(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let b = st.storage.get_last_block()?.ok_or_else(|| not_found("Block"))?;
    Ok(Json(json!({ "data": block_json(&st, &b, !transform(&q), last_height(&st)?)? })))
}

async fn block_transactions(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let b = resolve_block(&st, &id)?;
    let tip = last_height(&st)?;
    let raw = !transform(&q);
    let items = b.transactions.iter().map(|t| tx_json(&st, t, Some(b.timestamp), raw, tip)).collect::<Result<Vec<_>, _>>()?;
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate(&format!("/blocks/{id}/transactions"), &q, total, page)))
}

// ------------------------------------------------------------ transactions

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Any,
    Sent,
    Received,
}

/// Shared implementation of every transaction listing (filters: type, typeGroup, senderId, recipientId, address, blockId).
fn list_transactions(st: &AppState, q: &Params, wallet: Option<&str>, role: Role, path: &str) -> ApiResult {
    let tip = last_height(st)?;
    let raw = !transform(q);
    let wallet_addr = wallet.map(|s| s.to_string()).or_else(|| q.get("address").cloned()).or_else(|| q.get("senderId").cloned()).or_else(|| q.get("recipientId").cloned());
    let ids = st.storage.transaction_ids(wallet_addr.as_deref())?;
    let type_f = q.get("type").and_then(|v| v.parse::<u16>().ok());
    let group_f = q.get("typeGroup").and_then(|v| v.parse::<u32>().ok());
    let sender_f = q.get("senderId").cloned();
    let recipient_f = q.get("recipientId").cloned();
    let block_f = q.get("blockId").cloned();
    let (page, limit) = page_params(q);

    let mut matched: Vec<Value> = Vec::new();
    let mut total = 0usize;
    for id in ids {
        let Some((tx, ts)) = st.storage.get_transaction_with_timestamp(&id)? else { continue };
        let sender = crate::crypto::address_from_public_key(&tx.sender_public_key, st.network.pubkey_hash).unwrap_or_default();
        let receives = |addr: &str| {
            tx.recipient_id.as_deref() == Some(addr)
                || tx.asset.as_ref().and_then(|a| a.payments.as_ref()).map(|p| p.iter().any(|x| x.recipient_id == addr)).unwrap_or(false)
        };
        if let Some(w) = wallet {
            match role {
                Role::Sent if sender != w => continue,
                Role::Received if !receives(w) => continue,
                _ => {}
            }
        }
        if type_f.map_or(false, |t| tx.type_ != t) || group_f.map_or(false, |g| tx.type_group != g) {
            continue;
        }
        if sender_f.as_deref().map_or(false, |s| sender != s) || recipient_f.as_deref().map_or(false, |r| !receives(r)) {
            continue;
        }
        if block_f.as_deref().map_or(false, |b| tx.block_id.as_deref() != Some(b)) {
            continue;
        }
        total += 1;
        if total > (page - 1) * limit && matched.len() < limit {
            matched.push(tx_json(st, &tx, Some(ts), raw, tip)?);
        }
    }
    Ok(Json(paginate(path, q, total, matched)))
}

async fn transactions(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    list_transactions(&st, &q, None, Role::Any, "/transactions")
}

async fn transaction_by_id(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let tip = last_height(&st)?;
    if let Some((tx, ts)) = st.storage.get_transaction_with_timestamp(&id)? {
        return Ok(Json(json!({ "data": tx_json(&st, &tx, Some(ts), !transform(&q), tip)? })));
    }
    if let Some(tx) = st.mempool.get(&id).await {
        return Ok(Json(json!({ "data": tx_json(&st, &tx, None, !transform(&q), tip)? })));
    }
    Err(not_found("Transaction"))
}

async fn unconfirmed(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let tip = last_height(&st)?;
    let raw = !transform(&q);
    let items = st.mempool.all().await.iter().map(|t| tx_json(&st, t, None, raw, tip)).collect::<Result<Vec<_>, _>>()?;
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/transactions/unconfirmed", &q, total, page)))
}

async fn unconfirmed_by_id(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let tx = st.mempool.get(&id).await.ok_or_else(|| not_found("Transaction"))?;
    Ok(Json(json!({ "data": tx_json(&st, &tx, None, !transform(&q), last_height(&st)?)? })))
}

#[derive(serde::Deserialize)]
struct PostBody {
    transactions: Vec<Transaction>,
}

async fn post_transactions(State(st): State<Shared>, body: axum::body::Bytes) -> ApiResult {
    let parsed: PostBody = serde_json::from_slice(&body)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("Invalid transaction payload: {e}")))?;
    if parsed.transactions.is_empty() || parsed.transactions.len() > 40 {
        return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, "transactions must contain 1..40 items".into()));
    }
    let (resp, errors) = st.mempool.add_many(parsed.transactions).await;
    let errors_json = if errors.is_empty() { Value::Null } else { serde_json::to_value(errors).map_err(Error::Json)? };
    Ok(Json(json!({ "data": resp, "errors": errors_json })))
}

// ------------------------------------------------------------------ wallets

async fn wallets(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let mut all = st.storage.all_wallets()?;
    all.sort_by(|a, b| b.balance.cmp(&a.balance).then_with(|| a.address.cmp(&b.address)));
    let items: Vec<Value> = all.iter().map(|w| wallet_json(&st, w, None, None)).collect();
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/wallets", &q, total, page)))
}

async fn wallet_by_id(State(st): State<Shared>, Path(id): Path<String>) -> ApiResult {
    let w = st.storage.find_wallet(&id)?.ok_or_else(|| not_found("Wallet"))?;
    let (rank, votes) = if w.is_delegate() {
        match find_delegate(&st, &w.address)? {
            Some(d) => (d["rank"].as_u64().map(|r| r as usize), d["votes"].as_str().and_then(|v| v.parse().ok())),
            None => (None, None),
        }
    } else {
        (None, None)
    };
    Ok(Json(json!({ "data": wallet_json(&st, &w, rank, votes) })))
}

async fn wallet_transactions(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let w = st.storage.find_wallet(&id)?.ok_or_else(|| not_found("Wallet"))?;
    list_transactions(&st, &q, Some(&w.address), Role::Any, &format!("/wallets/{id}/transactions"))
}

async fn wallet_transactions_sent(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let w = st.storage.find_wallet(&id)?.ok_or_else(|| not_found("Wallet"))?;
    list_transactions(&st, &q, Some(&w.address), Role::Sent, &format!("/wallets/{id}/transactions/sent"))
}

async fn wallet_transactions_received(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let w = st.storage.find_wallet(&id)?.ok_or_else(|| not_found("Wallet"))?;
    list_transactions(&st, &q, Some(&w.address), Role::Received, &format!("/wallets/{id}/transactions/received"))
}

// ---------------------------------------------------------------- delegates

async fn delegates(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let list = ranked_delegates(&st)?;
    let (total, page) = slice_page(list, &q);
    Ok(Json(paginate("/delegates", &q, total, page)))
}

async fn delegate_by_id(State(st): State<Shared>, Path(id): Path<String>) -> ApiResult {
    let d = find_delegate(&st, &id)?.ok_or_else(|| not_found("Delegate"))?;
    Ok(Json(json!({ "data": d })))
}

async fn delegate_voters(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let d = find_delegate(&st, &id)?.ok_or_else(|| not_found("Delegate"))?;
    let pk = d["publicKey"].as_str().unwrap_or_default().to_string();
    let mut voters: Vec<WalletState> = st.storage.all_wallets()?.into_iter().filter(|w| w.vote.as_deref() == Some(pk.as_str())).collect();
    voters.sort_by(|a, b| b.balance.cmp(&a.balance));
    let items: Vec<Value> = voters.iter().map(|w| wallet_json(&st, w, None, None)).collect();
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate(&format!("/delegates/{id}/voters"), &q, total, page)))
}

async fn delegate_blocks(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let d = find_delegate(&st, &id)?.ok_or_else(|| not_found("Delegate"))?;
    let pk = d["publicKey"].as_str().unwrap_or_default().to_string();
    let tip = last_height(&st)?;
    let raw = !transform(&q);
    let (page, limit) = page_params(&q);
    // Walk back from the tip; bounded scan keeps this cheap for the recent history wallets care about.
    let mut found = Vec::new();
    let mut skipped = 0usize;
    let mut h = tip;
    let mut scanned = 0u64;
    while h > 0 && found.len() < limit && scanned < 200_000 {
        if let Some(b) = st.storage.get_block_by_height(h)? {
            if b.generator_public_key == pk {
                if skipped < (page - 1) * limit {
                    skipped += 1;
                } else {
                    found.push(block_json(&st, &b, raw, tip)?);
                }
            }
        }
        h -= 1;
        scanned += 1;
    }
    let total = d["blocks"]["produced"].as_u64().unwrap_or(0) as usize;
    Ok(Json(paginate(&format!("/delegates/{id}/blocks"), &q, total, found)))
}
