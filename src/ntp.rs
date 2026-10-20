//! Author: TechnoL0g
//!
//! Start-up clock check (SNTP, RFC 4330): slots are 8 s wide, a drifting clock forges in the wrong slot.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;

const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// Local clock offset in milliseconds (positive = local clock ahead of NTP).
pub async fn clock_offset_ms(server: &str) -> Result<i64, String> {
    clock_offset_ms_at(&format!("{server}:123")).await
}

/// Same as `clock_offset_ms` with an explicit `host:port`.
pub async fn clock_offset_ms_at(addr: &str) -> Result<i64, String> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| e.to_string())?;
    socket.connect(addr).await.map_err(|e| e.to_string())?;
    let mut packet = [0u8; 48];
    packet[0] = 0x1b; // LI 0, version 3, mode 3 (client)
    let t1 = SystemTime::now();
    socket.send(&packet).await.map_err(|e| e.to_string())?;
    let mut buf = [0u8; 48];
    tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut buf)).await.map_err(|_| "ntp timeout".to_string())?.map_err(|e| e.to_string())?;
    let t4 = SystemTime::now();
    let ntp_ts = |off: usize| -> f64 {
        let secs = u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]) as f64;
        let frac = u32::from_be_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]) as f64 / 4_294_967_296.0;
        secs + frac - NTP_UNIX_OFFSET as f64
    };
    let unix = |t: SystemTime| t.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let (t2, t3) = (ntp_ts(32), ntp_ts(40));
    let offset = ((t2 - unix(t1)) + (t3 - unix(t4))) / 2.0;
    Ok((-offset * 1000.0).round() as i64)
}

/// Drift above this (ms) is a problem: blocks land in the wrong slot / arrive "before" their slot start.
pub const DRIFT_ALERT_MS: i64 = 1_000;

/// Last NTP probe (`/api/ntfry/metrics.clock`).
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClockStatus {
    pub server: String,
    /// Local clock minus NTP, ms (positive = local clock ahead). `None` when the last probe failed.
    pub offset_ms: Option<i64>,
    pub checked_at: i64,
    pub ok: bool,
    pub error: Option<String>,
    pub drifted: bool,
}

fn last() -> &'static Mutex<Option<ClockStatus>> {
    static L: OnceLock<Mutex<Option<ClockStatus>>> = OnceLock::new();
    L.get_or_init(Default::default)
}

/// Last probe result, `null` until the first one completes.
pub fn status() -> serde_json::Value {
    serde_json::to_value(last().lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap_or(serde_json::Value::Null)
}

/// Record a probe result; returns the previous `drifted` flag so the caller can alert on state changes only.
pub fn record(server: &str, res: Result<i64, String>, now_unix: i64) -> (ClockStatus, Option<bool>) {
    let st = match res {
        Ok(ms) => ClockStatus { server: server.into(), offset_ms: Some(ms), checked_at: now_unix, ok: true, error: None, drifted: ms.abs() > DRIFT_ALERT_MS },
        Err(e) => {
            // a failed probe keeps the previous verdict — no network ≠ good clock
            let prev = last().lock().unwrap_or_else(|e| e.into_inner()).clone();
            ClockStatus { server: server.into(), offset_ms: None, checked_at: now_unix, ok: false, error: Some(e), drifted: prev.as_ref().is_some_and(|p| p.drifted) }
        }
    };
    let prev = last().lock().unwrap_or_else(|e| e.into_inner()).replace(st.clone()).map(|p| p.drifted);
    (st, prev)
}

/// One probe: log it, remember it, alert on drift state changes (`clock-drift` / recovery).
pub async fn check_clock(server: &str) {
    let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (st, prev_drifted) = record(server, clock_offset_ms(server).await, now_unix);
    match (st.offset_ms, &st.error) {
        (Some(ms), _) if st.drifted => {
            tracing::warn!("Local clock is off by {ms}ms from NTP ({server}) — fix the system time (chrony / systemd-timesyncd), forging slots depend on it");
            if prev_drifted != Some(true) {
                let dir = if ms > 0 { "ahead of" } else { "behind" };
                crate::alerts::notify("clock-drift", format!("CLOCK DRIFT: system clock is {}ms {dir} NTP ({server}). Blocks from the network look like they arrive before their slot start and own blocks are forged late — enable NTP sync: `timedatectl set-ntp true` or install chrony.", ms.abs()), true);
            }
        }
        (Some(ms), _) => {
            tracing::info!("Your NTP connectivity has been verified by {server}. Local clock is off by {ms}ms from NTP");
            if prev_drifted == Some(true) {
                crate::alerts::notify("clock-drift", format!("Clock drift resolved: local clock is off by {ms}ms from NTP ({server})"), true);
            }
        }
        (None, Some(e)) => tracing::warn!("NTP check against {server} failed: {e}"),
        (None, None) => {}
    }
}

/// Probe at start and then every `every` (default 10 min): a clock that was fine at boot drifts for days unnoticed.
pub async fn monitor(server: String, every: Duration) {
    loop {
        check_clock(&server).await;
        tokio::time::sleep(every).await;
    }
}
