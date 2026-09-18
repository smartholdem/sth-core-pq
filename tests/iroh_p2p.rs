//! Author: TechnoL0g
//!
//! Two in-process iroh nodes: RPC GetStatus / GetBlocks between them.

use sth_core::config::Network;
use sth_core::mempool::Mempool;
use sth_core::p2p_iroh::{fetch_blocks, fetch_finality, fetch_status, IrohNode};
use sth_core::storage::Storage;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn rpc_between_two_nodes() {
    let network = Network::mainnet();
    let a_storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&a_storage, &network).unwrap();
    let a_pool = Arc::new(Mempool::new(a_storage.clone(), vec![], 10));
    let a = IrohNode::spawn(iroh::SecretKey::generate(), vec![], true, None, a_storage.clone(), a_pool, false, None, None).await.unwrap();

    let b_storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    let b_pool = Arc::new(Mempool::new(b_storage.clone(), vec![], 10));
    let b = IrohNode::spawn(iroh::SecretKey::generate(), vec![a.id()], true, None, b_storage.clone(), b_pool, false, None, None).await.unwrap();

    let _ = tokio::time::timeout(Duration::from_secs(15), a.endpoint.online()).await;
    let addr = a.endpoint.addr();
    let (height, id) = tokio::time::timeout(Duration::from_secs(30), fetch_status(&b.endpoint, &b.peers, addr.clone())).await.unwrap().unwrap();
    assert_eq!(height, 1);
    assert_eq!(id.as_deref(), Some(network.genesis_block_id.as_str()));
    let blocks = fetch_blocks(&b.endpoint, &b.peers, addr.clone(), 0, 10).await.unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].transactions.len(), 1855);
    // SHIP-35 GetFinality: nothing yet, then the certificate A holds
    assert!(fetch_finality(&b.endpoint, &b.peers, addr.clone()).await.unwrap().is_none());
    let cert = sth_core::storage::FinalityCert { height: 1, block_id: network.genesis_block_id.to_string(), votes: Default::default() };
    a_storage.put_finality_cert(&cert).unwrap();
    assert_eq!(fetch_finality(&b.endpoint, &b.peers, addr).await.unwrap(), Some(cert));
    assert_eq!(b.peers.len(), 1);
    assert_eq!(b.peers.best_height(), 1);
    a.shutdown().await;
    b.shutdown().await;
}

/// Node A forges block 2 on top of genesis; node B (empty peer table for legacy) catches up over iroh only.
#[tokio::test]
async fn catch_up_pulls_blocks_from_iroh_peers() {
    use sth_core::crypto::KeyPair;
    use sth_core::delegate::block_builder::forge_block;
    use sth_core::p2p_legacy::{catch_up, P2pOptions, PeerTable};

    let network = Network::mainnet();
    let a_storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&a_storage, &network).unwrap();
    let genesis = sth_core::genesis::mainnet_block().unwrap();
    let keys = KeyPair::from_passphrase("this is a top secret passphrase").unwrap();
    let block2 = forge_block(&network, &keys, &genesis, 8, vec![]).unwrap();
    let block3 = forge_block(&network, &keys, &block2, 16, vec![]).unwrap();
    a_storage.apply_block(&block2).unwrap();
    a_storage.apply_block(&block3).unwrap();
    let a_pool = Arc::new(Mempool::new(a_storage.clone(), vec![], 10));
    let a = IrohNode::spawn(iroh::SecretKey::generate(), vec![], true, None, a_storage.clone(), a_pool, false, None, None).await.unwrap();

    let b_storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    let b_pool = Arc::new(Mempool::new(b_storage.clone(), vec![], 10));
    let b = IrohNode::spawn(iroh::SecretKey::generate(), vec![], true, None, b_storage.clone(), b_pool, false, None, None).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(15), a.endpoint.online()).await;
    b.add_peer_addr(a.endpoint.addr());
    assert_eq!(b.refresh_peers().await, 1);
    assert_eq!(b.peers.best_height(), 3);

    let table = Arc::new(PeerTable::new(4001, Vec::<String>::new()));
    let opts = P2pOptions { verify: false, parallel: 2, quiet: true, iroh: Some(b.clone()), ..P2pOptions::default() };
    let applied = tokio::time::timeout(Duration::from_secs(60), catch_up(b_storage.clone(), table, &opts)).await.unwrap().unwrap();
    assert_eq!(applied, 2, "blocks 2 and 3 pulled over iroh (genesis is embedded)");
    assert_eq!(b_storage.get_last_height().unwrap(), 3);
    assert_eq!(b_storage.get_block_by_height(3).unwrap().unwrap().id, block3.id);
    a.shutdown().await;
    b.shutdown().await;
}

