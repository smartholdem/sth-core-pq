//! Author: TechnoL0g
//!
//! `/api/delegates*` and `/api/rounds/:round/delegates` — ranking by vote weight (cached per height),
//! filters username / address / publicKey / isResigned, voters, produced blocks.

use super::render::{block_json, delegate_json, wallet_json};
use super::{last_height, not_found, page_params, paginate, slice_page, transform, ApiError, ApiResult, AppState, Params, Shared};
use crate::storage::WalletState;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde_json::{json, Value};

/// Delegates ranked by vote weight, cached per chain height.
pub fn ranked(st: &AppState) -> Result<Vec<Value>, ApiError> {
    let tip = last_height(st)?;
    if let Ok(cache) = st.delegates_cache.lock() {
        if let Some((h, list)) = cache.as_ref() {
            if *h == tip {
                return Ok(list.clone());
            }
        }
    }
    let list: Vec<Value> = st
        .storage
        .delegate_ranking()?
        .iter()
        .enumerate()
        .map(|(i, (w, votes))| delegate_json(st, w, *votes, i + 1))
        .collect();
    if let Ok(mut cache) = st.delegates_cache.lock() {
        *cache = Some((tip, list.clone()));
    }
    Ok(list)
}

pub fn find(st: &AppState, id: &str) -> Result<Option<Value>, ApiError> {
    Ok(ranked(st)?.into_iter().find(|d| {
        d["username"].as_str() == Some(id) || d["address"].as_str() == Some(id) || d["publicKey"].as_str() == Some(id)
    }))
}

pub async fn list(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let mut list = ranked(&st)?;
    for (param, field) in [("username", "username"), ("address", "address"), ("publicKey", "publicKey")] {
        if let Some(v) = q.get(param) {
            list.retain(|d| d[field].as_str() == Some(v.as_str()));
        }
    }
    if let Some(r) = q.get("isResigned") {
        let want = r == "true";
        list.retain(|d| d["isResigned"].as_bool() == Some(want));
    }
    list.retain(|d| {
        q.range_ok("votes", d["votes"].as_str().and_then(|v| v.parse().ok()).unwrap_or(0))
            && q.range_ok("blocks.produced", d["blocks"]["produced"].as_u64().unwrap_or(0))
            && q.range_ok("forged.fees", d["forged"]["fees"].as_str().and_then(|v| v.parse().ok()).unwrap_or(0))
            && q.range_ok("forged.total", d["forged"]["total"].as_str().and_then(|v| v.parse().ok()).unwrap_or(0))
    });
    if let Some((field, asc)) = q.order() {
        let key = |d: &Value| -> u64 {
            match field {
                "rank" => d["rank"].as_u64().unwrap_or(0),
                "votes" => d["votes"].as_str().and_then(|v| v.parse().ok()).unwrap_or(0),
                "blocks.produced" => d["blocks"]["produced"].as_u64().unwrap_or(0),
                "forged.total" => d["forged"]["total"].as_str().and_then(|v| v.parse().ok()).unwrap_or(0),
                _ => d["rank"].as_u64().unwrap_or(0),
            }
        };
        if field == "username" {
            list.sort_by(|a, b| a["username"].as_str().cmp(&b["username"].as_str()));
            if !asc {
                list.reverse();
            }
        } else {
            list.sort_by_key(key);
            // rank is ascending by nature; every other numeric field defaults to descending
            if (field == "rank") != asc {
                list.reverse();
            }
        }
    }
    let (total, page) = slice_page(list, &q);
    Ok(Json(paginate("/delegates", &q, total, page)))
}

pub async fn by_id(State(st): State<Shared>, Path(id): Path<String>) -> ApiResult {
    let d = find(&st, &id)?.ok_or_else(|| not_found("Delegate"))?;
    Ok(Json(json!({ "data": d })))
}

pub async fn voters(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let d = find(&st, &id)?.ok_or_else(|| not_found("Delegate"))?;
    let pk = d["publicKey"].as_str().unwrap_or_default().to_string();
    let mut voters: Vec<WalletState> = st.storage.all_wallets()?.into_iter().filter(|w| w.vote.as_deref() == Some(pk.as_str())).collect();
    voters.retain(|w| q.range_ok("balance", w.balance.max(0) as u64) && q.range_ok("nonce", w.nonce));
    if let Some(a) = q.get("address") {
        voters.retain(|w| &w.address == a);
    }
    if let Some(p) = q.get("publicKey") {
        voters.retain(|w| w.public_key.as_deref() == Some(p.as_str()));
    }
    voters.sort_by(|a, b| b.balance.cmp(&a.balance).then_with(|| a.address.cmp(&b.address)));
    if q.ascending() {
        voters.reverse();
    }
    let items: Vec<Value> = voters.iter().map(|w| wallet_json(&st, w, None, None)).collect();
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate(&format!("/delegates/{id}/voters"), &q, total, page)))
}

pub async fn blocks(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let d = find(&st, &id)?.ok_or_else(|| not_found("Delegate"))?;
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
    if q.ascending() {
        found.reverse();
    }
    let total = d["blocks"]["produced"].as_u64().unwrap_or(0) as usize;
    Ok(Json(paginate(&format!("/delegates/{id}/blocks"), &q, total, found)))
}

/// Active delegates of a round: stored snapshot when available, the live top-N for the current round.
pub async fn round_delegates(State(st): State<Shared>, Path(round): Path<u64>) -> ApiResult {
    let tip = last_height(&st)?;
    let n = st.network.milestone(tip.max(1)).active_delegates as u64;
    let current = (tip.max(1) - 1) / n + 1;
    let list = match st.storage.get_round(round)? {
        Some(l) => l,
        None if round == current || round == current + 1 => st.storage.active_delegates(n as usize)?,
        None => return Err(not_found("Round")),
    };
    let data: Vec<Value> = list.into_iter().map(|d| json!({ "publicKey": d.public_key, "votes": d.votes.to_string() })).collect();
    Ok(Json(json!({ "data": data })))
}
