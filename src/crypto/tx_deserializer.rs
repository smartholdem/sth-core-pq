//! Author: TechnoL0g
//!
//! SHIP-11 (v2) transaction wire deserialisation — inverse of `tx_serializer.rs`, port of
//! `Transactions.Deserializer` + per-type `deserialize()` (used for snapshot import and P2P).

use super::address::address_from_bytes;
use super::bytes::ByteReader;
use super::hash::sha256;
use crate::error::{Error, Result};
use crate::models::{
    tx_type, DelegateAsset, HtlcClaimAsset, HtlcExpiration, HtlcLockAsset, HtlcRefundAsset, MultiSignatureAsset,
    Payment, SecondSignatureAsset, Transaction, TransactionAsset, TYPE_GROUP_CORE,
};

fn read_address(r: &mut ByteReader) -> Result<String> {
    Ok(address_from_bytes(r.bytes(21)?))
}

fn asset_mut(tx: &mut Transaction) -> &mut TransactionAsset {
    tx.asset.get_or_insert_with(TransactionAsset::default)
}

fn deserialize_type_payload(tx: &mut Transaction, r: &mut ByteReader) -> Result<()> {
    if tx.is_sobj() {
        let (type_, sub_type, action) = (r.u8()?, r.u8()?, r.u8()?);
        let reg_len = r.u8()? as usize;
        let registration_id = (reg_len > 0).then(|| r.hex(reg_len)).transpose()?;
        let name_len = r.u8()? as usize;
        let name = (name_len > 0).then(|| r.bytes(name_len).map(|b| String::from_utf8_lossy(b).into_owned())).transpose()?;
        let ipfs_len = r.u8()? as usize;
        let slot = (ipfs_len > 0).then(|| r.bytes(ipfs_len).map(|b| String::from_utf8_lossy(b).into_owned())).transpose()?;
        let transfer = action == crate::models::sobj::ACTION_TRANSFER;
        let sell = action == crate::models::sobj::ACTION_SELL;
        let e = crate::models::SmartObjectAsset {
            type_, sub_type, action, registration_id,
            recipient_id: if transfer { slot.clone() } else { None },
            price: if sell { Some(slot.as_deref().unwrap_or("0").parse().map_err(|_| Error::Serialization("sObject price is not a number".into()))?) } else { None },
            data: crate::models::SmartObjectData { name, ntfry_data: if transfer || sell { None } else { slot } },
        };
        asset_mut(tx).extra = e.into_map();
        return Ok(());
    }
    if tx.is_token() {
        use crate::models::{token, TokenAsset, TokenTransferItem};
        let mut a = TokenAsset { id: r.hex(32)?, ..Default::default() };
        match tx.type_ {
            token::INIT => {
                a.decimals = Some(r.u8()?);
                a.flags = Some(r.u8()?);
                a.initial_supply = Some(r.u64_le()?);
                a.supply_cap = Some(r.u64_le()?);
            }
            token::TRANSFER => {
                let n = r.u16_le()? as usize;
                let mut items = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    let amount = r.u64_le()?;
                    items.push(TokenTransferItem { recipient_id: read_address(r)?, amount });
                }
                a.transfers = Some(items);
                let ml = r.u8()? as usize;
                let memo = String::from_utf8_lossy(r.bytes(ml)?).into_owned();
                a.memo = (!memo.is_empty()).then_some(memo);
            }
            token::MINT => {
                a.amount = Some(r.u64_le()?);
                a.recipient_id = Some(read_address(r)?);
            }
            token::META => {
                use base64::Engine;
                let text = |r: &mut ByteReader, n: usize| -> Result<Option<String>> {
                    let s = String::from_utf8_lossy(r.bytes(n)?).into_owned();
                    Ok((!s.is_empty()).then_some(s))
                };
                let nl = r.u8()? as usize;
                let name = text(r, nl)?.unwrap_or_default();
                let dl = r.u16_le()? as usize;
                let description = text(r, dl)?;
                let sl = r.u8()? as usize;
                let website = text(r, sl)?;
                let lt = r.u8()?;
                let ll = r.u16_le()? as usize;
                let logo_raw = r.bytes(ll)?;
                let logo_type = crate::models::TokenMeta::logo_type_name(lt).map(str::to_string);
                let logo = (ll > 0).then(|| base64::engine::general_purpose::STANDARD.encode(logo_raw));
                a.meta = Some(crate::models::TokenMeta { name, description, website, logo_type, logo });
            }
            _ => a.amount = Some(r.u64_le()?),
        }
        asset_mut(tx).extra.insert("token".into(), serde_json::to_value(a).unwrap_or_default());
        return Ok(());
    }
    if tx.type_group != TYPE_GROUP_CORE {
        return Err(Error::Serialization(format!("unsupported typeGroup {}", tx.type_group)));
    }
    match tx.type_ {
        tx_type::TRANSFER => {
            tx.amount = r.u64_le()?;
            tx.expiration = Some(r.u32_le()?);
            tx.recipient_id = Some(read_address(r)?);
        }
        tx_type::SECOND_SIGNATURE => {
            if tx.is_pq() {
                let algorithm = r.u8()?;
                let len = r.u16_le()? as usize;
                let public_key = r.hex(len)?;
                asset_mut(tx).signature = Some(SecondSignatureAsset { public_key, algorithm: Some(algorithm) });
            } else {
                let public_key = r.hex(33)?;
                asset_mut(tx).signature = Some(SecondSignatureAsset { public_key, algorithm: None });
            }
        }
        tx_type::DELEGATE_REGISTRATION => {
            let len = r.u8()? as usize;
            let username = String::from_utf8(r.bytes(len)?.to_vec())
                .map_err(|e| Error::Serialization(format!("delegate username is not utf-8: {e}")))?;
            asset_mut(tx).delegate = Some(DelegateAsset { username });
        }
        tx_type::VOTE => {
            let count = r.u8()? as usize;
            let mut votes = Vec::with_capacity(count);
            for _ in 0..count {
                let sign = if r.u8()? == 1 { "+" } else { "-" };
                votes.push(format!("{sign}{}", r.hex(33)?));
            }
            asset_mut(tx).votes = Some(votes);
        }
        tx_type::MULTI_SIGNATURE => {
            let min = r.u8()?;
            let count = r.u8()? as usize;
            let mut public_keys = Vec::with_capacity(count);
            for _ in 0..count {
                public_keys.push(r.hex(33)?);
            }
            asset_mut(tx).multi_signature = Some(MultiSignatureAsset { min, public_keys });
        }
        tx_type::IPFS => {
            // multihash: <fn><len><digest>, base58-encoded as a whole
            let start = r.position();
            let _hash_fn = r.u8()?;
            let len = r.u8()? as usize;
            r.bytes(len)?;
            let raw = r.slice(start, r.position());
            asset_mut(tx).ipfs = Some(bs58::encode(raw).into_string());
        }
        tx_type::MULTI_PAYMENT => {
            let count = r.u16_le()? as usize;
            let mut payments = Vec::with_capacity(count);
            for _ in 0..count {
                let amount = r.u64_le()?;
                let recipient_id = read_address(r)?;
                payments.push(Payment { amount, recipient_id });
            }
            tx.amount = 0;
            asset_mut(tx).payments = Some(payments);
        }
        tx_type::DELEGATE_RESIGNATION => {}
        tx_type::HTLC_LOCK => {
            tx.amount = r.u64_le()?;
            let secret_hash = r.hex(32)?;
            let type_ = r.u8()?;
            let value = r.u32_le()?;
            tx.recipient_id = Some(read_address(r)?);
            asset_mut(tx).lock = Some(HtlcLockAsset { secret_hash, expiration: HtlcExpiration { type_, value } });
        }
        tx_type::HTLC_CLAIM => {
            let lock_transaction_id = r.hex(32)?;
            let unlock_secret = r.hex(32)?;
            asset_mut(tx).claim = Some(HtlcClaimAsset { lock_transaction_id, unlock_secret });
        }
        tx_type::HTLC_REFUND => {
            let lock_transaction_id = r.hex(32)?;
            asset_mut(tx).refund = Some(HtlcRefundAsset { lock_transaction_id });
        }
        other => return Err(Error::Serialization(format!("unsupported transaction type {other}"))),
    }
    Ok(())
}

