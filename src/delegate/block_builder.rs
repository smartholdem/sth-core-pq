//! Author: TechnoL0g
//!
//! Block assembly: pick mempool transactions (highest fee first, nonce order per sender, milestone
//! limits), compute totals / payload hash, sign the header with the delegate key.

use crate::config::Network;
use crate::crypto::{block_id, block_payload_hash, block_signing_hash, serialize_transaction, sign_ecdsa, KeyPair, SerializeOptions};
use crate::error::Result;
use crate::models::{Block, Transaction};
use crate::storage::Storage;

/// Choose and order transactions for the next block.
pub fn select_transactions(storage: &Storage, network: &Network, pool: Vec<Transaction>, height: u64) -> Result<Vec<Transaction>> {
    let m = network.milestone(height);
    let max_txs = m.max_transactions() as usize;
    let max_payload = m.max_payload() as usize;
    let mut txs = pool;
    // highest fee first; ties keep the older (lower nonce) first
    txs.sort_by(|a, b| b.fee.cmp(&a.fee).then(a.nonce.cmp(&b.nonce)));
    let mut chosen: Vec<Transaction> = Vec::new();
    let mut payload = 0usize;
    let mut next_nonce: std::collections::HashMap<String, u64> = Default::default();
    // a sender's transactions must be included in nonce order without gaps
    let mut deferred: Vec<Transaction> = Vec::new();
    let mut progress = true;
    let mut candidates = txs;
    while progress && chosen.len() < max_txs {
        progress = false;
        for tx in candidates.drain(..) {
            if chosen.len() >= max_txs {
                break;
            }
            let expected = match next_nonce.get(&tx.sender_public_key) {
                Some(n) => *n,
                None => {
                    let addr = crate::crypto::address_from_public_key(&tx.sender_public_key, network.pubkey_hash)?;
                    storage.get_wallet(&addr)?.map(|w| w.nonce).unwrap_or(0) + 1
                }
            };
            let nonce = tx.nonce.unwrap_or(0);
            if nonce != expected {
                if nonce > expected {
                    deferred.push(tx);
                }
                continue;
            }
            let size = serialize_transaction(&tx, SerializeOptions::default(), network)?.len();
            if payload + size > max_payload {
                continue;
            }
            payload += size;
            next_nonce.insert(tx.sender_public_key.clone(), nonce + 1);
            chosen.push(tx);
            progress = true;
        }
        candidates = std::mem::take(&mut deferred);
        if candidates.is_empty() {
            break;
        }
    }
    Ok(chosen)
}

/// Build and sign the block at `height` on top of `previous` for `slot_timestamp`.
pub fn forge_block(network: &Network, keys: &KeyPair, previous: &Block, timestamp: u32, txs: Vec<Transaction>) -> Result<Block> {
    let height = previous.height + 1;
    let m = network.milestone(height);
    let mut payload_length = 0u32;
    let mut total_amount = 0u64;
    let mut total_fee = 0u64;
    for tx in &txs {
        payload_length += serialize_transaction(tx, SerializeOptions::default(), network)?.len() as u32;
        total_amount += tx.amount;
        total_fee += tx.fee;
    }
    let mut block = Block {
        id: None,
        version: m.block_version(),
        timestamp,
        previous_block: previous.id.clone().unwrap_or_default(),
        height,
        number_of_transactions: txs.len() as u32,
        total_amount,
        total_fee,
        reward: m.reward,
        payload_length,
        payload_hash: String::new(),
        generator_public_key: keys.public_key_hex(),
        block_signature: None,
        transactions: txs,
    };
    for (i, tx) in block.transactions.iter_mut().enumerate() {
        tx.block_height = Some(height);
        tx.sequence = Some(i as u32);
    }
    block.payload_hash = block_payload_hash(&block)?;
    let hash = block_signing_hash(&block)?;
    block.block_signature = Some(sign_ecdsa(&hash, keys.private_key())?);
    block.id = Some(block_id(&block)?);
    for tx in block.transactions.iter_mut() {
        tx.block_id = block.id.clone();
    }
    Ok(block)
}
