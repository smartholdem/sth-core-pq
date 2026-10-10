//! Author: TechnoL0g
//!
//! Operator alerts: webhook delivery, message tagging and per-kind cooldown.

use axum::{routing::post, Json, Router};
use serde_json::Value;
use sth_core::node_config::AlertsConfig;

#[tokio::test]
async fn webhook_receives_tagged_alerts_and_cooldown_suppresses_repeats() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let app = Router::new().route(
        "/hook",
        post(move |Json(body): Json<Value>| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(body);
                "ok"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let cfg = AlertsConfig { enabled: true, webhook_url: format!("http://{addr}/hook"), cooldown_secs: 600, ..AlertsConfig::default() };
    assert!(cfg.active());
    assert!(!AlertsConfig::default().active(), "no target configured → off");
    sth_core::alerts::init(&cfg, "1.2.3.4:4001");
    assert!(sth_core::alerts::enabled());

    sth_core::alerts::notify("stall", "CHAIN STALLED: tip 10 unchanged", false).expect("first alert of a kind is sent").await.unwrap();
    let body = rx.recv().await.unwrap();
    assert_eq!(body["kind"], "stall");
    assert_eq!(body["text"], "[sth-core-pq 1.2.3.4:4001] CHAIN STALLED: tip 10 unchanged");
    assert!(body["at"].as_u64().unwrap() > 1_700_000_000);

    assert!(sth_core::alerts::notify("stall", "again", false).is_none(), "same kind inside the cooldown is suppressed");
    sth_core::alerts::notify("fork", "fork detected", false).expect("another kind passes").await.unwrap();
    assert_eq!(rx.recv().await.unwrap()["kind"], "fork");
    sth_core::alerts::notify("stall", "chain recovered", true).expect("force bypasses the cooldown").await.unwrap();
    assert_eq!(rx.recv().await.unwrap()["text"], "[sth-core-pq 1.2.3.4:4001] chain recovered");
}

/// `sth-core alerts-test`: with an empty chat id the command lists the chats the bot has seen (`getUpdates`), with a
/// chat id it sends a test message and surfaces Telegram's error description.
#[tokio::test]
async fn alerts_test_discovers_chat_ids_and_reports_delivery() {
    use axum::extract::Path;
    use axum::routing::get;
    let app = Router::new().route(
        "/:token/:method",
        get(fake_telegram).post(fake_telegram),
    );
    async fn fake_telegram(Path((token, method)): Path<(String, String)>, body: Option<Json<Value>>) -> Json<Value> {
        if token != "bot123:GOOD" {
            return Json(serde_json::json!({ "ok": false, "error_code": 401, "description": "Unauthorized" }));
        }
        Json(match method.as_str() {
            "getMe" => serde_json::json!({ "ok": true, "result": { "username": "sth_alerts_bot" } }),
            "getUpdates" => serde_json::json!({ "ok": true, "result": [
                { "update_id": 1, "message": { "chat": { "id": 123456789, "type": "private", "first_name": "Alice" }, "text": "/start" } },
                { "update_id": 2, "message": { "chat": { "id": 123456789, "type": "private", "first_name": "Alice" }, "text": "hi" } },
                { "update_id": 3, "my_chat_member": { "chat": { "id": -1001234567890i64, "type": "supergroup", "title": "STH delegates" } } }
            ] }),
            "sendMessage" => {
                let chat = body.as_ref().and_then(|b| b["chat_id"].as_str()).unwrap_or("");
                if chat == "123456789" { serde_json::json!({ "ok": true, "result": {} }) } else { serde_json::json!({ "ok": false, "error_code": 400, "description": "Bad Request: chat not found" }) }
            }
            _ => serde_json::json!({ "ok": false, "description": "unknown method" }),
        })
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::env::set_var("STH_TELEGRAM_API", format!("http://{addr}"));

    let lines = sth_core::alerts::test_delivery(&AlertsConfig::default(), "n").await;
    assert_eq!(lines.len(), 1);
    assert!(!lines[0].0 && lines[0].1.contains("nothing configured"), "{lines:?}");

    let bad = AlertsConfig { telegram_bot_token: "bad".into(), ..AlertsConfig::default() };
    let lines = sth_core::alerts::test_delivery(&bad, "n").await;
    assert!(lines.iter().any(|(ok, l)| !ok && l.contains("token rejected") && l.contains("Unauthorized")), "{lines:?}");

    // empty chat id → discovery, deduplicated, private + group
    let discover = AlertsConfig { telegram_bot_token: "123:GOOD".into(), ..AlertsConfig::default() };
    let lines = sth_core::alerts::test_delivery(&discover, "n").await;
    assert!(lines.iter().any(|(ok, l)| *ok && l.contains("@sth_alerts_bot")), "{lines:?}");
    let ids: Vec<&String> = lines.iter().filter(|(_, l)| l.contains("telegram_chat_id: \"")).map(|(_, l)| l).collect();
    assert_eq!(ids.len(), 2, "{lines:?}");
    assert!(ids[0].contains("\"123456789\"") && ids[0].contains("private Alice"), "{ids:?}");
    assert!(ids[1].contains("\"-1001234567890\"") && ids[1].contains("supergroup STH delegates"), "{ids:?}");

    let ok = AlertsConfig { telegram_bot_token: "123:GOOD".into(), telegram_chat_id: "123456789".into(), ..AlertsConfig::default() };
    let lines = sth_core::alerts::test_delivery(&ok, "1.2.3.4:4001").await;
    assert!(lines.iter().all(|(ok, _)| *ok), "{lines:?}");
    assert!(lines.iter().any(|(_, l)| l.contains("test message delivered to chat 123456789")), "{lines:?}");

    let wrong = AlertsConfig { telegram_bot_token: "123:GOOD".into(), telegram_chat_id: "42".into(), ..AlertsConfig::default() };
    let lines = sth_core::alerts::test_delivery(&wrong, "n").await;
    assert!(lines.iter().any(|(ok, l)| !ok && l.contains("chat not found") && l.contains("/start")), "{lines:?}");
    std::env::remove_var("STH_TELEGRAM_API");
}
