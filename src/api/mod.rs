//! Author: TechnoL0g
//!
//! Local REST API (axum) on 127.0.0.1:4003 — drop-in replacement for the legacy
//! `@smartholdem/core-api` JSON so wallets, explorers and netfory-provider keep working.
//! Every resource is served in the legacy "transformed" shape; `?transform=false` returns raw core objects.
//! Resources live in one module each: `node`, `blocks`, `transactions`, `wallets`, `delegates`, `locks`.

mod blocks;
mod delegates;
mod locks;
mod node;
mod render;
mod status_page;
mod transactions;
mod wallets;

pub use render::human_time;

use crate::config::Network;
use crate::error::Error;
use crate::mempool::Mempool;
use crate::p2p_legacy::PeerTable;
use crate::storage::Storage;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct AppState {
    pub storage: Arc<Storage>,
    pub mempool: Arc<Mempool>,
    pub network: Network,
    pub peers: Vec<String>,
    pub peer_table: Option<Arc<PeerTable>>,
    pub iroh: Option<Arc<crate::p2p_iroh::IrohNode>>,
    pub forging: Option<Arc<Mutex<crate::delegate::ForgingStatus>>>,
    pub started: Instant,
    delegates_cache: Mutex<Option<(u64, Vec<Value>)>>,
}

impl AppState {
    pub fn new(storage: Arc<Storage>, mempool: Arc<Mempool>, peers: Vec<String>) -> Self {
        let network = storage.network().clone();
        Self { storage, mempool, network, peers, peer_table: None, iroh: None, forging: None, started: Instant::now(), delegates_cache: Mutex::new(None) }
    }

    pub fn with_forging(mut self, status: Arc<Mutex<crate::delegate::ForgingStatus>>) -> Self {
        self.forging = Some(status);
        self
    }

    pub fn with_iroh(mut self, node: Arc<crate::p2p_iroh::IrohNode>) -> Self {
        self.iroh = Some(node);
        self
    }

    /// Expose the live legacy peer table through `/api/peers` and `/api/node/peers`.
    pub fn with_peer_table(mut self, table: Arc<PeerTable>) -> Self {
        self.peer_table = Some(table);
        self
    }
}

pub type Shared = Arc<AppState>;

/// Query parameters in original order (legacy pagination links reproduce the caller's order).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(transparent)]
pub struct Params(Vec<(String, String)>);

impl Params {
    pub fn get(&self, key: &str) -> Option<&String> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn parse<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        self.get(key).and_then(|v| v.parse().ok())
    }

    /// `key`, `key.from`, `key.to` → inclusive numeric range check.
    pub fn range_ok(&self, key: &str, value: u64) -> bool {
        if let Some(exact) = self.parse::<u64>(key) {
            return value == exact;
        }
        self.parse::<u64>(&format!("{key}.from")).map_or(true, |f| value >= f)
            && self.parse::<u64>(&format!("{key}.to")).map_or(true, |t| value <= t)
    }

    pub fn has_range(&self, key: &str) -> bool {
        self.get(key).is_some() || self.get(&format!("{key}.from")).is_some() || self.get(&format!("{key}.to")).is_some()
    }

    pub fn order(&self) -> Option<(&str, bool)> {
        self.get("orderBy").map(|o| match o.split_once(':') {
            Some((f, dir)) => (f, dir.eq_ignore_ascii_case("asc")),
            None => (o.as_str(), false),
        })
    }

    pub fn ascending(&self) -> bool {
        self.order().map(|(_, asc)| asc).unwrap_or(false)
    }
}

/// Legacy error envelope `{ statusCode, error, message }`.
pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "statusCode": self.0.as_u16(),
            "error": self.0.canonical_reason().unwrap_or("Error"),
            "message": self.1
        });
        (self.0, Json(body)).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

pub fn not_found(what: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("{what} not found"))
}

pub type ApiResult = std::result::Result<Json<Value>, ApiError>;

