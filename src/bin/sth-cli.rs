//! Author: TechnoL0g
//!
//! `sth-cli` — lightweight client for any SmartHoldem node's REST API (think `bitcoin-cli`): read commands, signed
//! transactions (`tx …`), confirmation waiting. No database, no P2P — only HTTP. Node URL: `--api`, env `STH_API`,
//! or the value remembered in `~/.config/sth-cli/api` (written whenever `--api` is given); default http://127.0.0.1:4003.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use sth_core::cli::{self, ChainSource, TxArgs};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "sth-cli", version, about = "SmartHoldem node client — query the REST API and send signed transactions")]
struct Cli {
    /// Node REST API, e.g. https://node0.smartholdem.io or http://127.0.0.1:4004 (remembered for next runs).
    #[arg(long, global = true, env = "STH_API")]
    api: Option<String>,
    /// Machine-readable output (raw JSON).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Node status: height, version, network, sync state.
    Status,
    /// Wallet: balance, nonce, tokens, smart objects.
    Wallet { address: String },
    /// Token by ticker or id: supply, owner, manifest, holders.
    Token { ticker: String },
    /// Smart object (sObject) by registration id, or by name with --type.
    Obj {
        id: String,
        #[arg(long, default_value_t = 5)]
        r#type: u8,
    },
    /// Open sale orders (100 per page).
    Market {
        #[arg(long)]
        r#type: Option<u8>,
        #[arg(long, default_value_t = 1)]
        page: usize,
    },
    /// Quantum Shield stage A: print the `sthpq1:` commitment of the ML-DSA-44 key derived from --second-passphrase
    /// (put it into the vendorField of a self-transfer before milestone pq; register the same key after).
    PqCommitment {
        #[arg(long, env = "STH_SECOND_PASSPHRASE", hide_env_values = true)]
        second_passphrase: String,
    },
    /// Transaction by id; --wait blocks until it is confirmed.
    TxStatus {
        id: String,
        #[arg(long)]
        wait: bool,
    },
    /// Build, sign and post a transaction (transfer, sObject-*, token-*).
    Tx(TxArgs),
    /// Generate a new wallet for the connected network: 12-word passphrase, address, public key (nothing is sent anywhere).
    CreateWallet {
        /// Derive the address from an existing passphrase instead of generating one.
        #[arg(long, env = "STH_PASSPHRASE", hide_env_values = true)]
        passphrase: Option<String>,
    },
    /// Delegate wizard: wallet → funding → `delegate-register` → PQ key → node.yaml in one pass.
    DelegateSetup(cli::DelegateSetupArgs),
    /// Live feed of a wallet: incoming / outgoing coins, tokens, sObject deals — polls the node every few seconds.
    Watch {
        address: String,
        /// Poll interval, seconds.
        #[arg(long, default_value_t = 4)]
        every: u64,
        /// Print the last N transactions first, then follow.
        #[arg(long, default_value_t = 5)]
        tail: usize,
    },
}

fn api_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config").join("sth-cli").join("api"))
}

fn resolve_api(flag: Option<String>) -> String {
    let api = match flag {
        Some(a) => {
            if let Some(f) = api_file() {
                let _ = std::fs::create_dir_all(f.parent().unwrap()).and_then(|_| std::fs::write(&f, &a));
            }
            a
        }
        None => api_file().and_then(|f| std::fs::read_to_string(f).ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "http://127.0.0.1:4003".into()),
    };
    api.trim_end_matches('/').to_string()
}

async fn get(http: &reqwest::Client, api: &str, path: &str) -> Result<Value> {
    let url = format!("{api}{path}");
    let r = http.get(&url).timeout(Duration::from_secs(20)).send().await.with_context(|| format!("GET {url}"))?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("not found: {path}");
    }
    Ok(r.error_for_status()?.json::<Value>().await?)
}

