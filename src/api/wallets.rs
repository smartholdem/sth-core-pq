//! Author: TechnoL0g
//!
//! `/api/wallets*` — listing with address/publicKey/balance/nonce filters, `top`, per-wallet
//! transactions (all / sent / received), votes and locks.

use super::render::{lock_json, wallet_json};
use super::transactions::{list_transactions, Role};
use super::{not_found, paginate, slice_page, ApiResult, AppState, Params, Shared};
use crate::storage::WalletState;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde_json::{json, Value};

/// Wallets matching the query, in the requested order (default balance desc = the cached snapshot order).
fn filtered_wallets(st: &AppState, q: &Params) -> Result<Vec<WalletState>, crate::error::Error> {
    if let Some(a) = q.get("address") {
        return Ok(st.storage.get_wallet(a)?.into_iter().collect());
    }
    if let Some(pk) = q.get("publicKey") {
        return Ok(st.storage.find_wallet(pk)?.into_iter().filter(|w| w.public_key.as_deref() == Some(pk.as_str())).collect());
    }
    let all = st.wallets_snapshot()?;
    let (field, asc) = q.order().unwrap_or(("balance", false));
    let mut out: Vec<WalletState> = all.iter().filter(|w| q.range_ok("balance", w.balance.max(0) as u64) && q.range_ok("nonce", w.nonce)).cloned().collect();
    match field {
        "nonce" => out.sort_by(|a, b| b.nonce.cmp(&a.nonce).then_with(|| a.address.cmp(&b.address))),
        "address" => out.sort_by(|a, b| a.address.cmp(&b.address)),
        _ => {}
    }
    if asc {
        out.reverse();
    }
    Ok(out)
}

pub async fn list(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let all = filtered_wallets(&st, &q)?;
    let (total, page) = slice_page(all, &q);
    let items: Vec<Value> = page.iter().map(|w| wallet_json(&st, w, None, None)).collect();
    Ok(Json(paginate("/wallets", &q, total, items)))
}

/// Richest wallets: a page straight out of the balance-sorted snapshot — no scan, no per-request sort.
pub async fn top(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let all = st.wallets_snapshot()?;
    let (page, limit) = super::page_params(&q);
    let items: Vec<Value> = all.iter().skip((page - 1) * limit).take(limit).map(|w| wallet_json(&st, w, None, None)).collect();
    Ok(Json(paginate("/wallets/top", &q, all.len(), items)))
}

/// Explorer search box: `?q=` matches an address prefix (case-sensitive base58), a delegate username prefix
/// (case-insensitive) or a public key prefix (hex). Richest matches first, `limit` ≤ 50 (default 10), `q` ≥ 2 chars.
pub async fn search(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let needle = q.get("q").map(|s| s.trim().to_string()).unwrap_or_default();
    let limit = q.parse::<usize>("limit").unwrap_or(10).clamp(1, 50);
    if needle.len() < 2 {
        return Ok(Json(json!({ "meta": { "query": needle, "count": 0 }, "data": [] })));
    }
    let lower = needle.to_lowercase();
    let is_hex = needle.len() >= 4 && needle.chars().all(|c| c.is_ascii_hexdigit());
    let all = st.wallets_snapshot()?;
    let hit = |w: &WalletState| -> Option<&'static str> {
        if w.address.starts_with(&needle) {
            Some("address")
        } else if w.username.as_deref().is_some_and(|u| u.to_lowercase().starts_with(&lower)) {
            Some("username")
        } else if is_hex && w.public_key.as_deref().is_some_and(|pk| pk.starts_with(&lower)) {
            Some("publicKey")
        } else {
            None
        }
    };
    let mut data: Vec<Value> = Vec::with_capacity(limit);
    let mut total = 0usize;
    for w in all.iter() {
        let Some(kind) = hit(w) else { continue };
        total += 1;
        if data.len() < limit {
            data.push(json!({
                "address": w.address,
                "publicKey": w.public_key,
                "username": w.username,
                "balance": w.balance.to_string(),
                "isDelegate": w.is_delegate(),
                "isResigned": w.resigned,
                "match": kind,
            }));
        }
    }
    Ok(Json(json!({ "meta": { "query": needle, "count": data.len(), "totalCount": total }, "data": data })))
}

pub async fn by_id(State(st): State<Shared>, Path(id): Path<String>) -> ApiResult {
    let w = st.storage.find_wallet(&id)?.ok_or_else(|| not_found("Wallet"))?;
    let (rank, votes) = if w.is_delegate() {
        match super::delegates::find(&st, &w.address)? {
            Some(d) => (d["rank"].as_u64().map(|r| r as usize), d["votes"].as_str().and_then(|v| v.parse().ok())),
            None => (None, None),
        }
    } else {
        (None, None)
    };
    Ok(Json(json!({ "data": wallet_json(&st, &w, rank, votes) })))
}

fn wallet(st: &AppState, id: &str) -> Result<WalletState, super::ApiError> {
    st.storage.find_wallet(id)?.ok_or_else(|| not_found("Wallet"))
}

pub async fn transactions(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let w = wallet(&st, &id)?;
    list_transactions(&st, &q, Some(&w.address), Role::Any, &[], &format!("/wallets/{id}/transactions"))
}

pub async fn transactions_sent(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let w = wallet(&st, &id)?;
    list_transactions(&st, &q, Some(&w.address), Role::Sent, &[], &format!("/wallets/{id}/transactions/sent"))
}

pub async fn transactions_received(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let w = wallet(&st, &id)?;
    list_transactions(&st, &q, Some(&w.address), Role::Received, &[], &format!("/wallets/{id}/transactions/received"))
}

pub async fn votes(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let w = wallet(&st, &id)?;
    list_transactions(&st, &q, Some(&w.address), Role::Sent, &[("type", "3"), ("typeGroup", "1")], &format!("/wallets/{id}/votes"))
}

pub async fn locks(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let w = wallet(&st, &id)?;
    let items: Vec<Value> = w.locks.values().map(|l| lock_json(&st, l)).collect();
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate(&format!("/wallets/{id}/locks"), &q, total, page)))
}

/// `GET /api/wallets/:id/audit` — stored nonce / balance vs. the values recomputed from the wallet's transaction index
/// (state drift detector; `sth-core audit-wallet --fix` repairs offline).
pub async fn audit(State(st): State<Shared>, Path(id): Path<String>) -> ApiResult {
    let w = st.storage.find_wallet(&id)?.ok_or_else(|| not_found("Wallet"))?;
    let a = st.storage.audit_wallet(&w.address)?;
    Ok(Json(json!({ "data": a })))
}
