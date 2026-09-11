//! Author: TechnoL0g
//! Process memory (RSS) probe + periodic log line so operators can spot leaks on small VPS.

use std::time::Duration;
use tracing::info;

/// Resident set size of this process in bytes (Linux via /proc, macOS/Windows via `ps`/`tasklist`-free fallback = None).
pub fn rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        return Some(pages * 4096);
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Logs `memory: rss …` every `every` seconds (INFO) — compare across hours to detect growth.
pub async fn watchdog(every: Duration) {
    let mut ticker = tokio::time::interval(every);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        if let Some(rss) = rss_bytes() {
            info!("memory: rss {:.0} MB", rss as f64 / 1_048_576.0);
        }
    }
}
