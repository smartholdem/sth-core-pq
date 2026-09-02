//! Author: TechnoL0g
//!
//! `sth-core` - SmartHoldem relay node core.
//! Rust rewrite of the `@smartholdem/core` data structures, cryptography and state storage.

pub mod api;
pub mod config;
pub mod crypto;
pub mod error;
pub mod mempool;
pub mod models;
pub mod node_pool;
pub mod p2p_legacy;
pub mod snapshot;
pub mod storage;
pub mod sync;

pub use error::{Error, Result};
pub use models::{Block, Transaction};
