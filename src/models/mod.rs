//! Author: TechnoL0g
//!
//! Data models with JSON shape identical to `@smartholdem/core` (`IBlockData`, `ITransactionData`).

mod block;
mod serde_utils;
mod transaction;

pub use block::{Block, BLOCK_VERSION_PQ};
pub use transaction::{
    sobj, token, tx_type, PqSignatureBlock, VERSION_PQ, TokenAsset, TokenMeta, TokenTransferItem, TYPE_GROUP_TOKEN, DelegateAsset, SmartObjectAsset, SmartObjectData, HtlcClaimAsset, HtlcExpiration, HtlcLockAsset, HtlcRefundAsset,
    MultiSignatureAsset, Payment, SecondSignatureAsset, Transaction, TransactionAsset, TYPE_GROUP_CORE, TYPE_GROUP_SOBJ,
};
pub use serde_utils::{opt_string_u64, string_i64, string_u64};
