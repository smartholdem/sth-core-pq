//! Author: TechnoL0g
//! `/api/tokens*` — native tokens (typeGroup 3). See docs/TOKENS_RU.md.

use super::render::human_time;
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

/// SHIP-43 order → JSON (`total` = remaining × price in smartoshi).
pub fn order_json(o: &crate::storage::TokenOrder, t: Option<&TokenState>) -> Value {
    json!({
        "orderId": o.id,
        "tokenId": o.token_id,
        "symbol": t.map(|t| t.symbol.clone()),
        "decimals": t.map(|t| t.decimals),
        "seller": o.seller,
        "price": o.price.to_string(),
        "amount": o.amount.to_string(),
        "filled": o.filled.to_string(),
        "remaining": o.remaining().to_string(),
        "total": (o.remaining() as u128 * o.price as u128).to_string(),
        "height": o.height,
        "open": o.is_open(),
        "closed": o.closed,
        "fills": o.fills,
    })
}

/// Open asks of a token, cheapest first.
pub async fn orders(State(st): State<Shared>, Path(key): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let (id, t) = resolve(&st, &key)?;
    let items: Vec<Value> = st.storage.token_orders(&id)?.iter().map(|o| order_json(o, Some(&t))).collect();
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate(&format!("/tokens/{key}/orders"), &q, total, page)))
}

/// One order (open or closed) with its fills.
pub async fn order(State(st): State<Shared>, Path((key, order_id)): Path<(String, String)>) -> ApiResult {
    let (id, t) = resolve(&st, &key)?;
    let o = st.storage.token_order(&order_id)?.filter(|o| o.token_id == id).ok_or_else(|| not_found("Order"))?;
    Ok(Json(json!({ "data": order_json(&o, Some(&t)) })))
}

/// Executed fills, newest first: `txId, buyer, seller, amount, price, total, height, timestamp`. With `groupBy=day`
/// (or `hour`) — OHLCV candles oldest first over the last `days` (default 30, max 365, 0 = all) for price charts.
pub async fn trades(State(st): State<Shared>, Path(key): Path<String>, Query(q): Query<Params>) -> ApiResult {
    let (id, t) = resolve(&st, &key)?;
    let group = q.get("groupBy").map(String::as_str);
    if let Some(bucket) = match group { Some("day") => Some(86_400u32), Some("hour") => Some(3_600u32), _ => None } {
        // days=0 → whole history
        let days = q.parse::<u32>("days").unwrap_or(30).min(365);
        let since = if days == 0 { 0 } else { st.network.now_epoch().saturating_sub(days * 86_400) };
        // slot → (open, high, low, close, volume, value, trades); trades arrive newest first
        let mut candles: std::collections::BTreeMap<u32, (u64, u64, u64, u64, u64, u64, u64)> = Default::default();
        for tr in st.storage.token_trades(&id) {
            let tr = tr?;
            if tr.timestamp < since {
                break;
            }
            let slot = tr.timestamp - tr.timestamp % bucket;
            let c = candles.entry(slot).or_insert((tr.price, tr.price, tr.price, tr.price, 0, 0, 0));
            c.0 = tr.price;
            c.1 = c.1.max(tr.price);
            c.2 = c.2.min(tr.price);
            c.4 += tr.amount;
            c.5 += tr.total;
            c.6 += 1;
        }
        let data: Vec<Value> = candles
            .iter()
            .map(|(slot, c)| json!({ "timestamp": slot, "time": human_time(st.network.epoch_to_unix(*slot)), "open": c.0.to_string(), "high": c.1.to_string(), "low": c.2.to_string(), "close": c.3.to_string(), "volume": c.4.to_string(), "value": c.5.to_string(), "trades": c.6 }))
            .collect();
        return Ok(Json(json!({ "meta": { "groupBy": group, "days": days, "symbol": t.symbol, "decimals": t.decimals, "count": data.len() }, "data": data })));
    }
    let (page, limit) = super::page_params(&q);
    let mut total = 0usize;
    let mut data = Vec::with_capacity(limit);
    for tr in st.storage.token_trades(&id) {
        let tr = tr?;
        total += 1;
        if total > (page - 1) * limit && data.len() < limit {
            data.push(json!({ "txId": tr.tx_id, "orderId": tr.order_id, "buyer": tr.buyer, "seller": tr.seller, "amount": tr.amount.to_string(), "price": tr.price.to_string(), "total": tr.total.to_string(), "height": tr.height, "timestamp": tr.timestamp, "time": human_time(st.network.epoch_to_unix(tr.timestamp)), "symbol": t.symbol, "decimals": t.decimals }));
        }
    }
    Ok(Json(paginate(&format!("/tokens/{key}/trades"), &q, total, data)))
}

/// Open orders of a wallet across tokens.
pub async fn wallet_orders(State(st): State<Shared>, Path(address): Path<String>) -> ApiResult {
    let items: Vec<Value> = st
        .storage
        .wallet_orders(&address)?
        .iter()
        .map(|o| order_json(o, st.storage.token_state(&o.token_id).ok().flatten().as_ref()))
        .collect();
    Ok(Json(json!({ "data": items })))
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
