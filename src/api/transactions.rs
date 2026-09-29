//! Author: TechnoL0g
//!
//! `/api/transactions*` and `/api/votes*` — full legacy filter set (type, typeGroup, senderId,
//! senderPublicKey, recipientId, address, blockId, id, version, vendorField, timestamp/amount/fee/nonce ranges).

use super::render::{sender_address, tx_json};
use super::{last_height, not_found, page_params, paginate, paginate_with, slice_page, transform, ApiError, ApiResult, AppState, Params, Shared};
use crate::error::Error;
use crate::models::{tx_type, Transaction, TYPE_GROUP_CORE};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

#[derive(Clone, Copy, PartialEq)]
pub enum Role {
    Any,
    Sent,
    Received,
}

fn receives(tx: &Transaction, addr: &str) -> bool {
    tx.recipient_id.as_deref() == Some(addr)
        || tx.asset.as_ref().and_then(|a| a.payments.as_ref()).map(|p| p.iter().any(|x| x.recipient_id == addr)).unwrap_or(false)
        || tx.token_asset().is_some_and(|a| a.recipient_id.as_deref() == Some(addr) || a.transfers.iter().flatten().any(|t| t.recipient_id == addr))
        || tx.sobj_asset().is_some_and(|e| e.recipient_id.as_deref() == Some(addr))
}

/// Query filters that apply to a single transaction (block timestamp handled by the caller).
pub fn matches(st: &AppState, tx: &Transaction, q: &Params) -> bool {
    if q.parse::<u16>("type").map_or(false, |t| tx.type_ != t) || q.parse::<u32>("typeGroup").map_or(false, |g| tx.type_group != g) {
        return false;
    }
    if q.get("id").map_or(false, |id| tx.id.as_deref() != Some(id)) || q.parse::<u8>("version").map_or(false, |v| tx.version != v) {
        return false;
    }
    if q.get("senderPublicKey").map_or(false, |pk| &tx.sender_public_key != pk) {
        return false;
    }
    if q.get("senderId").map_or(false, |s| sender_address(st, tx) != *s) || q.get("recipientId").map_or(false, |r| !receives(tx, r)) {
        return false;
    }
    if q.get("blockId").map_or(false, |b| tx.block_id.as_deref() != Some(b)) {
        return false;
    }
    if q.get("vendorField").map_or(false, |vf| tx.vendor_field.as_deref() != Some(vf)) {
        return false;
    }
    if !q.range_ok("amount", tx.amount) || !q.range_ok("fee", tx.fee) || !q.range_ok("nonce", tx.nonce.unwrap_or(0)) {
        return false;
    }
    if !q.range_ok("sequence", tx.sequence.unwrap_or(0) as u64) {
        return false;
    }
    true
}

/// Query keys that need the decoded transaction (everything except paging / ordering / wallet index selection).
const TX_FILTER_KEYS: &[&str] = &["type", "typeGroup", "id", "version", "senderPublicKey", "senderId", "recipientId", "blockId", "vendorField", "amount", "fee", "nonce", "sequence"];

/// Index entries decoded per request once the page is full; beyond it `totalCount` is extrapolated (legacy `totalCountIsEstimate`).
const SCAN_BUDGET: u64 = 50_000;

fn has_tx_filters(q: &Params) -> bool {
    q.iter().any(|(k, _)| TX_FILTER_KEYS.iter().any(|f| k == f || k.strip_prefix(f).is_some_and(|rest| rest.starts_with('.'))))
}

