//! Author: TechnoL0g
//!
//! Legacy "transformed" JSON renderers: block, transaction, wallet, delegate, lock.

use super::{AppState, ApiError};
use crate::config::{Network, TOTAL_SUPPLY};
use crate::error::Error;
use crate::models::{Block, Transaction};
use crate::storage::{LockRecord, WalletState};
use serde_json::{json, Map, Value};

/// `1788383912` → `2026-09-02T21:18:32.000Z`
pub fn human_time(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z", secs / 3600, (secs % 3600) / 60, secs % 60)
}

pub fn timestamp_json(net: &Network, epoch: u32) -> Value {
    let unix = net.epoch_to_unix(epoch);
    json!({ "epoch": epoch, "unix": unix, "human": human_time(unix) })
}

pub fn sender_address(st: &AppState, tx: &Transaction) -> String {
    crate::crypto::address_from_public_key(&tx.sender_public_key, st.network.pubkey_hash).unwrap_or_default()
}

pub fn block_json(st: &AppState, block: &Block, raw: bool, tip: u64) -> Result<Value, ApiError> {
    if raw {
        return Ok(serde_json::to_value(block.header()).map_err(Error::Json)?);
    }
    let generator = st.storage.find_wallet(&block.generator_public_key)?;
    let address = generator
        .as_ref()
        .map(|w| w.address.clone())
        .or_else(|| crate::crypto::address_from_public_key(&block.generator_public_key, st.network.pubkey_hash).ok())
        .unwrap_or_default();
    let mut gen = Map::new();
    if let Some(u) = generator.as_ref().and_then(|w| w.username.clone()) {
        gen.insert("username".into(), Value::String(u));
    }
    gen.insert("address".into(), Value::String(address));
    gen.insert("publicKey".into(), Value::String(block.generator_public_key.clone()));
    Ok(json!({
        "id": block.id,
        "version": block.version,
        "height": block.height,
        "previous": block.previous_block,
        "forged": {
            "reward": block.reward.to_string(),
            "fee": block.total_fee.to_string(),
            "amount": block.total_amount.to_string(),
            "total": (block.reward + block.total_fee).to_string(),
        },
        "payload": { "hash": block.payload_hash, "length": block.payload_length },
        "generator": Value::Object(gen),
        "signature": block.block_signature,
        "confirmations": tip.saturating_sub(block.height),
        "transactions": block.number_of_transactions,
        "timestamp": timestamp_json(&st.network, block.timestamp),
    }))
}

pub fn tx_json(st: &AppState, tx: &Transaction, block_ts: Option<u32>, raw: bool, tip: u64) -> Result<Value, ApiError> {
    if raw {
        return Ok(serde_json::to_value(tx).map_err(Error::Json)?);
    }
    let mut v = Map::new();
    v.insert("id".into(), json!(tx.id));
    if let Some(b) = &tx.block_id {
        v.insert("blockId".into(), json!(b));
    }
    v.insert("version".into(), json!(tx.version));
    v.insert("type".into(), json!(tx.type_));
    v.insert("typeGroup".into(), json!(tx.type_group));
    v.insert("amount".into(), json!(tx.amount.to_string()));
    v.insert("fee".into(), json!(tx.fee.to_string()));
    v.insert("sender".into(), json!(sender_address(st, tx)));
    v.insert("senderPublicKey".into(), json!(tx.sender_public_key));
    if let Some(r) = &tx.recipient_id {
        v.insert("recipient".into(), json!(r));
    }
    v.insert("signature".into(), json!(tx.signature));
    if let Some(s) = tx.second_signature_any() {
        v.insert("signSignature".into(), json!(s));
    }
    if let Some(s) = &tx.signatures {
        v.insert("signatures".into(), json!(s));
    }
    if let Some(vf) = &tx.vendor_field {
        v.insert("vendorField".into(), json!(vf));
    }
    if let Some(a) = &tx.asset {
        v.insert("asset".into(), serde_json::to_value(a).map_err(Error::Json)?);
    }
    match (tx.block_height, block_ts) {
        (Some(h), Some(ts)) => {
            v.insert("confirmations".into(), json!(tip.saturating_sub(h) + 1));
            v.insert("timestamp".into(), timestamp_json(&st.network, ts));
        }
        _ => {
            v.insert("confirmations".into(), json!(0));
        }
    }
    v.insert("nonce".into(), json!(tx.nonce.unwrap_or(0).to_string()));
    Ok(Value::Object(v))
}

