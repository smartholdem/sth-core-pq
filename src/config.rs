//! Author: TechnoL0g
//!
//! Network constants for SmartHoldem mainnet, mirrored from
//! `@smartholdem/crypto-networks/src/mainnet/{network,milestones}.json`.

use std::time::{SystemTime, UNIX_EPOCH};

/// 1 STH = 100_000_000 smartoshi.
pub const SMARTOSHI: u64 = 100_000_000;

/// Milestone parameters active from a given height.
#[derive(Debug, Clone)]
pub struct Milestone {
    pub height: u64,
    pub reward: u64,
    pub active_delegates: u32,
    pub blocktime: u32,
    pub block_version: u32,
    pub id_full_sha256: bool,
    pub max_transactions: u32,
    pub max_payload: u64,
    pub vendor_field_length: u16,
    pub aip11: bool,
    pub aip37: bool,
}

/// Static network configuration.
#[derive(Debug, Clone)]
pub struct Network {
    pub name: &'static str,
    pub message_prefix: &'static str,
    pub pubkey_hash: u8,
    pub wif: u8,
    pub nethash: &'static str,
    /// Id of block 1 (core patches the computed genesis id with this value).
    pub genesis_block_id: &'static str,
    pub token: &'static str,
    pub burn_address: &'static str,
    /// Unix timestamp (seconds) of the chain epoch (block timestamp 0).
    pub epoch_unix: i64,
    milestones: Vec<Milestone>,
}

impl Network {
    /// SmartHoldem mainnet (network byte 63 / 0x3f).
    pub fn mainnet() -> Self {
        let base = Milestone {
            height: 1,
            reward: 0,
            active_delegates: 21,
            blocktime: 8,
            block_version: 0,
            id_full_sha256: true,
            max_transactions: 500,
            max_payload: 84_000_000,
            vendor_field_length: 255,
            aip11: true,
            aip37: false,
        };
        let m151200 = Milestone { height: 151_200, ..base.clone() };
        let m564000 = Milestone { height: 564_000, aip37: true, ..base.clone() };
        Self {
            name: "mainnet",
            message_prefix: "mainnet message:\n",
            pubkey_hash: 63,
            wif: 255,
            nethash: "b0987b45a3a754da1362bc6818548cb34f65750c9ac81d28c93e7545224df2d2",
            genesis_block_id: "ea60ebb15e3e8abe9e47a7ef18145d65c570c3dfe2b0ac678c665787860bae32",
            token: "STH",
            burn_address: "STHsmartHoLdemBurnAddrHereXXXmUW7f",
            // 2023-08-29T00:00:00.000Z
            epoch_unix: 1_693_267_200,
            milestones: vec![base, m151200, m564000],
        }
    }

    /// Milestone in effect at `height` (last milestone with `height <= h`).
    pub fn milestone(&self, height: u64) -> &Milestone {
        let h = height.max(1);
        self.milestones
            .iter()
            .rev()
            .find(|m| m.height <= h)
            .unwrap_or(&self.milestones[0])
    }

    /// Convert a chain (epoch) timestamp to unix seconds.
    pub fn epoch_to_unix(&self, epoch: u32) -> i64 {
        self.epoch_unix + epoch as i64
    }

    /// Convert unix seconds to a chain (epoch) timestamp.
    pub fn unix_to_epoch(&self, unix: i64) -> u32 {
        (unix - self.epoch_unix).max(0) as u32
    }

    /// Current chain timestamp (`Slots.getTime()` in core).
    pub fn now_epoch(&self) -> u32 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.unix_to_epoch(now)
    }
}

impl Default for Network {
    fn default() -> Self {
        Self::mainnet()
    }
}

/// Static fees (smartoshi) of core transaction types, as served by `/api/transactions/fees`.
pub const STATIC_FEES: [(&str, u16, u64); 11] = [
    ("transfer", 0, 100_000_000),
    ("secondSignature", 1, 500_000_000),
    ("delegateRegistration", 2, 1_000_000_000_000),
    ("vote", 3, 100_000_000),
    ("multiSignature", 4, 500_000_000),
    ("ipfs", 5, 500_000_000),
    ("multiPayment", 6, 10_000_000),
    ("delegateResignation", 7, 2_500_000_000),
    ("htlcLock", 8, 10_000_000),
    ("htlcClaim", 9, 0),
    ("htlcRefund", 10, 0),
];

/// Total supply of the chain (smartoshi), reported by `/api/blockchain`.
pub const TOTAL_SUPPLY: u64 = 24_977_000_000_000_000;
