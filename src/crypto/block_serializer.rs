//! Author: TechnoL0g
//!
//! Block header serialisation, id derivation and full block verification —
//! port of `Blocks.Serializer` / `Block.verify()` (idFullSha256 = true on mainnet).

use super::bytes::ByteWriter;
use super::ecdsa::verify_ecdsa;
use super::hash::{sha256, sha256_multi};
use super::tx_serializer::{transaction_id, verify_transaction_signature};
use crate::config::Network;
use crate::error::{Error, Result};
use crate::models::Block;
use rayon::prelude::*;

/// Serialise the block header (`Serializer.serialize`), optionally appending the signature(s).
pub fn serialize_block(block: &Block, include_signature: bool) -> Result<Vec<u8>> {
    serialize_block_parts(block, include_signature, include_signature)
}

/// Header, then `blockSignature` (DER), then — for version-1 blocks — `pqSignature` as `alg u8 || len u16 LE || sig`.
fn serialize_block_parts(block: &Block, include_signature: bool, include_pq: bool) -> Result<Vec<u8>> {
    if block.previous_block.len() != 64 {
        return Err(Error::Serialization(format!(
            "previousBlock of block {} must be a 64-char sha256 hex, got '{}'",
            block.height, block.previous_block
        )));
    }
    let mut w = ByteWriter::with_capacity(if block.pq_signature.is_some() { 2_600 } else { 256 });
    w.u32_le(block.version)
        .u32_le(block.timestamp)
        .u32_le(block.height as u32)
        .hex(&block.previous_block)?
        .u32_le(block.number_of_transactions)
        .u64_le(block.total_amount)
        .u64_le(block.total_fee)
        .u64_le(block.reward)
        .u32_le(block.payload_length)
        .hex(&block.payload_hash)?
        .hex(&block.generator_public_key)?;
    if include_signature {
        if let Some(sig) = &block.block_signature {
            w.hex(sig)?;
        }
    }
    if include_pq && block.version >= crate::models::BLOCK_VERSION_PQ {
        if let Some(pq) = &block.pq_signature {
            let sig = hex::decode(&pq.signature)?;
            w.u8(pq.algorithm).u16_le(sig.len() as u16).bytes(&sig);
        }
    }
    Ok(w.into_inner())
}

/// Stage C message signed by the delegate's ML-DSA key: `M_B = sha256(header || blockSignature)`.
pub fn block_pq_message(block: &Block) -> Result<[u8; 32]> {
    Ok(sha256(&serialize_block_parts(block, true, false)?))
}

/// Verify the version-1 `pqSignature` against the delegate's registered PQ public key (hex).
pub fn verify_block_pq_signature(block: &Block, pq_public_key_hex: &str) -> Result<bool> {
    let Some(pq) = &block.pq_signature else { return Ok(false) };
    if pq.algorithm != super::pq::ALG_ML_DSA_44 {
        return Ok(false);
    }
    super::pq::verify_with_ctx(&hex::decode(pq_public_key_hex)?, &block_pq_message(block)?, &hex::decode(&pq.signature)?, super::pq::BLOCK_CTX)
}

/// Block id = sha256(header || signature), hex.
/// Legacy `Serializer.serializeWithTransactions`: signed header, then a `u32 LE` length per
/// transaction, then all transaction bytes (the payload of `p2p.blocks.postBlock`).
pub fn serialize_block_with_transactions(block: &Block, network: &Network) -> Result<Vec<u8>> {
    let mut out = serialize_block(block, true)?;
    let mut bodies = Vec::with_capacity(block.transactions.len());
    for tx in &block.transactions {
        bodies.push(super::tx_serializer::serialize_transaction(tx, super::tx_serializer::SerializeOptions::default(), network)?);
    }
    for b in &bodies {
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    }
    for b in bodies {
        out.extend_from_slice(&b);
    }
    Ok(out)
}

pub fn block_id(block: &Block) -> Result<String> {
    Ok(hex::encode(sha256(&serialize_block(block, true)?)))
}

/// Hash signed by the forging delegate = sha256(header without signature).
pub fn block_signing_hash(block: &Block) -> Result<[u8; 32]> {
    Ok(sha256(&serialize_block(block, false)?))
}

/// Verify the delegate's DER ECDSA signature over the header.
pub fn verify_block_signature(block: &Block) -> Result<bool> {
    let sig = block
        .block_signature
        .as_ref()
        .ok_or_else(|| Error::Signature("block has no blockSignature".into()))?;
    let hash = block_signing_hash(block)?;
    verify_ecdsa(&hash, &hex::decode(sig)?, &block.generator_public_key)
}

/// payloadHash = sha256(concat(tx.id bytes)) over the embedded transactions (in `sequence` order).
pub fn block_payload_hash(block: &Block) -> Result<String> {
    let mut txs: Vec<_> = block.transactions.iter().collect();
    if txs.iter().all(|tx| tx.sequence.is_some()) {
        txs.sort_by_key(|tx| tx.sequence);
    }
    let mut id_bytes = Vec::with_capacity(txs.len());
    for tx in txs {
        let id = match &tx.id {
            Some(id) => id.clone(),
            None => transaction_id(tx)?,
        };
        id_bytes.push(hex::decode(id)?);
    }
    Ok(hex::encode(sha256_multi(id_bytes.iter().map(|b| b.as_slice()))))
}

#[derive(Debug, Clone, Default)]
pub struct BlockVerification {
    pub verified: bool,
    pub errors: Vec<String>,
}

