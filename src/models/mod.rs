//! Author: TechnoL0g
//!
//! Data models with JSON shape identical to `@smartholdem/core` (`IBlockData`, `ITransactionData`).

mod block;
mod serde_utils;
mod transaction;

pub use block::Block;
pub use transaction::{
    tx_type, DelegateAsset, HtlcClaimAsset, HtlcExpiration, HtlcLockAsset, HtlcRefundAsset,
    MultiSignatureAsset, Payment, SecondSignatureAsset, Transaction, TransactionAsset, TYPE_GROUP_CORE,
};
pub use serde_utils::{opt_string_u64, string_i64, string_u64};
