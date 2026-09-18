//! Author: TechnoL0g
//!
//! Transaction builder / signer shared by `sth-cli tx …` and `sth-core tx …`: builds, signs and posts transactions through a
//! node's REST API. The passphrase never leaves this process; the node only receives the signed JSON, exactly like a wallet.
//! Chain parameters (address byte, fees, blocktime) come either from the node's own config or from `GET /api/node/configuration`.

use crate::config::TokenFees;
use crate::crypto::pq::PqKeyPair;
use crate::node_config::DynamicFeesConfig;
use crate::crypto::{sign_schnorr_legacy, transaction_id, transaction_pq_message, transaction_signing_hash, KeyPair};
use crate::models::{sobj, token, PqSignatureBlock, Transaction, TokenMeta, VERSION_PQ};
use crate::node_config::NodeConfig;
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Where the chain parameters come from.
pub enum ChainSource {
    /// The node's own `node.yaml` (`sth-core tx`): network files on disk, API on 127.0.0.1:<api.port>.
    Config(NodeConfig),
    /// A remote node (`sth-cli`): everything from `GET /api/node/configuration`.
    Api(String),
}

/// What the signer needs to know about the chain.
pub struct ChainParams {
    pub pubkey_hash: u8,
    pub transfer_fee: u64,
    pub vote_fee: u64,
    pub delegate_registration_fee: u64,
    pub delegate_resignation_fee: u64,
    pub second_signature_fee: u64,
    pub token_fees: TokenFees,
    pub blocktime: u64,
    /// Quantum Shield stage B: v3 accepted, surcharge per byte of second-signature blocks.
    pub pq_active: bool,
    pub pq_fee_per_byte: u64,
    /// SHIP-41: the node's dynamic fee policy (`transactionPool.dynamicFees`) when enabled; None = static fees.
    pub dynamic_fees: Option<DynamicFeesConfig>,
}

/// Fee margin (SHIP-41 recommendation): round the minimum up to a multiple of 0.001 STH.
const FEE_STEP: u64 = 100_000;

/// SHIP-41 minimum for a core v2 transaction of `bytes` on this node, rounded up to [`FEE_STEP`].
pub fn dynamic_fee(df: &DynamicFeesConfig, type_name: &str, bytes: usize) -> u64 {
    let min = df.min_fee(type_name, bytes, df.min_fee_pool.max(df.min_fee_broadcast));
    min.div_ceil(FEE_STEP) * FEE_STEP
}

impl ChainParams {
    pub fn from_configuration(v: &Value) -> Result<Self> {
        let c = &v["constants"];
        let num = |x: &Value| x.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| x.as_u64()).unwrap_or(0);
        let pubkey_hash = v["version"].as_u64().ok_or_else(|| anyhow!("configuration without network version (pubKeyHash)"))? as u8;
        let tf = &c["tokenFees"];
        Ok(Self {
            pubkey_hash,
            transfer_fee: num(&c["fees"]["staticFees"]["transfer"]),
            vote_fee: num(&c["fees"]["staticFees"]["vote"]),
            delegate_registration_fee: num(&c["fees"]["staticFees"]["delegateRegistration"]),
            delegate_resignation_fee: num(&c["fees"]["staticFees"]["delegateResignation"]),
            second_signature_fee: num(&c["fees"]["staticFees"]["secondSignature"]),
            pq_active: v["pq"]["active"] == true,
            pq_fee_per_byte: num(&v["pq"]["feePerByte"]),
            dynamic_fees: {
                let d = &v["transactionPool"]["dynamicFees"];
                (d["enabled"] == true).then(|| DynamicFeesConfig {
                    enabled: true,
                    min_fee_pool: d["minFeePool"].as_u64().unwrap_or(3_000),
                    min_fee_broadcast: d["minFeeBroadcast"].as_u64().unwrap_or(3_000),
                    addon_bytes: d["addonBytes"].as_object().map(|m| m.iter().filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n))).collect()).unwrap_or_default(),
                })
            },
            token_fees: TokenFees { init: num(&tf["init"]), transfer: num(&tf["transfer"]), transfer_per_recipient: num(&tf["transferPerRecipient"]), mint: num(&tf["mint"]), burn: num(&tf["burn"]), meta: num(&tf["meta"]), init_burn_percent: tf["initBurnPercent"].as_u64().unwrap_or(50) as u8 },
            blocktime: c["blocktime"].as_u64().unwrap_or(8),
        })
    }
}

#[derive(Args)]
pub struct TxArgs {
    /// Sender passphrase (or set STH_PASSPHRASE).
    #[arg(long, env = "STH_PASSPHRASE", hide_env_values = true)]
    passphrase: String,
    /// Local REST API (default: http://127.0.0.1:<api.port from node.yaml>).
    #[arg(long)]
    api: Option<String>,
    /// Second passphrase: legacy second signature, or the ML-DSA key of a PQ-locked wallet (Quantum Shield). For
    /// `pq-register` this is the NEW PQ passphrase.
    #[arg(long, env = "STH_SECOND_PASSPHRASE", hide_env_values = true)]
    second_passphrase: Option<String>,
    /// Explicit nonce (default: wallet nonce from the API + 1).
    #[arg(long)]
    nonce: Option<u64>,
    /// Print the signed transaction instead of posting it.
    #[arg(long)]
    dry_run: bool,
    /// Explicit fee in STH for core transactions (default: the node's dynamic minimum per SHIP-41 rounded up to 0.001 STH,
    /// or the static fee when the node runs with dynamic fees disabled).
    #[arg(long)]
    fee: Option<String>,
    #[command(subcommand)]
    cmd: TxCmd,
}

