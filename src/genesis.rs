//! Author: TechnoL0g
//!
//! Genesis block from the network files (`genesisBlock.json`, 1855 transactions). Legacy peers
//! reset the socket on `getBlocks(lastBlockHeight = 0)`, so an empty node seeds block 1 from here.

use crate::config::{Network, MAINNET_GENESIS_GZ};
use crate::crypto::verify_block;
use crate::error::{Error, Result};
use crate::models::Block;
use crate::storage::Storage;
use std::io::Read;

/// Raw `genesisBlock.json` (as served by `/api/node/configuration/crypto`).
pub fn genesis_json(gz: &[u8]) -> Result<serde_json::Value> {
    let mut json = Vec::with_capacity(1 << 20);
    flate2::read::GzDecoder::new(gz).read_to_end(&mut json).map_err(|e| Error::Serialization(format!("genesis gunzip: {e}")))?;
    Ok(serde_json::from_slice(&json)?)
}

pub fn genesis_block(gz: &[u8]) -> Result<Block> {
    Ok(serde_json::from_value(genesis_json(gz)?)?)
}

pub fn mainnet_block() -> Result<Block> {
    genesis_block(MAINNET_GENESIS_GZ)
}

/// Apply the genesis block when the database is empty. Returns true when it was applied.
pub fn ensure_genesis(storage: &Storage, network: &Network) -> Result<bool> {
    if storage.get_last_height()? != 0 {
        return Ok(false);
    }
    let block = genesis_block(network.genesis_gz())?;
    if block.id.as_deref() != Some(network.genesis_block_id.as_str()) {
        return Err(Error::BlockValidation("embedded genesis id does not match the network".into()));
    }
    let v = verify_block(&block, network);
    if !v.verified {
        return Err(Error::BlockValidation(format!("embedded genesis rejected: {:?}", v.errors)));
    }
    storage.apply_block(&block)?;
    storage.flush()?;
    tracing::info!(id = %network.genesis_block_id, transactions = block.transactions.len(), "genesis block applied");
    Ok(true)
}
