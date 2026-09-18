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
