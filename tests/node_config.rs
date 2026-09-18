//! Author: TechnoL0g
//!
//! node.yaml parsing: defaults, overrides, reward address derivation, validation.

use sth_core::node_config::NodeConfig;

#[test]
fn default_yaml_roundtrips() {
    let yaml = NodeConfig::default_yaml();
    assert!(yaml.starts_with("# sth-core relay node configuration"));
    let cfg = NodeConfig::parse(&yaml).unwrap();
    assert_eq!(cfg, NodeConfig::default());
    assert_eq!(cfg.p2p.parallel_peers, 4);
    assert_eq!(cfg.api.host, "127.0.0.1");
    assert!(cfg.reward_address().unwrap().is_none());
}

#[test]
fn partial_yaml_uses_defaults_and_overrides() {
    let cfg = NodeConfig::parse("db_path: /var/sth\np2p:\n  parallel_peers: 8\n  legacy_peers: [1.2.3.4]\nrewards:\n  reward_address: SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw\n").unwrap();
    assert_eq!(cfg.db_path, "/var/sth");
    assert_eq!(cfg.p2p.parallel_peers, 8);
    assert_eq!(cfg.p2p.legacy_peers, vec!["1.2.3.4".to_string()]);
    assert_eq!(cfg.p2p.legacy_port, 4001);
    assert_eq!(cfg.reward_address().unwrap().as_deref(), Some("SRhZmNqRwtRbFvaHHHAeZZWCBxaGVwg9dw"));
}

#[test]
fn reward_passphrase_is_ignored() {
    // the old `reward_passphrase` key must not break existing configs (serde ignores unknown keys) and derives nothing
    let cfg = NodeConfig::parse("rewards:\n  reward_passphrase: 'this is a top secret passphrase'\n").unwrap();
    assert!(cfg.reward_address().unwrap().is_none());
}

#[test]
fn validation_rejects_bad_values() {
    assert!(NodeConfig::parse("network: devnet\n").is_err());
    assert!(NodeConfig::parse("p2p:\n  parallel_peers: 0\n").is_err());
    assert!(NodeConfig::parse("rewards:\n  reward_address: notanaddress\n").is_err());
    assert!(NodeConfig::parse("unknown: 1\n").is_ok());
}

#[test]
fn write_default_refuses_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node.yaml");
    NodeConfig::write_default(&path).unwrap();
    assert!(NodeConfig::write_default(&path).is_err());
    assert_eq!(NodeConfig::load(&path).unwrap(), NodeConfig::default());
}

#[test]
fn quorum_share_is_validated() {
    assert!(NodeConfig::parse("delegate:\n  quorum_share: 1.5\n").is_err());
    let cfg = NodeConfig::parse("delegate:\n  quorum_share: 0.66\n  enabled: true\n  secrets: [x]\n").unwrap();
    assert_eq!(cfg.delegate.quorum_share, 0.66);
    assert_eq!(NodeConfig::default().delegate.quorum_share, 0.5);
    assert!(NodeConfig::default_yaml().contains("quorum_share"));
}
