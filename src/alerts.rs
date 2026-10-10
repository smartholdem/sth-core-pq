//! Author: TechnoL0g
//!
//! Operator alerts: CHAIN STALLED / recovered, fork rollbacks and audit drift pushed to a Telegram chat and/or a generic
//! JSON webhook (`alerts:` section of node.yaml). Best effort, rate-limited per kind, never blocks the node.

use crate::node_config::AlertsConfig;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

struct Alerts {
    cfg: AlertsConfig,
    node: String,
    last: Mutex<HashMap<&'static str, Instant>>,
}

static ALERTS: OnceLock<Alerts> = OnceLock::new();

/// Install the configuration once at start (`node` = short label shown in every message, see `node_label`).
pub fn init(cfg: &AlertsConfig, node: impl Into<String>) {
    let _ = ALERTS.set(Alerts { cfg: cfg.clone(), node: node.into(), last: Default::default() });
}

/// `[sth-core-pq <label>]` prefix of every message: `alerts.node_name`, else the machine's hostname, plus the public
/// address (`p2p.legacy_public_addr`) when known and the network when it is not mainnet — several nodes in one chat
/// must be told apart.
pub fn node_label(cfg: &crate::node_config::NodeConfig, network: &str) -> String {
    let name = cfg.alerts.node_name.trim();
    let name = if name.is_empty() { hostname() } else { name.to_string() };
    let mut parts = vec![name];
    let addr = cfg.p2p.legacy_public_addr.trim();
    if !addr.is_empty() {
        parts.push(addr.to_string());
    }
    if network != "mainnet" {
        parts.push(network.to_string());
    }
    parts.join(" ")
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "node".into())
}

/// Telegram Bot API base (`STH_TELEGRAM_API` overrides it for tests).
fn telegram_api() -> String {
    std::env::var("STH_TELEGRAM_API").unwrap_or_else(|_| "https://api.telegram.org".into())
}

pub fn enabled() -> bool {
    ALERTS.get().is_some_and(|a| a.cfg.active())
}

/// Send `text` tagged with `kind` (cooldown per kind, `cooldown_secs`); `force` bypasses the cooldown (recovery messages).
/// Returns the delivery task so CLI callers can await it; the node just drops the handle.
pub fn notify(kind: &'static str, text: impl Into<String>, force: bool) -> Option<tokio::task::JoinHandle<()>> {
    let a = ALERTS.get().filter(|a| a.cfg.active())?;
    {
        let mut last = a.last.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if !force && last.get(kind).is_some_and(|t| now.duration_since(*t) < Duration::from_secs(a.cfg.cooldown_secs)) {
            return None;
        }
        last.insert(kind, now);
    }
    let text = text.into();
    let (cfg, node) = (a.cfg.clone(), a.node.clone());
    Some(tokio::spawn(async move { send_now(&cfg, &node, kind, &text).await }))
}

/// Deliver one message right away (no cooldown): Telegram and/or webhook. Used by `notify` and by the freeze watchdog
/// thread, which runs it on its own tiny runtime because the main one is frozen.
pub async fn send_now(cfg: &AlertsConfig, node: &str, kind: &str, text: &str) {
    let text = format!("[sth-core-pq {node}] {text}");
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(10)).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "alert client");
            return;
        }
    };
    if !cfg.telegram_bot_token.is_empty() && !cfg.telegram_chat_id.is_empty() {
        let url = format!("{}/bot{}/sendMessage", telegram_api(), cfg.telegram_bot_token);
        let body = serde_json::json!({ "chat_id": cfg.telegram_chat_id, "text": text, "disable_web_page_preview": true });
        match client.post(&url).json(&body).send().await {
            Ok(r) if r.status().is_success() => tracing::info!(kind, "alert sent to telegram"),
            Ok(r) => tracing::warn!(kind, status = %r.status(), "telegram rejected the alert"),
            Err(e) => tracing::warn!(kind, error = %e, "telegram alert failed"),
        }
    }
    if !cfg.webhook_url.is_empty() {
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let body = serde_json::json!({ "kind": kind, "text": text, "at": at });
        match client.post(&cfg.webhook_url).json(&body).send().await {
            Ok(r) if r.status().is_success() => tracing::info!(kind, "alert sent to webhook"),
            Ok(r) => tracing::warn!(kind, status = %r.status(), "webhook rejected the alert"),
            Err(e) => tracing::warn!(kind, error = %e, "webhook alert failed"),
        }
    }
}

