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
        Self { txs: RwLock::new(HashMap::new()), storage, network, relay_nodes, peers, relay_fanout, client, max_size, events }
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
            match self.validate(&tx).await {
                Ok(()) => {
                    self.txs.write().await.insert(id.clone(), tx);
                    resp.accept.push(id.clone());
                    resp.broadcast.push(id);
                }
                Err((type_, message)) => {
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

    async fn validate(&self, tx: &Transaction) -> std::result::Result<(), (String, String)> {
        let id = tx.id.clone().unwrap_or_default();
        let pool = self.txs.read().await;
        if pool.contains_key(&id) {
            return Err(("ERR_DUPLICATE".into(), format!("Transaction {id} is already in the pool")));
        }
        if pool.len() >= self.max_size {
            return Err(("ERR_POOL_FULL".into(), "Transaction pool is full".into()));
        }
        if self.storage.get_transaction(&id).map_err(storage_err)?.is_some() {
            return Err(("ERR_FORGED".into(), format!("Transaction {id} is already forged")));
        }
        if tx.version != 2 || !(tx.type_group == TYPE_GROUP_CORE || tx.is_entity()) {
            return Err(("ERR_UNKNOWN".into(), "Only version 2 core and entity transactions are supported".into()));
        }
        if let Some(n) = tx.network {
            if n != self.network.pubkey_hash {
                return Err(("ERR_WRONG_NETWORK".into(), format!("Transaction network {n} does not match {}", self.network.pubkey_hash)));
            }
        }
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
        let (balance, nonce, second_pk) = wallet.map(|w| (w.balance, w.nonce, w.second_public_key)).unwrap_or((0, 0, None));
        let pending: Vec<&Transaction> = pool.values().filter(|p| p.sender_public_key == tx.sender_public_key).collect();
        // a registration still waiting in the pool already binds the sender's next transactions
        let pending_second_pk = pending
            .iter()
            .filter_map(|p| crate::rules::registered_second_key(p).ok().flatten())
            .next()
            .map(str::to_string);
        crate::rules::check_second_signature(tx, second_pk.as_deref().or(pending_second_pk.as_deref()))
            .map_err(|e| ("ERR_BAD_DATA".into(), e))?;
        crate::rules::registered_second_key(tx).map_err(|e| ("ERR_BAD_DATA".into(), e))?;
        if tx.type_group != crate::models::TYPE_GROUP_CORE {
            if !tx.is_entity() {
                return Err(("ERR_UNKNOWN".into(), format!("unsupported typeGroup {} type {}", tx.type_group, tx.type_)));
            }
            let height = self.storage.get_last_height().map_err(storage_err)?;
            if !self.storage.network().milestone(height + 1).aip36 {
                return Err(("ERR_UNKNOWN".into(), "Entity transactions are not activated yet (aip36)".into()));
            }
            let wallet = self.storage.get_wallet(&sender).map_err(storage_err)?;
            let no_pending = Default::default();
            let taken = |name: &str, t: u8| self.storage.entity_by_name(name, t).ok().flatten().is_some();
            crate::rules::check_entity(tx, wallet.as_ref(), &no_pending, taken).map_err(|e| ("ERR_APPLY".into(), e))?;
            if let Some(e) = tx.entity_asset().filter(|e| e.action == crate::models::entity::ACTION_REGISTER) {
                let name = e.data.name.unwrap_or_default().to_lowercase();
                let clash = pool.values().any(|p| {
                    p.entity_asset().is_some_and(|q| q.action == crate::models::entity::ACTION_REGISTER && q.type_ == e.type_
                        && q.data.name.as_deref().map(str::to_lowercase) == Some(name.clone()))
                });
                if clash {
                    return Err(("ERR_PENDING".into(), format!("Entity registration for \"{name}\" already in the pool")));
                }
            }
        }
        let pending_spent: i128 = pending.iter().map(|p| spent(p)).sum();
        let expected_nonce = nonce + 1 + pending.len() as u64;
        match tx.nonce {
            Some(n) if n == expected_nonce => {}
            Some(n) => {
                return Err(("ERR_BAD_NONCE".into(), format!("Sender {sender}: nonce {n}, expected {expected_nonce}")));
            }
            None => return Err(("ERR_BAD_NONCE".into(), "Transaction nonce is missing".into())),
        }
        if (balance as i128) - pending_spent < spent(tx) {
            return Err(("ERR_LOW_BALANCE".into(), format!("Sender {sender} has insufficient balance")));
        }
        if tx.type_ == tx_type::TRANSFER && tx.recipient_id.is_none() {
            return Err(("ERR_BAD_DATA".into(), "Transfer without recipient".into()));
        }
        Ok(())
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
                pool.remove(id);
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

/// Total outflow of a transaction (amount + fee + multipayment sum).
fn spent(tx: &Transaction) -> i128 {
    let payments: u128 = tx
        .asset
        .as_ref()
        .and_then(|a| a.payments.as_ref())
        .map(|p| p.iter().map(|x| x.amount as u128).sum())
        .unwrap_or(0);
    tx.amount as i128 + tx.fee as i128 + payments as i128
}