pub fn router(state: Shared) -> Router {
    // unknown routes answer with the legacy error envelope instead of an empty 404

    Router::new()
        .route("/", get(node::hello))
        .route("/status", get(status_page::page))
        .route("/api/blockchain", get(node::blockchain))
        .route("/api/node/status", get(node::status))
        .route("/api/node/syncing", get(node::syncing))
        .route("/api/node/configuration", get(node::configuration))
        .route("/api/node/configuration/crypto", get(node::configuration_crypto))
        .route("/api/node/fees", get(node::fees))
        .route("/api/node/peers", get(node::peer_dashboard))
        .route("/api/node/forging", get(node::forging))
        .route("/api/peers", get(node::peers))
        .route("/api/peers/:ip", get(node::peer_by_ip))
        .route("/api/rounds/:round/delegates", get(delegates::round_delegates))
        .route("/api/blocks", get(blocks::list))
        .route("/api/blocks/first", get(blocks::first))
        .route("/api/blocks/last", get(blocks::last))
        .route("/api/blocks/:id", get(blocks::by_id))
        .route("/api/blocks/:id/transactions", get(blocks::transactions))
        .route("/api/transactions", get(transactions::list).post(transactions::post))
        .route("/api/transactions/fees", get(node::transaction_fees))
        .route("/api/transactions/types", get(node::transaction_types))
        .route("/api/transactions/schemas", get(node::transaction_schemas))
        .route("/api/transactions/unconfirmed", get(transactions::unconfirmed))
        .route("/api/transactions/unconfirmed/:id", get(transactions::unconfirmed_by_id))
        .route("/api/transactions/:id", get(transactions::by_id))
        .route("/api/votes", get(transactions::votes))
        .route("/api/votes/:id", get(transactions::vote_by_id))
        .route("/api/locks", get(locks::list))
        .route("/api/locks/unlocked", post(locks::unlocked))
        .route("/api/locks/:id", get(locks::by_id))
        .route("/api/entities", get(node::entities))
        .route("/api/entities/:id", get(node::entity_by_id))
        .route("/api/wallets", get(wallets::list))
        .route("/api/wallets/top", get(wallets::top))
        .route("/api/wallets/:id", get(wallets::by_id))
        .route("/api/wallets/:id/transactions", get(wallets::transactions))
        .route("/api/wallets/:id/transactions/sent", get(wallets::transactions_sent))
        .route("/api/wallets/:id/transactions/received", get(wallets::transactions_received))
        .route("/api/wallets/:id/votes", get(wallets::votes))
        .route("/api/wallets/:id/locks", get(wallets::locks))
        .route("/api/delegates", get(delegates::list))
        .route("/api/delegates/:id", get(delegates::by_id))
        .route("/api/delegates/:id/voters", get(delegates::voters))
        .route("/api/delegates/:id/blocks", get(delegates::blocks))
        .fallback(|| async { not_found_plain() })
        .with_state(state)
}

fn not_found_plain() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "Not Found".into())
}

/// Bind and serve until the process exits.
pub async fn serve(state: Shared, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "local REST API listening");
    serve_listener(state, listener).await
}

pub async fn serve_listener(state: Shared, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

// ------------------------------------------------------------------ pagination

pub fn page_params(q: &Params) -> (usize, usize) {
    let limit = q.parse::<usize>("limit").unwrap_or(100).clamp(1, 100);
    let page = q.parse::<usize>("page").unwrap_or(1).max(1);
    (page, limit)
}

pub fn transform(q: &Params) -> bool {
    q.get("transform").map(|v| v != "false").unwrap_or(true)
}

pub fn paginate(path: &str, q: &Params, total: usize, data: Vec<Value>) -> Value {
    let (page, limit) = page_params(q);
    let page_count = total.div_ceil(limit).max(1);
    // Legacy link layout: caller's params in order (page substituted), then defaults `limit`, `transform=true`,
    // and `orderBy` last (Joi re-appends it after normalisation).
    let link = |p: usize| -> Value {
        let mut parts: Vec<String> = Vec::new();
        let mut seen_page = false;
        for (k, v) in &q.0 {
            match k.as_str() {
                "page" => {
                    seen_page = true;
                    parts.push(format!("page={p}"));
                }
                "orderBy" | "transform" => {}
                _ => parts.push(format!("{k}={v}")),
            }
        }
        if !seen_page {
            parts.push(format!("page={p}"));
        }
        if q.get("limit").is_none() {
            parts.push(format!("limit={limit}"));
        }
        parts.push(format!("transform={}", if transform(q) { "true" } else { "false" }));
        if let Some(o) = q.get("orderBy") {
            parts.push(format!("orderBy={o}"));
        }
        Value::String(format!("{path}?{}", parts.join("&")))
    };
    json!({
        "meta": {
            "totalCountIsEstimate": false,
            "count": data.len(),
            "pageCount": page_count,
            "totalCount": total,
            "next": if page < page_count { link(page + 1) } else { Value::Null },
            "previous": if page > 1 { link(page - 1) } else { Value::Null },
            "self": link(page),
            "first": link(1),
            "last": link(page_count),
        },
        "data": data
    })
}

pub fn slice_page<T>(items: Vec<T>, q: &Params) -> (usize, Vec<T>) {
    let (page, limit) = page_params(q);
    let total = items.len();
    (total, items.into_iter().skip((page - 1) * limit).take(limit).collect())
}

pub fn last_height(st: &AppState) -> Result<u64, ApiError> {
    Ok(st.storage.get_last_height()?)
}
