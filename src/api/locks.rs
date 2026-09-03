//! Author: TechnoL0g
//!
//! `/api/locks*` — open HTLC locks with the legacy filter set; `POST /api/locks/unlocked` returns the
//! claim / refund transactions that closed the given lock ids.

use super::render::{lock_json, tx_json};
use super::{last_height, not_found, paginate, slice_page, transform, ApiError, ApiResult, Params, Shared};
use crate::models::tx_type;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

pub async fn list(State(st): State<Shared>, Query(q): Query<Params>) -> ApiResult {
    let mut locks = st.storage.open_locks()?;
    if let Some(v) = q.get("lockId") {
        locks.retain(|l| &l.lock_id == v);
    }
    if let Some(v) = q.get("senderPublicKey") {
        locks.retain(|l| &l.sender_public_key == v);
    }
    if let Some(v) = q.get("recipientId") {
        locks.retain(|l| &l.recipient_id == v);
    }
    if let Some(v) = q.get("secretHash") {
        locks.retain(|l| &l.secret_hash == v);
    }
    if let Some(v) = q.get("vendorField") {
        locks.retain(|l| l.vendor_field.as_deref() == Some(v.as_str()));
    }
    if let Some(v) = q.parse::<u8>("expirationType") {
        locks.retain(|l| l.expiration.type_ == v);
    }
    locks.retain(|l| q.range_ok("amount", l.amount) && q.range_ok("expirationValue", l.expiration.value as u64) && q.range_ok("timestamp.epoch", l.timestamp as u64));
    let mut items: Vec<Value> = locks.iter().map(|l| lock_json(&st, l)).collect();
    if let Some(v) = q.get("isExpired") {
        let want = v == "true";
        items.retain(|l| l["isExpired"].as_bool() == Some(want));
    }
    items.sort_by(|a, b| b["timestamp"]["epoch"].as_u64().cmp(&a["timestamp"]["epoch"].as_u64()));
    if q.ascending() {
        items.reverse();
    }
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/locks", &q, total, page)))
}

pub async fn by_id(State(st): State<Shared>, Path(id): Path<String>) -> ApiResult {
    let l = st.storage.get_lock(&id)?.ok_or_else(|| not_found("Lock"))?;
    Ok(Json(json!({ "data": lock_json(&st, &l) })))
}

#[derive(serde::Deserialize)]
struct UnlockedBody {
    ids: Vec<String>,
}

pub async fn unlocked(State(st): State<Shared>, Query(q): Query<Params>, body: axum::body::Bytes) -> ApiResult {
    let parsed: UnlockedBody =
        serde_json::from_slice(&body).map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("Invalid payload: {e}")))?;
    let tip = last_height(&st)?;
    let raw = !transform(&q);
    let mut items = Vec::new();
    for id in parsed.ids.iter().take(100) {
        // The lock itself is a transaction; a closed lock has a claim/refund whose asset references it.
        let Some(lock) = st.storage.get_transaction(id)? else { continue };
        if lock.type_ != tx_type::HTLC_LOCK {
            continue;
        }
        let Some(recipient) = lock.recipient_id.as_deref() else { continue };
        for cand in st.storage.transaction_ids(Some(recipient))? {
            let Some((tx, ts)) = st.storage.get_transaction_with_timestamp(&cand)? else { continue };
            let closes = match tx.type_ {
                tx_type::HTLC_CLAIM => tx.asset.as_ref().and_then(|a| a.claim.as_ref()).map(|c| &c.lock_transaction_id == id),
                tx_type::HTLC_REFUND => tx.asset.as_ref().and_then(|a| a.refund.as_ref()).map(|r| &r.lock_transaction_id == id),
                _ => None,
            };
            if closes == Some(true) {
                items.push(tx_json(&st, &tx, Some(ts), raw, tip)?);
                break;
            }
        }
    }
    let (total, page) = slice_page(items, &q);
    Ok(Json(paginate("/locks/unlocked", &q, total, page)))
}
