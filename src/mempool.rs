//! Author: TechnoL0g
//!
//! In-memory transaction pool with legacy-compatible admission checks and best-effort
//! relaying to the legacy node pool (until iroh gossip takes over in Phase 5).

use crate::config::Network;
use crate::crypto::{address_from_public_key, transaction_id, validate_address, verify_transaction_signature};
use crate::models::{tx_type, Transaction, TYPE_GROUP_CORE};
use crate::p2p_legacy::PeerTable;
use crate::storage::Storage;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Result of `POST /api/transactions` in the legacy `{ accept, broadcast, excess, invalid }` shape.
#[derive(Debug, Default, Serialize)]
pub struct PoolResponse {
    pub accept: Vec<String>,
    pub broadcast: Vec<String>,
    pub excess: Vec<String>,
    pub invalid: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolError {
    #[serde(rename = "type")]
    pub type_: String,
    pub message: String,
}

pub struct Mempool {
    txs: RwLock<HashMap<String, Transaction>>,
    storage: Arc<Storage>,
    network: Network,
    relay_nodes: Vec<String>,
    peers: Option<Arc<PeerTable>>,
    relay_fanout: usize,
    client: reqwest::Client,
    max_size: usize,
    max_bytes: usize,
    bytes: std::sync::atomic::AtomicUsize,
    dynamic_fees: crate::node_config::DynamicFeesConfig,
    /// Core transactions refused with `ERR_LOW_FEE` since start (metrics page).
    low_fee_rejected: std::sync::atomic::AtomicU64,
    /// Accepted transactions, for other transports (iroh gossip) to re-publish.
    events: tokio::sync::broadcast::Sender<Vec<Transaction>>,
}

impl Mempool {
    pub fn new(storage: Arc<Storage>, relay_nodes: Vec<String>, max_size: usize) -> Self {
        Self::build(storage, relay_nodes, None, 0, max_size)
    }

