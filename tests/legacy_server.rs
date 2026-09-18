//! Author: TechnoL0g
//!
//! Inbound legacy P2P server round-trip with our own client: hello, getStatus, getPeers, getBlocks, postBlock.

use sth_core::config::Network;
use sth_core::crypto::{serialize_block_with_transactions, KeyPair};
use sth_core::delegate::block_builder::forge_block;
use sth_core::genesis::{ensure_genesis, mainnet_block};
use sth_core::mempool::Mempool;
use sth_core::p2p_legacy::{LegacyPeer, LegacyServer, PeerTable};
use sth_core::storage::Storage;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn legacy_server_serves_blocks_and_accepts_post_block() {
    let network = Network::mainnet();
    let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    ensure_genesis(&storage, &network).unwrap();
    let genesis = mainnet_block().unwrap();
    let keys = KeyPair::from_passphrase("this is a top secret passphrase").unwrap();
    let block2 = forge_block(&network, &keys, &genesis, 8, vec![]).unwrap();
    storage.apply_block(&block2).unwrap();
    let mempool = Arc::new(Mempool::new(storage.clone(), vec![], 10));
    let table = Arc::new(PeerTable::new(4001, ["10.0.0.1".to_string()]));
    table.record_success("10.0.0.1", Duration::from_millis(5), Some(2));
    let server = LegacyServer::new(storage.clone(), mempool, table, false);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    tokio::spawn(server.serve(addr));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut peer = LegacyPeer::connect(&format!("ws://{addr}/"), 0, Duration::from_secs(5)).await.unwrap();
    let status = peer.get_status().await.unwrap();
    assert_eq!(status.state.as_ref().unwrap().height, 2);
    assert_eq!(status.state.unwrap().header.unwrap().id, block2.id.clone().unwrap());
    assert_eq!(status.config.unwrap().network.unwrap().nethash, network.nethash);
    let peers = peer.get_peers().await.unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].ip, "10.0.0.1");

    let blocks = peer.get_blocks(0, 10).await.unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].transactions.len(), 1855);
    assert_eq!(blocks[0].id.as_deref(), Some(network.genesis_block_id.as_str()));
    assert_eq!(blocks[1].id, block2.id);

    // a fresh block on top of our tip is verified and applied
    let block3 = forge_block(&network, &keys, &block2, 16, vec![]).unwrap();
    let resp = peer.post_block(serialize_block_with_transactions(&block3, &network).unwrap()).await.unwrap();
    assert!(resp.status);
    assert_eq!(resp.height, 3);
    assert_eq!(storage.get_last_height().unwrap(), 3);
    // re-posting the same block is acknowledged, an unchained one is rejected
    let again = peer.post_block(serialize_block_with_transactions(&block3, &network).unwrap()).await.unwrap();
    assert!(again.status);
    let orphan = forge_block(&network, &keys, &block2, 24, vec![]).unwrap();
    let rejected = peer.post_block(serialize_block_with_transactions(&orphan, &network).unwrap()).await.unwrap();
    assert!(!rejected.status);
    assert_eq!(storage.get_last_height().unwrap(), 3);
}

/// A node that forged blocks while isolated (private fork) must roll them back during catch-up
/// and continue on the branch the network serves.
#[tokio::test]
async fn catch_up_rolls_back_private_fork() {
    let _ = tracing_subscriber::fmt().with_env_filter("sth_core=warn").with_test_writer().try_init();
    let network = Network::mainnet();
    let keys = KeyPair::from_passphrase("this is a top secret passphrase").unwrap();
    let genesis = mainnet_block().unwrap();
    let block2 = forge_block(&network, &keys, &genesis, 8, vec![]).unwrap();
    // network branch: 2 -> 3 -> 4
    let block3 = forge_block(&network, &keys, &block2, 16, vec![]).unwrap();
    let block4 = forge_block(&network, &keys, &block3, 24, vec![]).unwrap();
    let remote = Arc::new(Storage::temporary(network.clone()).unwrap());
    ensure_genesis(&remote, &network).unwrap();
    for b in [&block2, &block3, &block4] {
        remote.apply_block(b).unwrap();
    }
    // our branch: 2 -> 3' (forged alone with another timestamp)
    let local = Arc::new(Storage::temporary(network.clone()).unwrap());
    ensure_genesis(&local, &network).unwrap();
    local.set_undo_enabled(true);
    local.apply_block(&block2).unwrap();
    let block3_fork = forge_block(&network, &keys, &block2, 32, vec![]).unwrap();
    local.apply_block(&block3_fork).unwrap();
    assert_ne!(block3_fork.id, block3.id);

    let mempool = Arc::new(Mempool::new(remote.clone(), vec![], 10));
    let server = LegacyServer::new(remote.clone(), mempool, Arc::new(PeerTable::new(4001, Vec::<String>::new())), false);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    tokio::spawn(server.serve(addr));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let url = format!("ws://{addr}/");
    let table = Arc::new(PeerTable::new(0, [url.clone()]));
    table.record_success(&url, Duration::from_millis(5), Some(4));
    let opts = sth_core::p2p_legacy::P2pOptions { verify: false, parallel: 1, quiet: true, ..Default::default() };
    let applied = tokio::time::timeout(Duration::from_secs(30), sth_core::p2p_legacy::catch_up(local.clone(), table, &opts)).await.unwrap().unwrap();
    assert!(applied >= 2, "applied {applied}");
    assert_eq!(local.get_last_height().unwrap(), 4);
    assert_eq!(local.get_block_by_height(3).unwrap().unwrap().id, block3.id);
    assert_eq!(local.get_last_block().unwrap().unwrap().id, block4.id);
}