pub fn lock_json(st: &AppState, l: &LockRecord) -> Value {
    let now = st.network.now_epoch();
    let tip = st.storage.get_last_height().unwrap_or(0);
    // expiration type 1 = epoch timestamp, 2 = block height
    let expired = match l.expiration.type_ {
        1 => (l.expiration.value as u64) <= now as u64,
        _ => (l.expiration.value as u64) <= tip,
    };
    let mut v = json!({
        "lockId": l.lock_id,
        "amount": l.amount.to_string(),
        "secretHash": l.secret_hash,
        "senderPublicKey": l.sender_public_key,
        "recipientId": l.recipient_id,
        "timestamp": timestamp_json(&st.network, l.timestamp),
        "expirationType": l.expiration.type_,
        "expirationValue": l.expiration.value,
        "isExpired": expired,
    });
    if let Some(vf) = &l.vendor_field {
        v["vendorField"] = json!(vf);
    }
    v
}

pub fn wallet_json(st: &AppState, w: &WalletState, rank: Option<usize>, votes: Option<u64>) -> Value {
    let mut attrs = Map::new();
    if let Some(v) = &w.vote {
        attrs.insert("vote".into(), json!(v));
    }
    if let Some(s) = &w.second_public_key {
        attrs.insert("secondPublicKey".into(), json!(s));
    }
    if let Some(m) = &w.multi_signature {
        attrs.insert("multiSignature".into(), json!({ "min": m.min, "publicKeys": m.public_keys }));
    }
    if !w.locks.is_empty() {
        let locks: Map<String, Value> = w
            .locks
            .values()
            .map(|l| {
                (
                    l.lock_id.clone(),
                    json!({ "amount": l.amount.to_string(), "recipientId": l.recipient_id, "timestamp": l.timestamp, "vendorField": l.vendor_field,
                            "secretHash": l.secret_hash, "expiration": { "type": l.expiration.type_, "value": l.expiration.value } }),
                )
            })
            .collect();
        attrs.insert("htlc".into(), json!({ "locks": Value::Object(locks), "lockedBalance": w.locked_balance().to_string() }));
    }
    if let Some(u) = &w.username {
        let mut d = Map::new();
        d.insert("username".into(), json!(u));
        d.insert("voteBalance".into(), json!(votes.unwrap_or(0).to_string()));
        d.insert("forgedFees".into(), json!(w.forged_fees.to_string()));
        d.insert("forgedRewards".into(), json!(w.forged_rewards.to_string()));
        d.insert("producedBlocks".into(), json!(w.produced_blocks));
        if let Some(r) = rank {
            d.insert("rank".into(), json!(r));
        }
        if let Some(lb) = &w.last_block {
            d.insert("lastBlock".into(), json!({ "id": lb.id, "height": lb.height, "timestamp": timestamp_json(&st.network, lb.timestamp) }));
        }
        if w.resigned {
            d.insert("resigned".into(), json!(true));
        }
        attrs.insert("delegate".into(), Value::Object(d));
    }
    json!({
        "address": w.address,
        "publicKey": w.public_key,
        "balance": w.balance.to_string(),
        "nonce": w.nonce.to_string(),
        "attributes": Value::Object(attrs),
    })
}

pub fn delegate_json(st: &AppState, w: &WalletState, votes: u64, rank: usize) -> Value {
    let approval = (votes as f64 / TOTAL_SUPPLY as f64 * 10_000.0).round() / 100.0;
    let last = w.last_block.as_ref().map(|lb| json!({ "id": lb.id, "height": lb.height, "timestamp": timestamp_json(&st.network, lb.timestamp) }));
    json!({
        "username": w.username,
        "address": w.address,
        "publicKey": w.public_key,
        "votes": votes.to_string(),
        "rank": rank,
        "isResigned": w.resigned,
        "blocks": { "produced": w.produced_blocks, "last": last },
        "production": { "approval": approval },
        "forged": {
            "fees": w.forged_fees.to_string(),
            "rewards": w.forged_rewards.to_string(),
            "total": (w.forged_fees + w.forged_rewards).to_string(),
        },
    })
}
