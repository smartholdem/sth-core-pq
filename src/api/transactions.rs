//! Author: TechnoL0g
//!
//! `/api/transactions*` and `/api/votes*` — full legacy filter set (type, typeGroup, senderId,
//! senderPublicKey, recipientId, address, blockId, id, version, vendorField, timestamp/amount/fee/nonce ranges).

use super::render::{sender_address, tx_json};
use super::{last_height, not_found, page_params, paginate, slice_page, transform, ApiError, ApiResult, AppState, Params, Shared};
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

/// Shared implementation of every transaction listing.
pub fn list_transactions(st: &AppState, q: &Params, wallet: Option<&str>, role: Role, extra: &[(&str, &str)], path: &str) -> ApiResult {
    let tip = last_height(st)?;
    let raw = !transform(q);
    let wallet_addr = wallet
        .map(|s| s.to_string())
        .or_else(|| q.get("address").cloned())
        .or_else(|| q.get("senderId").cloned())
        .or_else(|| q.get("recipientId").cloned());
    let mut ids = st.storage.transaction_ids(wallet_addr.as_deref())?;
    if q.ascending() {
        ids.reverse();
    }
    let extra_type = extra.iter().find(|(k, _)| *k == "type").and_then(|(_, v)| v.parse::<u16>().ok());
    let extra_group = extra.iter().find(|(k, _)| *k == "typeGroup").and_then(|(_, v)| v.parse::<u32>().ok());
    let (page, limit) = page_params(q);
    let ts_filter = q.has_range("timestamp");

    let mut matched: Vec<Value> = Vec::new();
    let mut total = 0usize;
    for id in ids {
        let Some((tx, ts)) = st.storage.get_transaction_with_timestamp(&id)? else { continue };
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
        if !matches(st, &tx, q) || (ts_filter && !q.range_ok("timestamp", ts as u64)) {
            continue;
        }
        total += 1;
        if total > (page - 1) * limit && matched.len() < limit {
            matched.push(tx_json(st, &tx, Some(ts), raw, tip)?);
        }
    }
    Ok(Json(paginate(path, q, total, matched)))
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

pub async fn unconfirmed(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let tip = last_height(&st)?;
    let raw = !transform(&q);
    let items = st.mempool.all().await.iter().map(|t| tx_json(&st, t, None, raw, tip)).collect::<Result<Vec<_>, _>>()?;
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/transactions/unconfirmed", &q, total, page)))
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
    let (resp, errors) = st.mempool.add_many(parsed.transactions).await;
    let errors_json = if errors.is_empty() { Value::Null } else { serde_json::to_value(errors).map_err(Error::Json)? };
    Ok(Json(json!({ "data": resp, "errors": errors_json })))
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
