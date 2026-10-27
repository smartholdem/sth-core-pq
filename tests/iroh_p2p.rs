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
    let setup = RelaySetup { n0: false, extra: vec![], guard: Default::default() };
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

/// sthpq address book: node B pulls A's fresh records over RPC `GetAddrs`; A refuses tampered / private records.
#[tokio::test]
async fn address_book_records_are_served_over_rpc() {
    use sth_core::node_config::DiscoveryConfig;
    use sth_core::p2p_iroh::addrbook::{fetch_addrs, SignedAddr};
    use sth_core::p2p_iroh::DiscoveryOpts;

    let network = Network::mainnet();
    let a_storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&a_storage, &network).unwrap();
    let a_pool = Arc::new(Mempool::new(a_storage.clone(), vec![], 10));
    let discovery = DiscoveryOpts { cfg: DiscoveryConfig { listen_port: 0, ..DiscoveryConfig::default() }, pinned: vec![] };
    let a = IrohNode::spawn_with(iroh::SecretKey::generate(), vec![], true, None, a_storage.clone(), a_pool, false, None, None, Some(discovery)).await.unwrap();
    let book = a.addrbook.get().cloned().expect("address book enabled");

    let b_storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    let b_pool = Arc::new(Mempool::new(b_storage.clone(), vec![], 10));
    let b = IrohNode::spawn(iroh::SecretKey::generate(), vec![a.id()], true, None, b_storage.clone(), b_pool, false, None, None).await.unwrap();

    // a third node's record, learned by A "over gossip" and confirmed alive
    let c = iroh::SecretKey::generate();
    let now = sth_core::p2p_iroh::addrbook::now_unix();
    let record = SignedAddr::sign(&c, &network.nethash, vec!["185.199.110.9:4004".into()], Some("https://relay-fsn7.sth.cx/".into()), now);
    assert!(book.insert(record.clone(), "gossip").unwrap());
    assert!(book.insert(SignedAddr::sign(&c, &network.nethash, vec!["10.0.0.9:4004".into()], None, now + 1), "gossip").is_err(), "private address refused");
    let mut forged = record.clone();
    forged.seq += 5;
    assert!(book.insert(forged, "gossip").is_err(), "re-signed by nobody → rejected");

    let _ = tokio::time::timeout(Duration::from_secs(15), a.endpoint.online()).await;
    let addr = a.endpoint.addr();
    let none = tokio::time::timeout(Duration::from_secs(30), fetch_addrs(&b.endpoint, addr.clone(), 50)).await.unwrap().unwrap();
    assert!(none.is_empty(), "never answered → not shared yet");
    book.mark(c.public(), true);
    let got = fetch_addrs(&b.endpoint, addr, 50).await.unwrap();
    assert_eq!(got, vec![record]);
    // persisted for the next start
    assert_eq!(a_storage.load_addr_entries().len(), 1);
    let s = book.snapshot();
    assert_eq!(s["entries"], 1);
    assert_eq!(s["fresh"], 1);
    a.shutdown().await;
    b.shutdown().await;
}

/// `discovery.n0_dns: false` → preset `Minimal` (no n0 DNS, no n0 relays): two such nodes still talk over RPC with a
/// pinned `id@ip:port` bootstrap address (what gateways give the network).
#[tokio::test]
async fn minimal_preset_without_n0_talks_via_pinned_bootstrap() {
    use sth_core::node_config::DiscoveryConfig;
    use sth_core::p2p_iroh::DiscoveryOpts;

    let network = Network::mainnet();
    let cfg = DiscoveryConfig { n0_dns: false, mainline: false, listen_port: 0, ..DiscoveryConfig::default() };
    let a_storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    sth_core::genesis::ensure_genesis(&a_storage, &network).unwrap();
    let a_pool = Arc::new(Mempool::new(a_storage.clone(), vec![], 10));
    let a = IrohNode::spawn_with(iroh::SecretKey::generate(), vec![], true, None, a_storage.clone(), a_pool, false, None, None, Some(DiscoveryOpts { cfg: cfg.clone(), pinned: vec![] })).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), a.endpoint.online()).await;
    let a_addr = a.endpoint.addr();
    let direct = a_addr.ip_addrs().next().copied().expect("node A has a local socket address");

    let (id, sock) = sth_core::p2p_iroh::addrbook::parse_bootstrap(&format!("{}@{}", a.id(), direct)).unwrap();
    let pinned = vec![iroh::EndpointAddr::new(id).with_ip_addr(sock.unwrap())];
    let b_storage = Arc::new(Storage::temporary(network.clone()).unwrap());
    let b_pool = Arc::new(Mempool::new(b_storage.clone(), vec![], 10));
    let b = IrohNode::spawn_with(iroh::SecretKey::generate(), vec![a.id()], true, None, b_storage.clone(), b_pool, false, None, None, Some(DiscoveryOpts { cfg, pinned })).await.unwrap();

    // B dials A by id only: the pinned address in MemoryLookup is the whole "discovery"
    let (height, _) = tokio::time::timeout(Duration::from_secs(30), fetch_status(&b.endpoint, &b.peers, a.id())).await.unwrap().unwrap();
    assert_eq!(height, 1);
    assert!(a.relay_urls.is_empty(), "no relays configured → none reported");
    a.shutdown().await;
    b.shutdown().await;
}
