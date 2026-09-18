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
