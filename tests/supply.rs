//! Author: TechnoL0g
//!
//! `/api/blockchain` supply = genesis `totalAmount` + block rewards per milestone (legacy supply calculator), not a mainnet constant.

use sth_core::config::{Network, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ, MAINNET_MILESTONES_JSON, MAINNET_NETWORK_JSON, TOTAL_SUPPLY};

fn with_rewards() -> Network {
    // reward 0 until 1000, 2 STH from 1000, 1 STH from 5000
    let ms = MAINNET_MILESTONES_JSON.replace(
        r#"{
        "height": 151200,"#,
        r#"{ "height": 1000, "reward": "200000000" }, { "height": 5000, "reward": "100000000" },
    {
        "height": 151200,"#,
    );
    assert!(ms.contains("\"height\": 1000"), "milestone replace failed");
    Network::from_json(MAINNET_NETWORK_JSON, &ms, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap()
}

#[test]
fn mainnet_supply_is_genesis_total_amount_at_any_height() {
    let net = Network::mainnet();
    assert_eq!(net.genesis_total_amount, TOTAL_SUPPLY);
    assert_eq!(net.supply(1), TOTAL_SUPPLY);
    assert_eq!(net.supply(11_800_000), TOTAL_SUPPLY);
}

#[test]
fn rewards_accumulate_per_milestone_segment() {
    let net = with_rewards();
    let g = net.genesis_total_amount;
    assert_eq!(net.supply(999), g);
    assert_eq!(net.supply(1000), g + 200_000_000);
    assert_eq!(net.supply(4999), g + 4000 * 200_000_000);
    // 4000 blocks × 2 STH (1000..=4999) + 1 block × 1 STH (5000)
    assert_eq!(net.supply(5000), g + 4000 * 200_000_000 + 100_000_000);
    assert_eq!(net.supply(6000), g + 4000 * 200_000_000 + 1001 * 100_000_000);
}
