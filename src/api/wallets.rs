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

fn filtered_wallets(st: &AppState, q: &Params) -> Result<Vec<WalletState>, crate::error::Error> {
    if let Some(a) = q.get("address") {
        return Ok(st.storage.get_wallet(a)?.into_iter().collect());
    }
    if let Some(pk) = q.get("publicKey") {
        return Ok(st.storage.find_wallet(pk)?.into_iter().filter(|w| w.public_key.as_deref() == Some(pk.as_str())).collect());
    }
    let mut all = st.storage.all_wallets()?;
    all.retain(|w| q.range_ok("balance", w.balance.max(0) as u64) && q.range_ok("nonce", w.nonce));
    Ok(all)
}

fn sort_wallets(all: &mut [WalletState], q: &Params) {
    let (field, asc) = q.order().unwrap_or(("balance", false));
    match field {
        "nonce" => all.sort_by(|a, b| b.nonce.cmp(&a.nonce).then_with(|| a.address.cmp(&b.address))),
        "address" => all.sort_by(|a, b| a.address.cmp(&b.address)),
        _ => all.sort_by(|a, b| b.balance.cmp(&a.balance).then_with(|| a.address.cmp(&b.address))),
    }
    if asc {
        all.reverse();
    }
}

pub async fn list(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let mut all = filtered_wallets(&st, &q)?;
    sort_wallets(&mut all, &q);
    let items: Vec<Value> = all.iter().map(|w| wallet_json(&st, w, None, None)).collect();
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/wallets", &q, total, page)))
}

pub async fn top(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let mut all = st.storage.all_wallets()?;
    all.sort_by(|a, b| b.balance.cmp(&a.balance).then_with(|| a.address.cmp(&b.address)));
    let items: Vec<Value> = all.iter().map(|w| wallet_json(&st, w, None, None)).collect();
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/wallets/top", &q, total, page)))
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
