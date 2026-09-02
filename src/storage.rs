//! Author: TechnoL0g
//!
//! Sled-backed chain state.
//!
//! Key layout (single tree):
//!   `b:<height BE u64>`  -> compact block: [u32 LE len][header bytes] then [u32 LE len][tx bytes]*
//!                          (wire format, ~5x smaller than JSON; decoded on read)
//!   `bid:<block id>`     -> height BE u64
//!   `t:<tx id>`          -> height BE u64 (secondary index)
//!   `w:<address>`        -> wallet state JSON
//!   `wp:<publicKey>` / `wu:<username>` -> address (wallet lookup by key / delegate name)
//!   `tl:<height BE><seq BE>`             -> tx id (global transaction order)
//!   `wt:<address>:<height BE><seq BE>`   -> tx id (per-wallet transaction history)
//!   `meta:last_height`   -> height BE u64

use crate::config::Network;
use crate::crypto::{
    address_from_public_key, block_id, deserialize_block_header, deserialize_transaction, serialize_block,
    serialize_transaction, transaction_id, SerializeOptions,
};
use crate::error::{Error, Result};
use crate::models::{string_i64, string_u64, tx_type, Block, Transaction};
use serde::{Deserialize, Serialize};
use sled::transaction::{ConflictableTransactionError, TransactionError, TransactionalTree};
use sled::IVec;
use std::collections::BTreeMap;
use std::path::Path;

const PREFIX_BLOCK: &[u8] = b"b:";
const PREFIX_BLOCK_ID: &[u8] = b"bid:";
const PREFIX_TX: &[u8] = b"t:";
const PREFIX_WALLET: &[u8] = b"w:";
const PREFIX_WALLET_PK: &[u8] = b"wp:";
const PREFIX_WALLET_USERNAME: &[u8] = b"wu:";
const PREFIX_TX_LIST: &[u8] = b"tl:";
const PREFIX_WALLET_TX: &[u8] = b"wt:";
const KEY_LAST_HEIGHT: &[u8] = b"meta:last_height";

/// Wallet state as exposed by `GET /api/wallets/:address`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletState {
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    #[serde(with = "string_i64")]
    pub balance: i64,
    #[serde(with = "string_u64")]
    pub nonce: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_public_key: Option<String>,
    #[serde(default)]
    pub produced_blocks: u64,
    #[serde(default, with = "string_u64")]
    pub forged_fees: u64,
    #[serde(default, with = "string_u64")]
    pub forged_rewards: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_block: Option<LastBlock>,
}

/// Last block forged by a delegate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastBlock {
    pub id: String,
    pub height: u64,
    pub timestamp: u32,
}

impl WalletState {
    pub fn is_delegate(&self) -> bool {
        self.username.is_some()
    }

    fn new(address: &str) -> Self {
        Self {
            address: address.to_string(),
            public_key: None,
            balance: 0,
            nonce: 0,
            username: None,
            vote: None,
            second_public_key: None,
            produced_blocks: 0,
            forged_fees: 0,
            forged_rewards: 0,
            last_block: None,
        }
    }

    fn apply(&mut self, d: &WalletDelta) -> Result<()> {
        let balance = self.balance as i128 + d.balance;
        self.balance = i64::try_from(balance)
            .map_err(|_| Error::Overflow(format!("balance overflow for {}", self.address)))?;
        self.nonce = self
            .nonce
            .checked_add(d.nonce)
            .ok_or_else(|| Error::Overflow(format!("nonce overflow for {}", self.address)))?;
        if let Some(pk) = &d.public_key {
            self.public_key.get_or_insert_with(|| pk.clone());
        }
        if let Some(u) = &d.username {
            self.username = Some(u.clone());
        }
        if let Some(v) = &d.vote {
            self.vote = v.clone();
        }
        if let Some(s) = &d.second_public_key {
            self.second_public_key = Some(s.clone());
        }
        self.produced_blocks += d.produced_blocks;
        self.forged_fees = self.forged_fees.saturating_add(d.forged_fees);
        self.forged_rewards = self.forged_rewards.saturating_add(d.forged_rewards);
        if let Some(lb) = &d.last_block {
            self.last_block = Some(lb.clone());
        }
        Ok(())
    }
}

