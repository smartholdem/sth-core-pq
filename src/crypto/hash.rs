//! Author: TechnoL0g
//!
//! Hash primitives (`HashAlgorithms` in core).

use ripemd::Ripemd160;
use sha2::{Digest, Sha256};

/// SHA-256 digest.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(data));
    out
}

/// SHA-256 over the concatenation of several buffers (core's `sha256(Buffer[])`).
pub fn sha256_multi<'a, I: IntoIterator<Item = &'a [u8]>>(parts: I) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    out
}

/// RIPEMD-160 digest.
pub fn ripemd160(data: &[u8]) -> [u8; 20] {
    let mut out = [0u8; 20];
    out.copy_from_slice(&Ripemd160::digest(data));
    out
}
