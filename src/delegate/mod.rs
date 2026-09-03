//! Author: TechnoL0g
//!
//! Pluggable delegate (forging) module. Activated only when `delegate.secrets` (or a legacy
//! `delegates.json` via `delegate.secrets_file`) is configured; otherwise the node stays a pure relay.
//! Rewards: the protocol credits block reward + fees to the generator wallet itself (reward is 0 on
//! mainnet today, delegates earn the fees of the blocks they sign).

pub mod block_builder;
pub mod forger;
pub mod round;

pub use forger::{Forger, ForgerOptions, ForgingStatus};

use crate::error::{Error, Result};
use crate::storage::Storage;
use std::sync::Arc;
use std::time::Duration;

/// Passphrases from `delegate.secrets` plus a legacy `delegates.json` (`{ "secrets": [...] }`).
pub fn load_secrets(secrets: &[String], secrets_file: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = secrets.iter().filter(|s| !s.trim().is_empty()).cloned().collect();
    if !secrets_file.trim().is_empty() {
        #[derive(serde::Deserialize)]
        struct File {
            #[serde(default)]
            secrets: Vec<String>,
        }
        let raw = std::fs::read_to_string(secrets_file.trim()).map_err(|e| Error::Config(format!("cannot read {secrets_file}: {e}")))?;
        let f: File = serde_json::from_str(&raw).map_err(|e| Error::Config(format!("{secrets_file}: {e}")))?;
        out.extend(f.secrets.into_iter().filter(|s| !s.trim().is_empty()));
    }
    Ok(out)
}

/// Snapshot the active delegates at every round boundary (`/api/rounds/:round/delegates`, forging order).
pub fn spawn_round_tracker(storage: Arc<Storage>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tick.tick().await;
            let Ok(tip) = storage.get_last_height() else { continue };
            if tip == 0 {
                continue;
            }
            let n = storage.network().milestone(tip).active_delegates as u64;
            let info = round::round_info(tip, n);
            if tip % n != 0 {
                continue;
            }
            if let Ok(None) = storage.get_round(info.next_round) {
                let st = storage.clone();
                let saved = tokio::task::spawn_blocking(move || -> Result<()> {
                    let list = st.active_delegates(n as usize)?;
                    st.save_round(info.next_round, &list)
                })
                .await;
                match saved {
                    Ok(Ok(())) => tracing::info!("Starting Round {} (saved active delegates)", forger::group(info.next_round)),
                    Ok(Err(e)) => tracing::warn!(error = %e, "round snapshot failed"),
                    Err(e) => tracing::warn!(error = %e, "round snapshot task failed"),
                }
            }
        }
    })
}
