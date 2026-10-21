// STH-Core-PQ: Quantum Shield — ML-DSA-44 (NIST FIPS 204)
use sth_core::crypto::pq::{self, PqKeyPair};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Deterministic PQ key: same passphrase → same key on every device
    let delegate = PqKeyPair::from_passphrase("delegate second passphrase");
    let public_key = delegate.public_key(); // 1 312 bytes

    // Stage A: on-chain commitment sha256(pk) in vendorField
    println!("commitment : {}", delegate.commitment());

    // Stage C: hybrid block = secp256k1 ECDSA + ML-DSA-44 over M_B = sha256(header || blockSignature)
    let m_b = sha2::Sha256::digest(b"block header || ecdsa blockSignature");
    let pq_signature = delegate.sign_with_ctx(&m_b, pq::BLOCK_CTX)?; // 2 420 bytes

    let valid = pq::verify_with_ctx(&public_key, &m_b, &pq_signature, pq::BLOCK_CTX)?;
    println!("pk {} B · sig {} B · verified: {valid}", public_key.len(), pq_signature.len());

    // Any tampering with the block is rejected
    let mut forged = pq_signature.clone();
    forged[0] ^= 0x01;
    let rejected = !pq::verify_with_ctx(&public_key, &m_b, &forged, pq::BLOCK_CTX)?;
    println!("tampered signature rejected: {rejected}");
    Ok(())
}

use sha2::Digest as _;
