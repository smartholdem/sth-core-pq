//! Interactive `sth-core init newnet`: asks the network parameters in the terminal, ENTER keeps the shown default.

use crate::newnet::{burn_address_for, pubkey_hash_for, NewNetOptions};
use std::io::{BufRead, Write};

const MAX_SLIP44: u32 = 0x7fff_ffff;
const STEPS: usize = 16;
const COIN: u64 = 100_000_000;
/// Genesis supply must fit u64 smartoshi.
const MAX_SUPPLY_COINS: u64 = u64::MAX / COIN;
const MAX_DELEGATES: u16 = 1000;

fn valid_name(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && s.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

/// Shows `\n` as the two characters a user can type.
fn escape(s: &str) -> String {
    s.replace('\n', "\\n")
}

struct Prompt<'a, R: BufRead, W: Write> {
    input: &'a mut R,
    out: &'a mut W,
    step: usize,
}

impl<R: BufRead, W: Write> Prompt<'_, R, W> {
    /// One question; empty line or EOF = default. `check` returns the parsed value or the reason to ask again.
    fn ask<T>(&mut self, title: &str, hint: &str, default: &str, check: impl Fn(&str) -> Result<T, String>) -> std::io::Result<T> {
        self.step += 1;
        loop {
            writeln!(self.out, "\n[{}/{STEPS}] {title}", self.step)?;
            if !hint.is_empty() {
                writeln!(self.out, "      {hint}")?;
            }
            write!(self.out, "      default: {default}  (press ENTER to keep) > ")?;
            self.out.flush()?;
            let mut line = String::new();
            let eof = self.input.read_line(&mut line)? == 0;
            let v = line.trim();
            match check(if v.is_empty() { default } else { v }) {
                Ok(x) => return Ok(x),
                Err(e) if eof => return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)),
                Err(e) => writeln!(self.out, "      ✗ {e}")?,
            }
        }
    }

    fn yes(&mut self, title: &str, hint: &str) -> std::io::Result<bool> {
        self.ask(title, hint, "Y", |v| match v.to_ascii_lowercase().as_str() {
            "y" | "yes" => Ok(true),
            "n" | "no" => Ok(false),
            _ => Err("answer Y or n".into()),
        })
    }
}