/// Live: bind with the NETFORY n1 relays only and wait until a home relay is connected.
/// `cargo test --test iroh_p2p n1_relays -- --ignored --nocapture`
#[tokio::test]
#[ignore]
async fn n1_relays_reachable() {
    use sth_core::p2p_iroh::RelaySetup;
    let network = Network::mainnet();
    let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
    let pool = Arc::new(Mempool::new(storage.clone(), vec![], 10));
    let setup = RelaySetup { n0: false, extra: vec![] };
    assert_eq!(setup.relay_map().urls::<Vec<iroh::RelayUrl>>().len(), 2);
    let node = IrohNode::spawn(iroh::SecretKey::generate(), vec![], true, Some(setup), storage, pool, false, None, None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), node.endpoint.online()).await.expect("no n1 relay connected within 30 s");
    let relays = node.connected_relays();
    println!("home relays: {relays:?}");
    assert!(relays.iter().any(|u| u.contains("sth.cx")));
    node.shutdown().await;
}

#[test]
fn delegate_announce_is_bound_to_node_and_time() {
    use sth_core::p2p_iroh::delegates::{sign_announces, verify_announce};
    let key = sth_core::crypto::KeyPair::from_passphrase("this is a top secret passphrase").unwrap();
    let me = iroh::SecretKey::generate().public();
    let other = iroh::SecretKey::generate().public();
    let a = sign_announces(&[key.clone()], &me);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].public_key, key.public_key_hex());
    assert!(verify_announce(&a[0], &me));
    assert!(!verify_announce(&a[0], &other), "announce replayed from another node must fail");
    let mut stale = a[0].clone();
    stale.timestamp -= 3600;
    assert!(!verify_announce(&stale, &me));
    let mut forged = a[0].clone();
    forged.public_key = sth_core::crypto::KeyPair::from_passphrase("other").unwrap().public_key_hex();
    assert!(!verify_announce(&forged, &me));
}

/// A forges for a delegate and announces it over gossip; B learns "this delegate runs on Rust".
/// Takes ~2 min (swarm re-join 60 s + announce tick): `cargo test --test iroh_p2p rust_delegates -- --ignored`
#[tokio::test]
#[ignore]
async fn rust_delegates_propagate_over_gossip() {
    let network = Network::mainnet();
    let a_storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&a_storage, &network).unwrap();
    let a_pool = Arc::new(Mempool::new(a_storage.clone(), vec![], 10));
    let a = IrohNode::spawn(iroh::SecretKey::generate(), vec![], true, None, a_storage.clone(), a_pool, false, None, None).await.unwrap();
    let key = sth_core::crypto::KeyPair::from_passphrase("this is a top secret passphrase").unwrap();
    a.set_forging_keys(vec![key.clone()], true);
    assert!(a.rust_delegates.snapshot().contains_key(&key.public_key_hex()), "own delegates are counted immediately");

    let b_storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&b_storage, &network).unwrap();
    let b_pool = Arc::new(Mempool::new(b_storage.clone(), vec![], 10));
    let b = IrohNode::spawn(iroh::SecretKey::generate(), vec![a.id()], true, None, b_storage.clone(), b_pool, false, None, None).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(15), a.endpoint.online()).await;
    b.add_peer_addr(a.endpoint.addr());
    a.add_peer_addr(b.endpoint.addr());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
    loop {
        if let Some(info) = b.rust_delegates.snapshot().get(&key.public_key_hex()) {
            assert_eq!(info.node, a.id());
            assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "B never learned A's delegate over gossip");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    a.shutdown().await;
    b.shutdown().await;
}