/// Shared implementation of every transaction listing.
///
/// Walks the height-ordered sled index lazily: unfiltered lists cost one block decode per row of the page and an O(1)
/// counter; filtered lists decode each block once and stop counting after `SCAN_BUDGET` entries (estimated total).
pub fn list_transactions(st: &AppState, q: &Params, wallet: Option<&str>, role: Role, extra: &[(&str, &str)], path: &str) -> ApiResult {
    let tip = last_height(st)?;
    let raw = !transform(q);
    let wallet_addr = wallet
        .map(|s| s.to_string())
        .or_else(|| q.get("address").cloned())
        .or_else(|| q.get("senderId").cloned())
        .or_else(|| q.get("recipientId").cloned());
    let heights = if q.has_range("timestamp") {
        let exact = q.parse::<u32>("timestamp");
        match st.storage.heights_for_timestamps(exact.or_else(|| q.parse("timestamp.from")), exact.or_else(|| q.parse("timestamp.to")))? {
            Some(r) => r,
            None => return Ok(Json(paginate(path, q, 0, Vec::new()))),
        }
    } else {
        0..=u64::MAX
    };
    let (page, limit) = page_params(q);
    let skip = (page - 1) * limit;
    let needs_tx = has_tx_filters(q) || !extra.is_empty() || (wallet.is_some() && role != Role::Any);
    let mut cache: Option<(u64, crate::models::Block)> = None;
    let mut matched: Vec<Value> = Vec::new();

    if !needs_tx {
        let total = st.storage.transaction_index_count(wallet_addr.as_deref(), heights.clone())? as usize;
        for entry in st.storage.transaction_index(wallet_addr.as_deref(), heights, q.ascending()).skip(skip).take(limit) {
            let entry = entry?;
            let Some((tx, ts)) = st.storage.transaction_at(&entry, &mut cache)? else { continue };
            matched.push(tx_json(st, &tx, Some(ts), raw, tip)?);
        }
        return Ok(Json(paginate(path, q, total, matched)));
    }

    let extra_type = extra.iter().find(|(k, _)| *k == "type").and_then(|(_, v)| v.parse::<u16>().ok());
    let extra_group = extra.iter().find(|(k, _)| *k == "typeGroup").and_then(|(_, v)| v.parse::<u32>().ok());
    let mut total = 0usize;
    let mut scanned = 0u64;
    let mut estimate = false;
    for entry in st.storage.transaction_index(wallet_addr.as_deref(), heights.clone(), q.ascending()) {
        let entry = entry?;
        if matched.len() >= limit && scanned >= SCAN_BUDGET {
            estimate = true;
            break;
        }
        scanned += 1;
        let Some((tx, ts)) = st.storage.transaction_at(&entry, &mut cache)? else { continue };
        if let Some(w) = wallet {
            match role {
                Role::Sent if sender_address(st, &tx) != w => continue,
                Role::Received if !receives(&tx, w) => continue,
                _ => {}
            }
        }
        if extra_type.map_or(false, |t| tx.type_ != t) || extra_group.map_or(false, |g| tx.type_group != g) {
            continue;
        }
        if !matches(st, &tx, q) {
            continue;
        }
        total += 1;
        if total > skip && matched.len() < limit {
            matched.push(tx_json(st, &tx, Some(ts), raw, tip)?);
        }
    }
    if estimate {
        let in_range = st.storage.transaction_index_count(wallet_addr.as_deref(), heights)?;
        total = ((total as u128 * in_range as u128) / scanned.max(1) as u128) as usize;
        total = total.max(skip + matched.len());
    }
    Ok(Json(paginate_with(path, q, total, estimate, matched)))
}

pub async fn list(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    list_transactions(&st, &q, None, Role::Any, &[], "/transactions")
}

pub async fn by_id(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let tip = last_height(&st)?;
    if let Some((tx, ts)) = st.storage.get_transaction_with_timestamp(&id)? {
        return Ok(Json(json!({ "data": tx_json(&st, &tx, Some(ts), !transform(&q), tip)? })));
    }
    if let Some(tx) = st.mempool.get(&id).await {
        return Ok(Json(json!({ "data": tx_json(&st, &tx, None, !transform(&q), tip)? })));
    }
    Err(not_found("Transaction"))
}

