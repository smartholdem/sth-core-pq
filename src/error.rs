//! Author: TechnoL0g
//!
//! Unified error type for the whole crate. No `unwrap()` in library code —
//! every fallible path returns `Result<T, Error>`.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("hex decode error: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("invalid address: {0}")]
    Address(String),
    #[error("invalid public key: {0}")]
    PublicKey(String),
    #[error("invalid signature: {0}")]
    Signature(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("storage error: {0}")]
    Storage(#[from] sled::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("block validation failed: {0}")]
    BlockValidation(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("arithmetic overflow: {0}")]
    Overflow(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("sync error: {0}")]
    Sync(String),
}

pub type Result<T> = std::result::Result<T, Error>;
