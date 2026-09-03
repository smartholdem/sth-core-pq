//! Author: TechnoL0g
//!
//! Start-up clock check (SNTP, RFC 4330): slots are 8 s wide, a drifting clock forges in the wrong slot.

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

/// Log the drift; warn when it exceeds one second (the legacy core does the same check at start).
pub async fn check_clock(server: &str) {
    match clock_offset_ms(server).await {
        Ok(ms) if ms.abs() > 1_000 => tracing::warn!("Local clock is off by {ms}ms from NTP ({server}) — fix the system time, forging slots depend on it"),
        Ok(ms) => tracing::info!("Your NTP connectivity has been verified by {server}. Local clock is off by {ms}ms from NTP"),
        Err(e) => tracing::warn!("NTP check against {server} failed: {e}"),
    }
}
