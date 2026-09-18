//! Author: TechnoL0g
//!
//! Live checks against the mainnet legacy peers (network required): `cargo test --test p2p_live -- --ignored`.

use sth_core::p2p_legacy::{broadcast, fetch_peer_list, PeerTable};
use std::time::Duration;

#[tokio::test]
#[ignore]
async fn peer_table_refresh_and_relay_roundtrip() {
    let seeds = fetch_peer_list(4001).await;
    let table = PeerTable::new(4001, seeds);
    let alive = table.refresh(16, Duration::from_secs(8)).await;
    assert!(alive > 0, "no legacy peer reachable");
    assert!(table.best_height() > 11_700_000);
    let best = table.best(3);
    assert!(!best.is_empty());
    println!("alive={alive} best={best:?} height={}", table.best_height());

    // A syntactically valid but unsigned transfer: peers answer the request (200) and reject the tx internally.
    let tx = sth_core::models::Transaction { ..serde_json::from_str(r#"{"version":2,"network":63,"typeGroup":1,"type":0,"nonce":"1","senderPublicKey":"036560f20d578da7b8433248d0c82e68121163958d533dc74b0e7d8dbabc0606a0","fee":"100000000","amount":"1","expiration":0,"recipientId":"SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw","signature":"1d311090b61358077d2f59972b0913ec3687ec82f8a7b752121df934b701a7bc07e3e0d7bf051bf939a5291790ae5ed43ed59d1b6feb8dda0f76f07f747d8601"}"#).unwrap() };
    let bytes = sth_core::crypto::serialize_transaction(&tx, Default::default(), &sth_core::config::Network::mainnet()).unwrap();
    let delivered = broadcast(&table, vec![bytes], 2, Duration::from_secs(10)).await;
    println!("delivered={delivered}");
}
