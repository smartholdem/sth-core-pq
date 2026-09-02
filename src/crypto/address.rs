//! Author: TechnoL0g
//!
//! Address derivation: `base58check(network_byte || ripemd160(compressed_pubkey))`.

use super::hash::ripemd160;
use crate::error::{Error, Result};

/// Derive an address from a compressed secp256k1 public key (hex) for the given network byte.
pub fn address_from_public_key(public_key_hex: &str, network_byte: u8) -> Result<String> {
    let pk = hex::decode(public_key_hex)?;
    if pk.len() != 33 {
        return Err(Error::PublicKey(format!(
            "expected 33-byte compressed key, got {} bytes",
            pk.len()
        )));
    }
    let mut payload = Vec::with_capacity(21);
    payload.push(network_byte);
    payload.extend_from_slice(&ripemd160(&pk));
    Ok(bs58::encode(payload).with_check().into_string())
}

/// Base58check-encode raw address bytes (network byte + 20-byte hash).
pub fn address_from_bytes(bytes: &[u8]) -> String {
    bs58::encode(bytes).with_check().into_string()
}

/// Decode an address into its 21 raw bytes (network byte + 20-byte hash), verifying the checksum.
pub fn address_to_bytes(address: &str) -> Result<[u8; 21]> {
    let raw = bs58::decode(address)
        .with_check(None)
        .into_vec()
        .map_err(|e| Error::Address(format!("{address}: {e}")))?;
    let bytes: [u8; 21] = raw
        .as_slice()
        .try_into()
        .map_err(|_| Error::Address(format!("{address}: expected 21 bytes, got {}", raw.len())))?;
    Ok(bytes)
}

/// Checksum + network byte validation.
pub fn validate_address(address: &str, network_byte: u8) -> bool {
    matches!(address_to_bytes(address), Ok(b) if b[0] == network_byte)
}
