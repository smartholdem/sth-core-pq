//! Author: TechnoL0g
//!
//! ECDSA over secp256k1 with strict DER encoding and low-S enforcement.
//! Port of `Hash.verifyECDSA` from `@smartholdem/crypto` (used for block signatures).

use crate::error::{Error, Result};
use k256::ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
use k256::ecdsa::{Signature, SigningKey, VerifyingKey};

/// Replicates the byte-level DER sanity checks from core's `verifyECDSA`.
fn der_is_strict(sig: &[u8]) -> bool {
    if sig.len() < 8 || sig[0] != 0x30 {
        return false;
    }
    let sig_len = sig[1] as usize;
    let r_len = sig[3] as usize;
    let s_len_idx = 4 + r_len + 1;
    if s_len_idx >= sig.len() {
        return false;
    }
    let s_len = sig[s_len_idx] as usize;

    if sig.len() != 4 + r_len + 2 + s_len || sig_len != 2 + r_len + 2 + s_len || sig_len > 127 {
        return false;
    }
    if r_len == 0 || s_len == 0 {
        return false;
    }
    let r_first = sig[4] as i8;
    let s_first = sig[4 + r_len + 2] as i8;
    if r_first < 0 || s_first < 0 {
        return false;
    }
    // A leading zero is only allowed to make a negative-looking integer positive.
    if r_first == 0 && (r_len < 2 || (sig[5] as i8) >= 0) {
        return false;
    }
    if s_first == 0 && (s_len < 2 || (sig[4 + r_len + 3] as i8) >= 0) {
        return false;
    }
    true
}

/// Verify a DER-encoded ECDSA signature over a 32-byte hash. High-S signatures are rejected.
pub fn verify_ecdsa(hash: &[u8; 32], der_signature: &[u8], public_key_hex: &str) -> Result<bool> {
    if !der_is_strict(der_signature) {
        return Ok(false);
    }
    let sig = match Signature::from_der(der_signature) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    if sig.normalize_s().is_some() {
        return Ok(false);
    }
    let pk = hex::decode(public_key_hex)?;
    let vk = VerifyingKey::from_sec1_bytes(&pk)
        .map_err(|e| Error::PublicKey(format!("invalid secp256k1 public key: {e}")))?;
    Ok(vk.verify_prehash(hash, &sig).is_ok())
}

/// Produce a low-S DER-encoded ECDSA signature (hex) over a 32-byte hash.
pub fn sign_ecdsa(hash: &[u8; 32], private_key: &[u8; 32]) -> Result<String> {
    let sk = SigningKey::from_slice(private_key)
        .map_err(|e| Error::Signature(format!("invalid private key: {e}")))?;
    let sig: Signature = sk
        .sign_prehash(hash)
        .map_err(|e| Error::Signature(format!("ecdsa signing failed: {e}")))?;
    let sig = sig.normalize_s().unwrap_or(sig);
    Ok(hex::encode(sig.to_der().as_bytes()))
}
