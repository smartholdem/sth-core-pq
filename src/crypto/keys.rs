//! Author: TechnoL0g
//!
//! Key pairs: private key = sha256(passphrase), compressed secp256k1 public key.

use super::address::address_from_public_key;
use super::hash::sha256;
use crate::error::{Error, Result};
use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::SecretKey;

#[derive(Debug, Clone)]
pub struct KeyPair {
    private_key: [u8; 32],
    public_key: [u8; 33],
}

impl KeyPair {
    /// SmartHoldem key derivation: private key is the SHA-256 of the passphrase.
    pub fn from_passphrase(passphrase: &str) -> Result<Self> {
        Self::from_private_key(&sha256(passphrase.as_bytes()))
    }

    pub fn from_private_key_hex(hex_key: &str) -> Result<Self> {
        let bytes = hex::decode(hex_key)?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| Error::PublicKey("private key must be 32 bytes".into()))?;
        Self::from_private_key(&arr)
    }

    pub fn from_private_key(private_key: &[u8; 32]) -> Result<Self> {
        let sk = SecretKey::from_slice(private_key)
            .map_err(|e| Error::PublicKey(format!("invalid private key: {e}")))?;
        let encoded = sk.public_key().to_encoded_point(true);
        let public_key: [u8; 33] = encoded
            .as_bytes()
            .try_into()
            .map_err(|_| Error::PublicKey("unexpected public key length".into()))?;
        Ok(Self { private_key: *private_key, public_key })
    }

    pub fn private_key(&self) -> &[u8; 32] {
        &self.private_key
    }

    pub fn private_key_hex(&self) -> String {
        hex::encode(self.private_key)
    }

    pub fn public_key(&self) -> &[u8; 33] {
        &self.public_key
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.public_key)
    }

    pub fn address(&self, network_byte: u8) -> Result<String> {
        address_from_public_key(&self.public_key_hex(), network_byte)
    }
}