fn coins(v: &Value) -> String {
    let n: i128 = v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_i64().map(|x| x as i128)).unwrap_or(0);
    let (int, frac) = (n / 100_000_000, (n % 100_000_000).abs());
    if frac == 0 { format!("{int}") } else { format!("{int}.{}", format!("{frac:08}").trim_end_matches('0')) }
}

/// Human line for a transaction as seen from `me`.
fn describe(t: &Value, me: &str) -> String {
    let sender = t["sender"].as_str().unwrap_or("?");
    let mine = sender == me;
    let dir = |to: &str| if mine { format!("→ {to}") } else { format!("← {sender}") };
    let amount = coins(&t["amount"]);
    let token = &t["asset"]["token"];
    let obj = &t["asset"];
    let ttype = (t["typeGroup"].as_u64().unwrap_or(1), t["type"].as_u64().unwrap_or(0));
    match ttype {
        (1, 0) => format!("{} {amount} STH {}{}", if mine { "sent" } else { "received" }, dir(t["recipient"].as_str().unwrap_or("?")), t["vendorField"].as_str().map(|m| format!("  \"{m}\"")).unwrap_or_default()),
        (1, 6) => {
            let items: Vec<&Value> = t["asset"]["payments"].as_array().into_iter().flatten().filter(|p| mine || p["recipientId"] == me).collect();
            let sum: i128 = items.iter().map(|p| p["amount"].as_str().and_then(|s| s.parse::<i128>().ok()).unwrap_or(0)).sum();
            format!("multipayment {} STH {} ({} recipients)", coins(&json!(sum.to_string())), if mine { "sent".into() } else { dir("") }, items.len())
        }
        (1, 3) => format!("vote {}", t["asset"]["votes"]),
        (1, 2) => format!("delegate registration {}", t["asset"]["delegate"]["username"]),
        (1, 8) => format!("HTLC lock {amount} STH {}", dir(t["recipient"].as_str().unwrap_or("?"))),
        (1, 9) | (1, 10) => format!("HTLC {}", if ttype.1 == 9 { "claim" } else { "refund" }),
        (2, 6) => {
            let name = obj["data"]["name"].as_str().map(|n| format!(" {n}")).unwrap_or_default();
            let reg = obj["registrationId"].as_str().map(|r| format!(" {}…", &r[..8])).unwrap_or_default();
            match obj["action"].as_u64().unwrap_or(0) {
                0 => format!("sObject registered{name} (type {})", obj["type"]),
                1 => format!("sObject updated{reg} → {}", obj["data"]["ntfryData"].as_str().unwrap_or("")),
                2 => format!("sObject resigned{reg}"),
                3 => format!("sObject{reg} handed over {}", dir(obj["recipientId"].as_str().unwrap_or("?"))),
                4 => format!("sale order{reg}: {} STH{}", coins(&obj["price"]), if coins(&obj["price"]) == "0" { " (cancelled)" } else { "" }),
                5 => format!("sObject{reg} {} — price paid to the previous owner", if mine { "bought" } else { "sold" }),
                a => format!("sObject action {a}"),
            }
        }
        (3, ty) => {
            let id = token["id"].as_str().map(|r| format!("{}…", &r[..8])).unwrap_or_default();
            match ty {
                0 => format!("token init {id}: supply {} decimals {}", token["initialSupply"], token["decimals"]),
                1 => {
                    let items: Vec<&Value> = token["transfers"].as_array().into_iter().flatten().filter(|x| mine || x["recipientId"] == me).collect();
                    let sum: u128 = items.iter().map(|x| x["amount"].as_str().and_then(|s| s.parse::<u128>().ok()).unwrap_or(0)).sum();
                    format!("token {id} {} {sum} units {}{}", if mine { "sent" } else { "received" }, if mine { format!("→ {} recipient(s)", items.len()) } else { dir("") }, token["memo"].as_str().map(|m| format!("  \"{m}\"")).unwrap_or_default())
                }
                2 => format!("token {id} mint {} units {}", token["amount"], dir(token["recipientId"].as_str().unwrap_or("?"))),
                3 => format!("token {id} burn {} units", token["amount"]),
                4 => format!("token {id} manifest: {}", token["meta"]["name"].as_str().unwrap_or("")),
                _ => format!("token tx type {ty}"),
            }
        }
        (g, ty) => format!("tx group {g} type {ty}"),
    }
}

