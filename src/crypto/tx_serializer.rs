//! Author: TechnoL0g
//!
//! AIP-11 (v2) transaction wire serialisation — byte-exact port of
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
            w.hex(&sig.public_key)?;
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
    let bytes = serialize_transaction(tx, SerializeOptions::default(), &Network::mainnet())?;
    Ok(hex::encode(sha256(&bytes)))
}

/// Hash that the sender signs (bytes without signature / secondSignature).
pub fn transaction_signing_hash(tx: &Transaction) -> Result<[u8; 32]> {
    let bytes = serialize_transaction(tx, SerializeOptions::for_signing(), &Network::mainnet())?;
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

/// Verify the second signature against the wallet's registered second public key.
pub fn verify_transaction_second_signature(tx: &Transaction, second_public_key_hex: &str) -> Result<bool> {
    let sig = match tx.second_signature_any() {
        Some(s) => s,
        None => return Ok(false),
    };
    let opts = SerializeOptions { exclude_signature: false, exclude_second_signature: true, exclude_multi_signature: false };
    let bytes = serialize_transaction(tx, opts, &Network::mainnet())?;
    verify_signature(&sha256(&bytes), sig, second_public_key_hex)
}
