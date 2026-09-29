//! Author: TechnoL0g
//!
//! `/api/sobj*` — smart objects (sObjects), legacy-compatible response shape:
//! `{ id, address, publicKey, isResigned, type, subType, data: { name, ntfryData } }`.

use super::{not_found, paginate, slice_page, ApiResult, Params, Shared};
use crate::storage::SmartObject;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde_json::{json, Value};

fn sobj_json(st: &Shared, id: &str, owner: &str, rec: &SmartObject) -> Value {
    let public_key = st.storage.get_wallet(owner).ok().flatten().and_then(|w| w.public_key);
    json!({
        "id": id, "address": owner, "publicKey": public_key, "isResigned": rec.resigned,
        "type": rec.type_, "subType": rec.sub_type,
        "data": { "name": rec.data.name, "ntfryData": rec.data.ntfry_data },
        "price": rec.price.map(|p| p.to_string()), "forSale": rec.price.is_some(),
    })
}

fn filter(items: &mut Vec<Value>, q: &serde_json::Map<String, Value>) {
    let eq = |item: &Value, key: &str, want: &Value| match (item.pointer(key), want) {
        (Some(Value::String(a)), Value::String(b)) => a.eq_ignore_ascii_case(b),
        (Some(a), b) => a == b,
        _ => false,
    };
    for (key, path) in [("type", "/type"), ("subType", "/subType"), ("isResigned", "/isResigned"), ("address", "/address"), ("publicKey", "/publicKey"), ("id", "/id")] {
        if let Some(v) = q.get(key) {
            let want = match v {
                Value::String(s) if key != "address" && key != "publicKey" && key != "id" => serde_json::from_str(s).unwrap_or(Value::String(s.clone())),
                other => other.clone(),
            };
            items.retain(|i| eq(i, path, &want));
        }
    }
    if let Some(Value::String(n)) = q.get("name") {
        items.retain(|i| i["data"]["name"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(n)));
    }
}

fn all(st: &Shared) -> Result<Vec<Value>, crate::error::Error> {
    let mut items: Vec<Value> = st.storage.all_sobjects()?.iter().map(|(id, owner, rec)| sobj_json(st, id, owner, rec)).collect();
    items.sort_by(|a, b| a["data"]["name"].as_str().cmp(&b["data"]["name"].as_str()));
    Ok(items)
}

pub async fn list(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let mut items = all(&st)?;
    let filters: serde_json::Map<String, Value> = q.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
    filter(&mut items, &filters);
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/sobj", &q, total, page)))
}

pub async fn by_id(State(st): State<Shared>, Path(id): Path<String>) -> ApiResult {
    let (owner, rec) = st.storage.get_sobject(&id)?.ok_or_else(|| not_found("SmartObject"))?;
    Ok(Json(json!({ "data": sobj_json(&st, &id, &owner, &rec) })))
}

pub async fn search(State(st): State<Shared>, Query(q): Query<Params>, Json(body): Json<Value>) -> ApiResult {
    let mut items = all(&st)?;
    if let Value::Object(f) = body {
        filter(&mut items, &f);
    }
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/sobj/search", &q, total, page)))
}

/// SHIP-42 stage 1: Service Nodes = type-7 sObjects. `active` = not resigned, collateral floor met and the EndpointId is
/// not claimed by an earlier registration (first registration by height owns it).
pub fn service_nodes(st: &Shared) -> Result<Vec<Value>, super::ApiError> {
    let tip = super::last_height(st)?;
    let ms = st.network.milestone(tip.max(1));
    let collateral = ms.masternodes.collateral_amount;
    let mut nodes: Vec<(u64, Value)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut all: Vec<(String, String, SmartObject, u64)> = Vec::new();
    for (id, owner, rec) in st.storage.all_sobjects()? {
        if rec.type_ != crate::models::sobj::TYPE_SERVICE_NODE {
            continue;
        }
        let height = st.storage.get_transaction_with_timestamp(&id)?.and_then(|(tx, _)| tx.block_height).unwrap_or(0);
        all.push((id, owner, rec, height));
    }
    all.sort_by_key(|(_, _, _, h)| *h);
    for (id, owner, rec, since) in all {
        let Some((endpoint, roles, api, storage_gb)) = crate::models::sobj::parse_service_node(rec.data.ntfry_data.as_deref().unwrap_or_default()) else { continue };
        let wallet = st.storage.get_wallet(&owner)?;
        let balance = wallet.as_ref().map(|w| w.balance).unwrap_or(0);
        let duplicate = !rec.resigned && !seen.insert(endpoint.clone());
        let reason = if rec.resigned { Some("resigned") } else if balance < collateral as i64 { Some("collateral") } else if duplicate { Some("endpointTaken") } else { None };
        nodes.push((since, json!({
            "id": id,
            "name": rec.data.name,
            "address": owner,
            "publicKey": wallet.as_ref().and_then(|w| w.public_key.clone()),
            "endpointId": endpoint,
            "roles": roles,
            "api": api,
            "storageGb": storage_gb,
            "collateral": collateral.to_string(),
            "balance": balance.to_string(),
            "active": reason.is_none(),
            "inactiveReason": reason,
            "since": since,
            "ntfryData": rec.data.ntfry_data,
        })));
    }
    Ok(nodes.into_iter().map(|(_, v)| v).collect())
}

/// `GET /api/masternodes?active=true&role=relay` — Service Node registry.
pub async fn masternodes(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let mut items = service_nodes(&st)?;
    if let Some(a) = q.get("active") {
        let want = a == "true";
        items.retain(|n| n["active"].as_bool() == Some(want));
    }
    if let Some(r) = q.get("role") {
        items.retain(|n| n["roles"].as_array().is_some_and(|rs| rs.iter().any(|x| x == r)));
    }
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/masternodes", &q, total, page)))
}

/// `GET /api/masternodes/:id` — by registration id, name, address or EndpointId.
pub async fn masternode(State(st): State<Shared>, Path(id): Path<String>) -> ApiResult {
    let key = id.to_ascii_lowercase();
    let node = service_nodes(&st)?
        .into_iter()
        .find(|n| n["id"] == id || n["address"] == id || n["endpointId"] == key || n["name"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(&id)))
        .ok_or_else(|| not_found("Service Node"))?;
    Ok(Json(json!({ "data": node })))
}