#[derive(Subcommand)]
pub enum TxCmd {
    /// Send coins (amount in whole coins, e.g. 12.5).
    Transfer {
        #[arg(long)]
        to: String,
        #[arg(long)]
        amount: String,
        #[arg(long)]
        memo: Option<String>,
    },
    /// Vote for a delegate (username or public key). A wallet voting for someone else switches in the same transaction.
    Vote {
        #[arg(long)]
        delegate: String,
    },
    /// Withdraw the wallet's current vote.
    Unvote,
    /// Register this wallet as a delegate (username: 1–20 chars of a-z 0-9 ! @ $ & _ .). Then put the passphrase into
    /// node.yaml → delegate.secrets and restart the node to forge.
    DelegateRegister {
        #[arg(long)]
        username: String,
    },
    /// Resign as a delegate (permanent: the username stays taken, the wallet never forges again).
    DelegateResign,
    /// Quantum Shield: register (or rotate to) the ML-DSA-44 key derived from --second-passphrase (v3, needs milestone pq).
    PqRegister {
        /// Current second passphrase (legacy second signature or the PQ key being replaced) — proof of the old key.
        #[arg(long, env = "STH_OLD_SECOND_PASSPHRASE", hide_env_values = true)]
        old_second_passphrase: Option<String>,
    },
    /// Register a SmartObject (sObject); type 5 = token ticker.
    ObjRegister {
        #[arg(long, default_value_t = token::TICKER_SOBJ_TYPE)]
        r#type: u8,
        #[arg(long, default_value_t = 0)]
        sub_type: u8,
        #[arg(long)]
        name: String,
        /// Pointer string stored in the chain (`ntfryData`).
        #[arg(long)]
        data: Option<String>,
    },
    /// Update the pointer string of an sObject you own.
    ObjUpdate {
        /// Registration id (64 hex) or type-5 ticker.
        #[arg(long)]
        id: String,
        #[arg(long)]
        data: String,
    },
    /// Hand an sObject you own (with its token registry) to another address (needs milestone sobjV2).
    ObjTransfer {
        /// Registration id (64 hex) or type-5 ticker.
        #[arg(long)]
        id: String,
        #[arg(long)]
        to: String,
    },
    /// Put an sObject you own up for sale (price in whole coins; 0 cancels the order).
    ObjSell {
        #[arg(long)]
        id: String,
        #[arg(long)]
        price: String,
    },
    /// Buy an sObject with an open sale order (pays the recorded price to the owner).
    ObjBuy {
        #[arg(long)]
        id: String,
    },
    /// Initialize a token on a type-5 sObject you own (supply / cap in whole tokens).
    TokenInit {
        /// Ticker (type-5 sObject name) or its registration id.
        #[arg(long)]
        ticker: String,
        #[arg(long, default_value_t = 0)]
        decimals: u8,
        #[arg(long)]
        supply: String,
        /// Defaults to supply (fixed supply).
        #[arg(long)]
        cap: Option<String>,
        #[arg(long)]
        mintable: bool,
        #[arg(long)]
        burnable: bool,
    },
    /// Transfer tokens: --to ADDRESS:AMOUNT (repeatable, up to 64).
    TokenTransfer {
        #[arg(long)]
        ticker: String,
        #[arg(long = "to", required = true)]
        to: Vec<String>,
        #[arg(long)]
        memo: Option<String>,
    },
    TokenMint {
        #[arg(long)]
        ticker: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        amount: String,
    },
    TokenBurn {
        #[arg(long)]
        ticker: String,
        #[arg(long)]
        amount: String,
    },
    /// Publish / update the on-chain manifest (name, description, website, SVG or PNG logo ≤ 8 KB).
    TokenMeta {
        #[arg(long)]
        ticker: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        website: Option<String>,
        /// Path to a .svg or .png file.
        #[arg(long)]
        logo: Option<PathBuf>,
    },
}

#[derive(Clone)]
pub struct Ctx {
    pub api: String,
    pub http: reqwest::Client,
    pub keys: KeyPair,
    pub address: String,
}

impl Ctx {
    async fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.api, path);
        let r = self.http.get(&url).send().await.with_context(|| format!("GET {url}"))?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Value::Null);
        }
        Ok(r.error_for_status()?.json::<Value>().await?["data"].take())
    }


    /// Type-5 registration id from a ticker (or pass-through for a 64-hex id).
    async fn sobj_id(&self, key: &str) -> Result<String> {
        if key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(key.to_lowercase());
        }
        let list = self.get(&format!("/api/sobj?type=5&name={}", key.to_uppercase())).await?;
        list.as_array().and_then(|a| a.first()).and_then(|e| e["id"].as_str()).map(str::to_string).ok_or_else(|| anyhow!("no type-5 sObject named {key} — register it first (obj-register --name {})", key.to_uppercase()))
    }

    /// (id, decimals) of an initialized token.
    async fn token(&self, key: &str) -> Result<(String, u8)> {
        let t = self.get(&format!("/api/tokens/{key}")).await?;
        let id = t["id"].as_str().ok_or_else(|| anyhow!("token {key} is not initialized (token-init first)"))?;
        Ok((id.to_string(), t["decimals"].as_u64().unwrap_or(0) as u8))
    }

    async fn next_nonce(&self) -> Result<u64> {
        let w = self.get(&format!("/api/wallets/{}", self.address)).await?;
        Ok(w["nonce"].as_str().and_then(|n| n.parse::<u64>().ok()).or_else(|| w["nonce"].as_u64()).unwrap_or(0) + 1)
    }
}

/// "12.5" with `decimals` → integer units.
fn units(s: &str, decimals: u8) -> Result<u64> {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if frac.len() > decimals as usize {
        bail!("{s}: more than {decimals} decimal places");
    }
    if int.is_empty() && frac.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        bail!("{s}: not a number");
    }
    let scale = 10u64.pow(decimals as u32);
    let int: u64 = if int.is_empty() { 0 } else { int.parse()? };
    let frac: u64 = if frac.is_empty() { 0 } else { format!("{frac:0<width$}", width = decimals as usize).parse()? };
    int.checked_mul(scale).and_then(|v| v.checked_add(frac)).ok_or_else(|| anyhow!("{s}: amount overflows"))
}

