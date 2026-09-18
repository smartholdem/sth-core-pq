//! Author: TechnoL0g
//!
//! Sled-backed chain state.
//!
//! Key layout (single tree):
//!   `b:<height BE u64>`  → compact block: [u32 LE len][header bytes] then [u32 LE len][tx bytes]*
//!                          (wire format, ~5x smaller than JSON; decoded on read)
//!   `bid:<block id>`     → height BE u64
//!   `t:<tx id>`          → height BE u64 (secondary index)
//!   `w:<address>`        → wallet state JSON
//!   `wp:<publicKey>` / `wu:<username>` → address (wallet lookup by key / delegate name)
//!   `tl:<height BE><seq BE>`             → tx id (global transaction order)
//!   `wt:<address>:<height BE><seq BE>`   → tx id (per-wallet transaction history)
//!   `lk:<lock id>`       → open HTLC lock JSON (`/api/locks`)
//!   `rd:<round BE u64>`  → active delegates of a round JSON (written while following the chain)
//!   `meta:last_height`   → height BE u64

use crate::config::Network;
use crate::crypto::{
    address_from_public_key, block_id, deserialize_block_header, deserialize_transaction, serialize_block,
    serialize_transaction, transaction_id, SerializeOptions,
};
use crate::error::{Error, Result};
use crate::models::{string_i64, string_u64, tx_type, Block, HtlcExpiration, MultiSignatureAsset, Transaction};
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
const PREFIX_LOCK: &[u8] = b"lk:";
const PREFIX_ROUND: &[u8] = b"rd:";
const PREFIX_UNDO: &[u8] = b"undo:";
/// `dv:<delegate public key>` → vote weight (BE u64), maintained incrementally on every wallet write.
const PREFIX_VOTES: &[u8] = b"dv:";
/// SmartObject (sObject) name index: `en:<name lowercase>-<type>` → registration transaction id (network-wide
/// uniqueness). The `en:` prefix is historical (kept so existing databases need no migration) — it is an sObject key.
const PREFIX_SOBJ_NAME: &[u8] = b"en:";
/// SmartObject owner index: `eo:<registrationId>` → current owner address, written only after an sObject transfer /
/// buy (default owner = registrant). Historical prefix, sObject key.
const PREFIX_SOBJ_OWNER: &[u8] = b"eo:";
/// `mk:<registrationId>` → owner address for every open sale order (sobjV2 market).
const PREFIX_MARKET: &[u8] = b"mk:";
/// `tk:<tokenId>` → owner address (immutable, written by TokenInit); `tks:<SYMBOL>` → tokenId.
const PREFIX_TOKEN_OWNER: &[u8] = b"tk:";
const PREFIX_TOKEN_SYMBOL: &[u8] = b"tks:";
/// `pqc:<address>` → alg — wallets with a Quantum Shield commitment (count for metrics).
const PREFIX_PQ_COMMIT: &[u8] = b"pqc:";
/// `pqk:<address>` → alg — wallets with a registered PQ key (stage B, count for metrics).
const PREFIX_PQ_KEY: &[u8] = b"pqk:";
/// `fc:<height BE u64>` → SHIP-35 finality certificate (JSON).
const PREFIX_FINALITY: &[u8] = b"fc:";

/// `eq:<delegate public key>:<height BE u64>` → proven double vote of that delegate at that height (SHIP-35 slashing, full history).
const PREFIX_EQUIVOCATION: &[u8] = b"eq:";

/// SHIP-35 equivocation proof: one delegate key signed two different block ids at the same height. Anyone can verify it
/// from the two signatures alone; a node that holds it excludes the delegate from round snapshots while `finality.slashing`
/// is on (`banned_until_round`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EquivocationProof {
    pub height: u64,
    pub public_key: String,
    pub block_ids: [String; 2],
    pub signatures: [String; 2],
    /// Tip height of the node that recorded the proof (round of exclusion starts at the next round).
    pub detected_height: u64,
    pub banned_until_round: u64,
}

/// SHIP-35 finality certificate: ≥ quorum delegate votes for one `(height, blockId)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinalityCert {
    pub height: u64,
    pub block_id: String,
    /// delegate public key → Schnorr signature over the vote hash
    pub votes: BTreeMap<String, String>,
}
const KEY_VOTE_INDEX: &[u8] = b"meta:vote_index";
/// Blocks that can be rolled back (undo records kept) once undo logging is on.
pub const UNDO_DEPTH: u64 = 1_000;
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resigned: bool,
    /// Open HTLC locks created by this wallet, keyed by lock transaction id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub locks: BTreeMap<String, LockRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_signature: Option<MultiSignatureAsset>,
    /// smart objects (sObjects) registered by this wallet, keyed by registration transaction id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", alias = "entities")]
    pub sobjects: BTreeMap<String, SmartObject>,
    /// Quantum Shield stage A: last `sthpq1:` commitment published by this wallet (self-transfer vendorField).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pq_commitment: Option<crate::crypto::pq::PqCommitment>,
    /// Quantum Shield stage B: registered ML-DSA key (type 1, v3). Replaces the legacy second public key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pq_key: Option<crate::crypto::pq::PqKey>,
    /// Native token balances: tokenId → amount in minimal units.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tokens: BTreeMap<String, u64>,
    /// Tokens this wallet initialized (owner side): tokenId → registry state. Lives in the owner wallet so undo is automatic.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tokens_issued: BTreeMap<String, TokenState>,
}

/// Consensus state of a native token (SPEC-TOKENS-NATIVE §5), stored in the owner's wallet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenState {
    pub symbol: String,
    pub decimals: u8,
    pub flags: u8,
    pub supply: u64,
    pub supply_cap: u64,
    pub owner: String,
    pub init_height: u64,
    /// On-chain manifest (TokenMeta), last one wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<crate::models::TokenMeta>,
}

/// `attributes.sobjects[<registrationId>]` — one SmartObject (sObject): a named, owned, transferable on-chain object (typeGroup 2 / type 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmartObject {
    #[serde(rename = "type")]
    pub type_: u8,
    pub sub_type: u8,
    pub data: crate::models::SmartObjectData,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resigned: bool,
    /// Open sale order (smartoshi), sobjV2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<u64>,
}