fn detect_schnorr(remaining: usize) -> bool {
    remaining == 64
        || remaining == 128
        || remaining % 65 == 0
        || (remaining >= 64 && (remaining - 64) % 65 == 0)
        || (remaining >= 128 && (remaining - 128) % 65 == 0)
}

/// v3: `sig1 (64) || (alg u8 || len u16 LE || sig)* || [0xff || multisig 65-byte parts]`.
fn deserialize_signatures_v3(tx: &mut Transaction, r: &mut ByteReader) -> Result<()> {
    if r.remaining() == 0 {
        return Ok(());
    }
    tx.signature = Some(r.hex(64)?);
    let mut blocks = Vec::new();
    while r.remaining() > 0 && r.peek_u8(0) != Some(0xff) {
        let algorithm = r.u8()?;
        let len = r.u16_le()? as usize;
        blocks.push(crate::models::PqSignatureBlock { algorithm, signature: r.hex(len)? });
    }
    if !blocks.is_empty() {
        tx.second_signatures = Some(blocks);
    }
    if r.remaining() > 0 {
        r.u8()?;
        if r.remaining() % 65 != 0 {
            return Err(Error::Serialization("v3 multisignature buffer not exhausted".into()));
        }
        let mut sigs = Vec::with_capacity(r.remaining() / 65);
        while r.remaining() > 0 {
            sigs.push(r.hex(65)?);
        }
        tx.signatures = Some(sigs);
    }
    Ok(())
}