/// Fills `opts` from the answers. Returns false if the user declined the final confirmation.
pub fn run<R: BufRead, W: Write>(opts: &mut NewNetOptions, input: &mut R, out: &mut W) -> std::io::Result<bool> {
    writeln!(out, "Create a new SmartHoldem network. Press ENTER to accept the default shown for each question.")?;
    let mut p = Prompt { input, out, step: 0 };
    opts.title = p.ask("Network name", "network id and the folder created for its files (milestones.json, genesis, node.yaml); lowercase a-z and '-' only", "smartholdem-testnet", |v| {
        if valid_name(v) { Ok(v.to_string()) } else { Err("only lowercase letters a-z and '-', starting with a letter".into()) }
    })?;
    let ticker_rule = |v: &str| if !v.is_empty() && v.len() <= 8 && v.chars().all(|c| c.is_ascii_alphanumeric()) { Ok(v.to_string()) } else { Err("1-8 letters or digits".to_string()) };
    opts.symbol = p.ask("Symbol", "currency symbol shown in wallets and explorers", "TSH", ticker_rule)?;
    opts.ticker = p.ask("Token", "ticker of the native coin; its first letter becomes the first letter of every address", &opts.symbol.clone(), ticker_rule)?;
    let default_prefix = "SmartHoldemTestNet message:\n";
    opts.message_prefix = p.ask("Message prefix", "prepended to signed messages; type \\n for a new line", &escape(default_prefix), |v| Ok::<_, String>(v.replace("\\n", "\n")))?;
    let allowed = crate::newnet::address_start_symbols();
    let first = opts.ticker.chars().next().filter(|c| allowed.contains(*c)).unwrap_or('S');
    let start: char = p.ask("Address start symbol", &format!("first character of every address of this network (the token's first letter by default); allowed: {allowed}"), &first.to_string(), |v| {
        let mut it = v.chars();
        match (it.next(), it.next()) {
            (Some(c), None) if allowed.contains(c) => Ok(c),
            _ => Err(format!("one character from: {allowed}")),
        }
    })?;
    opts.pubkey_hash = pubkey_hash_for(start).expect("allowed symbols always have a version byte");
    let example = crate::crypto::KeyPair::from_passphrase("example address").and_then(|k| k.address(opts.pubkey_hash)).unwrap_or_default();
    writeln!(p.out, "      → pubKeyHash {} (every address starts with '{start}'), example address: {example}", opts.pubkey_hash)?;
    opts.slip44 = p.ask("slip44", &format!("BIP-44 coin type for HD wallets, 0-{MAX_SLIP44} (mainnet STH = 255)"), "255", |v| {
        v.parse::<u32>().ok().filter(|n| *n <= MAX_SLIP44).ok_or_else(|| format!("a number from 0 to {MAX_SLIP44}"))
    })?;
    opts.wif = p.ask("wif", "version byte of WIF-encoded private keys, 0-255", "255", |v| v.parse::<u8>().map_err(|_| "a number from 0 to 255".to_string()))?;
    opts.delegates = p.ask("Delegates", &format!("number of forging delegates created in the genesis block, 1-{MAX_DELEGATES} (mainnet 21); all of them forge on this node"), &opts.delegates.to_string(), |v| {
        v.parse::<u16>().ok().filter(|n| (1..=MAX_DELEGATES).contains(n)).ok_or_else(|| format!("a number from 1 to {MAX_DELEGATES}"))
    })?;
    let ticker = opts.ticker.clone();
    let treasury = p.ask("Treasury", &format!("premine in whole {ticker} sent to the treasury wallet in the genesis block (passphrase in delegates.json); the rest of the supply comes from block rewards"), &(opts.treasury_supply / COIN).to_string(), |v| {
        v.replace(['_', ','], "").parse::<u64>().ok().filter(|n| (1..=MAX_SUPPLY_COINS).contains(n)).ok_or_else(|| format!("a whole number from 1 to {MAX_SUPPLY_COINS}"))
    })?;
    let room = (MAX_SUPPLY_COINS - treasury) / u64::from(opts.delegates);
    let stake = p.ask("Delegate stake", &format!("whole {ticker} each delegate gets in the genesis block and votes for itself with (its forging weight); 1-{room}"), &(opts.delegate_stake / COIN).min(room).to_string(), |v| {
        v.replace(['_', ','], "").parse::<u64>().ok().filter(|n| (1..=room).contains(n)).ok_or_else(|| format!("a whole number from 1 to {room}"))
    })?;
    opts.treasury_supply = treasury * COIN;
    opts.delegate_stake = stake * COIN;
    opts.block_reward = p.ask("Block reward", &format!("{ticker} the delegate receives for every block it forges (new coins, from block 2), e.g. 2 or 0.5; 0 = no reward"), &crate::newnet::coins(opts.block_reward), |v| {
        crate::newnet::parse_coins(v).filter(|n| *n <= 1_000_000 * COIN).ok_or_else(|| "a number from 0 to 1000000 with up to 8 decimals".to_string())
    })?;
    let port = |v: &str, taken: &[u16]| match v.parse::<u16>() {
        Ok(n) if n >= 1024 && !taken.contains(&n) => Ok(n),
        Ok(n) if taken.contains(&n) => Err(format!("port {n} is already used by this network")),
        _ => Err("a port from 1024 to 65535".to_string()),
    };
    opts.p2p_port = p.ask("P2P port", "port other nodes of this network connect to: TCP for legacy P2P, UDP for iroh; 1024-65535 (mainnet 4001)", &opts.p2p_port.to_string(), |v| port(v, &[]))?;
    let taken = [opts.p2p_port];
    opts.api_port = p.ask("API port", "REST API for wallets and sth-cli, 1024-65535 (mainnet 4003)", &opts.api_port.to_string(), |v| port(v, &taken))?;
    let taken = [opts.p2p_port, opts.api_port];
    opts.metrics_port = p.ask("Metrics port", "web page with node metrics (http://host:port/), 1024-65535", &opts.metrics_port.to_string(), |v| port(v, &taken))?;
    p.step += 1;
    let burn = burn_address_for(&opts.ticker, opts.pubkey_hash, &opts.title);
    writeln!(p.out, "\n[{}/{STEPS}] burnAddress (generated, no private key exists — coins sent there are gone forever)\n      {burn}", p.step)?;
    opts.all_features = p.yes(
        "Activate all features? [Y/n]",
        "Y = from the first blocks: native tokens + token market, SmartObjects V2, strictBalance, Quantum Shield v3 transactions, hard finality, service nodes · n = mainnet base rules, tokens and PQ off",
    )?;
    let dir = format!("./{}", opts.title);
    writeln!(p.out, "\nSummary\n  name           {}\n  symbol / token {} / {}\n  messagePrefix  {}\n  address start  {} (pubKeyHash {})\n  slip44 / wif   {} / {}\n  delegates      {}\n  genesis supply {} {} = treasury {} + {} × {} stake\n  block reward   {}\n  ports          p2p {} · api {} · metrics {}\n  burnAddress    {burn}\n  all features   {}\n  epoch          {} (now — slot 0 of the new chain)\n  folder         {dir}",
        opts.title, opts.symbol, opts.ticker, escape(&opts.message_prefix), start, opts.pubkey_hash, opts.slip44, opts.wif, opts.delegates, treasury + stake * u64::from(opts.delegates), opts.ticker, treasury, opts.delegates, stake, if opts.block_reward > 0 { format!("{} {} per block", crate::newnet::coins(opts.block_reward), opts.ticker) } else { "off".to_string() }, opts.p2p_port, opts.api_port, opts.metrics_port, if opts.all_features { "yes" } else { "no" }, opts.epoch.get_or_insert_with(crate::newnet::now_epoch))?;
    write!(p.out, "Create the network? [Y/n] > ")?;
    p.out.flush()?;
    let mut line = String::new();
    p.input.read_line(&mut line)?;
    Ok(!matches!(line.trim().to_ascii_lowercase().as_str(), "n" | "no"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(input: &str) -> (NewNetOptions, bool, String) {
        let mut o = NewNetOptions::default();
        let mut out = Vec::new();
        let ok = run(&mut o, &mut input.as_bytes(), &mut out).unwrap();
        (o, ok, String::from_utf8(out).unwrap())
    }

    #[test]
    fn enter_everywhere_gives_the_defaults() {
        let (o, ok, out) = answers("\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n");
        assert!(ok);
        assert_eq!((o.title.as_str(), o.symbol.as_str(), o.ticker.as_str()), ("smartholdem-testnet", "TSH", "TSH"));
        assert_eq!(o.message_prefix, "SmartHoldemTestNet message:\n");
        assert_eq!((o.slip44, o.wif), (255, 255));
        assert!(o.all_features);
        assert_eq!((o.delegates, o.p2p_port, o.api_port, o.metrics_port), (21, 4002, 4004, 4889));
        assert_eq!((o.treasury_supply, o.delegate_stake), (100_000_000 * COIN, 1_000 * COIN));
        assert!(out.contains("genesis supply 100021000 TSH") && out.contains("block reward   off") && out.contains("example address: T"), "{out}");
        assert_eq!(o.block_reward, 0);
        assert!(crate::crypto::KeyPair::from_passphrase("x").unwrap().address(o.pubkey_hash).unwrap().starts_with('T'));
        assert!(out.contains("default: smartholdem-testnet") && out.contains("[16/16] Activate all features?") && out.contains("[9/16] Treasury"), "{out}");
    }

    #[test]
    fn invalid_answers_are_asked_again() {
        let (o, ok, out) = answers("My_Net\nmy-net\nABC\nSTH\n\n\n99999999999\n7\n\n0\n5\n0\n5_000_000\n99999999999999999999\n2,500\n-1\n2.5\n80\n5002\n5002\n5004\n5002\n5889\nn\nn\n");
        assert_eq!(o.title, "my-net");
        assert_eq!((o.symbol.as_str(), o.ticker.as_str(), o.pubkey_hash, o.slip44, o.wif), ("ABC", "STH", 63, 7, 255));
        assert_eq!((o.delegates, o.p2p_port, o.api_port, o.metrics_port), (5, 5002, 5004, 5889));
        assert_eq!((o.treasury_supply, o.delegate_stake, o.block_reward), (5_000_000 * COIN, 2_500 * COIN, 250_000_000));
        assert!(out.contains("2.5 STH per block"), "{out}");
        assert!(!o.all_features);
        assert!(!ok, "declined at the confirmation");
        assert!(out.contains("only lowercase letters") && out.contains("a number from 0 to 2147483647") && out.contains("a number from 1 to 1000"), "{out}");
        assert!(out.contains("a port from 1024 to 65535") && out.contains("port 5002 is already used"), "{out}");
    }

    #[test]
    fn letter_outside_base58_defaults_to_s_and_symbol_is_validated() {
        let (o, _, out) = answers("\n\nOIL\n\n\n\n\n\n\n\n\n\n\n\n\n\n");
        assert_eq!(o.pubkey_hash, 63);
        assert!(out.contains("default: S  (press"), "{out}");
        let (o, _, out) = answers("\n\n\n\n0\nAB\nD\n\n\n\n\n\n\n\n\n\n\n");
        assert_eq!(o.pubkey_hash, 30, "D = 30");
        assert!(out.contains("one character from:") && out.contains("→ pubKeyHash 30"), "{out}");
    }
}