/// Open HTLC lock (`attributes.htlc.locks[id]` / `/api/locks`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockRecord {
    pub lock_id: String,
    pub sender_public_key: String,
    pub recipient_id: String,
    #[serde(with = "string_u64")]
    pub amount: u64,
    pub secret_hash: String,
    pub expiration: HtlcExpiration,
    pub timestamp: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_field: Option<String>,
}

/// Delegate ranking entry (`/api/delegates`, `/api/rounds/:round/delegates`, forger).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegateRank {
    pub public_key: String,
    #[serde(with = "string_u64")]
    pub votes: u64,
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

    /// Sum of open HTLC locks (`attributes.htlc.lockedBalance`).
    pub fn locked_balance(&self) -> u64 {
        self.locks.values().map(|l| l.amount).sum()
    }

    pub fn new(address: &str) -> Self {
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
            resigned: false,
            locks: BTreeMap::new(),
            multi_signature: None,
            sobjects: BTreeMap::new(),
            pq_commitment: None,
            pq_key: None,
            tokens: BTreeMap::new(),
            tokens_issued: BTreeMap::new(),
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
        if d.resigned {
            self.resigned = true;
        }
        for id in &d.locks_removed {
            self.locks.remove(id);
        }
        for l in &d.locks_added {
            self.locks.insert(l.lock_id.clone(), l.clone());
        }
        if let Some(m) = &d.multi_signature {
            self.multi_signature = Some(m.clone());
        }
        if let Some(c) = &d.pq_commitment {
            self.pq_commitment = Some(c.clone());
        }
        if let Some(k) = &d.pq_key {
            self.pq_key = Some(k.clone());
            self.second_public_key = None;
        }
        for (id, st) in &d.tokens_issued {
            self.tokens_issued.insert(id.clone(), st.clone());
        }
        for (id, m) in &d.token_meta {
            let st = self.tokens_issued.get_mut(id).ok_or_else(|| Error::Sync(format!("token {id} not issued by {}", self.address)))?;
            st.meta = Some(m.clone());
        }
        for (id, change) in &d.token_supply {
            let st = self.tokens_issued.get_mut(id).ok_or_else(|| Error::Sync(format!("token {id} not issued by {}", self.address)))?;
            let v = st.supply as i128 + change;
            if v < 0 || v > u64::MAX as i128 {
                return Err(Error::Sync(format!("token {id} supply out of range")));
            }
            st.supply = v as u64;
        }
        for (id, change) in &d.tokens {
            let v = *self.tokens.get(id).unwrap_or(&0) as i128 + change;
            if v < 0 || v > u64::MAX as i128 {
                return Err(Error::Sync(format!("negative token balance for {} ({id})", self.address)));
            }
            if v == 0 {
                self.tokens.remove(id);
            } else {
                self.tokens.insert(id.clone(), v as u64);
            }
        }
        for id in &d.sobjects_removed {
            self.sobjects.remove(id);
        }
        for id in &d.tokens_removed {
            self.tokens_issued.remove(id);
        }
        for (id, rec) in &d.sobjects {
            self.sobjects.insert(id.clone(), rec.clone());
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
    resigned: bool,
    locks_added: Vec<LockRecord>,
    locks_removed: Vec<String>,
    multi_signature: Option<MultiSignatureAsset>,
    /// sObject records written by this block (register / update / resign), keyed by registration id.
    sobjects: Vec<(String, SmartObject)>,
    /// sObjects / token registries handed over to another wallet (sObject transfer).
    sobjects_removed: Vec<String>,
    tokens_removed: Vec<String>,
    /// sObject ownership changes to index: id → new owner.
    sobj_owner: Vec<(String, String)>,
    pq_commitment: Option<crate::crypto::pq::PqCommitment>,
    pq_key: Option<crate::crypto::pq::PqKey>,
    /// tokenId → balance change.
    tokens: BTreeMap<String, i128>,
    /// TokenInit by this wallet.
    tokens_issued: Vec<(String, TokenState)>,
    /// tokenId → supply change (applied to the owner wallet's registry entry).
    token_supply: BTreeMap<String, i128>,
    /// TokenMeta by the owner (applied to its registry entry).
    token_meta: Vec<(String, crate::models::TokenMeta)>,
}

fn sobj_name_key(name: &str, type_: u8) -> String {
    format!("{}-{}", name.to_lowercase(), type_)
}

fn height_key(height: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(PREFIX_BLOCK.len() + 8);
    k.extend_from_slice(PREFIX_BLOCK);
    k.extend_from_slice(&height.to_be_bytes());
    k
}

/// `(voted delegate, weight)` of a wallet — weight = balance + locked balance, like the legacy `voteBalance`.
fn vote_weight(w: Option<&WalletState>) -> Option<(String, u64)> {
    let w = w?;
    let pk = w.vote.clone()?;
    Some((pk, w.balance.max(0) as u64 + w.locked_balance()))
}

fn be_u64(v: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b[..v.len().min(8)].copy_from_slice(&v[..v.len().min(8)]);
    u64::from_be_bytes(b)
}

fn undo_key(height: u64) -> Vec<u8> {
    let mut k = PREFIX_UNDO.to_vec();
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
    // token recipients (transfer items, mint) and the new owner of a transferred sObject see the transaction in their history
    if let Some(a) = tx.token_asset() {
        out.extend(a.transfers.iter().flatten().map(|t| t.recipient_id.clone()));
        out.extend(a.recipient_id.clone());
    }
    if let Some(e) = tx.sobj_asset() {
        out.extend(e.recipient_id.clone());
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
    /// When set, every applied block stores the previous wallet states so it can be rolled back
    /// (fork handling while following / forging). Off during bulk import and catch-up.
    undo_enabled: std::sync::atomic::AtomicBool,
}

/// Previous wallet states touched by one block (`None` = wallet did not exist).
type UndoRecord = Vec<(String, Option<WalletState>)>;

impl Storage {
    /// Open (or create) the database at `path` with the default 64 MiB page cache.
    pub fn open<P: AsRef<Path>>(path: P, network: Network) -> Result<Self> {
        Self::open_with_cache(path, network, 64)
    }

    /// Open (or create) the database at `path` with a `cache_mb` MiB sled page cache.
    pub fn open_with_cache<P: AsRef<Path>>(path: P, network: Network, cache_mb: u32) -> Result<Self> {
        // zstd page compression: block headers / wallet JSON compress ~2-3x.
        let db = sled::Config::new()
            .path(path)
            .use_compression(true)
            .compression_factor(3)
            .cache_capacity(u64::from(cache_mb.max(8)) * 1024 * 1024)
            .open()?;
        let tree = db.open_tree("chain")?;
        Ok(Self { db, tree, network, undo_enabled: Default::default() })
    }

    /// In-memory database (tests / ephemeral nodes).
    pub fn temporary(network: Network) -> Result<Self> {
        let db = sled::Config::new().temporary(true).open()?;
        let tree = db.open_tree("chain")?;
        Ok(Self { db, tree, network, undo_enabled: Default::default() })
    }

    pub fn set_undo_enabled(&self, on: bool) {
        self.undo_enabled.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn undo_enabled(&self) -> bool {
        self.undo_enabled.load(std::sync::atomic::Ordering::Relaxed)
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
        Self::write_wallets_undo(t, deltas, None)
    }

    fn write_wallets_undo(
        t: &TransactionalTree,
        deltas: &BTreeMap<String, WalletDelta>,
        mut undo: Option<&mut UndoRecord>,
    ) -> std::result::Result<Vec<WalletState>, ConflictableTransactionError<Error>> {
        let mut out = Vec::with_capacity(deltas.len());
        for (address, delta) in deltas {
            let key = prefixed(PREFIX_WALLET, address);
            let previous: Option<WalletState> = match t.get(&key)? {
                Some(v) => Some(serde_json::from_slice(&v).map_err(Error::Json).or_else(abort)?),
                None => None,
            };
            if let Some(u) = undo.as_deref_mut() {
                u.push((address.clone(), previous.clone()));
            }
            let previous_ref = previous.clone();
            let mut state = previous.unwrap_or_else(|| WalletState::new(address));
            let before_vote = vote_weight(previous_ref.as_ref());
            state.apply(delta).or_else(abort)?;
            Self::adjust_votes(t, before_vote, vote_weight(Some(&state)))?;
            let json = serde_json::to_vec(&state).map_err(Error::Json).or_else(abort)?;
            t.insert(key, json)?;
            if let Some(pk) = &state.public_key {
                t.insert(prefixed(PREFIX_WALLET_PK, pk), address.as_bytes())?;
            }
            if let Some(u) = &state.username {
                t.insert(prefixed(PREFIX_WALLET_USERNAME, u), address.as_bytes())?;
            }
            if let Some(c) = &state.pq_commitment {
                t.insert(prefixed(PREFIX_PQ_COMMIT, address), &[c.algorithm][..])?;
            }
            if let Some(k) = &state.pq_key {
                t.insert(prefixed(PREFIX_PQ_KEY, address), &[k.algorithm][..])?;
            }
            for id in &delta.locks_removed {
                t.remove(prefixed(PREFIX_LOCK, id))?;
            }
            for l in &delta.locks_added {
                let json = serde_json::to_vec(l).map_err(Error::Json).or_else(abort)?;
                t.insert(prefixed(PREFIX_LOCK, &l.lock_id), json)?;
            }
            for (id, rec) in &delta.sobjects {
                if let Some(name) = &rec.data.name {
                    t.insert(prefixed(PREFIX_SOBJ_NAME, &sobj_name_key(name, rec.type_)), id.as_bytes())?;
                }
                // market index follows the record: open order → owner, otherwise gone
                if rec.price.is_some() && !rec.resigned {
                    t.insert(prefixed(PREFIX_MARKET, id), address.as_bytes())?;
                } else {
                    t.remove(prefixed(PREFIX_MARKET, id))?;
                }
            }
            for (id, st) in &delta.tokens_issued {
                t.insert(prefixed(PREFIX_TOKEN_OWNER, id), address.as_bytes())?;
                t.insert(prefixed(PREFIX_TOKEN_SYMBOL, &st.symbol), id.as_bytes())?;
            }
            for (id, owner) in &delta.sobj_owner {
                t.insert(prefixed(PREFIX_SOBJ_OWNER, id), owner.as_bytes())?;
            }
            out.push(state);
        }
        Ok(out)
    }

    // ------------------------------------------------------- native tokens
    pub fn token_owner(&self, id: &str) -> Result<Option<String>> {
        Ok(self.tree.get(prefixed(PREFIX_TOKEN_OWNER, id))?.map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    pub fn token_id_by_symbol(&self, symbol: &str) -> Result<Option<String>> {
        Ok(self.tree.get(prefixed(PREFIX_TOKEN_SYMBOL, symbol))?.map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    pub fn token_state(&self, id: &str) -> Result<Option<TokenState>> {
        let Some(owner) = self.token_owner(id)? else { return Ok(None) };
        Ok(self.get_wallet(&owner)?.and_then(|w| w.tokens_issued.get(id).cloned()))
    }

    /// All initialized tokens (scans the `tk:` index; one wallet read per token).
    pub fn all_tokens(&self) -> Result<Vec<(String, TokenState)>> {
        let mut out = Vec::new();
        for item in self.tree.scan_prefix(PREFIX_TOKEN_OWNER) {
            let (k, _) = item?;
            let id = String::from_utf8_lossy(&k[PREFIX_TOKEN_OWNER.len()..]).into_owned();
            if let Some(st) = self.token_state(&id)? {
                out.push((id, st));
            }
        }
        Ok(out)
    }

    /// Holders of a token (full wallet scan — fine for the API, not for consensus).
    pub fn token_holders(&self, id: &str) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        for item in self.tree.scan_prefix(PREFIX_WALLET) {
            let (_, v) = item?;
            let w: WalletState = serde_json::from_slice(&v)?;
            if let Some(b) = w.tokens.get(id) {
                out.push((w.address.clone(), *b));
            }
        }
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(out)
    }

    // ------------------------------------------------------- smart objects (sObjects)

    /// Registration id holding `name` for sObject `type_` (network-wide unique, case-insensitive).
    pub fn sobj_by_name(&self, name: &str, type_: u8) -> Result<Option<String>> {
        Ok(self
            .tree
            .get(prefixed(PREFIX_SOBJ_NAME, &sobj_name_key(name, type_)))?
            .map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    /// sObject record + owner address by registration id.
    pub fn get_sobject(&self, registration_id: &str) -> Result<Option<(String, SmartObject)>> {
        let owner = match self.tree.get(prefixed(PREFIX_SOBJ_OWNER, registration_id))? {
            Some(v) => String::from_utf8_lossy(&v).into_owned(),
            None => {
                let Some(tx) = self.get_transaction(registration_id)? else { return Ok(None) };
                address_from_public_key(&tx.sender_public_key, self.network.pubkey_hash)?
            }
        };
        Ok(self.get_wallet(&owner)?.and_then(|w| w.sobjects.get(registration_id).map(|r| (owner.clone(), r.clone()))))
    }

    /// All sObjects (registration id, owner, record), registration order not guaranteed.
    pub fn all_sobjects(&self) -> Result<Vec<(String, String, SmartObject)>> {
        let mut out = Vec::new();
        for item in self.tree.scan_prefix(PREFIX_SOBJ_NAME) {
            let (_, v) = item?;
            let id = String::from_utf8_lossy(&v).into_owned();
            if let Some((owner, rec)) = self.get_sobject(&id)? {
                out.push((id, owner, rec));
            }
        }
        Ok(out)
    }

    /// Open sale orders: (registrationId, owner, record), oldest registration first — `offset`/`limit` paginate the index scan.
    pub fn market_orders(&self, offset: usize, limit: usize) -> Result<(Vec<(String, String, SmartObject)>, usize)> {
        let mut out = Vec::new();
        let mut total = 0usize;
        for item in self.tree.scan_prefix(PREFIX_MARKET) {
            let (k, v) = item?;
            total += 1;
            if total <= offset || out.len() >= limit {
                continue;
            }
            let id = String::from_utf8_lossy(&k[PREFIX_MARKET.len()..]).into_owned();
            let owner = String::from_utf8_lossy(&v).into_owned();
            if let Some(rec) = self.get_wallet(&owner)?.and_then(|w| w.sobjects.get(&id).cloned()) {
                out.push((id, owner, rec));
            }
        }
        Ok((out, total))
    }

    // ------------------------------------------------------- locks / rounds / ranking

    /// All open HTLC locks.
    pub fn open_locks(&self) -> Result<Vec<LockRecord>> {
        let mut out = Vec::new();
        for item in self.tree.scan_prefix(PREFIX_LOCK) {
            let (_, v) = item?;
            out.push(serde_json::from_slice(&v)?);
        }
        Ok(out)
    }

    pub fn get_lock(&self, id: &str) -> Result<Option<LockRecord>> {
        match self.tree.get(prefixed(PREFIX_LOCK, id))? {
            Some(v) => Ok(Some(serde_json::from_slice(&v)?)),
            None => Ok(None),
        }
    }

    /// Move a voter's weight between delegates inside a transaction (`dv:` index).
    fn adjust_votes(
        t: &TransactionalTree,
        before: Option<(String, u64)>,
        after: Option<(String, u64)>,
    ) -> std::result::Result<(), ConflictableTransactionError<Error>> {
        if before == after {
            return Ok(());
        }
        let apply = |pk: &str, delta: i128| -> std::result::Result<(), ConflictableTransactionError<Error>> {
            let key = prefixed(PREFIX_VOTES, pk);
            let current = t.get(&key)?.map(|v| be_u64(&v)).unwrap_or(0) as i128;
            let next = (current + delta).max(0) as u64;
            t.insert(key, next.to_be_bytes().as_slice())?;
            Ok(())
        };
        match (&before, &after) {
            (Some((a, wa)), Some((b, wb))) if a == b => apply(a, *wb as i128 - *wa as i128)?,
            _ => {
                if let Some((a, wa)) = &before {
                    apply(a, -(*wa as i128))?;
                }
                if let Some((b, wb)) = &after {
                    apply(b, *wb as i128)?;
                }
            }
        }
        Ok(())
    }

    /// Vote weight of a delegate from the incremental index.
    pub fn delegate_votes(&self, public_key: &str) -> Result<u64> {
        Ok(self.tree.get(prefixed(PREFIX_VOTES, public_key))?.map(|v| be_u64(&v)).unwrap_or(0))
    }

    /// Rebuild the `dv:` index from all wallets (one-off migration for databases created before it existed).
    pub fn rebuild_vote_index(&self) -> Result<usize> {
        let wallets = self.all_wallets()?;
        let mut votes: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for w in &wallets {
            if let Some((pk, weight)) = vote_weight(Some(w)) {
                *votes.entry(pk).or_default() += weight;
            }
        }
        let mut batch = sled::Batch::default();
        for item in self.tree.scan_prefix(PREFIX_VOTES) {
            batch.remove(item?.0);
        }
        for (pk, weight) in &votes {
            batch.insert(prefixed(PREFIX_VOTES, pk), weight.to_be_bytes().as_slice());
        }
        batch.insert(KEY_VOTE_INDEX, &[1u8][..]);
        self.tree.apply_batch(batch)?;
        Ok(votes.len())
    }

    /// Build the vote index once for databases that predate it.
    pub fn ensure_vote_index(&self) -> Result<()> {
        if self.tree.get(KEY_VOTE_INDEX)?.is_none() && self.get_last_height()? > 0 {
            let n = self.rebuild_vote_index()?;
            tracing::info!(delegates = n, "vote index built");
        } else if self.tree.get(KEY_VOTE_INDEX)?.is_none() {
            self.tree.insert(KEY_VOTE_INDEX, &[1u8][..])?;
        }
        Ok(())
    }

    /// Delegates ranked by vote weight (voters' balance + locked balance), resigned ones last.
    /// Ties are broken by public key, exactly like the legacy `buildDelegateRanking`. O(delegates), not O(wallets).
    pub fn delegate_ranking(&self) -> Result<Vec<(WalletState, u64)>> {
        let mut delegates: Vec<(WalletState, u64)> = Vec::new();
        for item in self.tree.scan_prefix(PREFIX_WALLET_USERNAME) {
            let (_, addr) = item?;
            let address = String::from_utf8_lossy(&addr).to_string();
            let Some(w) = self.get_wallet(&address)? else { continue };
            let votes = match &w.public_key {
                Some(pk) => self.delegate_votes(pk)?,
                None => 0,
            };
            delegates.push((w, votes));
        }
        delegates.sort_by(|(a, va), (b, vb)| a.resigned.cmp(&b.resigned).then(vb.cmp(va)).then_with(|| a.public_key.cmp(&b.public_key)));
        Ok(delegates)
    }

    /// Top `count` active (non-resigned) delegates — the forging set of the next round.
    /// Wallets that published a Quantum Shield commitment (stage A).
    pub fn pq_commitment_count(&self) -> usize {
        self.tree.scan_prefix(PREFIX_PQ_COMMIT).count()
    }

    /// Wallets protected by a registered PQ key (stage B).
    pub fn pq_key_count(&self) -> usize {
        self.tree.scan_prefix(PREFIX_PQ_KEY).count()
    }

    pub fn put_finality_cert(&self, cert: &FinalityCert) -> Result<()> {
        let mut k = PREFIX_FINALITY.to_vec();
        k.extend_from_slice(&cert.height.to_be_bytes());
        self.tree.insert(k, serde_json::to_vec(cert)?)?;
        Ok(())
    }

    pub fn finality_cert(&self, height: u64) -> Result<Option<FinalityCert>> {
        let mut k = PREFIX_FINALITY.to_vec();
        k.extend_from_slice(&height.to_be_bytes());
        Ok(self.tree.get(k)?.map(|v| serde_json::from_slice(&v)).transpose()?)
    }

    fn equivocation_prefix(public_key: &str) -> Vec<u8> {
        let mut k = PREFIX_EQUIVOCATION.to_vec();
        k.extend_from_slice(public_key.as_bytes());
        k.push(b':');
        k
    }

    pub fn put_equivocation(&self, proof: &EquivocationProof) -> Result<()> {
        let mut k = Self::equivocation_prefix(&proof.public_key);
        k.extend_from_slice(&proof.height.to_be_bytes());
        self.tree.insert(k, serde_json::to_vec(proof)?)?;
        Ok(())
    }

    /// Latest (highest height) proof against `public_key`.
    pub fn equivocation(&self, public_key: &str) -> Result<Option<EquivocationProof>> {
        match self.tree.scan_prefix(Self::equivocation_prefix(public_key)).next_back() {
            Some(r) => Ok(Some(serde_json::from_slice(&r?.1)?)),
            None => Ok(None),
        }
    }

    /// Every proof against `public_key`, oldest first.
    pub fn equivocation_history(&self, public_key: &str) -> Result<Vec<EquivocationProof>> {
        self.tree.scan_prefix(Self::equivocation_prefix(public_key)).map(|r| Ok(serde_json::from_slice(&r?.1)?)).collect()
    }

    /// All proofs of all delegates (history), oldest first per delegate.
    pub fn equivocations(&self) -> Result<Vec<EquivocationProof>> {
        self.tree.scan_prefix(PREFIX_EQUIVOCATION).map(|r| Ok(serde_json::from_slice(&r?.1)?)).collect()
    }

    /// Delegates excluded from `round` by a proven equivocation (empty unless milestone `finality.slashing`).
    pub fn banned_delegates(&self, round: u64, height: u64) -> Result<Vec<String>> {
        if !self.network.milestone(height.max(1)).finality.slashing {
            return Ok(Vec::new());
        }
        let mut out: Vec<String> = self.equivocations()?.into_iter().filter(|p| p.banned_until_round > round).map(|p| p.public_key).collect();
        out.dedup();
        Ok(out)
    }

    /// Highest certificate held by this node (`fc:` keys are big-endian heights).
    pub fn latest_finality_cert(&self) -> Result<Option<FinalityCert>> {
        match self.tree.scan_prefix(PREFIX_FINALITY).next_back() {
            Some(r) => Ok(Some(serde_json::from_slice(&r?.1)?)),
            None => Ok(None),
        }
    }

    /// Top `count` delegates by vote for the round that starts after the current tip (slashed ones skipped, SHIP-35).
    pub fn active_delegates(&self, count: usize) -> Result<Vec<DelegateRank>> {
        let next = self.get_last_height()? + 1;
        let round = crate::delegate::round::round_info(next, self.network.milestone(next).active_delegates as u64).round;
        self.active_delegates_excluding(count, &self.banned_delegates(round, next)?)
    }

    /// Top `count` by vote ignoring slashing (who *would* be active — used to judge equivocation proofs about banned keys).
    pub fn active_delegates_ranked(&self, count: usize) -> Result<Vec<DelegateRank>> {
        self.active_delegates_excluding(count, &[])
    }

    fn active_delegates_excluding(&self, count: usize, banned: &[String]) -> Result<Vec<DelegateRank>> {
        Ok(self
            .delegate_ranking()?
            .into_iter()
            .filter(|(w, _)| !w.resigned && !w.public_key.as_deref().is_some_and(|pk| banned.iter().any(|b| b == pk)))
            .take(count)
            .map(|(w, votes)| DelegateRank { public_key: w.public_key.unwrap_or_default(), votes })
            .collect())
    }

    pub fn save_round(&self, round: u64, delegates: &[DelegateRank]) -> Result<()> {
        let mut k = PREFIX_ROUND.to_vec();
        k.extend_from_slice(&round.to_be_bytes());
        self.tree.insert(k, serde_json::to_vec(delegates)?)?;
        Ok(())
    }

    pub fn get_round(&self, round: u64) -> Result<Option<Vec<DelegateRank>>> {
        let mut k = PREFIX_ROUND.to_vec();
        k.extend_from_slice(&round.to_be_bytes());
        match self.tree.get(k)? {
            Some(v) => Ok(Some(serde_json::from_slice(&v)?)),
            None => Ok(None),
        }
    }

    /// Lowest height whose block timestamp is `>= ts` (binary search — timestamps grow with height).
    pub fn height_at_or_after(&self, ts: u32) -> Result<u64> {
        let (mut lo, mut hi) = (1u64, self.get_last_height()? + 1);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.get_block_by_height(mid)? {
                Some(b) if b.timestamp < ts => lo = mid + 1,
                _ => hi = mid,
            }
        }
        Ok(lo)
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
        if self.undo_enabled() && blocks.len() > 1 {
            for b in blocks {
                self.apply_blocks(std::slice::from_ref(b))?;
            }
            return Ok(());
        }
        let undo_on = self.undo_enabled();
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
                let mut undo: UndoRecord = Vec::new();
                Self::write_wallets_undo(t, &deltas, undo_on.then_some(&mut undo))?;
                for (block, encoded) in &prepared {
                    Self::write_block(t, block, encoded, self.network.pubkey_hash)?;
                }
                if undo_on {
                    let height = prepared[0].0.height;
                    let json = serde_json::to_vec(&undo).map_err(Error::Json).or_else(abort)?;
                    t.insert(undo_key(height), json)?;
                    if height > UNDO_DEPTH {
                        t.remove(undo_key(height - UNDO_DEPTH))?;
                    }
                }
                Ok(())
            })
            .map_err(map_tx_err)
    }

    /// Undo the last block: restore wallet states, drop the block and its indexes.
    /// Requires an undo record (undo logging must have been on when the block was applied).
    pub fn rollback_last_block(&self) -> Result<u64> {
        let height = self.get_last_height()?;
        if height <= 1 {
            return Err(Error::Sync("cannot roll back the genesis block".into()));
        }
        // SHIP-35: a certified block is final — hard mode refuses, soft mode only warns (legacy majority may still fork)
        if let Some(cert) = self.latest_finality_cert()?.filter(|c| c.height >= height) {
            if self.network.milestone(height).finality.active {
                return Err(Error::Sync(format!("FinalityViolation: block {height} is final (certificate at {} with {} votes), refusing to roll back", cert.height, cert.votes.len())));
            }
            tracing::warn!(height, certified = cert.height, votes = cert.votes.len(), "rolling back a FINAL block (finality soft mode) — the network forked past a certificate");
            let mut k = PREFIX_FINALITY.to_vec();
            k.extend_from_slice(&cert.height.to_be_bytes());
            self.tree.remove(k)?;
        }
        let block = self.get_block_by_height(height)?.ok_or_else(|| Error::NotFound(format!("block {height}")))?;
        let undo_raw = self.tree.get(undo_key(height))?.ok_or_else(|| Error::Sync(format!("no undo record for block {height}")))?;
        let undo: UndoRecord = serde_json::from_slice(&undo_raw)?;
        let net = self.network.pubkey_hash;
        // `tks:<SYMBOL>` → tokenId entries, to drop the symbol index of a rolled-back TokenInit
        let token_symbol_keys: Vec<(Vec<u8>, String)> = if block.transactions.iter().any(|t| t.type_group == crate::models::TYPE_GROUP_TOKEN) {
            self.tree.scan_prefix(PREFIX_TOKEN_SYMBOL).filter_map(|r| r.ok()).map(|(k, v)| (k.to_vec(), String::from_utf8_lossy(&v).into_owned())).collect()
        } else {
            Vec::new()
        };
        self.tree
            .transaction(|t| {
                for (address, _) in &undo {
                    if let Some(v) = t.get(prefixed(PREFIX_WALLET, address))? {
                        let cur: WalletState = serde_json::from_slice(&v).map_err(Error::Json).or_else(abort)?;
                        for id in cur.sobjects.keys() {
                            t.remove(prefixed(PREFIX_MARKET, id))?;
                        }
                    }
                }
                for (address, previous) in &undo {
                    let key = prefixed(PREFIX_WALLET, address);
                    let current: Option<WalletState> = match t.get(&key)? {
                        Some(v) => Some(serde_json::from_slice(&v).map_err(Error::Json).or_else(abort)?),
                        None => None,
                    };
                    Self::adjust_votes(t, vote_weight(current.as_ref()), vote_weight(previous.as_ref()))?;
                    // mk: index follows the wallets' sObject records (removals for every touched wallet ran first)
                    for (id, rec) in previous.iter().flat_map(|w| w.sobjects.iter()) {
                        if rec.price.is_some() && !rec.resigned {
                            t.insert(prefixed(PREFIX_MARKET, id), address.as_bytes())?;
                        }
                    }
                    // lk: index follows the wallets' lock maps
                    for id in current.iter().flat_map(|w| w.locks.keys()) {
                        t.remove(prefixed(PREFIX_LOCK, id))?;
                    }
                    if current.as_ref().is_some_and(|c| c.pq_commitment.is_some()) && !previous.as_ref().is_some_and(|p| p.pq_commitment.is_some()) {
                        t.remove(prefixed(PREFIX_PQ_COMMIT, address))?;
                    }
                    if current.as_ref().is_some_and(|c| c.pq_key.is_some()) && !previous.as_ref().is_some_and(|p| p.pq_key.is_some()) {
                        t.remove(prefixed(PREFIX_PQ_KEY, address))?;
                    }
                    match previous {
                        Some(w) => {
                            t.insert(key, serde_json::to_vec(w).map_err(Error::Json).or_else(abort)?)?;
                            for l in w.locks.values() {
                                t.insert(prefixed(PREFIX_LOCK, &l.lock_id), serde_json::to_vec(l).map_err(Error::Json).or_else(abort)?)?;
                            }
                        }
                        None => {
                            t.remove(key)?;
                            if let Some(c) = &current {
                                if let Some(pk) = &c.public_key {
                                    t.remove(prefixed(PREFIX_WALLET_PK, pk))?;
                                }
                                if let Some(u) = &c.username {
                                    t.remove(prefixed(PREFIX_WALLET_USERNAME, u))?;
                                }
                            }
                        }
                    }
                }
                // sObject → owner before this block (from the undo snapshot); covers transfer, buy and chains within the block
                let mut prev_owner: std::collections::HashMap<String, String> = std::collections::HashMap::new();
                for (address, previous) in &undo {
                    for id in previous.iter().flat_map(|w| w.sobjects.keys()) {
                        prev_owner.insert(id.clone(), address.clone());
                    }
                }
                for (seq, tx) in block.transactions.iter().enumerate() {
                    let Some(id) = &tx.id else { continue };
                    if let Some(e) = tx.sobj_asset().filter(|e| e.action == crate::models::sobj::ACTION_REGISTER) {
                        if let Some(name) = &e.data.name {
                            t.remove(prefixed(PREFIX_SOBJ_NAME, &sobj_name_key(name, e.type_)))?;
                        }
                    }
                    if let Some(e) = tx.sobj_asset().filter(|e| matches!(e.action, crate::models::sobj::ACTION_TRANSFER | crate::models::sobj::ACTION_BUY)) {
                        if let Some(reg) = e.registration_id.as_ref() {
                            let owner = match prev_owner.get(reg) {
                                Some(o) => o.clone(),
                                None => address_from_public_key(&tx.sender_public_key, net).or_else(abort)?,
                            };
                            t.insert(prefixed(PREFIX_SOBJ_OWNER, reg), owner.as_bytes())?;
                            if t.get(prefixed(PREFIX_TOKEN_OWNER, reg))?.is_some() {
                                t.insert(prefixed(PREFIX_TOKEN_OWNER, reg), owner.as_bytes())?;
                            }
                        }
                    }
                    if tx.type_group == crate::models::TYPE_GROUP_TOKEN && tx.type_ == crate::models::token::INIT {
                        if let Some(a) = tx.token_asset() {
                            t.remove(prefixed(PREFIX_TOKEN_OWNER, &a.id))?;
                            for (k, _) in token_symbol_keys.iter().filter(|(_, id)| *id == a.id) {
                                t.remove(k.as_slice())?;
                            }
                        }
                    }
                    t.remove(prefixed(PREFIX_TX, id))?;
                    let seq = tx.sequence.unwrap_or(seq as u32);
                    t.remove(tx_order_key(PREFIX_TX_LIST, "", height, seq))?;
                    for addr in tx_addresses(tx, net).or_else(abort)? {
                        t.remove(tx_order_key(PREFIX_WALLET_TX, &addr, height, seq))?;
                    }
                }
                if let Some(id) = &block.id {
                    t.remove(prefixed(PREFIX_BLOCK_ID, id))?;
                }
                t.remove(height_key(height))?;
                t.remove(undo_key(height))?;
                t.insert(KEY_LAST_HEIGHT, (height - 1).to_be_bytes().as_slice())?;
                Ok(())
            })
            .map_err(map_tx_err)?;
        Ok(height - 1)
    }

    /// Roll back to `height` (inclusive tip after the call).
    pub fn rollback_to(&self, height: u64) -> Result<u64> {
        let mut tip = self.get_last_height()?;
        while tip > height {
            tip = self.rollback_last_block()?;
        }
        Ok(tip)
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
        let ms = self.network.milestone(block.height);
        let init_count = block.transactions.iter().filter(|t| t.type_group == crate::models::TYPE_GROUP_TOKEN && t.type_ == crate::models::token::INIT).count() as u64;
        let burned = if init_count > 0 && !self.network.burn_address.is_empty() {
            init_count * ms.token_fees.init_burn()
        } else {
            0
        };
        if burned > 0 {
            deltas.entry(self.network.burn_address.clone()).or_default().balance += burned as i128;
        }
        {
            let d = deltas.entry(generator).or_default();
            d.balance += block.reward as i128 + block.total_fee as i128 - burned as i128;
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
            if let Some(e) = tx.sobj_asset() {
                let (Some(id), Some(existing)) = (tx.id.as_ref(), Some(deltas.get(&sender).map(|d| d.sobjects.clone()).unwrap_or_default())) else { continue };
                if matches!(e.action, crate::models::sobj::ACTION_TRANSFER | crate::models::sobj::ACTION_BUY) {
                    let Some(reg_id) = e.registration_id.clone() else { continue };
                    let buy = e.action == crate::models::sobj::ACTION_BUY;
                    // current owner: pending insert in this block wins over storage
                    let pending_owner = deltas.iter().find(|(_, d)| d.sobjects.iter().any(|(k, _)| *k == reg_id)).map(|(o, _)| o.clone());
                    let (from, to) = if buy {
                        let Some(owner) = pending_owner.clone().or_else(|| self.get_sobject(&reg_id).ok().flatten().map(|(o, _)| o)) else { continue };
                        (owner, sender.clone())
                    } else {
                        let Some(to) = e.recipient_id.clone() else { continue };
                        (sender.clone(), to)
                    };
                    let rec = deltas
                        .get(&from)
                        .and_then(|d| d.sobjects.iter().rev().find(|(k, _)| *k == reg_id).map(|(_, r)| r.clone()))
                        .or_else(|| self.get_wallet(&from).ok().flatten().and_then(|w| w.sobjects.get(&reg_id).cloned()));
                    let Some(mut rec) = rec else { continue };
                    if buy {
                        let price = rec.price.unwrap_or(0) as i128;
                        deltas.entry(sender.clone()).or_default().balance -= price;
                        deltas.entry(from.clone()).or_default().balance += price;
                    }
                    rec.price = None;
                    let token = deltas
                        .get(&from)
                        .and_then(|d| d.tokens_issued.iter().rev().find(|(k, _)| *k == reg_id).map(|(_, s)| s.clone()))
                        .or_else(|| self.token_state(&reg_id).ok().flatten());
                    {
                        // received earlier in this block and handed over again: drop the pending insert so removal wins
                        let d = deltas.entry(from.clone()).or_default();
                        d.sobjects.retain(|(k, _)| *k != reg_id);
                        d.tokens_issued.retain(|(k, _)| *k != reg_id);
                        d.sobj_owner.retain(|(k, _)| *k != reg_id);
                        d.sobjects_removed.push(reg_id.clone());
                        if token.is_some() {
                            d.tokens_removed.push(reg_id.clone());
                        }
                    }
                    let d = deltas.entry(to.clone()).or_default();
                    d.sobjects.push((reg_id.clone(), rec));
                    d.sobj_owner.push((reg_id.clone(), to.clone()));
                    if let Some(mut st) = token {
                        st.owner = to.clone();
                        d.tokens_issued.push((reg_id, st));
                    }
                    continue;
                }
                let record = match e.action {
                    crate::models::sobj::ACTION_REGISTER => {
                        (id.clone(), SmartObject { type_: e.type_, sub_type: e.sub_type, data: e.data.clone(), resigned: false, price: None })
                    }
                    _ => {
                        let Some(reg_id) = e.registration_id.clone() else { continue };
                        let current = existing
                            .iter()
                            .rev()
                            .find(|(k, _)| *k == reg_id)
                            .map(|(_, r)| r.clone())
                            .or_else(|| self.get_wallet(&sender).ok().flatten().and_then(|w| w.sobjects.get(&reg_id).cloned()));
                        let Some(mut rec) = current else { continue };
                        match e.action {
                            crate::models::sobj::ACTION_UPDATE => rec.data.ntfry_data = e.data.ntfry_data.clone(),
                            crate::models::sobj::ACTION_SELL => rec.price = e.price.filter(|p| *p > 0),
                            _ => rec.resigned = true,
                        }
                        (reg_id, rec)
                    }
                };
                deltas.entry(sender.clone()).or_default().sobjects.push(record);
                continue;
            }
            if let Some(a) = tx.token_asset() {
                use crate::models::token;
                match tx.type_ {
                    token::INIT => {
                        let symbol = self.get_sobject(&a.id)?.and_then(|(_, rec)| rec.data.name).unwrap_or_default();
                        let init = a.initial_supply.unwrap_or(0);
                        let st = TokenState { symbol, decimals: a.decimals.unwrap_or(0), flags: a.flags.unwrap_or(0), supply: init, supply_cap: a.supply_cap.unwrap_or(0), owner: sender.clone(), init_height: block.height, meta: None };
                        let d = deltas.entry(sender.clone()).or_default();
                        d.tokens_issued.push((a.id.clone(), st));
                        *d.tokens.entry(a.id.clone()).or_default() += init as i128;
                    }
                    token::TRANSFER => {
                        for it in a.transfers.unwrap_or_default() {
                            *deltas.entry(sender.clone()).or_default().tokens.entry(a.id.clone()).or_default() -= it.amount as i128;
                            *deltas.entry(it.recipient_id).or_default().tokens.entry(a.id.clone()).or_default() += it.amount as i128;
                        }
                    }
                    token::MINT => {
                        let amount = a.amount.unwrap_or(0) as i128;
                        *deltas.entry(a.recipient_id.clone().unwrap_or_default()).or_default().tokens.entry(a.id.clone()).or_default() += amount;
                        *deltas.entry(sender.clone()).or_default().token_supply.entry(a.id.clone()).or_default() += amount;
                    }
                    token::META => {
                        if let Some(m) = a.meta {
                            deltas.entry(sender.clone()).or_default().token_meta.push((a.id.clone(), m));
                        }
                    }
                    _ => {
                        let amount = a.amount.unwrap_or(0) as i128;
                        *deltas.entry(sender.clone()).or_default().tokens.entry(a.id.clone()).or_default() -= amount;
                        let owner = self.token_owner(&a.id)?.or_else(|| deltas.iter().find(|(_, d)| d.tokens_issued.iter().any(|(id, _)| *id == a.id)).map(|(o, _)| o.clone())).unwrap_or_else(|| sender.clone());
                        *deltas.entry(owner).or_default().token_supply.entry(a.id.clone()).or_default() -= amount;
                    }
                }
                continue;
            }
            if tx.type_group != crate::models::TYPE_GROUP_CORE {
                continue;
            }
            match tx.type_ {
                tx_type::TRANSFER => {
                    if let Some(r) = &tx.recipient_id {
                        deltas.entry(r.clone()).or_default().balance += tx.amount as i128;
                        // Quantum Shield stage A: self-transfer carrying `sthpq1:<alg>:<sha256(pk)>`
                        if *r == sender && tx.amount >= 1 {
                            if let Some((algorithm, commitment)) = tx.vendor_field.as_deref().and_then(crate::crypto::pq::parse_commitment) {
                                deltas.entry(sender.clone()).or_default().pq_commitment =
                                    Some(crate::crypto::pq::PqCommitment { algorithm, commitment, height: block.height });
                            }
                        }
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
                        if tx.is_pq() {
                            deltas.entry(sender.clone()).or_default().pq_key =
                                Some(crate::crypto::pq::PqKey { algorithm: sig.algorithm.unwrap_or(crate::crypto::pq::ALG_ML_DSA_44), public_key: sig.public_key.clone(), since: block.height });
                        } else {
                            deltas.entry(sender.clone()).or_default().second_public_key = Some(sig.public_key.clone());
                        }
                    }
                }
                tx_type::DELEGATE_RESIGNATION => {
                    deltas.entry(sender.clone()).or_default().resigned = true;
                }
                tx_type::MULTI_SIGNATURE => {
                    if let Some(ms) = tx.asset.as_ref().and_then(|a| a.multi_signature.as_ref()) {
                        let addr = crate::crypto::address_from_multi_signature(ms, net)?;
                        deltas.entry(addr).or_default().multi_signature = Some(ms.clone());
                    }
                }
                tx_type::HTLC_LOCK => {
                    if let (Some(lock), Some(id), Some(recipient)) =
                        (tx.asset.as_ref().and_then(|a| a.lock.as_ref()), &tx.id, &tx.recipient_id)
                    {
                        deltas.entry(sender.clone()).or_default().locks_added.push(LockRecord {
                            lock_id: id.clone(),
                            sender_public_key: tx.sender_public_key.clone(),
                            recipient_id: recipient.clone(),
                            amount: tx.amount,
                            secret_hash: lock.secret_hash.clone(),
                            expiration: lock.expiration.clone(),
                            timestamp: block.timestamp,
                            vendor_field: tx.vendor_field.clone(),
                        });
                    }
                }
                tx_type::HTLC_CLAIM => {
                    if let Some(c) = tx.asset.as_ref().and_then(|a| a.claim.as_ref()) {
                        if let Some(lock) = self.find_lock(&c.lock_transaction_id, batch)? {
                            if let Some(r) = &lock.recipient_id {
                                deltas.entry(r.clone()).or_default().balance += lock.amount as i128;
                            }
                            let lock_sender = address_from_public_key(&lock.sender_public_key, net)?;
                            deltas.entry(lock_sender).or_default().locks_removed.push(c.lock_transaction_id.clone());
                        }
                    }
                }
                tx_type::HTLC_REFUND => {
                    if let Some(r) = tx.asset.as_ref().and_then(|a| a.refund.as_ref()) {
                        if let Some(lock) = self.find_lock(&r.lock_transaction_id, batch)? {
                            let lock_sender = address_from_public_key(&lock.sender_public_key, net)?;
                            let d = deltas.entry(lock_sender).or_default();
                            d.balance += lock.amount as i128;
                            d.locks_removed.push(r.lock_transaction_id.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}
