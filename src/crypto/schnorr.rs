//! Author: TechnoL0g
//!
//! Legacy bip-schnorr over secp256k1 (pre-BIP340, "is-square" R convention), as
//! implemented by `bcrypto` and used for SmartHoldem v2 transaction signatures.
//!
//!   k  = sha256(a || m) mod n ; R = k*G ; if y(R) is not a quadratic residue → k = n - k
//!   e  = sha256(x(R) || A_compressed || m) mod n
//!   s  = k + e*a mod n ; sig = x(R)(32) || s(32)
//!
//! Verification: R' = s*G - e*A ; valid iff R' != O, y(R') is a QR and x(R') == r.
//! All field work is done with k256's native `FieldElement` (QR test = `sqrt()`), the two scalar
//! multiplications are fused into one Shamir/Straus linear combination.

use super::hash::sha256_multi;
use crate::error::{Error, Result};
use k256::elliptic_curve::group::Group;
use k256::elliptic_curve::ops::{LinearCombinationExt, Reduce};
use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::elliptic_curve::PrimeField;
use k256::{FieldBytes, FieldElement, ProjectivePoint, PublicKey, Scalar, SecretKey, U256};

/// Euler criterion via the field's square root (p ≡ 3 mod 4 → one exponentiation).
fn is_quadratic_residue(y_be: &[u8]) -> bool {
    field_element(y_be).is_some_and(|y| bool::from(y.sqrt().is_some()))
}

/// Canonical field element from 32 big-endian bytes (None when ≥ p or wrong length).
fn field_element(be: &[u8]) -> Option<FieldElement> {
    let bytes: [u8; 32] = be.try_into().ok()?;
    Option::<FieldElement>::from(FieldElement::from_bytes(&FieldBytes::from(bytes)))
}

fn scalar_from_hash(h: &[u8; 32]) -> Scalar {
    <Scalar as Reduce<U256>>::reduce(U256::from_be_slice(h))
}

/// Sign a 32-byte hash. Returns the 64-byte signature as hex.
pub fn sign_schnorr_legacy(hash: &[u8; 32], private_key: &[u8; 32]) -> Result<String> {
    let sk = SecretKey::from_slice(private_key)
        .map_err(|e| Error::Signature(format!("invalid private key: {e}")))?;
    let a: Scalar = *sk.to_nonzero_scalar();
    let a_comp = sk.public_key().to_encoded_point(true);

    let k0 = scalar_from_hash(&sha256_multi([&private_key[..], &hash[..]]));
    if bool::from(k0.is_zero()) {
        return Err(Error::Signature("schnorr nonce is zero".into()));
    }
    let r_point = (ProjectivePoint::GENERATOR * k0).to_affine().to_encoded_point(false);
    let (r_x, r_y) = match (r_point.x(), r_point.y()) {
        (Some(x), Some(y)) => (x, y),
        _ => return Err(Error::Signature("degenerate R point".into())),
    };
    let k = if is_quadratic_residue(r_y) { k0 } else { -k0 };

    let e = scalar_from_hash(&sha256_multi([&r_x[..], a_comp.as_bytes(), &hash[..]]));
    let s = k + e * a;

    let mut sig = Vec::with_capacity(64);
    sig.extend_from_slice(r_x);
    sig.extend_from_slice(&s.to_bytes());
    Ok(hex::encode(sig))
}

/// Verify a 64-byte legacy schnorr signature against a compressed public key (hex).
pub fn verify_schnorr_legacy(hash: &[u8; 32], signature: &[u8], public_key_hex: &str) -> Result<bool> {
    if signature.len() != 64 {
        return Ok(false);
    }
    let r_x = &signature[..32];
    let s_bytes = &signature[32..];

    // r must be a canonical field element (< p)
    if field_element(r_x).is_none() {
        return Ok(false);
    }
    let s_arr: [u8; 32] = match s_bytes.try_into() {
        Ok(a) => a,
        Err(_) => return Ok(false),
    };
    let s = match Option::<Scalar>::from(Scalar::from_repr(s_arr.into())) {
        Some(s) if !bool::from(s.is_zero()) => s,
        _ => return Ok(false),
    };

    let a_comp = hex::decode(public_key_hex)?;
    let a = PublicKey::from_sec1_bytes(&a_comp)
        .map_err(|e| Error::PublicKey(format!("invalid secp256k1 public key: {e}")))?;

    let e = scalar_from_hash(&sha256_multi([r_x, &a_comp[..], &hash[..]]));

    // R' = s*G - e*A as one linear combination
    let r_point = ProjectivePoint::lincomb_ext(&[(ProjectivePoint::GENERATOR, s), (-a.to_projective(), e)]);
    if bool::from(r_point.is_identity()) {
        return Ok(false);
    }
    let encoded = r_point.to_affine().to_encoded_point(false);
    let (x, y) = match (encoded.x(), encoded.y()) {
        (Some(x), Some(y)) => (x, y),
        _ => return Ok(false),
    };
    if !is_quadratic_residue(y) {
        return Ok(false);
    }
    Ok(&x[..] == r_x)
}
