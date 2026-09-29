//! Author: TechnoL0g
//!
//! Pluggable delegate (forging) module. Activated only when `delegate.secrets` (or a legacy
//! `delegates.json` via `delegate.secrets_file`) is configured; otherwise the node stays a pure relay.
//! Rewards: the protocol credits block reward + fees to the generator wallet itself (reward is 0 on
//! mainnet today, delegates earn the fees of the blocks they sign).

pub mod block_builder;
pub mod forger;
pub mod guard;
pub mod round;

pub use forger::{Forger, ForgerOptions, ForgingStatus};

use crate::error::{Error, Result};
use crate::storage::Storage;
use std::sync::Arc;
use std::time::Duration;

/// Passphrases from `delegate.secrets` plus a legacy `delegates.json` (`{ "secrets": [...] }`).
pub fn load_secrets(secrets: &[String], secrets_file: &str) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct File {
        #[serde(default)]
        secrets: Vec<String>,
    }
    let read = |path: &str| -> Result<Vec<String>> {
        let raw = std::fs::read_to_string(path).map_err(|e| Error::Config(format!("cannot read delegates file {path}: {e}")))?;
        let f: File = serde_json::from_str(&raw).map_err(|e| Error::Config(format!("{path}: {e} — expected the legacy format {{ \"secrets\": [\"passphrase\", …] }}")))?;
        if f.secrets.iter().all(|s| s.trim().is_empty()) {
            return Err(Error::Config(format!("{path}: no passphrases in \"secrets\"")));
        }
        Ok(f.secrets.into_iter().filter(|s| !s.trim().is_empty()).collect())
    };
    let mut out = Vec::new();
    for s in secrets.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        // `secrets: "./delegates.json"` — a legacy file given where the passphrases are expected
        if s.ends_with(".json") {
            out.extend(read(s)?);
        } else {
            out.push(s.to_string());
        }
    }
    if !secrets_file.trim().is_empty() {
        out.extend(read(secrets_file.trim())?);
    }
    Ok(out)
}

/// Round boundaries are snapshotted by `Storage::apply_blocks` itself (atomically with the boundary block). This task
/// only watches the chain and reports a missing snapshot — the forger refuses to forge without one.
pub fn spawn_round_tracker(storage: Arc<Storage>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        let mut warned_round = 0u64;
        loop {
            tick.tick().await;
            let Ok(tip) = storage.get_last_height() else { continue };
            if tip == 0 {
                continue;
            }
            let n = storage.network().milestone(tip).active_delegates as u64;
            let info = round::round_info(tip + 1, n);
            if let Ok(None) = storage.get_round(info.round) {
                if warned_round != info.round {
                    warned_round = info.round;
                    tracing::warn!(
                        "no active-delegates snapshot for round {} (height {}) — forging is paused until block {} is applied",
                        forger::group(info.round),
                        forger::group(tip),
                        forger::group(info.round_height + n - 1)
                    );
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_accept_a_legacy_delegates_json_in_either_field() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("delegates.json");
        std::fs::write(&file, "{\n\"secrets\": [\"secret1\",\"secret2\"]\n}\n").unwrap();
        let path = file.to_str().unwrap().to_string();
        // `secrets: "./delegates.json"` (user's spelling) parses as a one-element list and is read as a file
        let cfg = crate::node_config::NodeConfig::parse(&format!("delegate:\n  enabled: true\n  secrets: \"{path}\"\n")).unwrap();
        assert_eq!(cfg.delegate.secrets, vec![path.clone()]);
        assert_eq!(load_secrets(&cfg.delegate.secrets, &cfg.delegate.secrets_file).unwrap(), vec!["secret1", "secret2"]);
        // the documented field
        let cfg = crate::node_config::NodeConfig::parse(&format!("delegate:\n  enabled: true\n  secrets_file: {path}\n")).unwrap();
        assert_eq!(load_secrets(&cfg.delegate.secrets, &cfg.delegate.secrets_file).unwrap(), vec!["secret1", "secret2"]);
        // inline passphrases still work and mix with the file
        assert_eq!(load_secrets(&["one two".into(), path.clone()], "").unwrap(), vec!["one two", "secret1", "secret2"]);
        // bad file content → config error naming the expected format
        std::fs::write(&file, "[\"secret1\"]").unwrap();
        let e = load_secrets(&[], &path).unwrap_err().to_string();
        assert!(e.contains("expected the legacy format"), "{e}");
        std::fs::write(&file, "{\"secrets\": []}").unwrap();
        assert!(load_secrets(&[], &path).unwrap_err().to_string().contains("no passphrases"));
        assert!(load_secrets(&["./missing.json".into()], "").unwrap_err().to_string().contains("cannot read delegates file ./missing.json"));
    }
}