fn sign(mut t: Transaction, keys: &KeyPair) -> Result<Transaction> {
    let h = transaction_signing_hash(&t)?;
    t.signature = Some(sign_schnorr_legacy(&h, keys.private_key())?);
    t.id = Some(transaction_id(&t)?);
    Ok(t)
}

/// Second-signature strategy of the sender.
enum SecondSigner {
    None,
    /// v2 legacy second signature (secp256k1).
    Legacy(KeyPair),
    /// v3: one ML-DSA block of the registered algorithm.
    Pq(PqKeyPair, u8),
    /// v3 registration: proof of the current key (if any) + signature of the new key.
    Register { proof: Box<SecondSigner>, new: PqKeyPair },
}

/// v3 block sizes are fixed, so the surcharge is known before signing: 3 + 64 (alg 0) / 3 + 2420 (alg 1) bytes per block.
fn pq_block_bytes(s: &SecondSigner) -> u64 {
    match s {
        SecondSigner::None => 0,
        SecondSigner::Legacy(_) => 3 + 64,
        SecondSigner::Pq(..) => 3 + crate::crypto::pq::SIG_LEN as u64,
        SecondSigner::Register { proof, .. } => pq_block_bytes(proof) + 3 + crate::crypto::pq::SIG_LEN as u64,
    }
}

fn pq_block(s: &SecondSigner, m2: &[u8; 32]) -> Result<Vec<PqSignatureBlock>> {
    Ok(match s {
        SecondSigner::None => vec![],
        SecondSigner::Legacy(k) => vec![PqSignatureBlock { algorithm: 0, signature: sign_schnorr_legacy(m2, k.private_key())? }],
        SecondSigner::Pq(k, alg) => vec![PqSignatureBlock { algorithm: *alg, signature: hex::encode(k.sign(m2)?) }],
        SecondSigner::Register { proof, new } => {
            let mut v = pq_block(proof, m2)?;
            v.push(PqSignatureBlock { algorithm: crate::crypto::pq::ALG_ML_DSA_44, signature: hex::encode(new.sign(m2)?) });
            v
        }
    })
}

/// First signature + second signature (legacy v2 or v3 blocks with the PQ surcharge added to the fee).
fn sign_with_second(mut t: Transaction, keys: &KeyPair, second: &SecondSigner, fee_per_byte: u64) -> Result<Transaction> {
    match second {
        SecondSigner::None => return sign(t, keys),
        SecondSigner::Legacy(k) if !t.is_pq() => {
            let h = transaction_signing_hash(&t)?;
            t.signature = Some(sign_schnorr_legacy(&h, keys.private_key())?);
            let m2 = transaction_pq_message(&t)?;
            t.second_signature = Some(sign_schnorr_legacy(&m2, k.private_key())?);
            t.id = Some(transaction_id(&t)?);
            return Ok(t);
        }
        _ => {}
    }
    t.version = VERSION_PQ;
    t.fee += fee_per_byte * pq_block_bytes(second);
    if let SecondSigner::Register { new, .. } = second {
        if let Some(a) = t.asset.as_mut().and_then(|a| a.signature.as_mut()) {
            a.public_key = new.public_key_hex();
            a.algorithm = Some(crate::crypto::pq::ALG_ML_DSA_44);
        }
    }
    let h = transaction_signing_hash(&t)?;
    t.signature = Some(sign_schnorr_legacy(&h, keys.private_key())?);
    let m2 = transaction_pq_message(&t)?;
    t.second_signatures = Some(pq_block(second, &m2)?);
    t.id = Some(transaction_id(&t)?);
    Ok(t)
}

/// GET `<api>/api/node/configuration` → `data`.
pub async fn configuration(http: &reqwest::Client, api: &str) -> Result<Value> {
    let url = format!("{api}/api/node/configuration");
    Ok(http.get(&url).send().await.with_context(|| format!("GET {url}"))?.error_for_status()?.json::<Value>().await?["data"].take())
}

