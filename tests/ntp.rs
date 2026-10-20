//! Author: TechnoL0g
//!
//! SNTP client against a local mock server (no network): offset is computed from the server timestamps.

use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::test]
async fn sntp_offset_against_mock_server() {
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = server.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut buf = [0u8; 48];
        let (_, from) = server.recv_from(&mut buf).await.unwrap();
        let mut reply = [0u8; 48];
        reply[0] = 0x1c;
        // server clock = local clock + 5 s
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 2_208_988_800 + 5;
        for off in [32usize, 40] {
            reply[off..off + 4].copy_from_slice(&(now as u32).to_be_bytes());
        }
        server.send_to(&reply, from).await.unwrap();
    });
    // clock_offset_ms connects to "<server>:123"; point it at the mock through a host:port override
    let ms = sth_core::ntp::clock_offset_ms_at(&format!("127.0.0.1:{port}")).await.unwrap();
    assert!((-6_000..=-4_000).contains(&ms), "offset {ms}ms should be about -5000 (local behind)");
}

#[test]
fn drift_state_is_remembered_and_failures_keep_the_verdict() {
    let (st, prev) = sth_core::ntp::record("mock", Ok(-1_500), 1_000);
    assert!(st.drifted && st.ok && prev.is_none());
    let (st, prev) = sth_core::ntp::record("mock", Err("timeout".into()), 1_600);
    assert!(st.drifted && !st.ok && st.offset_ms.is_none() && prev == Some(true));
    let (st, prev) = sth_core::ntp::record("mock", Ok(120), 2_200);
    assert!(!st.drifted && prev == Some(true));
    let v = sth_core::ntp::status();
    assert_eq!(v["offsetMs"], 120);
    assert_eq!(v["server"], "mock");
    assert_eq!(v["drifted"], false);
}

#[test]
fn skew_hint_is_the_earliest_arrival_or_zero() {
    assert_eq!(sth_core::intake::skew_hint([40, 900, -847, -568].into_iter()), 847);
    assert_eq!(sth_core::intake::skew_hint([40, 900].into_iter()), 0);
    assert_eq!(sth_core::intake::skew_hint(std::iter::empty()), 0);
}
