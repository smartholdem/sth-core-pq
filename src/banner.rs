//! Author: TechnoL0g
//!
//! Startup banner (legacy-core style): one framed box on stdout — node, database, milestone rules, modules with
//! ●/○ switches, transaction types by group, state counters. `rows()` is pure (tests), `print()` renders it.

use crate::config::{Milestone, Network};
use crate::delegate::forger::group;
use crate::node_config::NodeConfig;

/// Facts gathered by `node::run` that are not in the configuration.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub tip_height: u64,
    pub tip_id: Option<String>,
    /// Seconds since the tip block's slot (None for an empty database).
    pub tip_age_secs: Option<i64>,
    /// `None` = `startup_scan: false` (not counted).
    pub wallets: Option<usize>,
    /// Registered delegates (`wu:` index) — always counted, it is cheap.
    pub delegates: usize,
    pub pq_commitments: usize,
    pub pq_keys: usize,
    pub pending: usize,
    /// Legacy seed peers known at start.
    pub legacy_peers: usize,
    /// `(EndpointId short, connected home relays)` when iroh is on.
    pub iroh: Option<(String, Vec<String>)>,
    /// Delegates loaded into the forger (0 = relay only).
    pub forging_keys: usize,
    /// Minimum fee of a transfer in this node's pool and in the legacy pools.
    pub min_transfer_fee: u64,
    pub legacy_min_transfer_fee: u64,
    pub legacy_dynamic_fees: bool,
    /// `api://<EndpointId>/sth-core` when the SHIP-44 wallet api is on.
    pub wallet_api_url: Option<String>,
}

