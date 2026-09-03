//! Author: TechnoL0g
//!
//! Cryptography: hashing, addresses, keys, ECDSA (DER, low-S), legacy bip-schnorr,
//! and byte-exact block / transaction serialisation used for id derivation and signing.

mod address;
mod block_deserializer;
mod block_serializer;
mod bytes;
mod ecdsa;
mod hash;
mod keys;
mod schnorr;
mod tx_deserializer;
mod tx_serializer;

pub use address::{address_from_bytes, address_from_multi_signature, address_from_public_key, address_to_bytes, validate_address};
pub use block_deserializer::deserialize_block_header;
pub use block_serializer::{
    serialize_block_with_transactions,
    block_id, block_payload_hash, block_signing_hash, serialize_block, verify_block,
    verify_block_signature, BlockVerification,
};
pub use bytes::{ByteReader, ByteWriter};
pub use ecdsa::{sign_ecdsa, verify_ecdsa};
pub use hash::{ripemd160, sha256};
pub use keys::KeyPair;
pub use schnorr::{sign_schnorr_legacy, verify_schnorr_legacy};
pub use tx_deserializer::deserialize_transaction;
pub use tx_serializer::{
    serialize_transaction, transaction_id, transaction_signing_hash, verify_transaction_second_signature,
    verify_transaction_signature, SerializeOptions,
};

use crate::error::Result;
use crate::models::{Block, Transaction};

/// Verify a signature over a 32-byte hash. Auto-detects the scheme like core's
/// `Verifier.internalVerifySignature`: 64 raw bytes → legacy schnorr, otherwise DER ECDSA.
pub fn verify_signature(hash: &[u8; 32], signature_hex: &str, public_key_hex: &str) -> Result<bool> {
    let sig = hex::decode(signature_hex)?;
    if sig.len() == 64 {
        verify_schnorr_legacy(hash, &sig, public_key_hex)
    } else {
        verify_ecdsa(hash, &sig, public_key_hex)
    }
}

/// Common id / signature interface for chain objects (`get_id(block)` / `get_id(tx)`).
pub trait ChainObject {
    fn get_id(&self) -> Result<String>;
    fn verify_signature(&self) -> Result<bool>;
}

impl ChainObject for Block {
    fn get_id(&self) -> Result<String> {
        block_id(self)
    }

    fn verify_signature(&self) -> Result<bool> {
        verify_block_signature(self)
    }
}

impl ChainObject for Transaction {
    fn get_id(&self) -> Result<String> {
        transaction_id(self)
    }

    fn verify_signature(&self) -> Result<bool> {
        verify_transaction_signature(self)
    }
}

/// Convenience alias mirroring the JS API name.
pub fn get_id<T: ChainObject>(obj: &T) -> Result<String> {
    obj.get_id()
}
