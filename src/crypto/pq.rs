//! Author: TechnoL0g
//! Quantum Shield: ML-DSA-44 (NIST FIPS 204) keys derived from a passphrase, plus the stage-A
//! commitment format (`sthpq1:<alg>:<sha256(pk)>` in a self-transfer vendorField). See docs/SPEC-PQ-V3.md.

use crate::error::{Error, Result};
use ml_dsa::{EncodedSignature, EncodedVerifyingKey, MlDsa44, Signature, SigningKey, VerifyingKey, B32};
use sha2::{Digest, Sha256};

/// Algorithm ids (first byte of every PQ object).
pub const ALG_ML_DSA_44: u8 = 0x01;
pub const PK_LEN: usize = 1312;
pub const SIG_LEN: usize = 2420;
/// Domain separation for the ML-DSA context string.
const CTX: &[u8] = b"sth-pq-v1";
/// Context of stage-C block signatures (`M_B = sha256(header || blockSignature)`).
pub const BLOCK_CTX: &[u8] = b"sth-pq-block-v1";
/// Commitment prefix in vendorField.
pub const COMMITMENT_PREFIX: &str = "sthpq1:";

pub struct PqKeyPair {
    signing: SigningKey<MlDsa44>,
}

impl PqKeyPair {
    /// Deterministic: seed ξ = sha256("sth-pq-v1" || passphrase). Same passphrase → same key on every device.
    pub fn from_passphrase(passphrase: &str) -> Self {
        let mut h = Sha256::new();
        h.update(CTX);
        h.update(passphrase.as_bytes());
        let seed: [u8; 32] = h.finalize().into();
        Self::from_seed(&seed)
    }

    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self { signing: SigningKey::<MlDsa44>::from_seed(&B32::from(*seed)) }
    }

    /// FIPS 204 pkEncode — 1 312 bytes.
    pub fn public_key(&self) -> Vec<u8> {
        self.signing.expanded_key().verifying_key().encode().to_vec()
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.public_key())
    }

    /// Deterministic ML-DSA-44 signature (2 420 bytes) over `message` with the sth context.
    pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        self.sign_with_ctx(message, CTX)
    }

    pub fn sign_with_ctx(&self, message: &[u8], ctx: &[u8]) -> Result<Vec<u8>> {
        let sig = self.signing.expanded_key().sign_deterministic(message, ctx).map_err(|_| Error::Serialization("ml-dsa sign".into()))?;
        Ok(sig.encode().to_vec())
    }

    /// What a wallet publishes in stage A: `sthpq1:01:<sha256(pk) hex>`.
    pub fn commitment(&self) -> String {
        commitment_for_public_key(&self.public_key())
    }
}

pub fn commitment_hash(public_key: &[u8]) -> [u8; 32] {
    Sha256::digest(public_key).into()
}

pub fn commitment_for_public_key(public_key: &[u8]) -> String {
    format!("{COMMITMENT_PREFIX}{:02x}:{}", ALG_ML_DSA_44, hex::encode(commitment_hash(public_key)))
}

pub fn verify(public_key: &[u8], message: &[u8], signature: &[u8]) -> Result<bool> {
    verify_with_ctx(public_key, message, signature, CTX)
}

pub fn verify_with_ctx(public_key: &[u8], message: &[u8], signature: &[u8], ctx: &[u8]) -> Result<bool> {
    if public_key.len() != PK_LEN || signature.len() != SIG_LEN {
        return Ok(false);
    }
    let pk = EncodedVerifyingKey::<MlDsa44>::try_from(public_key).map_err(|_| Error::Serialization("ml-dsa pk".into()))?;
    let sig = EncodedSignature::<MlDsa44>::try_from(signature).map_err(|_| Error::Serialization("ml-dsa sig".into()))?;
    let Some(sig) = Signature::<MlDsa44>::decode(&sig) else { return Ok(false) };
    Ok(VerifyingKey::<MlDsa44>::decode(&pk).verify_with_context(message, ctx, &sig))
}

/// Stage B: PQ public key registered by a wallet (type 1, v3). Lives in `WalletState.pq_key`, so undo is automatic.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PqKey {
    pub algorithm: u8,
    /// Raw PQ public key, hex (1 312 bytes for ML-DSA-44).
    pub public_key: String,
    pub since: u64,
}

/// Parsed stage-A commitment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PqCommitment {
    pub algorithm: u8,
    /// sha256(public key), hex.
    pub commitment: String,
    pub height: u64,
}

/// `sthpq1:<alg 2 hex>:<64 hex>` → (alg, hash hex). Only known algorithm ids are accepted.
pub fn parse_commitment(vendor_field: &str) -> Option<(u8, String)> {
    let rest = vendor_field.strip_prefix(COMMITMENT_PREFIX)?;
    let (alg, hash) = rest.split_once(':')?;
    if alg.len() != 2 || hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return None;
    }
    let alg = u8::from_str_radix(alg, 16).ok()?;
    (alg == ALG_ML_DSA_44).then(|| (alg, hash.to_string()))
}
