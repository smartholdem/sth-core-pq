//! Author: TechnoL0g
//!
//! `sth-core` — SmartHoldem relay node core.
//! Rust rewrite of the `@smartholdem/core` data structures, cryptography and state storage.

pub mod api;
pub mod cli;
pub mod config;
pub mod crypto;
pub mod delegate;
pub mod error;
pub mod genesis;
pub mod intake;
pub mod mem;
pub mod mempool;
pub mod models;
pub mod node;
pub mod newnet;
pub mod node_config;
pub mod node_pool;
pub mod ntp;
pub mod p2p_iroh;
pub mod p2p_legacy;
pub mod rules;
pub mod snapshot;
pub mod storage;
pub mod sync;

pub use error::{Error, Result};
pub use models::{Block, Transaction};