pub async fn run(source: ChainSource, args: TxArgs) -> Result<()> {
    let http = reqwest::Client::new();
    let (api, params) = match &source {
        ChainSource::Config(cfg) => {
            let api = args.api.clone().unwrap_or_else(|| format!("http://127.0.0.1:{}", cfg.api.port)).trim_end_matches('/').to_string();
            let network = cfg.load_network()?;
            let height = http.get(format!("{api}/api/node/status")).send().await.ok().and_then(|r| r.error_for_status().ok());
            let height = match height { Some(r) => r.json::<Value>().await.ok().and_then(|v| v["data"]["now"].as_u64()).unwrap_or(0), None => 0 };
            let ms = network.milestone(height + 1);
            let dynamic_fees = cfg.mempool.dynamic_fees.enabled.then(|| cfg.mempool.dynamic_fees.clone());
            (api, ChainParams { pubkey_hash: network.pubkey_hash, transfer_fee: ms.static_fee("transfer"), vote_fee: ms.static_fee("vote"), delegate_registration_fee: ms.static_fee("delegateRegistration"), delegate_resignation_fee: ms.static_fee("delegateResignation"), second_signature_fee: ms.static_fee("secondSignature"), token_fees: ms.token_fees.clone(), blocktime: ms.blocktime as u64, pq_active: ms.pq.active, pq_fee_per_byte: ms.pq.fee_per_byte, dynamic_fees })
        }
        ChainSource::Api(api) => {
            let api = args.api.clone().unwrap_or_else(|| api.clone()).trim_end_matches('/').to_string();
            let params = ChainParams::from_configuration(&configuration(&http, &api).await?)?;
            (api, params)
        }
    };
    let keys = KeyPair::from_passphrase(&args.passphrase)?;
    let address = keys.address(params.pubkey_hash)?;
    let ctx = Ctx { api, http, keys, address };
    let ms = &params;
    let net = params.pubkey_hash;

    let mut body = json!({ "version": 2, "network": net, "typeGroup": 1, "type": 0, "senderPublicKey": ctx.keys.public_key_hex(), "amount": "0", "expiration": 0 });
    let set = |body: &mut Value, extra: Value| body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    // how the sender's second key is currently set up (decides v2 vs v3 and which proof blocks a registration needs)
    let wallet = ctx.get(&format!("/api/wallets/{}", ctx.address)).await?;
    let legacy_second_pk = wallet["secondPublicKey"].as_str().map(str::to_string);
    let pq_locked = wallet["quantumShield"]["active"] == true;
    let pq_alg = wallet["quantumShield"]["algorithm"].as_u64().unwrap_or(1) as u8;
    let mut signer = SecondSigner::None;
    if let Some(sp) = &args.second_passphrase {
        signer = if matches!(args.cmd, TxCmd::PqRegister { .. }) {
            SecondSigner::None
        } else if pq_locked {
            SecondSigner::Pq(PqKeyPair::from_passphrase(sp), pq_alg)
        } else if legacy_second_pk.is_some() {
            SecondSigner::Legacy(KeyPair::from_passphrase(sp)?)
        } else {
            bail!("wallet {} has no second signature — --second-passphrase is not needed", ctx.address);
        };
    } else if pq_locked || legacy_second_pk.is_some() {
        bail!("wallet {} is protected by a second signature — pass --second-passphrase (or STH_SECOND_PASSPHRASE)", ctx.address);
    }
    match &args.cmd {
        TxCmd::PqRegister { old_second_passphrase } => {
            if !ms.pq_active {
                bail!("Quantum Shield stage B is not active on this network (milestone pq.active)");
            }
            let new = PqKeyPair::from_passphrase(args.second_passphrase.as_deref().ok_or_else(|| anyhow!("pq-register needs --second-passphrase (the new PQ passphrase)"))?);
            let proof = match (pq_locked, &legacy_second_pk, old_second_passphrase) {
                (true, _, Some(old)) => SecondSigner::Pq(PqKeyPair::from_passphrase(old), pq_alg),
                (false, Some(_), Some(old)) => SecondSigner::Legacy(KeyPair::from_passphrase(old)?),
                (false, None, _) => SecondSigner::None,
                _ => bail!("wallet already has a second key — pass --old-second-passphrase to prove it"),
            };
            signer = SecondSigner::Register { proof: Box::new(proof), new };
            set(&mut body, json!({ "version": VERSION_PQ, "type": 1, "fee": ms.second_signature_fee.to_string(), "asset": { "signature": { "algorithm": 1, "publicKey": "" } } }));
        }
        TxCmd::Vote { delegate } => {
            let d = ctx.get(&format!("/api/delegates/{delegate}")).await?;
            let pk = d["publicKey"].as_str().ok_or_else(|| anyhow!("delegate {delegate} not found (pass a username or a public key)"))?.to_string();
            let current = ctx.get(&format!("/api/wallets/{}", ctx.address)).await.ok().and_then(|w| w["attributes"]["vote"].as_str().map(str::to_string));
            let mut votes = Vec::new();
            match current.as_deref() {
                Some(c) if c == pk => bail!("wallet {} already votes for {} ({})", ctx.address, d["username"].as_str().unwrap_or(&pk), pk),
                Some(c) => {
                    println!("switch  unvote {c} in the same transaction");
                    votes.push(format!("-{c}"));
                }
                None => {}
            }
            votes.push(format!("+{pk}"));
            println!("vote    {} ({pk})", d["username"].as_str().unwrap_or("?"));
            set(&mut body, json!({ "type": 3, "fee": ms.vote_fee.to_string(), "asset": { "votes": votes } }));
        }
        TxCmd::DelegateRegister { username } => {
            let ok = (1..=20).contains(&username.len()) && username.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"!@$&_.".contains(&b));
            if !ok {
                bail!("username must be 1–20 characters of a-z 0-9 ! @ $ & _ . (lowercase)");
            }
            let w = ctx.get(&format!("/api/wallets/{}", ctx.address)).await?;
            if let Some(u) = w["attributes"]["delegate"]["username"].as_str() {
                bail!("wallet {} is already the delegate {u}", ctx.address);
            }
            if !ctx.get(&format!("/api/delegates/{username}")).await?.is_null() {
                bail!("username {username} is already taken");
            }
            println!("delegate  {username} (public key {})", ctx.keys.public_key_hex());
            set(&mut body, json!({ "type": 2, "fee": ms.delegate_registration_fee.to_string(), "asset": { "delegate": { "username": username } } }));
        }
        TxCmd::DelegateResign => {
            let w = ctx.get(&format!("/api/wallets/{}", ctx.address)).await?;
            let d = &w["attributes"]["delegate"];
            let Some(u) = d["username"].as_str() else { bail!("wallet {} is not a delegate", ctx.address) };
            if d["resigned"] == true {
                bail!("delegate {u} has already resigned");
            }
            println!("resign  {u} — permanent, the wallet will never forge again");
            set(&mut body, json!({ "type": 7, "fee": ms.delegate_resignation_fee.to_string() }));
        }
        TxCmd::Unvote => {
            let w = ctx.get(&format!("/api/wallets/{}", ctx.address)).await?;
            let current = w["attributes"]["vote"].as_str().ok_or_else(|| anyhow!("wallet {} does not vote for anyone", ctx.address))?.to_string();
            let name = ctx.get(&format!("/api/delegates/{current}")).await.ok().and_then(|d| d["username"].as_str().map(str::to_string)).unwrap_or_default();
            println!("unvote  {name} ({current})");
            set(&mut body, json!({ "type": 3, "fee": ms.vote_fee.to_string(), "asset": { "votes": [format!("-{current}")] } }));
        }
        TxCmd::Transfer { to, amount, memo } => {
            set(&mut body, json!({ "fee": ms.transfer_fee.to_string(), "amount": units(amount, 8)?.to_string(), "recipientId": to }));
            if let Some(m) = memo {
                set(&mut body, json!({ "vendorField": m }));
            }
        }
        TxCmd::ObjRegister { r#type, sub_type, name, data } => {
            let mut d = json!({ "name": name });
            if let Some(x) = data {
                d["ntfryData"] = json!(x);
            }
            set(&mut body, json!({ "typeGroup": 2, "type": sobj::TYPE, "fee": sobj::FEE_REGISTER.to_string(), "asset": { "type": r#type, "subType": sub_type, "action": sobj::ACTION_REGISTER, "data": d } }));
        }
        TxCmd::ObjUpdate { id, data } => {
            let reg = ctx.sobj_id(id).await?;
            let e = ctx.get(&format!("/api/sobj/{reg}")).await?;
            set(&mut body, json!({ "typeGroup": 2, "type": sobj::TYPE, "fee": sobj::FEE_UPDATE.to_string(), "asset": { "type": e["type"], "subType": e["subType"], "action": sobj::ACTION_UPDATE, "registrationId": reg, "data": { "ntfryData": data } } }));
        }
        TxCmd::ObjTransfer { id, to } => {
            let reg = ctx.sobj_id(id).await?;
            let e = ctx.get(&format!("/api/sobj/{reg}")).await?;
            set(&mut body, json!({ "typeGroup": 2, "type": sobj::TYPE, "fee": sobj::FEE_TRANSFER.to_string(), "asset": { "type": e["type"], "subType": e["subType"], "action": sobj::ACTION_TRANSFER, "registrationId": reg, "recipientId": to, "data": {} } }));
        }
        TxCmd::ObjSell { id, price } => {
            let reg = ctx.sobj_id(id).await?;
            let e = ctx.get(&format!("/api/sobj/{reg}")).await?;
            set(&mut body, json!({ "typeGroup": 2, "type": sobj::TYPE, "fee": sobj::FEE_SELL.to_string(), "asset": { "type": e["type"], "subType": e["subType"], "action": sobj::ACTION_SELL, "registrationId": reg, "price": units(price, 8)?.to_string(), "data": {} } }));
        }
        TxCmd::ObjBuy { id } => {
            let reg = ctx.sobj_id(id).await?;
            let e = ctx.get(&format!("/api/sobj/{reg}")).await?;
            let price = e["price"].as_str().ok_or_else(|| anyhow!("sObject {id} has no open sale order"))?;
            println!("price   {} coins → {}", units(price, 0)? as f64 / 1e8, e["address"].as_str().unwrap_or("?"));
            set(&mut body, json!({ "typeGroup": 2, "type": sobj::TYPE, "fee": sobj::FEE_BUY.to_string(), "asset": { "type": e["type"], "subType": e["subType"], "action": sobj::ACTION_BUY, "registrationId": reg, "data": {} } }));
        }
        TxCmd::TokenInit { ticker, decimals, supply, cap, mintable, burnable } => {
            let id = ctx.sobj_id(ticker).await?;
            let supply_u = units(supply, *decimals)?;
            let cap_u = match cap { Some(c) => units(c, *decimals)?, None => supply_u };
            let flags = if *mintable { token::FLAG_MINTABLE } else { 0 } | if *burnable { token::FLAG_BURNABLE } else { 0 };
            set(&mut body, json!({ "typeGroup": 3, "type": token::INIT, "fee": ms.token_fees.init.to_string(), "asset": { "token": { "id": id, "decimals": decimals, "flags": flags, "initialSupply": supply_u.to_string(), "supplyCap": cap_u.to_string() } } }));
        }
        TxCmd::TokenTransfer { ticker, to, memo } => {
            let (id, dec) = ctx.token(ticker).await?;
            let mut transfers = Vec::new();
            for item in to {
                let (addr, amt) = item.rsplit_once(':').ok_or_else(|| anyhow!("--to expects ADDRESS:AMOUNT, got {item}"))?;
                transfers.push(json!({ "recipientId": addr, "amount": units(amt, dec)?.to_string() }));
            }
            let fee = ms.token_fees.transfer + ms.token_fees.transfer_per_recipient * (transfers.len() as u64 - 1);
            let mut asset = json!({ "id": id, "transfers": transfers });
            if let Some(m) = memo {
                asset["memo"] = json!(m);
            }
            set(&mut body, json!({ "typeGroup": 3, "type": token::TRANSFER, "fee": fee.to_string(), "asset": { "token": asset } }));
        }
        TxCmd::TokenMint { ticker, to, amount } => {
            let (id, dec) = ctx.token(ticker).await?;
            set(&mut body, json!({ "typeGroup": 3, "type": token::MINT, "fee": ms.token_fees.mint.to_string(), "asset": { "token": { "id": id, "amount": units(amount, dec)?.to_string(), "recipientId": to } } }));
        }
        TxCmd::TokenBurn { ticker, amount } => {
            let (id, dec) = ctx.token(ticker).await?;
            set(&mut body, json!({ "typeGroup": 3, "type": token::BURN, "fee": ms.token_fees.burn.to_string(), "asset": { "token": { "id": id, "amount": units(amount, dec)?.to_string() } } }));
        }
        TxCmd::TokenMeta { ticker, name, description, website, logo } => {
            let (id, _) = ctx.token(ticker).await?;
            let mut meta = TokenMeta { name: name.clone(), description: description.clone(), website: website.clone(), logo_type: None, logo: None };
            if let Some(path) = logo {
                use base64::Engine;
                let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
                let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
                if ext != "svg" && ext != "png" {
                    bail!("logo must be a .svg or .png file");
                }
                if bytes.len() > token::META_MAX_LOGO {
                    bail!("logo is {} bytes, limit {}", bytes.len(), token::META_MAX_LOGO);
                }
                meta.logo_type = Some(ext);
                meta.logo = Some(base64::engine::general_purpose::STANDARD.encode(bytes));
            }
            crate::rules::check_token_meta(&meta).map_err(|e| anyhow!(e))?;
            set(&mut body, json!({ "typeGroup": 3, "type": token::META, "fee": ms.token_fees.meta.to_string(), "asset": { "token": { "id": id, "meta": meta } } }));
        }
    }
    let mut nonce = match args.nonce { Some(n) => n, None => ctx.next_nonce().await? };
    println!("sender  {}", ctx.address);
    // SHIP-41: core v2 transactions pay the node's dynamic minimum (size-based) instead of the static fee
    let core_v2 = body["typeGroup"] == 1 && body["version"] == 2 && !matches!(signer, SecondSigner::Register { .. });
    if let Some(fee) = &args.fee {
        if body["typeGroup"] != 1 {
            bail!("--fee applies to core transactions only; sObject and token fees are fixed by the chain");
        }
        body["fee"] = json!(units(fee, 8)?.to_string());
        println!("fee     {} STH (explicit)", fee);
    } else if let (true, Some(df)) = (core_v2, &params.dynamic_fees) {
        let mut probe = body.clone();
        probe["nonce"] = json!(nonce.to_string());
        let probe = sign_with_second(serde_json::from_value(probe)?, &ctx.keys, &signer, 0)?;
        let bytes = crate::crypto::serialize_transaction(&probe, crate::crypto::SerializeOptions::default(), crate::config::Network::mainnet_ref()).map(|b| b.len())?;
        let type_name = crate::config::FEE_NAMES.iter().find(|(_, t)| *t as u64 == body["type"].as_u64().unwrap_or(0)).map(|(n, _)| *n).unwrap_or("");
        let fee = dynamic_fee(df, type_name, bytes);
        let addon = df.addon_bytes.get(type_name).copied().unwrap_or(0);
        println!("fee     {} STH (dynamic, SHIP-41: ({addon} + {bytes} B) × {} → min {} STH, rounded up)", coins_str(fee), df.min_fee_pool.max(df.min_fee_broadcast), coins_str(df.min_fee(type_name, bytes, df.min_fee_pool.max(df.min_fee_broadcast))));
        body["fee"] = json!(fee.to_string());
    } else if core_v2 {
        println!("fee     {} STH (static — node runs without dynamic fees)", coins_str(body["fee"].as_str().and_then(|f| f.parse().ok()).unwrap_or(0)));
    }
    let url = format!("{}/api/transactions", ctx.api);
    for attempt in 0..2 {
        let mut b = body.clone();
        b["nonce"] = json!(nonce.to_string());
        let tx = sign_with_second(serde_json::from_value(b)?, &ctx.keys, &signer, ms.pq_fee_per_byte)?;
        if tx.is_pq() {
            println!("version 3 (Quantum Shield) · fee {} incl. PQ surcharge", tx.fee);
        }
        println!("tx id   {}", tx.id.as_deref().unwrap_or("?"));
        if args.dry_run {
            println!("{}", serde_json::to_string_pretty(&tx)?);
            return Ok(());
        }
        let resp: Value = ctx.http.post(&url).json(&json!({ "transactions": [tx] })).send().await.with_context(|| format!("POST {url}"))?.json().await?;
        if resp["data"]["accept"].as_array().is_some_and(|a| !a.is_empty()) {
            println!("{}", serde_json::to_string_pretty(&resp)?);
            break;
        }
        // the wallet nonce lags while earlier transactions sit in the pool — the node tells us the expected one
        let expected = resp["errors"].as_object().and_then(|e| e.values().next()).and_then(|e| e["message"].as_str()).and_then(|m| m.rsplit_once("expected ").and_then(|(_, n)| n.trim_end_matches('"').parse::<u64>().ok()));
        match expected {
            Some(n) if attempt == 0 && args.nonce.is_none() => {
                println!("nonce   {nonce} → {n} (pending transactions in the pool), retrying");
                nonce = n;
            }
            _ => {
                println!("{}", serde_json::to_string_pretty(&resp)?);
                bail!("transaction was not accepted by the node");
            }
        }
    }
    println!("accepted — included in the next block (≈ {} s)", ms.blocktime);
    if let TxCmd::DelegateRegister { username } = &args.cmd {
        println!("next    add the passphrase to node.yaml → delegate: {{ enabled: true, secrets: [\"…\"] }} and restart sth-core; `sth-cli status` / the metrics page show when {username} enters the active set");
    }
    Ok(())
}

fn coins_str(smartoshi: u64) -> String {
    let s = format!("{:.8}", smartoshi as f64 / 1e8);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `sth-cli delegate-setup`: wallet → funding → delegate registration → PQ key → node.yaml, in one pass.
#[derive(Args)]
pub struct DelegateSetupArgs {
    /// Delegate username (a-z 0-9 ! @ $ & _ ., 1–20 chars).
    #[arg(long)]
    pub username: String,
    /// Existing wallet passphrase (default: a fresh BIP39 mnemonic is generated and printed).
    #[arg(long, env = "STH_PASSPHRASE", hide_env_values = true)]
    pub passphrase: Option<String>,
    /// Passphrase of a funded wallet that pays the fees (registration + PQ key + reserve) to the new delegate wallet.
    #[arg(long, env = "STH_FUND_PASSPHRASE", hide_env_values = true)]
    pub fund_from: Option<String>,
    /// Extra STH left on the delegate wallet after the fees (default 1 STH).
    #[arg(long, default_value = "1")]
    pub reserve: String,
    /// Register a Quantum Shield ML-DSA-44 key too (default: yes when milestone pq.active is on). --no-pq skips it.
    #[arg(long, default_value_t = false)]
    pub no_pq: bool,
    /// PQ passphrase (default: generated).
    #[arg(long, env = "STH_PQ_PASSPHRASE", hide_env_values = true)]
    pub pq_passphrase: Option<String>,
    /// node.yaml to patch: delegate.enabled = true, passphrase → delegate.secrets, PQ passphrase → delegate.pq_secrets.
    #[arg(long)]
    pub node_config: Option<PathBuf>,
    /// Seconds to wait for each confirmation (funding, registration, PQ key).
    #[arg(long, default_value_t = 60)]
    pub timeout: u64,
}

async fn wait_for(what: &str, timeout: u64, mut check: impl FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>) -> Result<()> {
    for i in 0..timeout {
        if check().await {
            println!("        {what} confirmed");
            return Ok(());
        }
        if i % 5 == 0 {
            println!("        waiting for {what}… {i}s");
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    bail!("{what} was not confirmed within {timeout} s — check `sth-cli tx-status` and continue manually")
}

pub async fn delegate_setup(api: String, a: DelegateSetupArgs) -> Result<()> {
    let http = reqwest::Client::new();
    let api = api.trim_end_matches('/').to_string();
    let cfg = configuration(&http, &api).await?;
    let params = ChainParams::from_configuration(&cfg)?;
    let phrase = match &a.passphrase { Some(p) => p.clone(), None => crate::crypto::mnemonic::generate_mnemonic()? };
    let keys = KeyPair::from_passphrase(&phrase)?;
    let address = keys.address(params.pubkey_hash)?;
    let ctx = Ctx { api: api.clone(), http: http.clone(), keys: keys.clone(), address: address.clone() };
    let with_pq = params.pq_active && !a.no_pq;
    let pq_phrase = if with_pq { Some(match &a.pq_passphrase { Some(p) => p.clone(), None => crate::crypto::mnemonic::generate_mnemonic()? }) } else { None };

    println!("== 1/5 wallet");
    println!("address     {address}");
    println!("public key  {}", keys.public_key_hex());
    if a.passphrase.is_none() {
        println!("passphrase  {phrase}");
        println!("            ↑ WRITE IT DOWN — it is the delegate's forging key and is never shown again");
    }
    if let Some(p) = &pq_phrase {
        if a.pq_passphrase.is_none() {
            println!("pq phrase   {p}");
        }
    }

    // what is still to do (rerunning the wizard is safe) and what it costs: dynamic (SHIP-41) or static registration fee,
    // static + surcharge for the PQ key, plus the reserve
    let wallet = ctx.get(&format!("/api/wallets/{address}")).await?;
    let already = wallet["attributes"]["delegate"]["username"].as_str().map(str::to_string);
    let pq_locked = wallet["quantumShield"]["active"] == true;
    let reg_fee = if already.is_some() { 0 } else { match &params.dynamic_fees { Some(df) => dynamic_fee(df, "delegateRegistration", 124 + a.username.len()), None => params.delegate_registration_fee } };
    let pq_fee = if with_pq && !pq_locked { params.second_signature_fee + params.pq_fee_per_byte * (3 + crate::crypto::pq::SIG_LEN as u64) } else { 0 };
    let need = reg_fee + pq_fee + if reg_fee + pq_fee > 0 { units(&a.reserve, 8)? } else { 0 };
    println!("== 2/5 funding: registration {} STH{} + reserve {} STH = {} STH", coins_str(reg_fee), if pq_fee > 0 { format!(" + PQ key {} STH", coins_str(pq_fee)) } else { String::new() }, if need > 0 { a.reserve.as_str() } else { "0" }, coins_str(need));
    let balance = |ctx: Ctx| async move { ctx.get(&format!("/api/wallets/{}", ctx.address)).await.ok().and_then(|w| w["balance"].as_str().and_then(|b| b.parse::<u64>().ok()).or_else(|| w["balance"].as_u64())).unwrap_or(0) };
    let have = balance(ctx.clone()).await;
    if have < need {
        let Some(funder) = &a.fund_from else {
            bail!("wallet {address} holds {} STH, needs {} STH — send the difference and rerun with --passphrase, or pass --fund-from <funded passphrase>", coins_str(have), coins_str(need));
        };
        let missing = need - have;
        println!("        sending {} STH from the funding wallet", coins_str(missing));
        run(ChainSource::Api(api.clone()), TxArgs { passphrase: funder.clone(), api: None, second_passphrase: None, nonce: None, dry_run: false, fee: None, cmd: TxCmd::Transfer { to: address.clone(), amount: coins_str(missing), memo: Some(format!("delegate {}", a.username)) } }).await?;
        let c = ctx.clone();
        wait_for("funding", a.timeout, move || { let c = c.clone(); Box::pin(async move { balance(c).await >= need }) }).await?;
    } else {
        println!("        balance {} STH — enough", coins_str(have));
    }

    println!("== 3/5 delegate registration");
    match already {
        Some(u) if u == a.username => println!("        already registered as {u}"),
        Some(u) => bail!("wallet {address} is already the delegate {u}"),
        None => {
            run(ChainSource::Api(api.clone()), TxArgs { passphrase: phrase.clone(), api: None, second_passphrase: None, nonce: None, dry_run: false, fee: None, cmd: TxCmd::DelegateRegister { username: a.username.clone() } }).await?;
            let c = ctx.clone();
            let u = a.username.clone();
            wait_for("registration", a.timeout, move || { let c = c.clone(); let u = u.clone(); Box::pin(async move { c.get(&format!("/api/delegates/{u}")).await.ok().is_some_and(|d| d["address"] == c.address) }) }).await?;
        }
    }

    println!("== 4/5 quantum shield");
    if let Some(p) = &pq_phrase {
        if pq_locked {
            println!("        wallet already has a PQ key — skipped (rotate with `sth-cli tx pq-register` if needed)");
        } else {
            run(ChainSource::Api(api.clone()), TxArgs { passphrase: phrase.clone(), api: None, second_passphrase: Some(p.clone()), nonce: None, dry_run: false, fee: None, cmd: TxCmd::PqRegister { old_second_passphrase: None } }).await?;
            let c = ctx.clone();
            wait_for("PQ key", a.timeout, move || { let c = c.clone(); Box::pin(async move { c.get(&format!("/api/wallets/{}", c.address)).await.ok().is_some_and(|w| w["quantumShield"]["active"] == true) }) }).await?;
        }
    } else {
        println!("        skipped ({})", if a.no_pq { "--no-pq" } else { "milestone pq.active is off on this network" });
    }

    println!("== 5/5 node.yaml");
    match &a.node_config {
        Some(path) => {
            patch_node_yaml(path, &phrase, pq_phrase.as_deref())?;
            println!("        {} patched: delegate.enabled = true, secrets{} updated — restart sth-core to forge", path.display(), if pq_phrase.is_some() { " + pq_secrets" } else { "" });
        }
        None => {
            println!("        add to node.yaml and restart sth-core:");
            println!("        delegate:\n          enabled: true\n          secrets: [\"{phrase}\"]{}", pq_phrase.as_ref().map(|p| format!("\n          pq_secrets: [\"{p}\"]")).unwrap_or_default());
        }
    }
    println!("done — `sth-cli status` and the metrics page show when {} enters the active set (votes needed: top-{})", a.username, cfg["constants"]["activeDelegates"]);
    Ok(())
}

/// Text patch of the `delegate:` block (comments are preserved): `enabled: true`, passphrases appended to the lists.
fn patch_node_yaml(path: &PathBuf, secret: &str, pq_secret: Option<&str>) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let start = lines.iter().position(|l| l.trim_end() == "delegate:").ok_or_else(|| anyhow!("{}: no `delegate:` section", path.display()))?;
    let end = lines.iter().enumerate().skip(start + 1).find(|(_, l)| !l.is_empty() && !l.starts_with(' ') && !l.starts_with('#')).map(|(i, _)| i).unwrap_or(lines.len());
    let yaml_str = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let set_list = |lines: &mut Vec<String>, key: &str, value: &str, end: &mut usize| -> Result<()> {
        let Some(i) = (start + 1..*end).find(|&i| lines[i].trim_start().starts_with(&format!("{key}:"))) else { bail!("{}: no `{key}:` in the delegate section", path.display()) };
        let indent = lines[i].len() - lines[i].trim_start().len();
        let pad = " ".repeat(indent);
        let rest = lines[i].trim_start()[key.len() + 1..].trim();
        if (i + 1..*end).any(|j| lines[j].trim() == format!("- {}", yaml_str(value)) || lines[j].trim() == format!("- {value}")) || rest.contains(&yaml_str(value)) {
            return Ok(()); // already listed
        }
        if rest == "[]" {
            lines[i] = format!("{pad}{key}:");
            lines.insert(i + 1, format!("{pad}- {}", yaml_str(value)));
            *end += 1;
        } else if rest.starts_with('[') {
            let inner = rest.trim_start_matches('[').trim_end_matches(']').trim();
            lines[i] = format!("{pad}{key}: [{inner}{}{}]", if inner.is_empty() { "" } else { ", " }, yaml_str(value));
        } else {
            let mut j = i + 1;
            while j < *end && lines[j].trim_start().starts_with("- ") {
                j += 1;
            }
            lines.insert(j, format!("{pad}- {}", yaml_str(value)));
            *end += 1;
        }
        Ok(())
    };
    let mut end = end;
    if let Some(i) = (start + 1..end).find(|&i| lines[i].trim_start().starts_with("enabled:")) {
        let indent = lines[i].len() - lines[i].trim_start().len();
        lines[i] = format!("{}enabled: true", " ".repeat(indent));
    } else {
        lines.insert(start + 1, "  enabled: true".into());
        end += 1;
    }
    set_list(&mut lines, "secrets", secret, &mut end)?;
    if let Some(p) = pq_secret {
        set_list(&mut lines, "pq_secrets", p, &mut end)?;
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    NodeConfig::parse(&out).with_context(|| "patched node.yaml does not parse — nothing written")?;
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_yaml_patch_keeps_comments_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.yaml");
        std::fs::write(&path, NodeConfig::default_yaml()).unwrap();
        patch_node_yaml(&path, "one two three", Some("pq phrase")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let cfg = NodeConfig::parse(&text).unwrap();
        assert!(cfg.delegate.enabled);
        assert_eq!(cfg.delegate.secrets, vec!["one two three"]);
        assert_eq!(cfg.delegate.pq_secrets, vec!["pq phrase"]);
        assert!(text.contains("# mempool:"), "comment header preserved");
        // second run adds nothing, a second delegate is appended after the first
        patch_node_yaml(&path, "one two three", Some("pq phrase")).unwrap();
        patch_node_yaml(&path, "four five six", None).unwrap();
        let cfg = NodeConfig::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.delegate.secrets, vec!["one two three", "four five six"]);
        assert_eq!(cfg.delegate.pq_secrets, vec!["pq phrase"]);
        // inline list form
        std::fs::write(&path, "delegate:\n  enabled: false\n  secrets: [\"a\"]\n  pq_secrets: []\n").unwrap();
        patch_node_yaml(&path, "b", Some("p")).unwrap();
        let cfg = NodeConfig::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.delegate.secrets, vec!["a", "b"]);
        assert_eq!(cfg.delegate.pq_secrets, vec!["p"]);
    }
}