/// One line of the box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// Section header (`─ Modules ─`).
    Section(&'static str),
    /// `label  value`.
    Info(&'static str, String),
    /// `● label  value` (on) / `○ label  value` (off).
    Switch(&'static str, bool, String),
}

fn sth(v: u64) -> String {
    let s = format!("{}.{:08}", v / 100_000_000, v % 100_000_000);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s} STH")
}

fn short(id: &str) -> String {
    id.chars().take(10).collect::<String>() + "…"
}

fn age(secs: i64) -> String {
    match secs {
        s if s < 0 => "in the future".into(),
        s if s < 90 => format!("{s}s ago"),
        s if s < 5_400 => format!("{}m ago", s / 60),
        s if s < 172_800 => format!("{}h ago", s / 3_600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// The banner content, in display order.
pub fn rows(cfg: &NodeConfig, network: &Network, ms: &Milestone, f: &Facts) -> Vec<Row> {
    use Row::*;
    let p2p = &cfg.p2p;
    let mut r = Vec::with_capacity(28);
    r.push(Info("Network", format!("{} · nethash {} · pubKeyHash {}", network.name, short(&network.nethash), network.pubkey_hash)));
    r.push(Info("Database", format!("{} · sled · {} MB cache · undo depth {}", cfg.db_path, cfg.db_cache_mb, group(crate::storage::UNDO_DEPTH))));
    r.push(Info("Last block", match (&f.tip_id, f.tip_age_secs) {
        (Some(id), Some(a)) if f.tip_height > 0 => format!("{} · id {} · {}", group(f.tip_height), short(id), age(a)),
        _ => "none — genesis will be written".to_string(),
    }));
    r.push(Info("Milestone", format!("@{} · {}s slots · {} active delegates · reward {} · multiPayment {} · HTLC {}", group(f.tip_height + 1), ms.blocktime, ms.active_delegates, sth(ms.reward), ms.multi_payment_limit, if ms.htlc_enabled { "on" } else { "off" })));
    r.push(Info("Consensus", format!("DPoS + SHIP-35 finality (quorum {}/{}{}) · strict balance {}", ms.finality_quorum(), ms.active_delegates, if ms.finality.active { ", hard mode" } else { "" }, if ms.strict_balance { "on" } else { "off" })));
    r.push(Info("Fees", format!("dynamic {} (min transfer {}) · legacy pools {} {}", if cfg.mempool.dynamic_fees.enabled { "on" } else { "off" }, sth(f.min_transfer_fee), if f.legacy_dynamic_fees { "≥" } else { "exactly" }, sth(f.legacy_min_transfer_fee))));
    let pq_stage = if ms.pq.blocks { "C — PQ blocks" } else if ms.pq.active { "B — v3 transactions" } else { "A — commitments" };
    r.push(Info("Quantum Shield", format!("stage {pq_stage} · ML-DSA-44 · {} committed · {} PQ keys", group(f.pq_commitments as u64), group(f.pq_keys as u64))));

    r.push(Section("Modules"));
    let gateway = if p2p.legacy_listen.trim().is_empty() { "gateway off".to_string() } else { format!("gateway {}", if p2p.legacy_public_addr.trim().is_empty() { p2p.legacy_listen.trim() } else { p2p.legacy_public_addr.trim() }) };
    r.push(Switch("P2P legacy", p2p.legacy_enabled, if p2p.legacy_enabled { format!(":{} · {} peers · relay fanout {} · {gateway}", p2p.legacy_port, f.legacy_peers, p2p.relay_fanout.max(crate::p2p_legacy::relay::MIN_RELAY_FANOUT)) } else { "off — REST sync".into() }));
    r.push(Switch("P2P iroh", f.iroh.is_some(), match &f.iroh {
        Some((id, relays)) => format!(
            "me {id} · bootstrap {} · relay {} · serve blocks {}",
            p2p.iroh.bootstrap.len(),
            if !p2p.iroh.relay { "off".to_string() } else if relays.is_empty() { "connecting…".to_string() } else { relays.iter().map(|u| u.trim_start_matches("https://").trim_end_matches('/').to_string()).collect::<Vec<_>>().join(",") },
            if p2p.iroh.serve_blocks { "on" } else { "off" }
        ),
        None => "off".into(),
    }));
    let wa = &p2p.iroh.wallet_api;
    let wallet_api = f.iroh.is_some() && wa.enabled;
    r.push(Switch("Wallet API", wallet_api, if wallet_api { format!("netfory/api/1 + sth/events/1 · {} connections · {} rps", wa.max_connections, wa.rate_limit_rps) } else { "off".into() }));
    let ws = f.iroh.is_some() && p2p.iroh.ws_bridge.enabled;
    r.push(Switch("WS bridge", ws, if ws { format!("ws://{}/ws/events", p2p.iroh.ws_bridge.listen) } else { "off".into() }));
    r.push(Switch("REST API", cfg.api.enabled, if cfg.api.enabled { format!("http://{}:{}/api · CORS origins {}", cfg.api.host, cfg.api.port, cfg.api.cors_origins.len()) } else { "off".into() }));
    let metrics = cfg.api.enabled && cfg.api.page_metrics;
    r.push(Switch("Metrics page", metrics, if !metrics { "off".into() } else if cfg.api.metrics_listen.trim().is_empty() { format!("http://{}:{}/", cfg.api.host, cfg.api.port) } else { format!("http://{}/", cfg.api.metrics_listen.trim()) }));
    r.push(Switch("Delegate", f.forging_keys > 0, if f.forging_keys > 0 { format!("{} key(s) loaded{} · quorum {} of peers, min {}", f.forging_keys, if cfg.delegate.pq_secrets.is_empty() { "" } else { " + PQ" }, cfg.delegate.quorum_share, cfg.delegate.min_quorum_peers) } else { "relay only".into() }));
    r.push(Switch("Service Nodes", ms.masternodes.enabled, if ms.masternodes.enabled { format!("SHIP-42 · collateral {}", sth(ms.masternodes.collateral_amount)) } else { "off until milestone".into() }));
    r.push(Switch("Token Market", ms.token_market.enabled, if ms.token_market.enabled { format!("SHIP-43 · max {} open orders", ms.token_market.max_open_orders) } else { "off until milestone".into() }));
    let alerts = crate::alerts::enabled();
    r.push(Switch("Alerts", alerts, if alerts { [(!cfg.alerts.telegram_chat_id.is_empty(), "telegram"), (!cfg.alerts.webhook_url.is_empty(), "webhook")].iter().filter(|(b, _)| *b).map(|(_, n)| *n).collect::<Vec<_>>().join(" + ") } else { "off".into() }));
    r.push(Switch("Stall watchdog", true, format!("{} slots · auto rollback · iroh gap pull", crate::intake::STALL_SLOTS)));

    r.push(Section("Transaction types"));
    r.push(Info(if ms.tx_v2 { "group 1 core v2" } else { "group 1 core v1" }, format!("Transfer · SecondSignature · DelegateRegistration · Vote · MultiSignature · Ntfry · MultiPayment({}) · DelegateResignation · HtlcLock · HtlcClaim · HtlcRefund", ms.multi_payment_limit)));
    r.push(Switch("v3 Quantum Shield", ms.pq.active, if ms.pq.active { "hybrid secp256k1 + ML-DSA-44 · PqRegister (type 1 v3)".into() } else { "off until pq.active — stage A accepts vendorField commitments".into() }));
    r.push(Switch("group 2 sObjects", ms.sobj_active, if !ms.sobj_active { "off until milestone".into() } else if ms.sobj_v2 { "SmartObject register · update · resign · transfer · sell · buy (v2)".into() } else { "SmartObject register · update · resign".into() }));
    r.push(Switch("group 3 tokens", ms.tokens, if !ms.tokens { "off until milestone".into() } else if ms.token_market.enabled { "TokenInit · TokenTransfer · TokenMint · TokenBurn · TokenMeta · Sell · Buy · Cancel".into() } else { "TokenInit · TokenTransfer · TokenMint · TokenBurn · TokenMeta".into() }));

    r.push(Section("State"));
    r.push(Info("Delegates", format!("{} registered · {} active per round", group(f.delegates as u64), ms.active_delegates)));
    r.push(Info("Wallets", match f.wallets { Some(w) => group(w as u64), None => "not counted (startup_scan: false)".into() }));
    r.push(Info("Mempool", format!("{} pending · max {} · vote index dv: ready", group(f.pending as u64), group(cfg.mempool.max_size as u64))));

    r.push(Section("SHIP-44 Wallet API on iroh"));
    match &f.wallet_api_url {
        Some(url) if wallet_api => {
            r.push(Switch("Protocols", true, "netfory/api/1 (REST) + sth/events/1 (push subscriptions)".into()));
            r.push(Info("URL", url.clone()));
            r.push(Info("Limits", format!("max_connections {} · rate_limit_rps {} · WS bridge {}", group(wa.max_connections as u64), wa.rate_limit_rps, if ws { format!("ws://{}/ws/events", p2p.iroh.ws_bridge.listen) } else { "off".into() })));
        }
        _ => r.push(Switch("Protocols", false, if f.iroh.is_none() { "off — p2p.iroh.enabled: false".into() } else { "off — p2p.iroh.wallet_api.enabled: false".into() })),
    }
    r
}

use crate::console::{AMBER, BOLD, CYAN, DIM, GREEN, RESET};

const LABEL_W: usize = 18;
/// Values longer than this wrap at " · " separators onto continuation lines.
const MAX_VALUE_W: usize = 80;

fn wrap_value(value: &str, max: usize) -> Vec<String> {
    if width(value) <= max {
        return vec![value.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    for piece in value.split(" · ") {
        match out.last_mut() {
            Some(cur) if width(cur) + 3 + width(piece) <= max => {
                cur.push_str(" · ");
                cur.push_str(piece);
            }
            _ => out.push(piece.to_string()),
        }
    }
    out
}

fn width(s: &str) -> usize {
    s.chars().count()
}

/// The same rows as plain text for a chat message (Telegram / webhook `node-started` alert): no box, no colours.
pub fn plain(rows: &[Row]) -> String {
    let mut out = format!("NODE STARTED · sth-core-pq v{}\n", env!("CARGO_PKG_VERSION"));
    for row in rows {
        match row {
            Row::Section(name) => out.push_str(&format!("\n— {name} —\n")),
            Row::Info(label, value) => out.push_str(&format!("{label}: {value}\n")),
            Row::Switch(label, on, value) => out.push_str(&format!("{} {label}: {value}\n", if *on { "●" } else { "○" })),
        }
    }
    // Telegram sendMessage takes 4096 characters
    if out.chars().count() > 4000 {
        out = out.chars().take(3990).collect::<String>() + "\n…";
    }
    out
}

/// Render the box. `color` adds ANSI colours (stdout is a terminal).
pub fn render(rows: &[Row], color: bool) -> String {
    let title = format!("SmartHoldem sth-core-pq v{}", env!("CARGO_PKG_VERSION"));
    let paint = |code: &str, s: &str| if color { format!("{code}{s}{RESET}") } else { s.to_string() };
    // plain text per row (for the width), then the coloured variant; long values wrap at " · " onto indented lines
    let mut body: Vec<(usize, String, String)> = Vec::new();
    let indent = " ".repeat(LABEL_W + 2);
    for (i, r) in rows.iter().enumerate() {
        match r {
            Row::Section(name) => body.push((i, format!("─ {name} "), String::new())),
            Row::Info(label, value) => {
                for (j, part) in wrap_value(value, MAX_VALUE_W).into_iter().enumerate() {
                    if j == 0 {
                        body.push((i, format!("  {label:<LABEL_W$}{part}"), format!("  {}{part}", paint(BOLD, &format!("{label:<LABEL_W$}")))));
                    } else {
                        body.push((i, format!("{indent}{part}"), format!("{indent}{part}")));
                    }
                }
            }
            Row::Switch(label, on, value) => {
                let dot = if *on { "●" } else { "○" };
                for (j, part) in wrap_value(value, MAX_VALUE_W).into_iter().enumerate() {
                    let plain = if j == 0 { format!("{dot} {label:<LABEL_W$}{part}") } else { format!("{indent}{part}") };
                    let colored = if !*on {
                        paint(DIM, &plain)
                    } else if j == 0 {
                        format!("{} {}{part}", paint(GREEN, dot), paint(BOLD, &format!("{label:<LABEL_W$}")))
                    } else {
                        plain.clone()
                    };
                    body.push((i, plain, colored));
                }
            }
        }
    }
    let inner = body.iter().map(|(_, p, _)| width(p)).chain(std::iter::once(width(&title) + 2)).max().unwrap_or(40).max(40);
    let mut out = String::new();
    out.push_str(&format!("╭{}╮\n", "─".repeat(inner + 2)));
    out.push_str(&format!("│ {}{} │\n", paint(AMBER, &title), " ".repeat(inner - width(&title))));
    out.push_str(&format!("├{}┤\n", "─".repeat(inner + 2)));
    for (i, plain, colored) in &body {
        match &rows[*i] {
            Row::Section(_) => out.push_str(&format!("├{}{}┤\n", paint(AMBER, plain), "─".repeat(inner + 2 - width(plain)))),
            _ => out.push_str(&format!("│ {}{} │\n", if color { colored } else { plain }, " ".repeat(inner - width(plain)))),
        }
    }
    out.push_str(&format!("╰{}╯", "─".repeat(inner + 2)));
    out
}


/// ASCII logo shown above the box: `STH` block letters + "SmartHoldem Post Quantum CORE".
pub fn logo(color: bool) -> String {
    let art = ["  ____  _____ _   _ ", " / ___||_   _| | | |", " \\___ \\  | | | |_| |", "  ___) | | | |  _  |", " |____/  |_| |_| |_|"];
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let text = ["", "SmartHoldem", "Post Quantum CORE", version.as_str(), "ML-DSA-44 · DPoS · SHIP-35 finality · legacy P2P + iroh"];
    let paint = |code: &str, s: &str| if color { format!("{code}{s}{RESET}") } else { s.to_string() };
    art.iter()
        .zip(text)
        .map(|(a, t)| {
            let t = match t {
                "SmartHoldem" => paint(AMBER, t),
                "Post Quantum CORE" => paint(BOLD, t),
                v if v.starts_with('v') => paint(GREEN, v),
                other => paint(DIM, other),
            };
            format!("{}   {t}", paint(CYAN, a))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// QR code of `text` drawn with half-block characters (two module rows per line), quiet zone included.
/// `None` when the text does not fit a QR code.
pub fn qr(text: &str) -> Option<String> {
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::L).ok()?;
    let w = code.width();
    let dark = |x: i64, y: i64| -> bool {
        if x < 0 || y < 0 || x >= w as i64 || y >= w as i64 {
            return false;
        }
        code[(x as usize, y as usize)] == qrcode::Color::Dark
    };
    let margin = 2i64;
    let mut out = String::new();
    let mut y = -margin;
    while y < w as i64 + margin {
        let mut line = String::new();
        for x in -margin..w as i64 + margin {
            line.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push_str(line.trim_end());
        out.push('\n');
        y += 2;
    }
    Some(out.trim_end_matches('\n').to_string())
}

/// Print the logo, the banner and (when enabled) the wallet-api QR code on stdout — coloured on a terminal, plain for
/// pm2 / journald.
pub fn print(cfg: &NodeConfig, network: &Network, ms: &Milestone, f: &Facts) {
    let color = crate::console::color();
    println!("\n{}\n\n{}", logo(color), render(&rows(cfg, network, ms, f), color));
    if let Some(url) = f.wallet_api_url.as_deref().filter(|_| cfg.p2p.iroh.wallet_api.startup_qr) {
        if let Some(code) = qr(url) {
            let paint = |code: &str, s: &str| if color { format!("{code}{s}{RESET}") } else { s.to_string() };
            println!("\n{}\n{}\n{}\n", paint(BOLD, "Scan with a SmartHoldem wallet to pair with this node (SHIP-44 wallet api):"), paint(DIM, url), code);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            tip_height: 11_907_671,
            tip_id: Some("3f2a9c0d1e2f3a4b5c6d".into()),
            tip_age_secs: Some(12),
            wallets: Some(12_345),
            delegates: 26,
            pending: 5,
            legacy_peers: 14,
            iroh: Some(("3e141032f9".into(), vec!["https://relay-ru1.sth.cx/".into()])),
            forging_keys: 0,
            min_transfer_fee: 768_000,
            legacy_min_transfer_fee: 100_000_000,
            legacy_dynamic_fees: false,
            wallet_api_url: Some("api://59f751c338bfd059ddc0ead7064a7f0525034754f14353312c51b950d3d0f1a9/sth-core".into()),
            ..Default::default()
        }
    }

    #[test]
    fn rows_reflect_config_and_facts() {
        let cfg = NodeConfig::default();
        let net = Network::mainnet();
        let r = rows(&cfg, &net, &net.milestone(11_907_672), &facts());
        assert_eq!(r.iter().filter(|x| matches!(x, Row::Section(_))).count(), 4);
        assert!(r.contains(&Row::Info("URL", "api://59f751c338bfd059ddc0ead7064a7f0525034754f14353312c51b950d3d0f1a9/sth-core".into())), "{r:?}");
        assert!(r.iter().any(|x| matches!(x, Row::Info("Limits", v) if v.starts_with("max_connections ") && v.contains("rate_limit_rps "))), "{r:?}");
        assert!(r.contains(&Row::Switch("Protocols", true, "netfory/api/1 (REST) + sth/events/1 (push subscriptions)".into())));
        assert!(r.contains(&Row::Info("Last block", "11,907,671 · id 3f2a9c0d1e… · 12s ago".into())), "{r:?}");
        assert!(matches!(&r[3], Row::Info("Milestone", v) if v.starts_with("@11,907,672 · 8s slots · 21 active delegates")), "{:?}", r[3]);
        assert!(matches!(&r[5], Row::Info("Fees", v) if v.contains("min transfer 0.00768 STH") && v.contains("legacy pools exactly 1 STH")), "{:?}", r[5]);
        assert!(r.iter().any(|x| matches!(x, Row::Switch("P2P legacy", true, v) if v.contains(":4001 · 14 peers"))), "{r:?}");
        assert!(r.iter().any(|x| matches!(x, Row::Switch("P2P iroh", true, v) if v.contains("relay relay-ru1.sth.cx"))), "{r:?}");
        assert!(r.contains(&Row::Switch("Delegate", false, "relay only".into())));
        assert!(r.contains(&Row::Info("Delegates", "26 registered · 21 active per round".into())));
        assert!(r.contains(&Row::Info("Wallets", "12,345".into())));

        // chat copy of the banner (`node-started` alert): no box characters, sections + on/off marks, under Telegram's limit
        let text = plain(&r);
        assert!(text.starts_with(&format!("NODE STARTED · sth-core-pq v{}\n", env!("CARGO_PKG_VERSION"))), "{text}");
        assert!(text.contains("\n— Modules —\n") && text.contains("● P2P legacy: :4001 · 14 peers") && text.contains("○ Delegate: relay only") && text.contains("Wallets: 12,345\n"), "{text}");
        assert!(!text.contains('│') && !text.contains('╭') && text.chars().count() < 4000, "{text}");

        // empty database, everything off, no scan
        let mut off = NodeConfig::default();
        off.p2p.legacy_enabled = false;
        off.api.enabled = false;
        let r = rows(&off, &net, &net.milestone(1), &Facts::default());
        assert!(r.contains(&Row::Info("Last block", "none — genesis will be written".into())));
        assert!(r.contains(&Row::Switch("P2P legacy", false, "off — REST sync".into())));
        assert!(r.contains(&Row::Switch("P2P iroh", false, "off".into())));
        assert!(r.contains(&Row::Switch("REST API", false, "off".into())));
        assert!(r.contains(&Row::Switch("Metrics page", false, "off".into())));
        assert!(r.contains(&Row::Info("Wallets", "not counted (startup_scan: false)".into())));
        assert!(r.contains(&Row::Info("Delegates", "0 registered · 21 active per round".into())), "delegates are counted even without the scan");
        assert!(r.contains(&Row::Switch("Protocols", false, "off — p2p.iroh.enabled: false".into())), "{r:?}");
        assert!(r.last() == Some(&Row::Switch("Protocols", false, "off — p2p.iroh.enabled: false".into())), "the wallet api block is the last section");
    }

    #[test]
    fn render_draws_an_aligned_box() {
        let cfg = NodeConfig::default();
        let net = Network::mainnet();
        let r = rows(&cfg, &net, &net.milestone(11_907_672), &facts());
        let plain = render(&r, false);
        let lines: Vec<&str> = plain.lines().collect();
        let w = width(lines[0]);
        assert!(lines.iter().all(|l| width(l) == w), "every line has the box width {w}:\n{plain}");
        assert!(lines[0].starts_with('╭') && lines.last().unwrap().starts_with('╰'));
        assert!(lines[1].contains(&format!("SmartHoldem sth-core-pq v{}", env!("CARGO_PKG_VERSION"))));
        assert!(lines.iter().any(|l| l.starts_with("├─ Modules ")), "{plain}");
        assert!(lines.iter().any(|l| l.contains("● P2P legacy") && l.contains(":4001")), "{plain}");
        assert!(lines.iter().any(|l| l.contains("○ Delegate") && l.contains("relay only")), "{plain}");
        assert!(!plain.contains("\x1b["), "plain render has no ANSI codes");
        let colored = render(&r, true);
        assert!(colored.contains(GREEN) && colored.contains(AMBER) && colored.contains(RESET));
        // colours do not change the visible width
        let strip = |s: &str| { let mut out = String::new(); let mut esc = false; for c in s.chars() { if esc { if c == 'm' { esc = false } } else if c == '\x1b' { esc = true } else { out.push(c) } } out };
        assert!(colored.lines().all(|l| width(&strip(l)) == w), "{colored}");
    }

    #[test]
    fn logo_has_the_product_name_and_version() {
        let plain = logo(false);
        assert_eq!(plain.lines().count(), 5);
        assert!(plain.contains("SmartHoldem") && plain.contains("Post Quantum CORE") && plain.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))), "{plain}");
        assert!(plain.lines().all(|l| l.starts_with(' ') && width(&l[..l.char_indices().nth(20).map(|(i, _)| i).unwrap()]) == 20 && l.chars().nth(20) == Some(' ')), "art column is 20 wide:\n{plain}");
        assert!(!plain.contains("\x1b["));
        assert!(logo(true).contains(CYAN));
    }

    #[test]
    fn long_values_wrap_at_separators() {
        assert_eq!(wrap_value("a · b", 10), vec!["a · b"]);
        assert_eq!(wrap_value("aaaa · bbbb · cccc · dddd", 12), vec!["aaaa · bbbb", "cccc · dddd"]);
        let cfg = NodeConfig::default();
        let net = Network::mainnet();
        let plain = render(&rows(&cfg, &net, &net.milestone(11_907_672), &facts()), false);
        assert!(plain.lines().all(|l| width(l) <= MAX_VALUE_W + LABEL_W + 8), "box stays narrow:\n{plain}");
        assert!(plain.lines().filter(|l| l.contains("HtlcLock") || l.contains("Transfer ·")).count() >= 2, "tx types wrapped:\n{plain}");
    }

    #[test]
    fn qr_renders_the_wallet_api_url() {
        let url = "api://59f751c338bfd059ddc0ead7064a7f0525034754f14353312c51b950d3d0f1a9/sth-core";
        let q = qr(url).expect("fits a QR code");
        let lines: Vec<&str> = q.lines().collect();
        // version 4–6 code (33–41 modules) + 2-module quiet zone, two module rows per text line
        assert!((17..=24).contains(&lines.len()), "{} lines\n{q}", lines.len());
        assert!(lines.iter().all(|l| width(l) <= 45), "{q}");
        assert!(q.contains('█') && q.contains('▀') && q.contains('▄'), "{q}");
        assert!(lines[0].trim().is_empty(), "quiet zone on top");
        assert!(!lines[1].trim().is_empty());
        // decodes back: the finder pattern corners are dark
        assert_eq!(lines[1].chars().nth(2), Some('█'));
    }

    #[test]
    fn helpers() {
        assert_eq!(sth(100_000_000), "1 STH");
        assert_eq!(sth(25_000_000), "0.25 STH");
        assert_eq!(sth(0), "0 STH");
        assert_eq!(age(30), "30s ago");
        assert_eq!(age(600), "10m ago");
        assert_eq!(age(7_200), "2h ago");
        assert_eq!(age(200_000), "2d ago");
    }
}