/// Per-transaction cryptographic check: stored id matches the serialised bytes, signature is valid.
fn check_transaction(tx: &crate::models::Transaction) -> Option<String> {
    let id = tx.id.as_deref()?;
    match transaction_id(tx) {
        Ok(computed) if computed != id => return Some(format!("Invalid transaction id: {id} expected: {computed}")),
        Err(e) => return Some(format!("Cannot serialise transaction {id}: {e}")),
        _ => {}
    }
    match verify_transaction_signature(tx) {
        Ok(true) => None,
        Ok(false) => Some(format!("Invalid transaction signature: {id}")),
        Err(e) => Some(format!("Cannot verify transaction {id}: {e}")),
    }
}

/// Structural + cryptographic verification of a block (`Block.verify()` in core).
/// Does not check chain linkage (previousBlock vs. stored chain) — that is the sync layer's job.
pub fn verify_block(block: &Block, network: &Network) -> BlockVerification {
    let mut errors = Vec::new();
    let milestone = network.milestone(block.height);

    if block.height != 1 && block.previous_block.is_empty() {
        errors.push("Invalid previous block".into());
    }
    if block.reward != milestone.reward {
        errors.push(format!("Invalid block reward: {} expected: {}", block.reward, milestone.reward));
    }
    match verify_block_signature(block) {
        Ok(true) => {}
        Ok(false) => errors.push("Failed to verify block signature".into()),
        Err(e) => errors.push(format!("Failed to verify block signature: {e}")),
    }
    // stage C: version 1 (hybrid signature) only under `pq.blocks`; version 0 only until the grace window closes
    let (v1_ok, v0_ok) = network.block_versions_allowed(block.height);
    let version_ok = if block.version == crate::models::BLOCK_VERSION_PQ { v1_ok } else { v0_ok && block.version == milestone.block_version() };
    if !version_ok {
        errors.push(if block.version == 0 && v1_ok { "Invalid block version: pq.blocks requires version 1 (BlockPqRequiredError)".into() } else { "Invalid block version".into() });
    }
    if block.version == crate::models::BLOCK_VERSION_PQ {
        match &block.pq_signature {
            None => errors.push("Version 1 block without pqSignature (BlockPqSignatureMissingError)".into()),
            Some(pq) if pq.algorithm != super::pq::ALG_ML_DSA_44 => errors.push(format!("Unknown pqSignature algorithm {}", pq.algorithm)),
            Some(pq) if pq.signature.len() != super::pq::SIG_LEN * 2 => errors.push("Invalid pqSignature length".into()),
            _ => {}
        }
    } else if block.pq_signature.is_some() {
        errors.push("pqSignature is only valid on version 1 blocks".into());
    }
    if block.timestamp > network.now_epoch() + milestone.blocktime {
        errors.push("Invalid block timestamp".into());
    }
    if block.transactions.len() as u32 != block.number_of_transactions {
        errors.push("Invalid number of transactions".into());
    }
    if block.height > 1 && block.transactions.len() as u32 > milestone.max_transactions() {
        errors.push("Transactions length is too high".into());
    }
    if let Some(id) = &block.id {
        match block_id(block) {
            Ok(computed) if block.height != 1 && &computed != id => {
                errors.push(format!("Invalid block id: {id} expected: {computed}"));
            }
            Err(e) => errors.push(format!("Cannot compute block id: {e}")),
            _ => {}
        }
    }

    let mut total_amount: u128 = 0;
    let mut total_fee: u128 = 0;
    let mut seen = std::collections::HashSet::new();
    for tx in &block.transactions {
        let Some(id) = &tx.id else {
            errors.push("Transaction without id".into());
            continue;
        };
        if !seen.insert(id.as_str()) {
            errors.push(format!("Encountered duplicate transaction: {id}"));
        }
        if let Some(exp) = tx.expiration {
            if exp > 0 && (exp as u64) <= block.height {
                errors.push(format!("Encountered expired transaction: {id}"));
            }
        }
        total_amount += tx.amount as u128;
        total_fee += tx.fee as u128;
    }
    // id recomputation + signature verification dominate the cost → all cores
    errors.extend(block.transactions.par_iter().filter_map(check_transaction).collect::<Vec<_>>());
    for tx in &block.transactions {
        let id = tx.id.as_deref().unwrap_or("?");
        if let Err(e) = crate::rules::check_pq_format(tx, milestone) {
            errors.push(format!("{e}: {id}"));
        }
        if tx.type_group == crate::models::TYPE_GROUP_CORE {
            continue;
        }
        if tx.is_token() {
            if !milestone.tokens {
                errors.push(format!("Token transaction before tokens activation: {id}"));
            } else if let Err(e) = crate::rules::check_token_format(tx, milestone) {
                errors.push(format!("{e}: {id}"));
            }
        } else if !tx.is_sobj() {
            errors.push(format!("Unsupported transaction typeGroup {} type {}: {id}", tx.type_group, tx.type_));
        } else if !milestone.sobj_active {
            errors.push(format!("SmartObject transaction before sobj (ship13) activation: {id}"));
        } else if let Err(e) = crate::rules::check_sobj_format(tx, milestone) {
            errors.push(format!("{e}: {id}"));
        }
    }
    if total_amount != block.total_amount as u128 {
        errors.push("Invalid total amount".into());
    }
    if total_fee != block.total_fee as u128 {
        errors.push("Invalid total fee".into());
    }
    // The genesis block is trusted from configuration in core: its payloadHash is the nethash.
    if block.height == 1 {
        if block.payload_hash != network.nethash {
            errors.push("Genesis payload hash does not match the network nethash".into());
        }
    } else {
        match block_payload_hash(block) {
            Ok(h) if h != block.payload_hash => errors.push("Invalid payload hash".into()),
            Err(e) => errors.push(format!("Cannot compute payload hash: {e}")),
            _ => {}
        }
    }

    BlockVerification { verified: errors.is_empty(), errors }
}