/// Accumulated per-address changes produced by one block.
#[derive(Debug, Default, Clone)]
struct WalletDelta {
    balance: i128,
    nonce: u64,
    public_key: Option<String>,
    username: Option<String>,
    /// `Some(Some(pk))` = vote, `Some(None)` = unvote, `None` = untouched.
    vote: Option<Option<String>>,
    second_public_key: Option<String>,
    produced_blocks: u64,
    forged_fees: u64,
    forged_rewards: u64,
    last_block: Option<LastBlock>,
}

fn height_key(height: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(PREFIX_BLOCK.len() + 8);
    k.extend_from_slice(PREFIX_BLOCK);
    k.extend_from_slice(&height.to_be_bytes());
    k
}

fn prefixed(prefix: &[u8], suffix: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(prefix.len() + suffix.len());
    k.extend_from_slice(prefix);
    k.extend_from_slice(suffix.as_bytes());
    k
}

/// `<prefix><address>:<height BE><seq BE>` (address may be empty for the global list).
fn tx_order_key(prefix: &[u8], address: &str, height: u64, seq: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(prefix.len() + address.len() + 13);
    k.extend_from_slice(prefix);
    if !address.is_empty() {
        k.extend_from_slice(address.as_bytes());
        k.push(b':');
    }
    k.extend_from_slice(&height.to_be_bytes());
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

/// Every wallet touched by a transaction (sender, recipient, multipayment recipients).
fn tx_addresses(tx: &Transaction, network: u8) -> Result<Vec<String>> {
    let mut out = vec![address_from_public_key(&tx.sender_public_key, network)?];
    if let Some(r) = &tx.recipient_id {
        out.push(r.clone());
    }
    if let Some(payments) = tx.asset.as_ref().and_then(|a| a.payments.as_ref()) {
        out.extend(payments.iter().map(|p| p.recipient_id.clone()));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn ivec_to_height(v: &IVec) -> Result<u64> {
    let bytes: [u8; 8] = v
        .as_ref()
        .try_into()
        .map_err(|_| Error::Storage(sled::Error::Unsupported("corrupt height value".into())))?;
    Ok(u64::from_be_bytes(bytes))
}

fn map_tx_err(e: TransactionError<Error>) -> Error {
    match e {
        TransactionError::Abort(e) => e,
        TransactionError::Storage(e) => Error::Storage(e),
    }
}

fn abort<T>(e: Error) -> std::result::Result<T, ConflictableTransactionError<Error>> {
    Err(ConflictableTransactionError::Abort(e))
}

/// Compact on-disk block encoding: header bytes + serialised transactions, each length-prefixed.
fn encode_block(block: &Block, network: &Network) -> Result<Vec<u8>> {
    let header = serialize_block(block, true)?;
    let mut out = Vec::with_capacity(4 + header.len() + block.transactions.len() * 200);
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    for tx in &block.transactions {
        let bytes = serialize_transaction(tx, SerializeOptions::default(), network)?;
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

fn next_frame<'a>(bytes: &'a [u8], pos: &mut usize) -> Result<Option<&'a [u8]>> {
    if *pos == bytes.len() {
        return Ok(None);
    }
    let len_bytes: [u8; 4] = bytes
        .get(*pos..*pos + 4)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| Error::Storage(sled::Error::Unsupported("corrupt block record".into())))?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    let chunk = bytes
        .get(*pos + 4..*pos + 4 + len)
        .ok_or_else(|| Error::Storage(sled::Error::Unsupported("truncated block record".into())))?;
    *pos += 4 + len;
    Ok(Some(chunk))
}

fn decode_block(bytes: &[u8], network: &Network) -> Result<Block> {
    let mut pos = 0usize;
    let header = next_frame(bytes, &mut pos)?
        .ok_or_else(|| Error::Storage(sled::Error::Unsupported("empty block record".into())))?;
    let mut block = deserialize_block_header(header, network)?;
    let mut sequence = 0u32;
    while let Some(raw) = next_frame(bytes, &mut pos)? {
        let mut tx = deserialize_transaction(raw)?;
        tx.block_id = block.id.clone();
        tx.block_height = Some(block.height);
        tx.sequence = Some(sequence);
        sequence += 1;
        block.transactions.push(tx);
    }
    Ok(block)
}

/// Chain state store.
pub struct Storage {
    db: sled::Db,
    tree: sled::Tree,
    network: Network,
}

impl Storage {
    /// Open (or create) the database at `path`.
    pub fn open<P: AsRef<Path>>(path: P, network: Network) -> Result<Self> {
        // zstd page compression: block headers / wallet JSON compress ~2-3x; 64 MiB cache for API reads.
        let db = sled::Config::new()
            .path(path)
            .use_compression(true)
            .compression_factor(3)
            .cache_capacity(64 * 1024 * 1024)
            .open()?;
        let tree = db.open_tree("chain")?;
        Ok(Self { db, tree, network })
    }

    /// In-memory database (tests / ephemeral nodes).
    pub fn temporary(network: Network) -> Result<Self> {
        let db = sled::Config::new().temporary(true).open()?;
        let tree = db.open_tree("chain")?;
        Ok(Self { db, tree, network })
    }

    pub fn network(&self) -> &Network {
        &self.network
    }

    pub fn flush(&self) -> Result<()> {
        self.tree.flush()?;
        self.db.flush()?;
        Ok(())
    }

    // ------------------------------------------------------------------ blocks

    /// Persist a block and its indexes without touching wallet state.
    pub fn save_block(&self, block: &Block) -> Result<()> {
        let (block, encoded) = self.prepare_block(block)?;
        self.tree
            .transaction(|t| {
                Self::write_block(t, &block, &encoded, self.network.pubkey_hash)?;
                Ok(())
            })
            .map_err(map_tx_err)
    }

    /// Fill missing ids (block + transactions) and encode for storage.
    fn prepare_block(&self, block: &Block) -> Result<(Block, Vec<u8>)> {
        let mut block = block.clone();
        if block.id.is_none() {
            block.id = Some(block_id(&block)?);
        }
        for tx in &mut block.transactions {
            if tx.id.is_none() {
                tx.id = Some(transaction_id(tx)?);
            }
        }
        let encoded = encode_block(&block, &self.network)?;
        Ok((block, encoded))
    }

    fn decode(&self, v: &IVec) -> Result<Block> {
        decode_block(v, &self.network)
    }

    fn write_block(
        t: &TransactionalTree,
        block: &Block,
        encoded: &[u8],
        network: u8,
    ) -> std::result::Result<(), ConflictableTransactionError<Error>> {
        let height = block.height.to_be_bytes();
        t.insert(height_key(block.height), encoded)?;
        if let Some(id) = &block.id {
            t.insert(prefixed(PREFIX_BLOCK_ID, id), height.as_slice())?;
        }
        for (seq, tx) in block.transactions.iter().enumerate() {
            let id = match &tx.id {
                Some(id) => id,
                None => continue,
            };
            t.insert(prefixed(PREFIX_TX, id), height.as_slice())?;
            let seq = tx.sequence.unwrap_or(seq as u32);
            t.insert(tx_order_key(PREFIX_TX_LIST, "", block.height, seq), id.as_bytes())?;
            for addr in tx_addresses(tx, network).or_else(abort)? {
                t.insert(tx_order_key(PREFIX_WALLET_TX, &addr, block.height, seq), id.as_bytes())?;
            }
        }
        let last = match t.get(KEY_LAST_HEIGHT)? {
            Some(v) => ivec_to_height(&v).or_else(abort)?,
            None => 0,
        };
        if block.height > last {
            t.insert(KEY_LAST_HEIGHT, height.as_slice())?;
        }
        Ok(())
    }

    pub fn get_block_by_height(&self, height: u64) -> Result<Option<Block>> {
        match self.tree.get(height_key(height))? {
            Some(v) => Ok(Some(self.decode(&v)?)),
            None => Ok(None),
        }
    }

    pub fn get_block_by_id(&self, id: &str) -> Result<Option<Block>> {
        match self.tree.get(prefixed(PREFIX_BLOCK_ID, id))? {
            Some(v) => self.get_block_by_height(ivec_to_height(&v)?),
            None => Ok(None),
        }
    }

    /// Highest stored height (0 when empty).
    pub fn get_last_height(&self) -> Result<u64> {
        match self.tree.get(KEY_LAST_HEIGHT)? {
            Some(v) => ivec_to_height(&v),
            None => Ok(0),
        }
    }

    pub fn get_last_block(&self) -> Result<Option<Block>> {
        let h = self.get_last_height()?;
        if h == 0 {
            return Ok(None);
        }
        self.get_block_by_height(h)
    }

    /// Blocks ordered by height descending (API default), paginated.
    pub fn get_blocks(&self, offset: usize, limit: usize) -> Result<Vec<Block>> {
        let mut out = Vec::with_capacity(limit);
        for item in self.tree.scan_prefix(PREFIX_BLOCK).rev().skip(offset).take(limit) {
            let (_, v) = item?;
            out.push(self.decode(&v)?);
        }
        Ok(out)
    }

    /// Blocks `[from_height, from_height + limit)` ascending (used by sync / P2P).
    pub fn get_blocks_from(&self, from_height: u64, limit: usize) -> Result<Vec<Block>> {
        let mut out = Vec::with_capacity(limit);
        for item in self
            .tree
            .range(height_key(from_height)..height_key(from_height.saturating_add(limit as u64)))
        {
            let (_, v) = item?;
            out.push(self.decode(&v)?);
        }
        Ok(out)
    }

    /// Number of stored blocks (full key scan — slow on a large chain; prefer `get_last_height`).
    pub fn block_count(&self) -> usize {
        self.tree.scan_prefix(PREFIX_BLOCK).count()
    }

    // ------------------------------------------------------------ transactions

    pub fn get_transaction(&self, id: &str) -> Result<Option<Transaction>> {
        let height = match self.tree.get(prefixed(PREFIX_TX, id))? {
            Some(v) => ivec_to_height(&v)?,
            None => return Ok(None),
        };
        let block = match self.get_block_by_height(height)? {
            Some(b) => b,
            None => return Ok(None),
        };
        Ok(block.transactions.into_iter().find(|tx| tx.id.as_deref() == Some(id)))
    }

    /// Transaction plus the timestamp of its block (needed by the legacy API transformer).
    pub fn get_transaction_with_timestamp(&self, id: &str) -> Result<Option<(Transaction, u32)>> {
        let height = match self.tree.get(prefixed(PREFIX_TX, id))? {
            Some(v) => ivec_to_height(&v)?,
            None => return Ok(None),
        };
        let block = match self.get_block_by_height(height)? {
            Some(b) => b,
            None => return Ok(None),
        };
        let ts = block.timestamp;
        Ok(block.transactions.into_iter().find(|tx| tx.id.as_deref() == Some(id)).map(|tx| (tx, ts)))
    }

    /// Transaction ids newest-first, optionally restricted to one wallet.
    pub fn transaction_ids(&self, address: Option<&str>) -> Result<Vec<String>> {
        let prefix = match address {
            Some(a) => {
                let mut p = prefixed(PREFIX_WALLET_TX, a);
                p.push(b':');
                p
            }
            None => PREFIX_TX_LIST.to_vec(),
        };
        let mut out = Vec::new();
        for item in self.tree.scan_prefix(prefix).rev() {
            let (_, v) = item?;
            out.push(String::from_utf8_lossy(&v).into_owned());
        }
        Ok(out)
    }

    // ----------------------------------------------------------------- wallets

    /// Resolve address / public key / delegate username to a wallet.
    pub fn find_wallet(&self, id: &str) -> Result<Option<WalletState>> {
        if let Some(w) = self.get_wallet(id)? {
            return Ok(Some(w));
        }
        for prefix in [PREFIX_WALLET_PK, PREFIX_WALLET_USERNAME] {
            if let Some(v) = self.tree.get(prefixed(prefix, id))? {
                return self.get_wallet(&String::from_utf8_lossy(&v));
            }
        }
        Ok(None)
    }

    /// All wallets (full scan — used for delegate ranking / wallet listing).
    pub fn all_wallets(&self) -> Result<Vec<WalletState>> {
        let mut out = Vec::new();
        for item in self.tree.scan_prefix(PREFIX_WALLET) {
            let (_, v) = item?;
            out.push(serde_json::from_slice(&v)?);
        }
        Ok(out)
    }

    pub fn get_wallet(&self, address: &str) -> Result<Option<WalletState>> {
        match self.tree.get(prefixed(PREFIX_WALLET, address))? {
            Some(v) => Ok(Some(serde_json::from_slice(&v)?)),
            None => Ok(None),
        }
    }

    /// Atomically adjust balance / nonce of a wallet, creating it if needed.
    pub fn update_wallet_state(&self, address: &str, balance_delta: i64, nonce_delta: u64) -> Result<WalletState> {
        let delta = WalletDelta { balance: balance_delta as i128, nonce: nonce_delta, ..Default::default() };
        let mut deltas = BTreeMap::new();
        deltas.insert(address.to_string(), delta);
        self.tree
            .transaction(|t| {
                let states = Self::write_wallets(t, &deltas)?;
                states
                    .into_iter()
                    .next()
                    .map_or_else(|| abort(Error::NotFound(address.to_string())), Ok)
            })
            .map_err(map_tx_err)
    }

    fn write_wallets(
        t: &TransactionalTree,
        deltas: &BTreeMap<String, WalletDelta>,
    ) -> std::result::Result<Vec<WalletState>, ConflictableTransactionError<Error>> {
        let mut out = Vec::with_capacity(deltas.len());
        for (address, delta) in deltas {
            let key = prefixed(PREFIX_WALLET, address);
            let mut state = match t.get(&key)? {
                Some(v) => serde_json::from_slice(&v).map_err(Error::Json).or_else(abort)?,
                None => WalletState::new(address),
            };
            state.apply(delta).or_else(abort)?;
            let json = serde_json::to_vec(&state).map_err(Error::Json).or_else(abort)?;
            t.insert(key, json)?;
            if let Some(pk) = &state.public_key {
                t.insert(prefixed(PREFIX_WALLET_PK, pk), address.as_bytes())?;
            }
            if let Some(u) = &state.username {
                t.insert(prefixed(PREFIX_WALLET_USERNAME, u), address.as_bytes())?;
            }
            out.push(state);
        }
        Ok(out)
    }

    // ------------------------------------------------------------ block apply

    /// Save the block and apply all balance / nonce / vote effects in one atomic transaction.
    pub fn apply_block(&self, block: &Block) -> Result<()> {
        self.apply_blocks(std::slice::from_ref(block))
    }

    /// Apply a consecutive chunk of blocks in ONE sled transaction (bulk import / sync batches).
    /// Wallet deltas of all blocks are merged first, then wallets + blocks + indexes are written.
    pub fn apply_blocks(&self, blocks: &[Block]) -> Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        let mut prepared = Vec::with_capacity(blocks.len());
        for b in blocks {
            prepared.push(self.prepare_block(b)?);
        }
        let batch: Vec<&Block> = prepared.iter().map(|(b, _)| b).collect();
        let mut deltas: BTreeMap<String, WalletDelta> = BTreeMap::new();
        for block in &batch {
            self.accumulate_deltas(block, &batch, &mut deltas)?;
        }
        self.tree
            .transaction(|t| {
                Self::write_wallets(t, &deltas)?;
                for (block, encoded) in &prepared {
                    Self::write_block(t, block, encoded, self.network.pubkey_hash)?;
                }
                Ok(())
            })
            .map_err(map_tx_err)
    }

    /// Locate an HTLC lock transaction either in the current batch or in the database.
    fn find_lock(&self, id: &str, batch: &[&Block]) -> Result<Option<Transaction>> {
        for block in batch {
            if let Some(tx) = block.transactions.iter().find(|t| t.id.as_deref() == Some(id)) {
                return Ok(Some(tx.clone()));
            }
        }
        self.get_transaction(id)
    }

    fn accumulate_deltas(
        &self,
        block: &Block,
        batch: &[&Block],
        deltas: &mut BTreeMap<String, WalletDelta>,
    ) -> Result<()> {
        let net = self.network.pubkey_hash;

        let generator = address_from_public_key(&block.generator_public_key, net)?;
        {
            let d = deltas.entry(generator).or_default();
            d.balance += block.reward as i128 + block.total_fee as i128;
            d.public_key = Some(block.generator_public_key.clone());
            d.produced_blocks += 1;
            d.forged_fees += block.total_fee;
            d.forged_rewards += block.reward;
            d.last_block = Some(LastBlock {
                id: block.id.clone().unwrap_or_default(),
                height: block.height,
                timestamp: block.timestamp,
            });
        }

        for tx in &block.transactions {
            let sender = address_from_public_key(&tx.sender_public_key, net)?;
            {
                let d = deltas.entry(sender.clone()).or_default();
                d.balance -= tx.amount as i128 + tx.fee as i128;
                d.nonce += 1;
                d.public_key = Some(tx.sender_public_key.clone());
            }
            match tx.type_ {
                tx_type::TRANSFER => {
                    if let Some(r) = &tx.recipient_id {
                        deltas.entry(r.clone()).or_default().balance += tx.amount as i128;
                    }
                }
                tx_type::MULTI_PAYMENT => {
                    let payments = tx.asset.as_ref().and_then(|a| a.payments.as_ref());
                    for p in payments.into_iter().flatten() {
                        deltas.entry(p.recipient_id.clone()).or_default().balance += p.amount as i128;
                        deltas.entry(sender.clone()).or_default().balance -= p.amount as i128;
                    }
                }
                tx_type::VOTE => {
                    let votes = tx.asset.as_ref().and_then(|a| a.votes.as_ref());
                    for v in votes.into_iter().flatten() {
                        let d = deltas.entry(sender.clone()).or_default();
                        d.vote = Some(if let Some(pk) = v.strip_prefix('+') { Some(pk.to_string()) } else { None });
                    }
                }
                tx_type::DELEGATE_REGISTRATION => {
                    if let Some(del) = tx.asset.as_ref().and_then(|a| a.delegate.as_ref()) {
                        deltas.entry(sender.clone()).or_default().username = Some(del.username.clone());
                    }
                }
                tx_type::SECOND_SIGNATURE => {
                    if let Some(sig) = tx.asset.as_ref().and_then(|a| a.signature.as_ref()) {
                        deltas.entry(sender.clone()).or_default().second_public_key = Some(sig.public_key.clone());
                    }
                }
                tx_type::HTLC_CLAIM => {
                    if let Some(c) = tx.asset.as_ref().and_then(|a| a.claim.as_ref()) {
                        if let Some(lock) = self.find_lock(&c.lock_transaction_id, batch)? {
                            if let Some(r) = &lock.recipient_id {
                                deltas.entry(r.clone()).or_default().balance += lock.amount as i128;
                            }
                        }
                    }
                }
                tx_type::HTLC_REFUND => {
                    if let Some(r) = tx.asset.as_ref().and_then(|a| a.refund.as_ref()) {
                        if let Some(lock) = self.find_lock(&r.lock_transaction_id, batch)? {
                            let lock_sender = address_from_public_key(&lock.sender_public_key, net)?;
                            deltas.entry(lock_sender).or_default().balance += lock.amount as i128;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}
