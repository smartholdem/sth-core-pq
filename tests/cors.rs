//! Author: TechnoL0g
//!
//! CORS: браузерные клиенты (Vue/React-кошельки, эксплореры) должны получать Access-Control-Allow-Origin
//! на основном API и на порту метрик; `api.cors_origins` ограничивает список разрешённых origin.

use reqwest::header::{ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ORIGIN};
use sth_core::api::{serve_listener, AppState};
use sth_core::config::Network;
use sth_core::mempool::Mempool;
use sth_core::node_config::ApiConfig;
use sth_core::storage::Storage;
use std::sync::Arc;

const VUE: &str = "http://localhost:3000";

async fn spawn(origins: Vec<String>, metrics_port: bool) -> String {
    let storage = Arc::new(Storage::temporary(Network::mainnet()).unwrap());
    let mempool = Arc::new(Mempool::new(storage.clone(), vec![], 100));
    let state = Arc::new(AppState::new(storage, mempool, vec![]).with_cors_origins(origins));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    if metrics_port {
        let router = sth_core::api::ntfry::metrics_router(state);
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    } else {
        tokio::spawn(async move { serve_listener(state, listener).await.unwrap() });
    }
    format!("http://{addr}")
}

fn hdr(r: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    r.headers().get(name).map(|v| v.to_str().unwrap().to_string())
}

#[tokio::test]
async fn default_has_no_cors_headers() {
    let base = spawn(vec![], false).await;
    let r = reqwest::Client::new().get(format!("{base}/api/ntfry/metrics")).header(ORIGIN, VUE).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, ACCESS_CONTROL_ALLOW_ORIGIN), None, "CORS выключен по умолчанию");
}

#[tokio::test]
async fn wildcard_allows_any_origin_on_get() {
    let base = spawn(vec!["*".into()], false).await;
    let r = reqwest::Client::new().get(format!("{base}/api/ntfry/metrics")).header(ORIGIN, VUE).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, ACCESS_CONTROL_ALLOW_ORIGIN).as_deref(), Some("*"));
}

#[tokio::test]
async fn preflight_options_lists_methods_and_headers() {
    let base = spawn(vec![VUE.into()], false).await;
    let r = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, format!("{base}/api/transactions"))
        .header(ORIGIN, VUE)
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "content-type")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, ACCESS_CONTROL_ALLOW_ORIGIN).as_deref(), Some(VUE));
    let methods = hdr(&r, ACCESS_CONTROL_ALLOW_METHODS).unwrap();
    assert!(methods.contains("GET") && methods.contains("POST"), "{methods}");
    assert!(hdr(&r, ACCESS_CONTROL_ALLOW_HEADERS).unwrap().to_lowercase().contains("content-type"));
}

#[tokio::test]
async fn configured_origins_restrict_access() {
    let base = spawn(vec![format!("{VUE}/")], false).await;
    let client = reqwest::Client::new();

    let r = client.get(format!("{base}/api/ntfry/metrics")).header(ORIGIN, VUE).send().await.unwrap();
    assert_eq!(hdr(&r, ACCESS_CONTROL_ALLOW_ORIGIN).as_deref(), Some(VUE));

    let r = client.get(format!("{base}/api/ntfry/metrics")).header(ORIGIN, "http://evil.example").send().await.unwrap();
    assert_eq!(r.status(), 200, "запрос обслуживается, но без CORS-заголовка");
    assert_eq!(hdr(&r, ACCESS_CONTROL_ALLOW_ORIGIN), None);
}

#[tokio::test]
async fn metrics_port_has_cors_too() {
    let base = spawn(vec!["*".into()], true).await;
    let r = reqwest::Client::new().get(format!("{base}/api/ntfry/metrics")).header(ORIGIN, VUE).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, ACCESS_CONTROL_ALLOW_ORIGIN).as_deref(), Some("*"));
}

#[test]
fn cors_origins_default_is_empty_and_parses_from_yaml() {
    assert!(ApiConfig::default().cors_origins.is_empty());
    let cfg: ApiConfig = serde_yaml::from_str("cors_origins: [\"http://localhost:3000\", \"https://wallet.smartholdem.io\"]").unwrap();
    assert_eq!(cfg.cors_origins.len(), 2);
}
