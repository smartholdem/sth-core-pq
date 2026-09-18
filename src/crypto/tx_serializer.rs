//! Author: TechnoL0g
//!
//! SHIP-11 (v2) transaction wire serialisation — byte-exact port of
//! `Transactions.Serializer` + per-type `serialize()` from `@smartholdem/crypto`.

use super::address::address_to_bytes;
use super::bytes::ByteWriter;
use super::hash::sha256;
use super::verify_signature;
use crate::config::Network;
use crate::error::{Error, Result};
use crate::models::{tx_type, Transaction, TransactionAsset, TYPE_GROUP_CORE};

/// Mirrors core's `ISerializeOptions`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SerializeOptions {
    pub exclude_signature: bool,
    pub exclude_second_signature: bool,
    pub exclude_multi_signature: bool,
}

impl SerializeOptions {
    /// Bytes that are hashed for the primary signature (`Verifier.verifyHash`).
    pub fn for_signing() -> Self {
        Self { exclude_signature: true, exclude_second_signature: true, exclude_multi_signature: false }
    }
}

fn asset<'a>(tx: &'a Transaction, what: &str) -> Result<&'a TransactionAsset> {
    tx.asset
        .as_ref()
        .ok_or_else(|| Error::Serialization(format!("missing asset for {what}")))
}

fn write_address(w: &mut ByteWriter, address: &str) -> Result<()> {
    w.bytes(&address_to_bytes(address)?);
    Ok(())
}