/// `?senderId=<address>` narrows to one sender; items are sorted by sender, then nonce, so a wallet reads its pending nonces in order.
pub async fn unconfirmed(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let tip = last_height(&st)?;
    let raw = !transform(&q);
    let mut pending = st.mempool.all().await;
    if let Some(sender) = q.get("senderId") {
        pending.retain(|t| super::render::sender_address(&st, t) == *sender);
    }
    pending.sort_by(|a, b| a.sender_public_key.cmp(&b.sender_public_key).then(a.nonce.cmp(&b.nonce)));
    let items = pending
        .iter()
        .map(|t| {
            let mut v = tx_json(&st, t, None, raw, tip)?;
            if let (Some(obj), Some((entered, relayed))) = (v.as_object_mut(), t.id.as_deref().and_then(|id| st.mempool.pending_meta(id))) {
                obj.insert("pool".into(), json!({ "enteredHeight": entered, "ageBlocks": tip.saturating_sub(entered), "lastRelayHeight": relayed, "feeHint": st.mempool.fee_hint(t) }));
            }
            Ok::<_, ApiError>(v)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/transactions/unconfirmed", &q, total, page)))
}

/// `DELETE /api/transactions/unconfirmed[?senderId=]` — operator action from the node's own host only: direct loopback
/// connection, no reverse-proxy headers (nginx adds `X-Forwarded-For` / `X-Real-IP`), never over iroh.
pub async fn purge_unconfirmed(State(st): State<Shared>, peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>, headers: axum::http::HeaderMap, Query(q): Query<Params>) -> ApiResult {
    let local = peer.as_ref().is_some_and(|p| p.0.ip().is_loopback());
    let proxied = headers.contains_key("x-forwarded-for") || headers.contains_key("x-real-ip") || headers.contains_key("forwarded");
    if !local || proxied {
        return Err(ApiError(StatusCode::FORBIDDEN, "mempool purge is allowed only from 127.0.0.1 on the node itself (sth-cli mempool-purge)".into()));
    }
    let removed = st.mempool.purge(q.get("senderId").map(String::as_str)).await;
    Ok(Json(json!({ "data": { "removed": removed, "senderId": q.get("senderId") } })))
}

pub async fn unconfirmed_by_id(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let tx = st.mempool.get(&id).await.ok_or_else(|| not_found("Transaction"))?;
    Ok(Json(json!({ "data": tx_json(&st, &tx, None, !transform(&q), last_height(&st)?)? })))
}

#[derive(serde::Deserialize)]
struct PostBody {
    transactions: Vec<Transaction>,
}

pub async fn post(State(st): State<Shared>, body: axum::body::Bytes) -> ApiResult {
    let parsed: PostBody = serde_json::from_slice(&body)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("Invalid transaction payload: {e}")))?;
    if parsed.transactions.is_empty() || parsed.transactions.len() > 40 {
        return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, "transactions must contain 1..40 items".into()));
    }
    // fee advisor: legacy pools refuse fees other than what they demand → only PQ delegates forge such transactions
    let hints: serde_json::Map<String, Value> = parsed.transactions.iter().filter_map(|t| Some((crate::crypto::transaction_id(t).ok()?, st.mempool.fee_hint(t)?))).collect();
    let (resp, errors) = st.mempool.add_many(parsed.transactions).await;
    let errors_json = if errors.is_empty() { Value::Null } else { serde_json::to_value(errors).map_err(Error::Json)? };
    let hints: serde_json::Map<String, Value> = hints.into_iter().filter(|(id, _)| resp.accept.contains(id)).collect();
    let hints_json = if hints.is_empty() { Value::Null } else { Value::Object(hints) };
    Ok(Json(json!({ "data": resp, "errors": errors_json, "feeHints": hints_json })))
}

// -------------------------------------------------------------------- votes

pub async fn votes(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    list_transactions(&st, &q, None, Role::Any, &[("type", "3"), ("typeGroup", "1")], "/votes")
}

pub async fn vote_by_id(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let (tx, ts) = st.storage.get_transaction_with_timestamp(&id)?.ok_or_else(|| not_found("Vote"))?;
    if tx.type_ != tx_type::VOTE || tx.type_group != TYPE_GROUP_CORE {
        return Err(not_found("Vote"));
    }
    Ok(Json(json!({ "data": tx_json(&st, &tx, Some(ts), !transform(&q), last_height(&st)?)? })))
}