fn deserialize_signatures(tx: &mut Transaction, r: &mut ByteReader) -> Result<()> {
    if tx.is_pq() {
        return deserialize_signatures_v3(tx, r);
    }
    if detect_schnorr(r.remaining()) {
        let can_read_single = |r: &ByteReader| r.remaining() > 0 && (r.remaining() % 64 == 0 || r.remaining() % 65 != 0);
        if can_read_single(r) {
            tx.signature = Some(r.hex(64)?);
        }
        if can_read_single(r) {
            tx.second_signature = Some(r.hex(64)?);
        }
        if r.remaining() > 0 {
            if r.remaining() % 65 != 0 {
                return Err(Error::Serialization("signature buffer not exhausted".into()));
            }
            let count = r.remaining() / 65;
            let mut sigs = Vec::with_capacity(count);
            let mut seen = std::collections::HashSet::new();
            for _ in 0..count {
                let part = r.hex(65)?;
                if !seen.insert(part[..2].to_string()) {
                    return Err(Error::Serialization("duplicate participant in multi signature".into()));
                }
                sigs.push(part);
            }
            tx.signatures = Some(sigs);
        }
        return Ok(());
    }

    // DER ECDSA
    let der_len = |r: &ByteReader| -> Result<usize> {
        r.peek_u8(1)
            .map(|l| l as usize + 2)
            .ok_or_else(|| Error::Serialization("truncated DER signature".into()))
    };
    if r.remaining() > 0 {
        let n = der_len(r)?;
        tx.signature = Some(r.hex(n)?);
    }
    if r.remaining() > 0 && r.peek_u8(0) != Some(0xff) {
        let n = der_len(r)?;
        tx.second_signature = Some(r.hex(n)?);
    }
    if r.remaining() > 0 && r.peek_u8(0) == Some(0xff) {
        r.u8()?;
        tx.signatures = Some(vec![hex::encode(r.rest())]);
    }
    if r.remaining() > 0 {
        return Err(Error::Serialization("signature buffer not exhausted".into()));
    }
    Ok(())
}

/// Parse full SHIP-11 (v2/v3) bytes into a `Transaction` (id = sha256(bytes)).
pub fn deserialize_transaction(bytes: &[u8]) -> Result<Transaction> {
    let mut r = ByteReader::new(bytes);
    if r.u8()? != 0xff {
        return Err(Error::Serialization("missing 0xff transaction marker".into()));
    }
    let version = r.u8()?;
    let network = r.u8()?;
    if version == 1 {
        return Err(Error::Serialization("legacy v1 transactions are not supported on this chain".into()));
    }
    let type_group = r.u32_le()?;
    let type_ = r.u16_le()?;
    let nonce = r.u64_le()?;
    let sender_public_key = r.hex(33)?;
    let fee = r.u64_le()?;

    let mut tx = Transaction {
        version,
        network: Some(network),
        type_group,
        type_,
        nonce: Some(nonce),
        sender_public_key,
        fee,
        amount: 0,
        vendor_field: None,
        expiration: None,
        recipient_id: None,
        asset: None,
        signature: None,
        second_signature: None,
        sign_signature: None,
        signatures: None,
        second_signatures: None,
        timestamp: None,
        id: None,
        block_id: None,
        block_height: None,
        sequence: None,
    };

    let vf_len = r.u8()? as usize;
    if vf_len > 0 {
        let raw = r.bytes(vf_len)?;
        if tx.has_vendor_field() {
            tx.vendor_field = Some(String::from_utf8_lossy(raw).into_owned());
        }
    }

    deserialize_type_payload(&mut tx, &mut r)?;
    deserialize_signatures(&mut tx, &mut r)?;
    tx.id = Some(hex::encode(sha256(bytes)));
    Ok(tx)
}