fn serialize_type_payload(tx: &Transaction, w: &mut ByteWriter) -> Result<()> {
    if tx.is_sobj() {
        let e = tx.sobj_asset().ok_or_else(|| Error::Serialization("missing sObject asset".into()))?;
        let reg = e.registration_id.as_deref().map(hex::decode).transpose().map_err(|e| Error::Serialization(format!("bad registrationId: {e}")))?.unwrap_or_default();
        let name = e.data.name.as_deref().unwrap_or("").as_bytes();
        let price = e.price.map(|p| p.to_string());
        let slot = match e.action {
            crate::models::sobj::ACTION_TRANSFER => e.recipient_id.as_deref(),
            crate::models::sobj::ACTION_SELL => price.as_deref(),
            _ => e.data.ntfry_data.as_deref(),
        };
        let ipfs = slot.unwrap_or("").as_bytes();
        if reg.len() > 255 || name.len() > 255 || ipfs.len() > 255 {
            return Err(Error::Serialization("sObject field longer than 255 bytes".into()));
        }
        w.u8(e.type_).u8(e.sub_type).u8(e.action);
        w.u8(reg.len() as u8).bytes(&reg);
        w.u8(name.len() as u8).bytes(name);
        w.u8(ipfs.len() as u8).bytes(ipfs);
        return Ok(());
    }
    if tx.is_token() {
        use crate::models::token;
        let a = tx.token_asset().ok_or_else(|| Error::Serialization("missing token asset".into()))?;
        let id = hex::decode(&a.id).map_err(|e| Error::Serialization(format!("bad token id: {e}")))?;
        if id.len() != 32 {
            return Err(Error::Serialization("token id must be 32 bytes".into()));
        }
        w.bytes(&id);
        match tx.type_ {
            token::INIT => {
                w.u8(a.decimals.unwrap_or(0)).u8(a.flags.unwrap_or(0)).u64_le(a.initial_supply.unwrap_or(0)).u64_le(a.supply_cap.unwrap_or(0));
            }
            token::TRANSFER => {
                let items = a.transfers.unwrap_or_default();
                let memo = a.memo.as_deref().unwrap_or("").as_bytes();
                if items.len() > u16::MAX as usize || memo.len() > 255 {
                    return Err(Error::Serialization("token transfer too large".into()));
                }
                w.u16_le(items.len() as u16);
                for it in &items {
                    w.u64_le(it.amount);
                    write_address(w, &it.recipient_id)?;
                }
                w.u8(memo.len() as u8).bytes(memo);
            }
            token::MINT => {
                w.u64_le(a.amount.unwrap_or(0));
                write_address(w, a.recipient_id.as_deref().ok_or_else(|| Error::Serialization("missing recipientId".into()))?)?;
            }
            token::META => {
                let m = a.meta.as_ref().ok_or_else(|| Error::Serialization("missing token meta".into()))?;
                let (name, desc, site) = (m.name.as_bytes(), m.description.as_deref().unwrap_or("").as_bytes(), m.website.as_deref().unwrap_or("").as_bytes());
                let logo = if m.logo.is_some() { m.logo_bytes().ok_or_else(|| Error::Serialization("logo is not base64".into()))? } else { Vec::new() };
                if name.len() > 255 || desc.len() > u16::MAX as usize || site.len() > 255 || logo.len() > u16::MAX as usize {
                    return Err(Error::Serialization("token meta field too long".into()));
                }
                w.u8(name.len() as u8).bytes(name);
                w.u16_le(desc.len() as u16).bytes(desc);
                w.u8(site.len() as u8).bytes(site);
                w.u8(m.logo_type_byte()).u16_le(logo.len() as u16).bytes(&logo);
            }
            _ => {
                w.u64_le(a.amount.unwrap_or(0));
            }
        }
        return Ok(());
    }
    if tx.type_group != TYPE_GROUP_CORE {
        return Err(Error::Serialization(format!("unsupported typeGroup {}", tx.type_group)));
    }
    match tx.type_ {
        tx_type::TRANSFER => {
            w.u64_le(tx.amount).u32_le(tx.expiration.unwrap_or(0));
            if let Some(r) = &tx.recipient_id {
                write_address(w, r)?;
            }
        }
        tx_type::SECOND_SIGNATURE => {
            let sig = asset(tx, "secondSignature")?
                .signature
                .as_ref()
                .ok_or_else(|| Error::Serialization("missing asset.signature".into()))?;
            if tx.is_pq() {
                // v3: alg u8 || pk_len u16 LE || public key (PQ)
                let pk = hex::decode(&sig.public_key).map_err(|e| Error::Serialization(format!("bad PQ public key: {e}")))?;
                if pk.len() > u16::MAX as usize {
                    return Err(Error::Serialization("PQ public key too long".into()));
                }
                w.u8(sig.algorithm.unwrap_or(0)).u16_le(pk.len() as u16).bytes(&pk);
            } else {
                w.hex(&sig.public_key)?;
            }
        }
        tx_type::DELEGATE_REGISTRATION => {
            let d = asset(tx, "delegateRegistration")?
                .delegate
                .as_ref()
                .ok_or_else(|| Error::Serialization("missing asset.delegate".into()))?;
            let name = d.username.as_bytes();
            w.u8(name.len() as u8).bytes(name);
        }
        tx_type::VOTE => {
            let votes = asset(tx, "vote")?
                .votes
                .as_ref()
                .ok_or_else(|| Error::Serialization("missing asset.votes".into()))?;
            w.u8(votes.len() as u8);
            for v in votes {
                let (sign, pk) = v
                    .split_at_checked(1)
                    .ok_or_else(|| Error::Serialization(format!("bad vote {v}")))?;
                w.u8(if sign == "+" { 0x01 } else { 0x00 }).hex(pk)?;
            }
        }
        tx_type::MULTI_SIGNATURE => {
            let ms = asset(tx, "multiSignature")?
                .multi_signature
                .as_ref()
                .ok_or_else(|| Error::Serialization("missing asset.multiSignature".into()))?;
            w.u8(ms.min).u8(ms.public_keys.len() as u8);
            for pk in &ms.public_keys {
                w.hex(pk)?;
            }
        }
        tx_type::IPFS => {
            let ipfs = asset(tx, "ipfs")?
                .ipfs
                .as_ref()
                .ok_or_else(|| Error::Serialization("missing asset.ipfs".into()))?;
            let raw = bs58::decode(ipfs)
                .into_vec()
                .map_err(|e| Error::Serialization(format!("bad ipfs hash: {e}")))?;
            w.bytes(&raw);
        }
        tx_type::MULTI_PAYMENT => {
            let payments = asset(tx, "multiPayment")?
                .payments
                .as_ref()
                .ok_or_else(|| Error::Serialization("missing asset.payments".into()))?;
            w.u16_le(payments.len() as u16);
            for p in payments {
                w.u64_le(p.amount);
                write_address(w, &p.recipient_id)?;
            }
        }
        tx_type::DELEGATE_RESIGNATION => {}
        tx_type::HTLC_LOCK => {
            w.u64_le(tx.amount);
            if let Some(lock) = tx.asset.as_ref().and_then(|a| a.lock.as_ref()) {
                w.hex(&lock.secret_hash)?;
                w.u8(lock.expiration.type_).u32_le(lock.expiration.value);
            }
            if let Some(r) = &tx.recipient_id {
                write_address(w, r)?;
            }
        }
        tx_type::HTLC_CLAIM => {
            if let Some(c) = tx.asset.as_ref().and_then(|a| a.claim.as_ref()) {
                w.hex(&c.lock_transaction_id)?.hex(&c.unlock_secret)?;
            }
        }
        tx_type::HTLC_REFUND => {
            if let Some(r) = tx.asset.as_ref().and_then(|a| a.refund.as_ref()) {
                w.hex(&r.lock_transaction_id)?;
            }
        }
        other => return Err(Error::Serialization(format!("unsupported transaction type {other}"))),
    }
    Ok(())
}