/// `sth-core alerts-test`: check the `alerts:` section end to end. Empty `telegram_chat_id` → list the chats the bot has
/// seen (`getUpdates`) so the operator can copy the id; otherwise send a test message to Telegram and the webhook.
/// Returns `(ok, line)` pairs to print.
pub async fn test_delivery(cfg: &AlertsConfig, node: &str) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    if cfg.telegram_bot_token.is_empty() && cfg.webhook_url.is_empty() {
        out.push((false, "nothing configured: set alerts.telegram_bot_token (+ telegram_chat_id) and/or alerts.webhook_url in node.yaml".into()));
        return out;
    }
    if !cfg.enabled {
        out.push((false, "alerts.enabled is false — messages are never sent; testing the targets anyway".into()));
    }
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(15)).build() {
        Ok(c) => c,
        Err(e) => return vec![(false, format!("http client: {e}"))],
    };
    let text = format!("[sth-core-pq {node}] test alert — alerts are configured correctly");
    if !cfg.telegram_bot_token.is_empty() {
        let api = format!("{}/bot{}", telegram_api(), cfg.telegram_bot_token);
        match client.get(format!("{api}/getMe")).send().await {
            Ok(r) => {
                let v: serde_json::Value = r.json().await.unwrap_or_default();
                if v["ok"].as_bool() == Some(true) {
                    out.push((true, format!("telegram token valid: bot @{}", v["result"]["username"].as_str().unwrap_or("?"))));
                } else {
                    out.push((false, format!("telegram token rejected: {} — copy the token from @BotFather exactly (123456:ABC…)", v["description"].as_str().unwrap_or("unknown error"))));
                    return out;
                }
            }
            Err(e) => {
                out.push((false, format!("telegram unreachable: {e}")));
                return out;
            }
        }
        if cfg.telegram_chat_id.trim().is_empty() {
            let v: serde_json::Value = match client.get(format!("{api}/getUpdates")).send().await {
                Ok(r) => r.json().await.unwrap_or_default(),
                Err(e) => {
                    out.push((false, format!("getUpdates failed: {e}")));
                    return out;
                }
            };
            let mut seen: Vec<(i64, String)> = Vec::new();
            for u in v["result"].as_array().into_iter().flatten() {
                let chat = ["message", "edited_message", "channel_post", "my_chat_member"].iter().map(|k| &u[k]["chat"]).find(|c| c["id"].is_number());
                let Some(c) = chat else { continue };
                let Some(id) = c["id"].as_i64() else { continue };
                if seen.iter().any(|(i, _)| *i == id) {
                    continue;
                }
                let kind = c["type"].as_str().unwrap_or("chat");
                let name = c["title"].as_str().or(c["username"].as_str()).or(c["first_name"].as_str()).unwrap_or("");
                seen.push((id, format!("{kind} {name}")));
            }
            if seen.is_empty() {
                out.push((false, "telegram_chat_id is empty and the bot has not seen any chat yet: open the bot in Telegram and send it /start (or add it to a group and post a message there), then rerun `sth-core alerts-test`".into()));
            } else {
                out.push((false, "telegram_chat_id is empty — chats the bot has seen (put one id into node.yaml → alerts.telegram_chat_id):".into()));
                for (id, label) in seen {
                    out.push((true, format!("  telegram_chat_id: \"{id}\"    # {label}")));
                }
            }
        } else {
            let body = serde_json::json!({ "chat_id": cfg.telegram_chat_id, "text": text, "disable_web_page_preview": true });
            match client.post(format!("{api}/sendMessage")).json(&body).send().await {
                Ok(r) => {
                    let status = r.status();
                    let v: serde_json::Value = r.json().await.unwrap_or_default();
                    if status.is_success() && v["ok"].as_bool() == Some(true) {
                        out.push((true, format!("telegram: test message delivered to chat {}", cfg.telegram_chat_id)));
                    } else {
                        let why = v["description"].as_str().unwrap_or("unknown error");
                        let hint = if why.contains("chat not found") {
                            " — wrong chat id, or you never wrote to the bot (send it /start); groups have negative ids"
                        } else if why.contains("blocked") {
                            " — you blocked the bot in Telegram"
                        } else if why.contains("not enough rights") || why.contains("kicked") {
                            " — add the bot to the group / channel (as admin for channels)"
                        } else {
                            ""
                        };
                        out.push((false, format!("telegram: HTTP {status} {why}{hint}")));
                    }
                }
                Err(e) => out.push((false, format!("telegram: {e}"))),
            }
        }
    }
    if !cfg.webhook_url.is_empty() {
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let body = serde_json::json!({ "kind": "test", "text": text, "at": at });
        match client.post(&cfg.webhook_url).json(&body).send().await {
            Ok(r) if r.status().is_success() => out.push((true, format!("webhook: {} answered {}", cfg.webhook_url, r.status()))),
            Ok(r) => out.push((false, format!("webhook: {} answered {}", cfg.webhook_url, r.status()))),
            Err(e) => out.push((false, format!("webhook: {e}"))),
        }
    }
    out
}
