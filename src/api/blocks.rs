//! Author: TechnoL0g
//!
//! `/api/blocks*` — height / id / timestamp / generator filters, `orderBy=height:asc|desc`.

use super::render::{block_json, tx_json};
use super::{last_height, not_found, page_params, paginate, slice_page, transform, ApiResult, AppState, ApiError, Params, Shared};
use crate::models::Block;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde_json::Value;

pub async fn list(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let tip = last_height(&st)?;
    let raw = !transform(&q);
    let (page, limit) = page_params(&q);
    if let Some(id) = q.get("id") {
        let b = st.storage.get_block_by_id(id)?;
        let data = b.map(|b| block_json(&st, &b, raw, tip)).transpose()?.into_iter().collect::<Vec<_>>();
        return Ok(Json(paginate("/blocks", &q, data.len(), data)));
    }
    if let Some(h) = q.parse::<u64>("height") {
        let b = st.storage.get_block_by_height(h)?;
        let data = b.map(|b| block_json(&st, &b, raw, tip)).transpose()?.into_iter().collect::<Vec<_>>();
        return Ok(Json(paginate("/blocks", &q, data.len(), data)));
    }
    let mut from = q.parse::<u64>("height.from").unwrap_or(1).max(1);
    let mut to = q.parse::<u64>("height.to").unwrap_or(tip).min(tip);
    // timestamp filters are mapped onto a height window (block timestamps grow with height)
    if let Some(ts) = q.parse::<u32>("timestamp") {
        from = from.max(st.storage.height_at_or_after(ts)?);
        to = to.min(st.storage.height_at_or_after(ts + 1)?.saturating_sub(1));
    }
    if let Some(ts) = q.parse::<u32>("timestamp.from") {
        from = from.max(st.storage.height_at_or_after(ts)?);
    }
    if let Some(ts) = q.parse::<u32>("timestamp.to") {
        to = to.min(st.storage.height_at_or_after(ts + 1)?.saturating_sub(1));
    }
    if to < from {
        return Ok(Json(paginate("/blocks", &q, 0, vec![])));
    }
    if let Some(pk) = q.get("generatorPublicKey") {
        return by_generator(&st, &q, pk, from, to, raw, tip);
    }
    let asc = q.ascending();
    let total = (to - from + 1) as usize;
    let offset = (page - 1) * limit;
    let blocks: Vec<Block> = if asc {
        let start = from + offset as u64;
        if start > to { vec![] } else { st.storage.get_blocks_from(start, limit.min((to - start + 1) as usize))? }
    } else {
        let end = to.saturating_sub(offset as u64);
        if end < from || offset as u64 > to {
            vec![]
        } else {
            let start = end.saturating_sub(limit as u64 - 1).max(from);
            let mut v = st.storage.get_blocks_from(start, (end - start + 1) as usize)?;
            v.reverse();
            v
        }
    };
    let data = blocks.iter().map(|b| block_json(&st, b, raw, tip)).collect::<Result<Vec<_>, _>>()?;
    Ok(Json(paginate("/blocks", &q, total, data)))
}

/// Blocks of one generator inside `[from, to]`; scans newest-first, bounded to keep the API responsive.
fn by_generator(st: &AppState, q: &Params, pk: &str, from: u64, to: u64, raw: bool, tip: u64) -> ApiResult {
    let (page, limit) = page_params(q);
    let mut found: Vec<Value> = Vec::new();
    let mut total = 0usize;
    let mut h = to;
    let mut scanned = 0u64;
    while h >= from && scanned < 200_000 {
        if let Some(b) = st.storage.get_block_by_height(h)? {
            if b.generator_public_key == pk {
                total += 1;
                if total > (page - 1) * limit && found.len() < limit {
                    found.push(block_json(st, &b, raw, tip)?);
                }
            }
        }
        if h == 0 {
            break;
        }
        h -= 1;
        scanned += 1;
    }
    if q.ascending() {
        found.reverse();
    }
    Ok(Json(paginate("/blocks", q, total, found)))
}

pub fn resolve_block(st: &AppState, id: &str) -> Result<Block, ApiError> {
    let block = if id.len() == 64 {
        st.storage.get_block_by_id(id)?
    } else {
        id.parse::<u64>().ok().map(|h| st.storage.get_block_by_height(h)).transpose()?.flatten()
    };
    block.ok_or_else(|| not_found("Block"))
}

pub async fn by_id(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let b = resolve_block(&st, &id)?;
    Ok(Json(serde_json::json!({ "data": block_json(&st, &b, !transform(&q), last_height(&st)?)? })))
}

pub async fn first(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let b = st.storage.get_block_by_height(1)?.ok_or_else(|| not_found("Block"))?;
    Ok(Json(serde_json::json!({ "data": block_json(&st, &b, !transform(&q), last_height(&st)?)? })))
}

pub async fn last(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let b = st.storage.get_last_block()?.ok_or_else(|| not_found("Block"))?;
    Ok(Json(serde_json::json!({ "data": block_json(&st, &b, !transform(&q), last_height(&st)?)? })))
}

pub async fn transactions(State(st): State<Shared>, Path(id): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let b = resolve_block(&st, &id)?;
    let tip = last_height(&st)?;
    let raw = !transform(&q);
    let mut txs: Vec<&crate::models::Transaction> = b.transactions.iter().filter(|t| super::transactions::matches(&st, t, &q)).collect();
    if q.ascending() {
        // block order is already ascending by sequence
    } else if q.get("orderBy").is_some() {
        txs.reverse();
    }
    let items = txs.iter().map(|t| tx_json(&st, t, Some(b.timestamp), raw, tip)).collect::<Result<Vec<_>, _>>()?;
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate(&format!("/blocks/{id}/transactions"), &q, total, page)))
}