fn kv(rows: &[(&str, String)]) {
    let w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, v) in rows {
        println!("{k:<w$}  {v}");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let api = resolve_api(cli.api.clone());
    let http = reqwest::Client::new();
    match cli.cmd {
        Cmd::Tx(args) => cli::run(ChainSource::Api(api), args).await,
        Cmd::DelegateSetup(args) => cli::delegate_setup(api, args).await,
        Cmd::Status => {
            let s = get(&http, &api, "/api/node/status").await?["data"].take();
            let c = get(&http, &api, "/api/node/configuration").await?["data"].take();
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&json!({ "status": s, "configuration": c }))?);
                return Ok(());
            }
            let finality = {
                let f = &c["finality"];
                let now = s["now"].as_u64().unwrap_or(0);
                match f["finalizedHeight"].as_u64() {
                    Some(h) if h > 0 => format!("final up to {h} (lag {}) · {} mode · quorum {}/{}", now.saturating_sub(h), if f["hard"] == true { "hard" } else { "soft" }, f["quorum"], f["activeDelegates"]),
                    Some(_) => format!("no certificate yet · {} mode · quorum {}/{}", if f["hard"] == true { "hard" } else { "soft" }, f["quorum"], f["activeDelegates"]),
                    None => "legacy node (no SHIP-35 finality)".into(),
                }
            };
            kv(&[
                ("node", api.clone()),
                ("network", format!("{} ({}, address byte {})", c["symbol"].as_str().unwrap_or("?"), c["token"].as_str().unwrap_or("?"), c["version"])),
                ("nethash", c["nethash"].as_str().unwrap_or("?").to_string()),
                ("core", format!("{} {}", c["core"]["implementation"].as_str().unwrap_or("?"), c["core"]["version"].as_str().unwrap_or("?"))),
                ("height", format!("{} (network {}){}", s["now"], s["blocksCount"].as_i64().map(|d| s["now"].as_u64().unwrap_or(0) as i64 - d).unwrap_or(0), if s["syncing"] == true { " — syncing" } else { "" })),
                ("tokens", format!("{}{}", if c["constants"]["tokens"] == true { "active" } else { "dormant" }, if c["constants"]["sobjV2"] == true { " · sobjV2 · market open" } else { "" })),
                ("token init", { let tf = &c["constants"]["tokenFees"]; let pct = tf["initBurnPercent"].as_u64().unwrap_or(50); format!("{} STH · burn {}% ({} STH){}", coins(&tf["init"]), pct, coins(&tf["initBurn"]), if pct == 0 { " — burn disabled" } else { "" }) }),
                ("min core", c["constants"]["minCoreVersion"].as_str().filter(|v| !v.is_empty()).unwrap_or("—").to_string()),
                ("finality", finality),
            ]);
            Ok(())
        }
        Cmd::Wallet { address } => {
            let w = get(&http, &api, &format!("/api/wallets/{address}")).await?["data"].take();
            let t = get(&http, &api, &format!("/api/wallets/{address}/tokens")).await.map(|mut v| v["data"].take()).unwrap_or(Value::Array(vec![]));
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&json!({ "wallet": w, "tokens": t }))?);
                return Ok(());
            }
            let mut rows = vec![("address", address.clone()), ("balance", format!("{} STH", coins(&w["balance"]))), ("nonce", w["nonce"].to_string().trim_matches('"').to_string())];
            if let Some(pk) = w["publicKey"].as_str() {
                rows.push(("public key", pk.to_string()));
            }
            if let Some(d) = w["attributes"]["delegate"]["username"].as_str() {
                rows.push(("delegate", if w["attributes"]["delegate"]["resigned"] == true { format!("{d} (resigned)") } else { d.to_string() }));
            }
            if let Some(v) = w["attributes"]["vote"].as_str() {
                rows.push(("votes for", v.to_string()));
            }
            if w["quantumShield"]["active"] == true {
                rows.push(("quantum shield", format!("ACTIVE · ML-DSA-44 since block {} — every transaction needs --second-passphrase (v3)", w["quantumShield"]["since"])));
            } else if w["quantumShield"]["committed"] == true {
                rows.push(("quantum shield", "committed (stage A) — register with `tx pq-register` once milestone pq is active".into()));
            }
            kv(&rows);
            if let Some(list) = t.as_array().filter(|l| !l.is_empty()) {
                println!("\ntokens");
                for x in list {
                    let d = x["decimals"].as_u64().unwrap_or(0) as u32;
                    let bal: u128 = x["balance"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
                    let scale = 10u128.pow(d);
                    println!("  {:<10} {}{}", x["symbol"].as_str().unwrap_or("?"), bal / scale, if d > 0 { format!(".{:0width$}", bal % scale, width = d as usize) } else { String::new() });
                }
            }
            if let Some(ents) = w["attributes"]["sobjects"].as_object().filter(|e| !e.is_empty()) {
                println!("\nsmart objects");
                for (id, e) in ents {
                    println!("  {:<12} type {}  {}{}", e["data"]["name"].as_str().unwrap_or("—"), e["type"], &id[..16], if e["price"].is_string() { format!("  for sale: {} STH", coins(&e["price"])) } else { String::new() });
                }
            }
            Ok(())
        }
        Cmd::Token { ticker } => {
            let t = get(&http, &api, &format!("/api/tokens/{ticker}")).await?["data"].take();
            let h = get(&http, &api, &format!("/api/tokens/{ticker}/holders")).await.map(|mut v| v["data"].take()).unwrap_or(Value::Array(vec![]));
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&json!({ "token": t, "holders": h }))?);
                return Ok(());
            }
            let d = t["decimals"].as_u64().unwrap_or(0) as u32;
            let unit = |v: &Value| {
                let n: u128 = v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
                let scale = 10u128.pow(d);
                if d == 0 { format!("{n}") } else { format!("{}.{:0width$}", n / scale, n % scale, width = d as usize) }
            };
            let flags: Vec<&str> = [(t["mintable"] == true, "mintable"), (t["burnable"] == true, "burnable")].iter().filter(|(on, _)| *on).map(|(_, n)| *n).collect();
            let mut rows = vec![
                ("token", format!("{} — {}", t["symbol"].as_str().unwrap_or("?"), t["meta"]["name"].as_str().unwrap_or("(no manifest)"))),
                ("id", t["id"].as_str().unwrap_or("?").to_string()),
                ("supply", format!("{} / cap {}", unit(&t["supply"]), unit(&t["supplyCap"]))),
                ("decimals", d.to_string()),
                ("flags", if flags.is_empty() { "fixed supply".into() } else { flags.join(", ") }),
                ("owner", t["owner"].as_str().unwrap_or("?").to_string()),
                ("issued at", t["initHeight"].to_string()),
                ("holders", h.as_array().map(|a| a.len()).unwrap_or(0).to_string()),
            ];
            if let Some(desc) = t["meta"]["description"].as_str() {
                rows.push(("description", desc.to_string()));
            }
            if let Some(site) = t["meta"]["website"].as_str() {
                rows.push(("website", site.to_string()));
            }
            if let Some(l) = t["meta"]["logoUrl"].as_str() {
                rows.push(("logo", format!("{api}{l} ({} bytes {})", t["meta"]["logoSize"], t["meta"]["logoType"].as_str().unwrap_or(""))));
            }
            if t["sobj"]["forSale"] == true {
                rows.push(("for sale", format!("{} STH", coins(&t["sobj"]["price"]))));
            }
            kv(&rows);
            Ok(())
        }
        Cmd::PqCommitment { second_passphrase } => {
            let k = sth_core::crypto::pq::PqKeyPair::from_passphrase(&second_passphrase);
            if cli.json {
                println!("{}", serde_json::json!({ "algorithm": 1, "publicKey": k.public_key_hex(), "commitment": k.commitment() }));
            } else {
                kv(&[("algorithm", "1 (ML-DSA-44)".into()), ("public key", format!("{}…", &k.public_key_hex()[..32])), ("commitment", k.commitment())]);
                println!("\nsth-cli tx transfer --to <your address> --amount 0.00000001 --memo \"{}\"", k.commitment());
            }
            Ok(())
        }
        Cmd::Obj { id, r#type } => {
            let e = if id.len() == 64 {
                get(&http, &api, &format!("/api/sobj/{id}")).await?["data"].take()
            } else {
                let l = get(&http, &api, &format!("/api/sobj?type={type}&name={id}")).await?["data"].take();
                l.as_array().and_then(|a| a.first().cloned()).ok_or_else(|| anyhow!("no sObject named {id} of type {type}"))?
            };
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&e)?);
                return Ok(());
            }
            kv(&[
                ("object", e["data"]["name"].as_str().unwrap_or("—").to_string()),
                ("id", e["id"].as_str().unwrap_or("?").to_string()),
                ("type", format!("{} / sub {}", e["type"], e["subType"])),
                ("owner", e["address"].as_str().unwrap_or("?").to_string()),
                ("pointer", e["data"]["ntfryData"].as_str().unwrap_or("—").to_string()),
                ("status", if e["resigned"] == true { "resigned".into() } else if e["forSale"] == true { format!("for sale: {} STH", coins(&e["price"])) } else { "active".into() }),
            ]);
            Ok(())
        }
        Cmd::Market { r#type, page } => {
            let q = r#type.map(|t| format!("&type={t}")).unwrap_or_default();
            let r = get(&http, &api, &format!("/api/ntfry/market?page={page}&limit=100{q}")).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            let m = &r["meta"];
            println!("open orders: {} (page {} / {}){}", m["totalCount"], m["page"], m["pageCount"], if r["data"]["active"] == true { "" } else { " — market closed (sobjV2 off)" });
            for o in r["data"]["orders"].as_array().into_iter().flatten() {
                println!("  {:>12} STH  {:<12} type {}  {}  {}{}", coins(&o["price"]), o["name"].as_str().unwrap_or("—"), o["type"], o["owner"].as_str().unwrap_or("?"), &o["id"].as_str().unwrap_or("")[..16], if o["token"].is_object() { "  [token]" } else { "" });
            }
            Ok(())
        }
        Cmd::CreateWallet { passphrase } => {
            let c = cli::configuration(&http, &api).await?;
            let pubkey_hash = c["version"].as_u64().ok_or_else(|| anyhow!("node configuration without address byte"))? as u8;
            let generated = passphrase.is_none();
            let phrase = match passphrase { Some(p) => p, None => sth_core::crypto::mnemonic::generate_mnemonic()? };
            let keys = sth_core::crypto::KeyPair::from_passphrase(&phrase)?;
            let address = keys.address(pubkey_hash)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&json!({ "network": c["symbol"], "addressByte": pubkey_hash, "address": address, "publicKey": keys.public_key_hex(), "passphrase": phrase, "bip39": sth_core::crypto::mnemonic::validate_mnemonic(&phrase) }))?);
                return Ok(());
            }
            kv(&[
                ("network", format!("{} (address byte {pubkey_hash}, {})", c["symbol"].as_str().unwrap_or("?"), c["nethash"].as_str().map(|n| &n[..12]).unwrap_or("?"))),
                ("address", address),
                ("public key", keys.public_key_hex()),
                ("passphrase", phrase.clone()),
            ]);
            if generated {
                println!("\nWrite the 12 words down and keep them offline — they are the only key to this wallet. Nothing was sent to the node.");
            } else if !sth_core::crypto::mnemonic::validate_mnemonic(&phrase) {
                println!("\nnote: the passphrase is not a BIP-39 mnemonic (fine for SmartHoldem, but not importable as a seed phrase elsewhere)");
            }
            Ok(())
        }
        Cmd::Watch { address, every, tail } => {
            let path = format!("/api/wallets/{address}/transactions?limit={}&orderBy=timestamp:desc", tail.max(20));
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut first = true;
            eprintln!("watching {address} on {api} (every {every}s, Ctrl-C to stop)");
            loop {
                // a wallet that has not received anything yet is a 404 — keep waiting for its first transaction
                let fetched = match get(&http, &api, &path).await {
                    Err(e) if e.to_string().starts_with("not found") => Ok(json!({ "data": [] })),
                    other => other,
                };
                match fetched {
                    Ok(v) => {
                        let mut list: Vec<Value> = v["data"].as_array().cloned().unwrap_or_default();
                        list.reverse();
                        let skip = if first { list.len().saturating_sub(tail) } else { 0 };
                        for t in list.into_iter().skip(skip) {
                            let id = t["id"].as_str().unwrap_or("").to_string();
                            if !seen.insert(id.clone()) {
                                continue;
                            }
                            if cli.json {
                                println!("{}", serde_json::to_string(&t)?);
                                continue;
                            }
                            let when = t["timestamp"]["human"].as_str().map(|h| h.replace('T', " ").chars().take(19).collect::<String>()).unwrap_or_default();
                            let conf = t["confirmations"].as_u64().unwrap_or(0);
                            let mark = if conf == 0 { "pool ".to_string() } else if t["finalized"] == true { "final".to_string() } else { format!("✓{conf:<4}") };
                            println!("{when}  {mark}  {}  [{}…]", describe(&t, &address), &id[..12]);
                        }
                        first = false;
                    }
                    Err(e) => eprintln!("… {e}"),
                }
                tokio::time::sleep(Duration::from_secs(every.max(1))).await;
            }
        }
        Cmd::TxStatus { id, wait } => {
            let mut tries = 0u32;
            loop {
                match get(&http, &api, &format!("/api/transactions/{id}")).await {
                    Ok(v) if wait && tries < 60 && v["data"]["blockId"].as_str().is_none_or(str::is_empty) => {
                        // still in the pool
                        tries += 1;
                        if !cli.json {
                            eprint!("\rin the pool, waiting for a block… {tries}s ");
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Ok(v) => {
                        let t = &v["data"];
                        if cli.json {
                            println!("{}", serde_json::to_string_pretty(t)?);
                        } else {
                            kv(&[
                                ("tx", id.clone()),
                                ("type", format!("group {} type {}", t["typeGroup"], t["type"])),
                                ("block", if t["blockId"].as_str().is_some_and(|b| !b.is_empty()) {
                                    format!("{} ({})", t["blockId"].as_str().unwrap_or("?"), if t["finalized"] == true { "final — covered by a SHIP-35 finality certificate".to_string() } else { format!("{} confirmations, not final yet", t["confirmations"]) })
                                } else { "unconfirmed (in the pool)".into() }),
                                ("sender", t["sender"].as_str().unwrap_or("?").to_string()),
                                ("amount", format!("{} STH (fee {})", coins(&t["amount"]), coins(&t["fee"]))),
                            ]);
                        }
                        return Ok(());
                    }
                    Err(e) if wait && tries < 60 => {
                        tries += 1;
                        if !cli.json {
                            eprint!("\rwaiting for confirmation… {tries}s ");
                        }
                        let _ = e;
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }
}
