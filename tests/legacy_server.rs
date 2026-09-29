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
    let header = status.state.unwrap().header.unwrap();
    assert_eq!(header.id, block2.id.clone().unwrap());
    // full header (legacy peer verifier rebuilds the block from it and checks the signature before accepting us)
    assert_eq!(header.height, 2);
    assert_eq!(header.previous_block, network.genesis_block_id);
    assert_eq!(header.generator_public_key, keys.public_key_hex());
    assert_eq!(header.block_signature, block2.block_signature.clone().unwrap());
    assert_eq!(header.id_hex, header.id);
    let rebuilt = sth_core::models::Block {
        id: Some(header.id.clone()),
        version: header.version,
        timestamp: header.timestamp,
        previous_block: header.previous_block.clone(),
        height: header.height as u64,
        number_of_transactions: header.number_of_transactions,
        total_amount: header.total_amount.parse().unwrap(),
        total_fee: header.total_fee.parse().unwrap(),
        reward: header.reward.parse().unwrap(),
        payload_length: header.payload_length,
        payload_hash: header.payload_hash.clone(),
        generator_public_key: header.generator_public_key.clone(),
        block_signature: Some(header.block_signature.clone()),
        pq_signature: None,
        transactions: vec![],
    };
    assert!(sth_core::crypto::verify_block(&rebuilt, &network).verified, "status header must pass signature verification");
    let config = status.config.unwrap();
    // core 3 `p2p.peer.getStatus` reply schema: config.{version,network{name,nethash,explorer,token{name,symbol}},plugins} all required
    assert!((5..=24).contains(&config.version.len()), "{}", config.version);
    let net = config.network.unwrap();
    assert_eq!(net.nethash, network.nethash);
    assert_eq!(net.name, network.name);
    assert_eq!(net.explorer, network.explorer);
    assert!(!net.explorer.is_empty() && net.explorer.len() <= 128);
    let token = net.token.unwrap();
    assert_eq!((token.name.as_str(), token.symbol.as_str()), ("STH", "STH"));
    assert!((1..=8).contains(&token.name.len()) && (1..=4).contains(&token.symbol.len()));
    assert!(net.version <= 255);
    let api = config.plugins.get(sth_core::p2p_legacy::CORE_API_PLUGIN).expect("plugins map must not be empty (protobuf drops it → schema `required: plugins` fails)");
    assert_eq!((api.port, api.enabled), (4003, true));
    assert!(config.plugins.contains_key(sth_core::p2p_legacy::CORE_PQ_PLUGIN), "PQ marker plugin lets PQ Nodes recognise each other");
    let peers = peer.get_peers().await.unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].ip, "10.0.0.1");

    // `sth-core peers --accepted`: the server's accepted peer list includes 10.0.0.1 but not 9.9.9.9
    let mine = Arc::new(PeerTable::new(addr.port(), ["127.0.0.1".to_string()]));
    mine.record_success("127.0.0.1", Duration::from_millis(3), Some(2));
    mine.refresh(4, Duration::from_secs(5)).await;
    assert!(mine.is_pq("127.0.0.1"), "probe flags the PQ server via the marker plugin");
    assert_eq!(mine.alive_by_kind(), (0, 1));
    let ok = sth_core::p2p_legacy::acceptance_check(&mine, "10.0.0.1", Duration::from_secs(5)).await;
    assert_eq!((ok.asked, ok.listed), (1, 1), "{ok:?}");
    assert_eq!(ok.peers[0].3, true);
    let no = sth_core::p2p_legacy::acceptance_check(&mine, "9.9.9.9", Duration::from_secs(5)).await;
    assert_eq!((no.asked, no.listed), (1, 0), "{no:?}");

    // getCommonBlocks: highest asked id that is in our chain; unknown ids → no common block
    let common = peer.get_common_blocks(vec![network.genesis_block_id.clone(), block2.id.clone().unwrap(), "f".repeat(64)]).await.unwrap().unwrap();
    assert_eq!((common.height, common.id), (2, block2.id.clone().unwrap()));
    let common = peer.get_common_blocks(vec![network.genesis_block_id.clone()]).await.unwrap().unwrap();
    assert_eq!(common.height, 1);
    assert!(peer.get_common_blocks(vec!["f".repeat(64)]).await.unwrap().is_none());

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
    let rejected = peer.post_block(serialize_block_with_transactions(&orphan, &network).unwrap()).await;
    assert!(rejected.is_err() || !rejected.unwrap().status, "unchained block must not be accepted");
    assert_eq!(storage.get_last_height().unwrap(), 3);

    // a block WITH transactions (header ‖ u32-LE lengths ‖ txs): PQ Nodes rejected every such postBlock before 0.24.2
    // ("160 unexpected trailing bytes after header") and only learned the block from gossip a second later
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let a = alice.address(63).unwrap();
    storage.update_wallet_state(&a, 100_000_000_000, 0).unwrap();
    let tx = {
        let mut t: sth_core::models::Transaction = serde_json::from_value(serde_json::json!({
            "version": 2, "network": 63, "typeGroup": 1, "type": 0, "nonce": "1", "senderPublicKey": alice.public_key_hex(),
            "fee": "25000000", "amount": "300000000", "recipientId": keys.address(63).unwrap(), "expiration": 0
        }))
        .unwrap();
        let h = sth_core::crypto::transaction_signing_hash(&t).unwrap();
        t.signature = Some(sth_core::crypto::sign_schnorr_legacy(&h, alice.private_key()).unwrap());
        t.id = Some(sth_core::crypto::transaction_id(&t).unwrap());
        t
    };
    let block4 = forge_block(&network, &keys, &block3, 24, vec![tx]).unwrap();
    assert_eq!(block4.number_of_transactions, 1);
    let mut peer = LegacyPeer::connect("127.0.0.1", addr.port(), Duration::from_secs(5)).await.unwrap();
    let resp = peer.post_block(serialize_block_with_transactions(&block4, &network).unwrap()).await.expect("block with transactions decodes");
    assert!(resp.status && resp.height == 4, "{resp:?}");
    assert_eq!(storage.get_last_height().unwrap(), 4);
    assert_eq!(storage.get_block_by_height(4).unwrap().unwrap().transactions.len(), 1);
    assert_eq!(storage.get_wallet(&a).unwrap().unwrap().nonce, 1);
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
