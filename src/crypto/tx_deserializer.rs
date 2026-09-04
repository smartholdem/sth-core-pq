//! Author: TechnoL0g
//!
//! AIP-11 (v2) transaction wire deserialisation — inverse of `tx_serializer.rs`, port of
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
    if tx.is_entity() {
        let (type_, sub_type, action) = (r.u8()?, r.u8()?, r.u8()?);
        let reg_len = r.u8()? as usize;
        let registration_id = (reg_len > 0).then(|| r.hex(reg_len)).transpose()?;
        let name_len = r.u8()? as usize;
        let name = (name_len > 0).then(|| r.bytes(name_len).map(|b| String::from_utf8_lossy(b).into_owned())).transpose()?;
        let ipfs_len = r.u8()? as usize;
        let ipfs_data = (ipfs_len > 0).then(|| r.bytes(ipfs_len).map(|b| String::from_utf8_lossy(b).into_owned())).transpose()?;
        let e = crate::models::EntityAsset { type_, sub_type, action, registration_id, data: crate::models::EntityData { name, ipfs_data } };
        asset_mut(tx).extra = e.into_map();
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
            let public_key = r.hex(33)?;
            asset_mut(tx).signature = Some(SecondSignatureAsset { public_key });
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

fn deserialize_signatures(tx: &mut Transaction, r: &mut ByteReader) -> Result<()> {
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

/// Parse full AIP-11 bytes into a `Transaction` (id = sha256(bytes)).
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