    /// Mempool that relays over the legacy P2P peer table before falling back to REST nodes.
    pub fn with_p2p(storage: Arc<Storage>, relay_nodes: Vec<String>, peers: Arc<PeerTable>, relay_fanout: usize, max_size: usize) -> Self {
        Self::build(storage, relay_nodes, Some(peers), relay_fanout, max_size)
    }

    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Byte budget of the pool (wire size of all pending transactions); v3 transactions weigh ~2.5 KB each.
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes.max(1);
        self
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    pub fn bytes(&self) -> usize {
        self.bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Per-node fee policy for core transactions (legacy `dynamicFees`); blocks are never judged by it.
    pub fn with_dynamic_fees(mut self, cfg: crate::node_config::DynamicFeesConfig) -> Self {
        self.dynamic_fees = cfg;
        self
    }

    pub fn dynamic_fees(&self) -> &crate::node_config::DynamicFeesConfig {
        &self.dynamic_fees
    }

    pub fn low_fee_rejected(&self) -> u64 {
        self.low_fee_rejected.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Wire size of a plain v2 transfer without vendorField (59-byte common part + 33 payload + 64 signature).
    pub const TRANSFER_WIRE_BYTES: usize = 156;

    /// Fee a plain transfer needs to enter this node's pool right now (dynamic minimum or the static fee).
    pub fn min_transfer_fee(&self) -> u64 {
        let tip = self.storage.get_last_height().unwrap_or(0);
        let ms = self.storage.network().milestone(tip + 1);
        if self.dynamic_fees.enabled {
            self.dynamic_fees.min_fee("transfer", Self::TRANSFER_WIRE_BYTES, self.dynamic_fees.min_fee_pool)
        } else {
            ms.static_fee("transfer")
        }
    }

    /// Core v2 fee gate: dynamic `(addon + bytes) × rate` or exact static fee. Returns whether the transaction may be relayed.
    fn check_core_fee(&self, tx: &Transaction, size: usize, ms: &crate::config::Milestone) -> std::result::Result<bool, (String, String)> {
        if tx.type_group != TYPE_GROUP_CORE || tx.is_pq() {
            return Ok(true);
        }
        let name = crate::config::FEE_NAMES.iter().find(|(_, t)| *t == tx.type_).map(|(n, _)| *n).unwrap_or("");
        let df = &self.dynamic_fees;
        if !df.enabled {
            let static_fee = ms.static_fee_for_type(tx.type_);
            if tx.fee != static_fee {
                return Err(("ERR_LOW_FEE".into(), format!("Fee {} does not match the static fee {static_fee} of {name} (dynamic fees are disabled on this node)", tx.fee)));
            }
            return Ok(true);
        }
        let min_pool = df.min_fee(name, size, df.min_fee_pool);
        if tx.fee < min_pool {
            let addon = df.addon_bytes.get(name).copied().unwrap_or(0);
            return Err(("ERR_LOW_FEE".into(), format!("The fee {} is too low to enter the pool: minimum ({addon} + {size} bytes) × {} = {min_pool}", tx.fee, df.min_fee_pool)));
        }
        Ok(tx.fee >= df.min_fee(name, size, df.min_fee_broadcast))
    }

    /// Wire size of a transaction (falls back to the JSON length if it cannot be serialised).
    fn tx_size(&self, tx: &Transaction) -> usize {
        crate::crypto::serialize_transaction(tx, crate::crypto::SerializeOptions::default(), &self.network)
            .map(|b| b.len())
            .unwrap_or_else(|_| serde_json::to_string(tx).map(|s| s.len()).unwrap_or(0))
    }

    /// Stream of transaction batches accepted into the pool.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Vec<Transaction>> {
        self.events.subscribe()
    }

    fn build(storage: Arc<Storage>, relay_nodes: Vec<String>, peers: Option<Arc<PeerTable>>, relay_fanout: usize, max_size: usize) -> Self {
        let network = storage.network().clone();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent(concat!("sth-core/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        let (events, _) = tokio::sync::broadcast::channel(64);
        Self { txs: RwLock::new(HashMap::new()), storage, network, relay_nodes, peers, relay_fanout, client, max_size, max_bytes: crate::node_config::MempoolConfig::default().max_bytes, bytes: std::sync::atomic::AtomicUsize::new(0), dynamic_fees: Default::default(), low_fee_rejected: Default::default(), events }
    }

    pub async fn len(&self) -> usize {
        self.txs.read().await.len()
    }

    pub async fn get(&self, id: &str) -> Option<Transaction> {
        self.txs.read().await.get(id).cloned()
    }

    /// Pending transactions, newest nonce first is irrelevant here — insertion order is not kept.
    pub async fn all(&self) -> Vec<Transaction> {
        self.txs.read().await.values().cloned().collect()
    }

    /// Validate and admit transactions; returns the legacy response plus per-id errors.
    pub async fn add_many(&self, incoming: Vec<Transaction>) -> (PoolResponse, HashMap<String, PoolError>) {
        let mut resp = PoolResponse::default();
        let mut errors = HashMap::new();
        for mut tx in incoming {
            let id = match transaction_id(&tx) {
                Ok(id) => id,
                Err(e) => {
                    errors.insert("unknown".into(), PoolError { type_: "ERR_BAD_DATA".into(), message: e.to_string() });
                    resp.invalid.push("unknown".into());
                    continue;
                }
            };
            if let Some(given) = &tx.id {
                if given != &id {
                    errors.insert(given.clone(), PoolError { type_: "ERR_BAD_DATA".into(), message: format!("Transaction id {given} does not match its bytes ({id})") });
                    resp.invalid.push(given.clone());
                    continue;
                }
            }
            tx.id = Some(id.clone());
            let summary = describe(&tx, self.storage.network().pubkey_hash);
            match self.validate(&tx).await {
                Ok(broadcast) => {
                    tracing::info!("Received transaction {summary}");
                    tracing::debug!(tx = %serde_json::to_string(&tx).unwrap_or_default(), "transaction json");
                    self.bytes.fetch_add(self.tx_size(&tx), std::sync::atomic::Ordering::Relaxed);
                    self.txs.write().await.insert(id.clone(), tx);
                    resp.accept.push(id.clone());
                    if broadcast {
                        resp.broadcast.push(id);
                    } else {
                        tracing::info!("Transaction {id} is kept local: fee below min_fee_broadcast");
                    }
                }
                Err((type_, message)) => {
                    if type_ == "ERR_LOW_FEE" {
                        self.low_fee_rejected.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    tracing::warn!("Rejected transaction {summary}: {type_} {message}");
                    errors.insert(id.clone(), PoolError { type_, message });
                    resp.invalid.push(id);
                }
            }
        }
        if !resp.broadcast.is_empty() {
            let pool = self.txs.read().await;
            let accepted: Vec<Transaction> = resp.broadcast.iter().filter_map(|id| pool.get(id).cloned()).collect();
            drop(pool);
            let _ = self.events.send(accepted);
            self.relay(&resp.broadcast).await;
        }
        (resp, errors)
    }

    async fn validate(&self, tx: &Transaction) -> std::result::Result<bool, (String, String)> {
        let id = tx.id.clone().unwrap_or_default();
        let pool = self.txs.read().await;
        if pool.contains_key(&id) {
            return Err(("ERR_DUPLICATE".into(), format!("Transaction {id} is already in the pool")));
        }
        if pool.len() >= self.max_size {
            return Err(("ERR_POOL_FULL".into(), "Transaction pool is full".into()));
        }
        let size = self.tx_size(tx);
        if self.bytes() + size > self.max_bytes {
            return Err(("ERR_POOL_FULL".into(), format!("Transaction pool byte budget exhausted ({} of {} bytes used, {size} needed)", self.bytes(), self.max_bytes)));
        }
        if self.storage.get_transaction(&id).map_err(storage_err)?.is_some() {
            return Err(("ERR_FORGED".into(), format!("Transaction {id} is already forged")));
        }
        if !(tx.version == 2 || tx.is_pq()) || !(tx.type_group == TYPE_GROUP_CORE || tx.is_sobj() || tx.is_token()) {
            return Err(("ERR_UNKNOWN".into(), "Only version 2 / 3 core, sObject and token transactions are supported".into()));
        }
        if let Some(n) = tx.network {
            if n != self.network.pubkey_hash {
                return Err(("ERR_WRONG_NETWORK".into(), format!("Transaction network {n} does not match {}", self.network.pubkey_hash)));
            }
        }
        let tip = self.storage.get_last_height().map_err(storage_err)?;
        let broadcast = self.check_core_fee(tx, size, self.storage.network().milestone(tip + 1))?;
        match verify_transaction_signature(tx) {
            Ok(true) => {}
            Ok(false) => return Err(("ERR_BAD_DATA".into(), "Transaction signature is invalid".into())),
            Err(e) => return Err(("ERR_BAD_DATA".into(), e.to_string())),
        }
        let sender = address_from_public_key(&tx.sender_public_key, self.network.pubkey_hash)
            .map_err(|e| ("ERR_BAD_DATA".into(), e.to_string()))?;
        for recipient in recipients(tx) {
            if !validate_address(recipient, self.network.pubkey_hash) {
                return Err(("ERR_BAD_DATA".into(), format!("Invalid recipient {recipient}")));
            }
        }

        let wallet = self.storage.get_wallet(&sender).map_err(storage_err)?;
        let (balance, nonce, second_pk) = wallet.as_ref().map(|w| (w.balance, w.nonce, w.second_public_key.clone())).unwrap_or((0, 0, None));
        let pending: Vec<&Transaction> = pool.values().filter(|p| p.sender_public_key == tx.sender_public_key).collect();
        // a registration still waiting in the pool already binds the sender's next transactions
        let pending_second_pk = pending
            .iter()
            .filter_map(|p| crate::rules::registered_second_key(p).ok().flatten())
            .next()
            .map(str::to_string);
        {
            // Quantum Shield stage B: a PQ registration waiting in the pool already locks the sender's next transactions
            let tip = self.storage.get_last_height().map_err(storage_err)?;
            let ms = self.storage.network().milestone(tip + 1);
            let pending_pq = pending.iter().find(|p| p.is_pq() && p.type_group == TYPE_GROUP_CORE && p.type_ == tx_type::SECOND_SIGNATURE).and_then(|p| {
                let a = p.asset.as_ref()?.signature.as_ref()?;
                Some(crate::crypto::pq::PqKey { algorithm: a.algorithm.unwrap_or(1), public_key: a.public_key.clone(), since: tip + 1 })
            });
            let view = crate::rules::PqWalletView {
                second_public_key: if pending_pq.is_some() { None } else { second_pk.as_deref().or(pending_second_pk.as_deref()) },
                pq_key: pending_pq.as_ref().or(wallet.as_ref().and_then(|w| w.pq_key.as_ref())),
                commitment: wallet.as_ref().and_then(|w| w.pq_commitment.as_ref()),
            };
            let code = |e: &str| if e.starts_with("ERR_PQ_") { e.split(':').next().unwrap_or("ERR_PQ").to_string() } else { "ERR_BAD_DATA".to_string() };
            crate::rules::check_pq(tx, ms, tip + 1, self.storage.network().pq_activation_height(), view).map_err(|e| (code(&e), e))?;
        }
        crate::rules::check_second_signature(tx, second_pk.as_deref().or(pending_second_pk.as_deref()))
            .map_err(|e| ("ERR_BAD_DATA".into(), e))?;
        crate::rules::registered_second_key(tx).map_err(|e| ("ERR_BAD_DATA".into(), e))?;
        if tx.is_token() {
            let height = self.storage.get_last_height().map_err(storage_err)?;
            let ms = self.storage.network().milestone(height + 1);
            if !ms.tokens {
                return Err(("ERR_UNKNOWN".into(), "Token transactions are not activated yet".into()));
            }
            let a = crate::rules::check_token_format(tx, ms).map_err(|e| ("ERR_BAD_DATA".into(), e))?;
            if tx.type_ == crate::models::token::INIT && pool.values().any(|p| p.type_ == crate::models::token::INIT && p.token_asset().is_some_and(|o| o.id == a.id)) {
                return Err(("ERR_PENDING".into(), "TokenInit for this id is already pending".into()));
            }
            let view = PoolTokenView { storage: &self.storage, pool: &pool, sender_pk: &tx.sender_public_key };
            crate::rules::check_token(tx, &a, &sender, &view).map_err(|e| ("ERR_APPLY".into(), e))?;
        } else if tx.type_group != crate::models::TYPE_GROUP_CORE {
            if !tx.is_sobj() {
                return Err(("ERR_UNKNOWN".into(), format!("unsupported typeGroup {} type {}", tx.type_group, tx.type_)));
            }
            let height = self.storage.get_last_height().map_err(storage_err)?;
            if !self.storage.network().milestone(height + 1).sobj_active {
                return Err(("ERR_UNKNOWN".into(), "SmartObject transactions are not activated yet (milestone ship13 / sobj)".into()));
            }
            let mut wallet = self.storage.get_wallet(&sender).map_err(storage_err)?;
            if let Some(w) = wallet.as_mut() {
                // a purchase must be affordable after everything the sender already has in the pool
                w.balance = (w.balance as i128 - pending.iter().map(|p| self.spent(p)).sum::<i128>()).max(i64::MIN as i128) as i64;
            }
            let no_pending = Default::default();
            let taken = |name: &str, t: u8| self.storage.sobj_by_name(name, t).ok().flatten().is_some();
            let lookup = |reg: &str| self.storage.get_sobject(reg).ok().flatten();
            let supply = |id: &str| self.storage.token_state(id).ok().flatten().map(|s| s.supply);
            crate::rules::check_sobj(tx, self.storage.network().milestone(height + 1), wallet.as_ref(), &no_pending, taken, lookup, supply).map_err(|e| ("ERR_APPLY".into(), e))?;
            if let Some(e) = tx.sobj_asset().filter(|e| e.action == crate::models::sobj::ACTION_REGISTER) {
                let name = e.data.name.unwrap_or_default().to_lowercase();
                let clash = pool.values().any(|p| {
                    p.sobj_asset().is_some_and(|q| q.action == crate::models::sobj::ACTION_REGISTER && q.type_ == e.type_
                        && q.data.name.as_deref().map(str::to_lowercase) == Some(name.clone()))
                });
                if clash {
                    return Err(("ERR_PENDING".into(), format!("SmartObject registration for \"{name}\" already in the pool")));
                }
            }
        }
        let pending_spent: i128 = pending.iter().map(|p| self.spent(p)).sum();
        let expected_nonce = nonce + 1 + pending.len() as u64;
        match tx.nonce {
            Some(n) if n == expected_nonce => {}
            Some(n) => {
                return Err(("ERR_BAD_NONCE".into(), format!("Sender {sender}: nonce {n}, expected {expected_nonce}")));
            }
            None => return Err(("ERR_BAD_NONCE".into(), "Transaction nonce is missing".into())),
        }
        if (balance as i128) - pending_spent < self.spent(tx) {
            return Err(("ERR_LOW_BALANCE".into(), format!("Sender {sender} has insufficient balance")));
        }
        if tx.type_group == crate::models::TYPE_GROUP_CORE && tx.type_ == tx_type::TRANSFER && tx.recipient_id.is_none() {
            return Err(("ERR_BAD_DATA".into(), "Transfer without recipient".into()));
        }
        Ok(broadcast)
    }

    /// Drop transactions that made it into a block or whose nonce is now stale.
    pub async fn prune_confirmed(&self) {
        let snapshot: Vec<(String, Transaction)> =
            self.txs.read().await.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut stale = Vec::new();
        for (id, tx) in snapshot {
            let forged = self.storage.get_transaction(&id).ok().flatten().is_some();
            let stale_nonce = address_from_public_key(&tx.sender_public_key, self.network.pubkey_hash)
                .ok()
                .and_then(|a| self.storage.get_wallet(&a).ok().flatten())
                .map(|w| tx.nonce.unwrap_or(0) <= w.nonce)
                .unwrap_or(false);
            if forged || stale_nonce {
                stale.push(id);
            }
        }
        if !stale.is_empty() {
            let mut pool = self.txs.write().await;
            for id in &stale {
                if let Some(t) = pool.remove(id) {
                    self.bytes.fetch_sub(self.tx_size(&t).min(self.bytes()), std::sync::atomic::Ordering::Relaxed);
                }
            }
            tracing::debug!(removed = stale.len(), "mempool pruned");
        }
    }

    /// Best-effort forward so the transaction reaches the forging network: legacy P2P
    /// `postTransactions` (port 4001, IP peers) first, REST `POST /api/transactions` as fallback.
    async fn relay(&self, ids: &[String]) {
        let txs: Vec<Transaction> = {
            let pool = self.txs.read().await;
            ids.iter().filter_map(|id| pool.get(id).cloned()).collect()
        };
        let serialized: Vec<Vec<u8>> = txs
            .iter()
            .filter_map(|t| crate::crypto::serialize_transaction(t, crate::crypto::SerializeOptions::default(), &self.network).ok())
            .collect();
        if let Some(table) = &self.peers {
            let delivered = crate::p2p_legacy::broadcast(table, serialized, self.relay_fanout, std::time::Duration::from_secs(10)).await;
            if delivered > 0 {
                return;
            }
        }
        if self.relay_nodes.is_empty() {
            return;
        }
        let body = serde_json::json!({ "transactions": txs });
        for node in &self.relay_nodes {
            let url = format!("{node}/api/transactions");
            match self.client.post(&url).json(&body).send().await {
                Ok(r) if r.status().is_success() => {
                    tracing::info!(node, count = txs.len(), "transactions relayed to legacy network");
                    return;
                }
                Ok(r) => tracing::warn!(node, status = %r.status(), "relay rejected"),
                Err(e) => tracing::warn!(node, error = %e, "relay failed"),
            }
        }
    }
}

fn storage_err(e: crate::error::Error) -> (String, String) {
    ("ERR_UNKNOWN".into(), e.to_string())
}

fn recipients(tx: &Transaction) -> Vec<&str> {
    let mut out: Vec<&str> = tx.recipient_id.iter().map(|s| s.as_str()).collect();
    if let Some(p) = tx.asset.as_ref().and_then(|a| a.payments.as_ref()) {
        out.extend(p.iter().map(|p| p.recipient_id.as_str()));
    }
    out
}

impl Mempool {
    /// Total outflow of a transaction: amount + fee + multipayment sum + the price of an sObject buy (read from the order).
    fn spent(&self, tx: &Transaction) -> i128 {
        let payments: u128 = tx
            .asset
            .as_ref()
            .and_then(|a| a.payments.as_ref())
            .map(|p| p.iter().map(|x| x.amount as u128).sum())
            .unwrap_or(0);
        let buy_price = tx
            .sobj_asset()
            .filter(|e| e.action == crate::models::sobj::ACTION_BUY)
            .and_then(|e| self.storage.get_sobject(e.registration_id.as_deref()?).ok().flatten())
            .and_then(|(_, rec)| rec.price)
            .unwrap_or(0);
        tx.amount as i128 + tx.fee as i128 + payments as i128 + buy_price as i128
    }
}

fn sth(amount: u64) -> String {
    format!("{}.{:08} STH", amount / 100_000_000, amount % 100_000_000)
}

/// One-line, human-readable summary for the log: type, sender → recipient, amount, fee, id.
/// Token view for the mempool: storage state minus the sender's pending token spends.
struct PoolTokenView<'a> {
    storage: &'a crate::storage::Storage,
    pool: &'a HashMap<String, Transaction>,
    sender_pk: &'a str,
}

impl crate::rules::TokenView for PoolTokenView<'_> {
    fn token_owner(&self, id: &str) -> Option<String> {
        self.storage.token_owner(id).ok().flatten()
    }
    fn token_state(&self, id: &str) -> Option<crate::storage::TokenState> {
        self.storage.token_state(id).ok().flatten()
    }
    fn token_balance(&self, address: &str, id: &str) -> u64 {
        let on_chain = self.storage.get_wallet(address).ok().flatten().and_then(|w| w.tokens.get(id).copied()).unwrap_or(0);
        let pending: u64 = self
            .pool
            .values()
            .filter(|p| p.sender_public_key == self.sender_pk)
            .filter_map(|p| p.token_asset().filter(|a| a.id == id).map(|a| (p.type_, a)))
            .map(|(t, a)| match t {
                crate::models::token::TRANSFER => a.transfers.unwrap_or_default().iter().map(|i| i.amount).sum(),
                crate::models::token::BURN => a.amount.unwrap_or(0),
                _ => 0,
            })
            .sum();
        on_chain.saturating_sub(pending)
    }
    fn ticker_sobj(&self, id: &str) -> Option<(String, String, bool)> {
        let (owner, r) = self.storage.get_sobject(id).ok().flatten()?;
        (r.type_ == crate::models::token::TICKER_SOBJ_TYPE).then(|| (owner, r.data.name.unwrap_or_default(), r.resigned))
    }
}

fn describe(tx: &Transaction, pubkey_hash: u8) -> String {
    use crate::models::tx_type;
    let from = crate::crypto::address_from_public_key(&tx.sender_public_key, pubkey_hash).unwrap_or_else(|_| "?".into());
    let id = tx.id.as_deref().unwrap_or("?");
    let kind = if let Some(a) = tx.token_asset() {
        format!("Token[{}] {}", ["init", "transfer", "mint", "burn", "meta"].get(tx.type_ as usize).unwrap_or(&"?"), &a.id[..12])
    } else if tx.is_sobj() {
        let a = tx.sobj_asset().unwrap_or_default();
        format!("SmartObject[{}] {}", ["register", "update", "resign"].get(a.action as usize).unwrap_or(&"?"), a.data.name.unwrap_or_default())
    } else {
        match tx.type_ {
            tx_type::TRANSFER => "Transfer".to_string(),
            tx_type::SECOND_SIGNATURE => "SecondSignature".into(),
            tx_type::DELEGATE_REGISTRATION => format!("DelegateRegistration {}", tx.asset.as_ref().and_then(|a| a.delegate.as_ref()).map(|d| d.username.as_str()).unwrap_or("?")),
            tx_type::VOTE => format!("Vote {}", tx.asset.as_ref().and_then(|a| a.votes.as_ref()).map(|v| v.join(",")).unwrap_or_default()),
            tx_type::MULTI_SIGNATURE => "MultiSignature".into(),
            tx_type::IPFS => "Ipfs".into(),
            tx_type::MULTI_PAYMENT => format!("MultiPayment x{}", tx.asset.as_ref().and_then(|a| a.payments.as_ref()).map(|p| p.len()).unwrap_or(0)),
            tx_type::DELEGATE_RESIGNATION => "DelegateResignation".into(),
            tx_type::HTLC_LOCK => "HtlcLock".into(),
            tx_type::HTLC_CLAIM => "HtlcClaim".into(),
            tx_type::HTLC_REFUND => "HtlcRefund".into(),
            t => format!("type {t}"),
        }
    };
    let total: u64 = match tx.type_ {
        tx_type::MULTI_PAYMENT => tx.asset.as_ref().and_then(|a| a.payments.as_ref()).map(|p| p.iter().map(|x| x.amount).sum()).unwrap_or(0),
        _ => tx.amount,
    };
    let to = tx.recipient_id.as_deref().unwrap_or("-");
    let memo = tx.vendor_field.as_deref().map(|v| format!(" memo=\"{v}\"")).unwrap_or_default();
    format!("{kind} {from} -> {to} amount={} fee={} nonce={} id={id}{memo}", sth(total), sth(tx.fee), tx.nonce.unwrap_or(0))
}
