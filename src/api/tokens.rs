//! Author: TechnoL0g
//! `/api/tokens*` — native tokens (typeGroup 3). See docs/TOKENS_RU.md.

use super::{not_found, paginate, slice_page, ApiResult, Params, Shared};
use crate::storage::TokenState;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde_json::{json, Value};

/// Manifest without the logo bytes (`logo` is served by `/api/tokens/{id}/logo`).
pub fn meta_json(id: &str, t: &TokenState) -> Value {
    match &t.meta {
        Some(m) => json!({
            "name": m.name, "description": m.description, "website": m.website,
            "logoType": m.logo_type, "logoSize": m.logo_bytes().map(|b| b.len()).unwrap_or(0),
            "logoUrl": m.logo.as_ref().map(|_| format!("/api/tokens/{id}/logo")),
        }),
        None => Value::Null,
    }
}

pub fn token_json(st: &Shared, id: &str, t: &TokenState) -> Value {
    let sobj = st.storage.get_sobject(id).ok().flatten().map(|(_, rec)| json!({ "ntfryData": rec.data.ntfry_data, "resigned": rec.resigned, "price": rec.price.map(|p| p.to_string()), "forSale": rec.price.is_some() }));
    json!({
        "id": id, "symbol": t.symbol, "decimals": t.decimals, "flags": t.flags,
        "mintable": t.flags & crate::models::token::FLAG_MINTABLE != 0,
        "burnable": t.flags & crate::models::token::FLAG_BURNABLE != 0,
        "supply": t.supply.to_string(), "supplyCap": t.supply_cap.to_string(),
        "owner": t.owner, "initHeight": t.init_height, "sobj": sobj,
        "meta": meta_json(id, t),
    })
}

/// Logo bytes (SVG / PNG) straight from the chain, cacheable forever per (id, content).
pub async fn logo(State(st): State<Shared>, Path(key): Path<String>) -> Result<axum::response::Response, super::ApiError> {
    use axum::response::IntoResponse;
    let (_, t) = resolve(&st, &key)?;
    let m = t.meta.as_ref().filter(|m| m.logo.is_some()).ok_or_else(|| not_found("Logo"))?;
    let bytes = m.logo_bytes().ok_or_else(|| not_found("Logo"))?;
    let headers = [
        (axum::http::header::CONTENT_TYPE, m.mime().unwrap_or("application/octet-stream").to_string()),
        (axum::http::header::CACHE_CONTROL, "public, max-age=3600".to_string()),
        (axum::http::header::CONTENT_SECURITY_POLICY, "default-src 'none'; style-src 'unsafe-inline'".to_string()),
    ];
    Ok((headers, bytes).into_response())
}

fn resolve(st: &Shared, key: &str) -> Result<(String, TokenState), super::ApiError> {
    let id = if key.len() == 64 { key.to_string() } else { st.storage.token_id_by_symbol(&key.to_uppercase())?.ok_or_else(|| not_found("Token"))? };
    let t = st.storage.token_state(&id)?.ok_or_else(|| not_found("Token"))?;
    Ok((id, t))
}

pub async fn list(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let mut items: Vec<Value> = st.storage.all_tokens()?.iter().map(|(id, t)| token_json(&st, id, t)).collect();
    items.sort_by_key(|v| v["initHeight"].as_u64().unwrap_or(0));
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/tokens", &q, total, page)))
}

pub async fn by_id(State(st): State<Shared>, Path(key): Path<String>) -> ApiResult {
    let (id, t) = resolve(&st, &key)?;
    Ok(Json(json!({ "data": token_json(&st, &id, &t) })))
}

pub async fn holders(State(st): State<Shared>, Path(key): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let (id, t) = resolve(&st, &key)?;
    let items: Vec<Value> = st
        .storage
        .token_holders(&id)?
        .into_iter()
        .map(|(address, balance)| json!({ "address": address, "balance": balance.to_string(), "symbol": t.symbol, "decimals": t.decimals }))
        .collect();
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate(&format!("/tokens/{key}/holders"), &q, total, page)))
}

/// Token balances of one wallet: `[{ id, symbol, balance, decimals }]`.
pub async fn wallet_tokens(State(st): State<Shared>, Path(address): Path<String>) -> ApiResult {
    let w = st.storage.get_wallet(&address)?.ok_or_else(|| not_found("Wallet"))?;
    let data: Vec<Value> = w
        .tokens
        .iter()
        .map(|(id, bal)| {
            let t = st.storage.token_state(id).ok().flatten();
            json!({ "id": id, "balance": bal.to_string(), "symbol": t.as_ref().map(|t| t.symbol.clone()), "decimals": t.as_ref().map(|t| t.decimals) })
        })
        .collect();
    Ok(Json(json!({ "data": data })))
}
