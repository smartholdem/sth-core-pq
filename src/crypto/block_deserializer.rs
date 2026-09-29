//! Author: TechnoL0g
//!
//! Block header deserialisation — inverse of `serialize_block`, port of
//! `Blocks.Deserializer.deserializeHeader` (snapshot `blocks` records and P2P payloads).

use super::block_serializer::block_id;
use super::bytes::ByteReader;
use crate::config::Network;
use crate::error::{Error, Result};
use crate::models::Block;

/// Parse a serialised header (with signature). Transactions are not part of the header.
/// The genesis id is taken from the network config (core applies the same fix).
fn parse_header(bytes: &[u8], network: &Network, strict: bool) -> Result<(Block, usize)> {
    let mut r = ByteReader::new(bytes);
    let version = r.u32_le()?;
    let timestamp = r.u32_le()?;
    let height = r.u32_le()? as u64;
    let previous_block = r.hex(32)?;
    let number_of_transactions = r.u32_le()?;
    let total_amount = r.u64_le()?;
    let total_fee = r.u64_le()?;
    let reward = r.u64_le()?;
    let payload_length = r.u32_le()?;
    let payload_hash = r.hex(32)?;
    let generator_public_key = r.hex(33)?;

    let block_signature = if r.remaining() >= 2 {
        let len = r
            .peek_u8(1)
            .map(|l| l as usize + 2)
            .ok_or_else(|| Error::Serialization("truncated block signature".into()))?;
        Some(r.hex(len)?)
    } else {
        None
    };
    // stage C: `alg u8 || len u16 LE || ML-DSA signature` after the classic signature on version-1 blocks
    let pq_signature = if version >= crate::models::BLOCK_VERSION_PQ && r.remaining() >= 3 {
        let algorithm = r.u8()?;
        let len = r.u16_le()? as usize;
        Some(crate::models::PqSignatureBlock { algorithm, signature: r.hex(len)? })
    } else {
        None
    };
    if strict && r.remaining() > 0 {
        return Err(Error::Serialization(format!(
            "block {height}: {} unexpected trailing bytes after header",
            r.remaining()
        )));
    }
    let consumed = bytes.len() - r.remaining();

    let mut block = Block {
        id: None,
        version,
        timestamp,
        previous_block,
        height,
        number_of_transactions,
        total_amount,
        total_fee,
        reward,
        payload_length,
        payload_hash,
        generator_public_key,
        block_signature,
        pq_signature,
        transactions: Vec::new(),
    };
    block.id = Some(if height == 1 { network.genesis_block_id.to_string() } else { block_id(&block)? });
    Ok((block, consumed))
}

/// Header-only buffer (storage, snapshots): trailing bytes are an error.
pub fn deserialize_block_header(bytes: &[u8], network: &Network) -> Result<Block> {
    parse_header(bytes, network, true).map(|(b, _)| b)
}

/// Header at the start of a full block (`postBlock`: header ‖ u32-LE lengths ‖ transactions). Returns the header and how
/// many bytes it took, so the caller can continue with the transaction section (PQ Nodes rejected every block with
/// transactions pushed by another PQ Node before: "N unexpected trailing bytes after header").
pub fn deserialize_block_header_prefix(bytes: &[u8], network: &Network) -> Result<(Block, usize)> {
    parse_header(bytes, network, false)
}