/// Full wire bytes of a v2 transaction (`Transactions.Serializer.serialize`).
pub fn serialize_transaction(tx: &Transaction, opts: SerializeOptions, network: &Network) -> Result<Vec<u8>> {
    if tx.version == 1 {
        return Err(Error::Serialization("legacy v1 transactions are not supported on this chain".into()));
    }
    let mut w = ByteWriter::with_capacity(256);

    // serializeCommon
    w.u8(0xff)
        .u8(tx.version)
        .u8(tx.network.unwrap_or(network.pubkey_hash))
        .u32_le(tx.type_group)
        .u16_le(tx.type_);
    if let Some(nonce) = tx.nonce {
        w.u64_le(nonce);
    }
    w.hex(&tx.sender_public_key)?;
    w.u64_le(tx.fee);

    // serializeVendorField
    match &tx.vendor_field {
        Some(vf) if tx.has_vendor_field() && !vf.is_empty() => {
            let bytes = vf.as_bytes();
            if bytes.len() > 255 {
                return Err(Error::Serialization("vendorField exceeds 255 bytes".into()));
            }
            w.u8(bytes.len() as u8).bytes(bytes);
        }
        _ => {
            w.u8(0x00);
        }
    }

    serialize_type_payload(tx, &mut w)?;

    // serializeSignatures
    if let (Some(sig), false) = (&tx.signature, opts.exclude_signature) {
        w.hex(sig)?;
    }
    if tx.is_pq() {
        // v3: blocks `alg u8 || sig_len u16 LE || signature`, then optional 0xff + multisignatures
        if !opts.exclude_second_signature {
            for b in tx.pq_blocks() {
                let sig = hex::decode(&b.signature).map_err(|e| Error::Serialization(format!("bad PQ signature: {e}")))?;
                if sig.len() > u16::MAX as usize {
                    return Err(Error::Serialization("PQ signature too long".into()));
                }
                w.u8(b.algorithm).u16_le(sig.len() as u16).bytes(&sig);
            }
        }
        if let (Some(sigs), false) = (&tx.signatures, opts.exclude_multi_signature) {
            if !sigs.is_empty() {
                w.u8(0xff);
                for s in sigs {
                    w.hex(s)?;
                }
            }
        }
        return Ok(w.into_inner());
    }
    if let (Some(sig), false) = (tx.second_signature_any(), opts.exclude_second_signature) {
        w.hex(sig)?;
    }
    if let (Some(sigs), false) = (&tx.signatures, opts.exclude_multi_signature) {
        for s in sigs {
            w.hex(s)?;
        }
    }
    Ok(w.into_inner())
}

/// Transaction id = sha256(full serialised bytes), hex.
pub fn transaction_id(tx: &Transaction) -> Result<String> {
    let bytes = serialize_transaction(tx, SerializeOptions::default(), Network::mainnet_ref())?;
    Ok(hex::encode(sha256(&bytes)))
}

/// Hash that the sender signs (bytes without signature / secondSignature).
pub fn transaction_signing_hash(tx: &Transaction) -> Result<[u8; 32]> {
    let bytes = serialize_transaction(tx, SerializeOptions::for_signing(), Network::mainnet_ref())?;
    Ok(sha256(&bytes))
}

/// Verify the primary sender signature (schnorr for 64-byte sigs, DER ECDSA otherwise).
pub fn verify_transaction_signature(tx: &Transaction) -> Result<bool> {
    let sig = match &tx.signature {
        Some(s) => s,
        None => return Ok(false),
    };
    let hash = transaction_signing_hash(tx)?;
    verify_signature(&hash, sig, &tx.sender_public_key)
}

/// Message every v3 second-signature block signs: `M2 = sha256(BODY || SIG1)` (same bytes as the legacy second signature).
pub fn transaction_pq_message(tx: &Transaction) -> Result<[u8; 32]> {
    let opts = SerializeOptions { exclude_signature: false, exclude_second_signature: true, exclude_multi_signature: true };
    Ok(sha256(&serialize_transaction(tx, opts, Network::mainnet_ref())?))
}

/// Verify the second signature against the wallet's registered second public key.
pub fn verify_transaction_second_signature(tx: &Transaction, second_public_key_hex: &str) -> Result<bool> {
    let sig = match tx.second_signature_any() {
        Some(s) => s,
        None => return Ok(false),
    };
    let opts = SerializeOptions { exclude_signature: false, exclude_second_signature: true, exclude_multi_signature: false };
    let bytes = serialize_transaction(tx, opts, Network::mainnet_ref())?;
    verify_signature(&sha256(&bytes), sig, second_public_key_hex)
}
