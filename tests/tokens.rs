//! Author: TechnoL0g
//! Native tokens (typeGroup 3): wire format, activation, lifecycle (init → transfer → mint → burn), rules, rollback.

use sth_core::config::{Network, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ, MAINNET_MILESTONES_JSON, MAINNET_NETWORK_JSON};
use sth_core::crypto::{deserialize_transaction, serialize_transaction, sign_schnorr_legacy, transaction_id, transaction_signing_hash, KeyPair, SerializeOptions};
use sth_core::delegate::block_builder::forge_block;
use sth_core::delegate::round::slot_start;
use sth_core::genesis::mainnet_block;
use sth_core::models::{sobj, token, Block, Transaction};
use sth_core::storage::Storage;
use sth_core::sync::{apply_blocks, ChainTip};
use std::sync::Arc;

const INIT_FEE: u64 = 50_000_000_000;

/// mainnet with aip36 + tokens active from genesis
fn network_tokens() -> Network {
    let ms = MAINNET_MILESTONES_JSON.replace("\"ship11\": true", "\"ship11\": true, \"aip36\": true, \"tokens\": true");
    Network::from_json(MAINNET_NETWORK_JSON, &ms, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap()
}
/// … plus the SHIP-43 token market
fn network_market() -> Network {
    let ms = MAINNET_MILESTONES_JSON.replace("\"ship11\": true", "\"ship11\": true, \"aip36\": true, \"tokens\": true, \"tokenMarket\": { \"enabled\": true }");
    Network::from_json(MAINNET_NETWORK_JSON, &ms, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap()
}
fn sell(keys: &KeyPair, nonce: u64, id: &str, amount: u64, price: u64) -> Transaction {
    token_tx(keys, nonce, token::SELL, 100_000_000, serde_json::json!({ "id": id, "amount": amount.to_string(), "price": price.to_string() }))
}
fn buy(keys: &KeyPair, nonce: u64, id: &str, order: &str, amount: u64) -> Transaction {
    token_tx(keys, nonce, token::BUY, 10_000_000, serde_json::json!({ "id": id, "orderId": order, "amount": amount.to_string() }))
}
fn cancel(keys: &KeyPair, nonce: u64, id: &str, order: &str) -> Transaction {
    token_tx(keys, nonce, token::CANCEL, 10_000_000, serde_json::json!({ "id": id, "orderId": order }))
}

fn tx(keys: &KeyPair, nonce: u64, body: serde_json::Value) -> Transaction {
    let mut v = serde_json::json!({ "version": 2, "network": 63, "typeGroup": 1, "nonce": nonce.to_string(), "senderPublicKey": keys.public_key_hex(), "amount": "0", "expiration": 0 });
    v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
    serde_json::from_value(v).unwrap()
}
fn sign(mut t: Transaction, keys: &KeyPair) -> Transaction {
    let h = transaction_signing_hash(&t).unwrap();
    t.signature = Some(sign_schnorr_legacy(&h, keys.private_key()).unwrap());
    t.id = Some(transaction_id(&t).unwrap());
    t
}
fn ticker(keys: &KeyPair, nonce: u64, name: &str) -> Transaction {
    tx(keys, nonce, serde_json::json!({ "typeGroup": 2, "type": 6, "fee": sobj::FEE_REGISTER.to_string(), "asset": { "type": 5, "subType": 0, "action": 0, "data": { "name": name, "ntfryData": "QmV1a2b3c4d5e6f7g8h9" } } }))
}
fn token_tx(keys: &KeyPair, nonce: u64, type_: u16, fee: u64, asset: serde_json::Value) -> Transaction {
    tx(keys, nonce, serde_json::json!({ "typeGroup": 3, "type": type_, "fee": fee.to_string(), "asset": { "token": asset } }))
}
fn init(keys: &KeyPair, nonce: u64, id: &str, flags: u8, supply: u64, cap: u64) -> Transaction {
    token_tx(keys, nonce, token::INIT, INIT_FEE, serde_json::json!({ "id": id, "decimals": 2, "flags": flags, "initialSupply": supply.to_string(), "supplyCap": cap.to_string() }))
}
fn xfer(keys: &KeyPair, nonce: u64, id: &str, items: &[(&str, u64)], memo: &str) -> Transaction {
    let fee = 10_000_000 + 1_000_000 * (items.len() as u64 - 1);
    let transfers: Vec<_> = items.iter().map(|(to, a)| serde_json::json!({ "recipientId": to, "amount": a.to_string() })).collect();
    token_tx(keys, nonce, token::TRANSFER, fee, serde_json::json!({ "id": id, "transfers": transfers, "memo": memo }))
}
fn mint(keys: &KeyPair, nonce: u64, id: &str, to: &str, amount: u64) -> Transaction {
    token_tx(keys, nonce, token::MINT, 100_000_000, serde_json::json!({ "id": id, "amount": amount.to_string(), "recipientId": to }))
}
fn burn(keys: &KeyPair, nonce: u64, id: &str, amount: u64) -> Transaction {
    token_tx(keys, nonce, token::BURN, 10_000_000, serde_json::json!({ "id": id, "amount": amount.to_string() }))
}
fn meta(keys: &KeyPair, nonce: u64, id: &str, m: serde_json::Value) -> Transaction {
    token_tx(keys, nonce, token::META, 100_000_000, serde_json::json!({ "id": id, "meta": m }))
}
fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 8 8"><rect width="8" height="8" fill="#333"/></svg>"##;

struct Chain {
    network: Network,
    storage: Arc<Storage>,
    forger: KeyPair,
    tip_block: Block,
    tip: ChainTip,
    slot: u64,
}
impl Chain {
    fn new(network: Network) -> Self {
        let storage = Arc::new(Storage::temporary(network.clone()).unwrap());
        sth_core::genesis::ensure_genesis(&storage, &network).unwrap();
        let genesis = mainnet_block().unwrap();
        Self { network, storage, forger: KeyPair::from_passphrase("forger").unwrap(), tip_block: genesis.clone(), tip: ChainTip { height: 1, id: genesis.id.clone() }, slot: 1 }
    }
    fn try_block(&mut self, txs: Vec<Transaction>) -> Result<(), String> {
        self.slot += 1;
        let b = forge_block(&self.network, &self.forger, &self.tip_block, slot_start(self.slot, 8), txs).unwrap();
        match apply_blocks(&self.storage, &self.network, &[b.clone()], self.tip.clone(), true) {
            Ok(tip) => {
                self.tip = tip;
                self.tip_block = b;
                Ok(())
            }
            Err(e) => {
                self.slot -= 1;
                Err(e.to_string())
            }
        }
    }
    fn rollback(&mut self) {
        let h = self.storage.rollback_last_block().unwrap();
        self.tip_block = self.storage.get_block_by_height(h).unwrap().unwrap();
        self.tip = ChainTip { height: h, id: self.tip_block.id.clone() };
        self.slot -= 1;
    }
    fn bal(&self, addr: &str, id: &str) -> u64 {
        self.storage.get_wallet(addr).unwrap().unwrap().tokens.get(id).copied().unwrap_or(0)
    }
}

#[test]
fn wire_format_round_trips_and_activation_gate() {
    let keys = KeyPair::from_passphrase("alice").unwrap();
    let id = "ab".repeat(32);
    let t = sign(xfer(&keys, 1, &id, &[("SSU6TvycebBMTvc8WHiKTfG9xmX1QSniZu", 500), ("ShJFMECnwbcGXep2AaEV4BzGjBnde9krtH", 7)], "hi"), &keys);
    let bytes = serialize_transaction(&t, SerializeOptions::default(), Network::mainnet_ref()).unwrap();
    assert_eq!(&bytes[3..7], &3u32.to_le_bytes(), "typeGroup 3");
    assert_eq!(&bytes[7..9], &1u16.to_le_bytes());
    let payload = &bytes[9 + 8 + 33 + 8 + 1..];
    assert_eq!(&payload[..32], &[0xab; 32]);
    assert_eq!(&payload[32..34], &2u16.to_le_bytes(), "recipient count");
    let back = deserialize_transaction(&bytes).unwrap();
    assert_eq!(back.token_asset(), t.token_asset());
    assert_eq!(back.id, t.id);
    for t in [sign(init(&keys, 1, &id, 3, 100, 1000), &keys), sign(mint(&keys, 1, &id, "SSU6TvycebBMTvc8WHiKTfG9xmX1QSniZu", 5), &keys), sign(burn(&keys, 1, &id, 5), &keys)] {
        let bytes = serialize_transaction(&t, SerializeOptions::default(), Network::mainnet_ref()).unwrap();
        let back = deserialize_transaction(&bytes).unwrap();
        assert_eq!((back.token_asset(), back.id), (t.token_asset(), t.id));
    }
    // mainnet today: tokens are not activated → block rejected exactly like legacy would
    let mut c = Chain::new(Network::mainnet());
    let err = c.try_block(vec![sign(burn(&keys, 1, &id, 5), &keys)]).unwrap_err();
    assert!(err.contains("before tokens activation"), "{err}");
}

#[test]
fn token_lifecycle_rules_and_rollback() {
    let mut c = Chain::new(network_tokens());
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let bob = KeyPair::from_passphrase("bob").unwrap();
    let (a_addr, b_addr) = (alice.address(63).unwrap(), bob.address(63).unwrap());
    c.storage.update_wallet_state(&a_addr, 100_000_000_000_000, 0).unwrap();
    c.storage.update_wallet_state(&b_addr, 100_000_000_000_000, 0).unwrap();

    // ticker sObject (type 5) — the registry; lowercase names cannot become tokens
    let reg = sign(ticker(&alice, 1, "COFFEE"), &alice);
    let bad_name = sign(ticker(&bob, 1, "tea"), &bob);
    c.try_block(vec![reg.clone(), bad_name.clone()]).unwrap();
    let (id, bad_id) = (reg.id.clone().unwrap(), bad_name.id.clone().unwrap());

    // rules before init
    let e = c.try_block(vec![sign(init(&bob, 2, &id, 3, 1_000, 10_000), &bob)]).unwrap_err();
    assert!(e.contains("TokenNotSmartObjectOwnerError"), "{e}");
    let e = c.try_block(vec![sign(init(&bob, 2, &bad_id, 3, 1_000, 10_000), &bob)]).unwrap_err();
    assert!(e.contains("TokenSymbolInvalidError"), "{e}");
    let e = c.try_block(vec![sign(init(&alice, 2, &id, 0, 1_000, 10_000), &alice)]).unwrap_err();
    assert!(e.contains("TokenSupplyInvalidError"), "{e}");
    let e = c.try_block(vec![sign(token_tx(&alice, 2, token::INIT, 5, serde_json::json!({ "id": id, "decimals": 2, "flags": 3, "initialSupply": "1000", "supplyCap": "10000" })), &alice)]).unwrap_err();
    assert!(e.contains("StaticFeeMismatchError"), "{e}");
    let e = c.try_block(vec![sign(xfer(&alice, 2, &id, &[(&b_addr, 1)], ""), &alice)]).unwrap_err();
    assert!(e.contains("TokenNotFoundError"), "{e}");

    // init: mintable + burnable, 1 000 of 10 000; half of the fee is burned
    let burn_addr = c.network.burn_address.clone();
    let bal_of = |c: &Chain, a: &str| c.storage.get_wallet(a).unwrap().map(|w| w.balance).unwrap_or(0);
    let (forger_before, burn_before) = (bal_of(&c, &c.forger.address(63).unwrap()), bal_of(&c, &burn_addr));
    c.try_block(vec![sign(init(&alice, 2, &id, 3, 1_000, 10_000), &alice)]).unwrap();
    let st = c.storage.token_state(&id).unwrap().unwrap();
    assert_eq!((st.symbol.as_str(), st.decimals, st.flags, st.supply, st.supply_cap, st.owner.as_str()), ("COFFEE", 2, 3, 1_000, 10_000, a_addr.as_str()));
    assert_eq!(c.bal(&a_addr, &id), 1_000);
    assert_eq!(c.storage.token_id_by_symbol("COFFEE").unwrap().as_deref(), Some(id.as_str()));
    assert_eq!(bal_of(&c, &burn_addr) - burn_before, (INIT_FEE / 2) as i64, "half of the init fee is burned");
    assert_eq!(bal_of(&c, &c.forger.address(63).unwrap()) - forger_before, (INIT_FEE / 2) as i64, "forger gets the other half");
    let e = c.try_block(vec![sign(init(&alice, 3, &id, 3, 1_000, 10_000), &alice)]).unwrap_err();
    assert!(e.contains("TokenAlreadyInitializedError"), "{e}");

    // transfer (multi-recipient), insufficient balance, mint by non-owner, cap, burn
    c.try_block(vec![sign(xfer(&alice, 3, &id, &[(&b_addr, 300), (&c.forger.address(63).unwrap(), 100)], "pay"), &alice)]).unwrap();
    assert_eq!((c.bal(&a_addr, &id), c.bal(&b_addr, &id)), (600, 300));
    let e = c.try_block(vec![sign(xfer(&bob, 2, &id, &[(&a_addr, 301)], ""), &bob)]).unwrap_err();
    assert!(e.contains("TokenInsufficientBalanceError"), "{e}");
    let e = c.try_block(vec![sign(mint(&bob, 2, &id, &b_addr, 1), &bob)]).unwrap_err();
    assert!(e.contains("TokenNotOwnerError"), "{e}");
    let e = c.try_block(vec![sign(mint(&alice, 4, &id, &b_addr, 9_001), &alice)]).unwrap_err();
    assert!(e.contains("TokenSupplyCapExceededError"), "{e}");
    c.try_block(vec![sign(mint(&alice, 4, &id, &b_addr, 9_000), &alice)]).unwrap();
    assert_eq!((c.storage.token_state(&id).unwrap().unwrap().supply, c.bal(&b_addr, &id)), (10_000, 9_300));

    // burn by a holder (not the owner) lowers supply in the owner's registry entry; rollback restores everything
    c.storage.set_undo_enabled(true);
    c.try_block(vec![sign(burn(&bob, 2, &id, 300), &bob)]).unwrap();
    assert_eq!((c.storage.token_state(&id).unwrap().unwrap().supply, c.bal(&b_addr, &id)), (9_700, 9_000));
    c.storage.rollback_last_block().unwrap();
    assert_eq!((c.storage.token_state(&id).unwrap().unwrap().supply, c.bal(&b_addr, &id)), (10_000, 9_300));

    // rollback of the whole token: index entries disappear
    let mut c2 = Chain::new(network_tokens());
    c2.storage.update_wallet_state(&a_addr, 100_000_000_000_000, 0).unwrap();
    let reg = sign(ticker(&alice, 1, "TEA"), &alice);
    c2.try_block(vec![reg.clone()]).unwrap();
    c2.storage.set_undo_enabled(true);
    let tid = reg.id.clone().unwrap();
    c2.try_block(vec![sign(init(&alice, 2, &tid, 0, 500, 500), &alice)]).unwrap();
    assert!(c2.storage.token_state(&tid).unwrap().is_some());
    c2.storage.rollback_last_block().unwrap();
    assert!(c2.storage.token_state(&tid).unwrap().is_none());
    assert!(c2.storage.token_owner(&tid).unwrap().is_none());
    assert!(c2.storage.token_id_by_symbol("TEA").unwrap().is_none());
    assert_eq!(c2.bal(&a_addr, &tid), 0);
}

#[test]
fn token_meta_manifest_on_chain() {
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let bob = KeyPair::from_passphrase("bob").unwrap();
    let id = "cd".repeat(32);
    // wire format round-trips the whole manifest including the logo bytes
    let full = serde_json::json!({ "name": "Coffee Points", "description": "Баллы", "website": "https://coffee.example", "logoType": "svg", "logo": b64(SVG.as_bytes()) });
    let t = sign(meta(&alice, 1, &id, full.clone()), &alice);
    let bytes = serialize_transaction(&t, SerializeOptions::default(), Network::mainnet_ref()).unwrap();
    let back = deserialize_transaction(&bytes).unwrap();
    assert_eq!(back.token_asset().unwrap().meta, t.token_asset().unwrap().meta);
    assert_eq!(back.id, t.id);
    let minimal = sign(meta(&alice, 1, &id, serde_json::json!({ "name": "X" })), &alice);
    let back = deserialize_transaction(&serialize_transaction(&minimal, SerializeOptions::default(), Network::mainnet_ref()).unwrap()).unwrap();
    assert_eq!(back.token_asset().unwrap().meta, minimal.token_asset().unwrap().meta);

    let mut c = Chain::new(network_tokens());
    let a_addr = alice.address(63).unwrap();
    c.storage.update_wallet_state(&a_addr, 100_000_000_000_000, 0).unwrap();
    c.storage.update_wallet_state(&bob.address(63).unwrap(), 100_000_000_000_000, 0).unwrap();
    let reg = sign(ticker(&alice, 1, "COFFEE"), &alice);
    c.try_block(vec![reg.clone()]).unwrap();
    let id = reg.id.clone().unwrap();
    let e = c.try_block(vec![sign(meta(&alice, 2, &id, full.clone()), &alice)]).unwrap_err();
    assert!(e.contains("TokenNotFoundError"), "{e}");
    c.try_block(vec![sign(init(&alice, 2, &id, 3, 1_000, 10_000), &alice)]).unwrap();

    // format rules: logo type without logo, PNG magic, oversized logo, scripts in SVG, non-owner
    for (m, want) in [
        (serde_json::json!({ "name": "" }), "TokenMetaInvalidError"),
        (serde_json::json!({ "name": "X", "logoType": "svg" }), "TokenMetaInvalidError"),
        (serde_json::json!({ "name": "X", "logoType": "gif", "logo": b64(b"GIF89a") }), "TokenMetaInvalidError"),
        (serde_json::json!({ "name": "X", "logoType": "png", "logo": b64(b"not a png") }), "TokenLogoInvalidError"),
        (serde_json::json!({ "name": "X", "logoType": "svg", "logo": b64(&vec![b'<'; 8193]) }), "TokenLogoTooLargeError"),
        (serde_json::json!({ "name": "X", "logoType": "svg", "logo": b64(br#"<svg><script>alert(1)</script></svg>"#) }), "TokenLogoInvalidError"),
        (serde_json::json!({ "name": "X", "website": "has space" }), "TokenMetaInvalidError"),
    ] {
        let e = c.try_block(vec![sign(meta(&alice, 3, &id, m), &alice)]).unwrap_err();
        assert!(e.contains(want), "{e} (want {want})");
    }
    let e = c.try_block(vec![sign(meta(&bob, 1, &id, serde_json::json!({ "name": "Fake" })), &bob)]).unwrap_err();
    assert!(e.contains("TokenNotOwnerError"), "{e}");
    let e = c.try_block(vec![sign(token_tx(&alice, 3, token::META, 1, serde_json::json!({ "id": id, "meta": { "name": "X" } })), &alice)]).unwrap_err();
    assert!(e.contains("StaticFeeMismatchError"), "{e}");

    // publish, then update (last wins); rollback restores the previous manifest
    let png = [&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a][..], &[0u8; 100][..]].concat();
    c.try_block(vec![sign(meta(&alice, 3, &id, serde_json::json!({ "name": "Coffee", "logoType": "png", "logo": b64(&png) })), &alice)]).unwrap();
    let m = c.storage.token_state(&id).unwrap().unwrap().meta.unwrap();
    assert_eq!((m.name.as_str(), m.logo_type.as_deref(), m.logo_bytes().unwrap().len()), ("Coffee", Some("png"), 108));
    c.storage.set_undo_enabled(true);
    c.try_block(vec![sign(meta(&alice, 4, &id, full), &alice)]).unwrap();
    let m = c.storage.token_state(&id).unwrap().unwrap().meta.unwrap();
    assert_eq!((m.name.as_str(), m.mime()), ("Coffee Points", Some("image/svg+xml")));
    assert_eq!(m.logo_bytes().unwrap(), SVG.as_bytes());
    c.storage.rollback_last_block().unwrap();
    assert_eq!(c.storage.token_state(&id).unwrap().unwrap().meta.unwrap().name, "Coffee");
    // supply untouched by manifests
    assert_eq!(c.storage.token_state(&id).unwrap().unwrap().supply, 1_000);
}

/// `tokenFees.initBurnPercent` is a milestone parameter: 50 % up to height 3, then 0 % (burn disabled) — the burn address stops
/// growing and the forger keeps the whole fee; rollback across the switch restores both balances; > 100 is rejected at load.
#[test]
fn init_burn_percent_switches_by_milestone() {
    let ms = MAINNET_MILESTONES_JSON.replace("\"ship11\": true", "\"ship11\": true, \"aip36\": true, \"tokens\": true");
    let mut list: Vec<serde_json::Value> = serde_json::from_str(&ms).unwrap();
    list.push(serde_json::json!({ "height": 4, "tokenFees": { "initBurnPercent": 0 } }));
    let net = Network::from_json(MAINNET_NETWORK_JSON, &serde_json::to_string(&list).unwrap(), MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap();
    assert_eq!(net.milestone(3).token_fees.init_burn(), INIT_FEE / 2);
    assert_eq!((net.milestone(4).token_fees.init_burn(), net.milestone(4).token_fees.init), (0, INIT_FEE), "only the percent changes, the fee is inherited");

    let mut bad = list.clone();
    bad.push(serde_json::json!({ "height": 5, "tokenFees": { "initBurnPercent": 101 } }));
    let e = Network::from_json(MAINNET_NETWORK_JSON, &serde_json::to_string(&bad).unwrap(), MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap_err().to_string();
    assert!(e.contains("initBurnPercent"), "{e}");

    let mut c = Chain::new(net);
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let a_addr = alice.address(63).unwrap();
    c.storage.update_wallet_state(&a_addr, 100_000_000_000_000, 0).unwrap();
    let burn_addr = c.network.burn_address.clone();
    let forger_addr = c.forger.address(63).unwrap();
    let bal_of = |c: &Chain, a: &str| c.storage.get_wallet(a).unwrap().map(|w| w.balance).unwrap_or(0);

    // height 2: two tickers; height 3 (50 %): init COFFEE
    let (r1, r2) = (sign(ticker(&alice, 1, "COFFEE"), &alice), sign(ticker(&alice, 2, "BEANS"), &alice));
    c.try_block(vec![r1.clone(), r2.clone()]).unwrap();
    let (f0, b0) = (bal_of(&c, &forger_addr), bal_of(&c, &burn_addr));
    c.storage.set_undo_enabled(true);
    c.try_block(vec![sign(init(&alice, 3, r1.id.as_ref().unwrap(), 0, 100, 100), &alice)]).unwrap();
    assert_eq!(bal_of(&c, &burn_addr) - b0, (INIT_FEE / 2) as i64);
    assert_eq!(bal_of(&c, &forger_addr) - f0, (INIT_FEE / 2) as i64);

    // height 4 (0 %): init BEANS — nothing burned, forger takes the full fee
    let (f1, b1) = (bal_of(&c, &forger_addr), bal_of(&c, &burn_addr));
    c.try_block(vec![sign(init(&alice, 4, r2.id.as_ref().unwrap(), 0, 100, 100), &alice)]).unwrap();
    assert_eq!(bal_of(&c, &burn_addr) - b1, 0, "burn disabled by milestone");
    assert_eq!(bal_of(&c, &forger_addr) - f1, INIT_FEE as i64, "forger keeps the whole fee");

    // rollback both init blocks: balances return to the pre-init state
    c.storage.rollback_to(2).unwrap();
    assert_eq!((bal_of(&c, &forger_addr), bal_of(&c, &burn_addr)), (f0, b0));
}

/// SHIP-43 stage A: TokenSell / TokenBuy / TokenCancel have a fixed wire format and fees, are rejected before the
/// `tokenMarket` milestone and — until the order state machine lands — are refused by the wallet-aware check too.
#[test]
fn token_market_types_wire_format_and_activation_gate() {
    use sth_core::rules::check_token_format;
    let keys = KeyPair::from_passphrase("market maker").unwrap();
    let net = network_tokens();
    let token_id = "ab".repeat(32);
    let order_id = "cd".repeat(32);
    let ms = net.milestone(11_900_000);
    assert!(!ms.token_market.enabled);
    assert_eq!((ms.token_fees.sell, ms.token_fees.buy, ms.token_fees.cancel), (100_000_000, 10_000_000, 10_000_000));

    let sell = sign(token_tx(&keys, 1, token::SELL, 100_000_000, serde_json::json!({ "id": token_id, "amount": "100", "price": "250000000" })), &keys);
    let buy = sign(token_tx(&keys, 2, token::BUY, 10_000_000, serde_json::json!({ "id": token_id, "orderId": order_id, "amount": "40" })), &keys);
    let cancel = sign(token_tx(&keys, 3, token::CANCEL, 10_000_000, serde_json::json!({ "id": token_id, "orderId": order_id })), &keys);
    for t in [&sell, &buy, &cancel] {
        assert!(t.is_token());
        let bytes = serialize_transaction(t, SerializeOptions::default(), &net).unwrap();
        let back = deserialize_transaction(&bytes).unwrap();
        assert_eq!(back.token_asset(), t.token_asset(), "wire round-trip of type {}", t.type_);
        assert_eq!(transaction_id(&back).unwrap(), t.id.clone().unwrap());
        // not active on mainnet milestones
        let err = check_token_format(t, ms).unwrap_err();
        assert!(err.starts_with("TokenTypeNotActiveError"), "{err}");
    }
    let a = sell.token_asset().unwrap();
    assert_eq!((a.amount, a.price), (Some(100), Some(250_000_000)));
    assert_eq!(buy.token_asset().unwrap().order_id.as_deref(), Some(order_id.as_str()));

    // enabled milestone: format + fee rules apply, wallet-aware check still refuses (stage B pending)
    let ms_on = MAINNET_MILESTONES_JSON.replace("\"ship11\": true", "\"ship11\": true, \"aip36\": true, \"tokens\": true, \"tokenMarket\": { \"enabled\": true }");
    let net_on = Network::from_json(MAINNET_NETWORK_JSON, &ms_on, MAINNET_EXCEPTIONS_JSON, MAINNET_GENESIS_GZ).unwrap();
    let ms_on = net_on.milestone(11_900_000);
    assert!(ms_on.token_market.enabled);
    assert!(check_token_format(&sell, ms_on).is_ok());
    assert!(check_token_format(&buy, ms_on).is_ok());
    assert!(check_token_format(&cancel, ms_on).is_ok());
    let bad_fee = sign(token_tx(&keys, 4, token::SELL, 1, serde_json::json!({ "id": token_id, "amount": "1", "price": "1" })), &keys);
    assert!(check_token_format(&bad_fee, ms_on).unwrap_err().starts_with("StaticFeeMismatchError"));
    let zero = sign(token_tx(&keys, 5, token::SELL, 100_000_000, serde_json::json!({ "id": token_id, "amount": "0", "price": "1" })), &keys);
    assert!(check_token_format(&zero, ms_on).unwrap_err().starts_with("TokenOrderInvalidError"));
    let overflow = sign(token_tx(&keys, 6, token::SELL, 100_000_000, serde_json::json!({ "id": token_id, "amount": "18446744073709551615", "price": "2" })), &keys);
    assert!(check_token_format(&overflow, ms_on).unwrap_err().starts_with("TokenOrderOverflowError"));
    // a short orderId cannot even be serialised (32 bytes on the wire); the format check rejects the JSON form too
    let short_oid = token_tx(&keys, 7, token::CANCEL, 10_000_000, serde_json::json!({ "id": token_id, "orderId": "abcd" }));
    assert!(serialize_transaction(&short_oid, SerializeOptions::default(), &net_on).is_err());
    assert!(check_token_format(&short_oid, ms_on).unwrap_err().starts_with("TokenOrderInvalidError"));

    struct NoView;
    impl sth_core::rules::TokenView for NoView {
        fn token_owner(&self, _: &str) -> Option<String> { None }
        fn token_state(&self, _: &str) -> Option<sth_core::storage::TokenState> { None }
        fn token_balance(&self, _: &str, _: &str) -> u64 { u64::MAX }
        fn ticker_sobj(&self, _: &str) -> Option<(String, String, bool)> { None }
        fn order(&self, _: &str) -> Option<sth_core::storage::TokenOrder> { None }
        fn open_orders(&self, _: &str, _: &str) -> usize { 0 }
    }
    let err = sth_core::rules::check_token(&sell, sell.token_asset().as_ref().unwrap(), &keys.address(63).unwrap(), &NoView).unwrap_err();
    assert!(err.starts_with("TokenNotFoundError"), "{err}");
    let err = sth_core::rules::check_token(&buy, buy.token_asset().as_ref().unwrap(), &keys.address(63).unwrap(), &NoView).unwrap_err();
    assert!(err.starts_with("TokenOrderNotFoundError"), "{err}");
}

/// SHIP-43 stage B: escrow on sell, partial fills with the STH leg, cancel returns the remainder, exact rollback,
/// mempool sees pending fills, order book API.
#[tokio::test]
async fn token_market_orders_fill_cancel_and_rollback() {
    let mut c = Chain::new(network_market());
    let alice = KeyPair::from_passphrase("alice").unwrap();
    let bob = KeyPair::from_passphrase("bob").unwrap();
    let carol = KeyPair::from_passphrase("carol").unwrap();
    let (a, b, k) = (alice.address(63).unwrap(), bob.address(63).unwrap(), carol.address(63).unwrap());
    for addr in [&a, &b, &k] {
        c.storage.update_wallet_state(addr, 100_000_000_000_000, 0).unwrap();
    }
    let reg = sign(ticker(&alice, 1, "UFO"), &alice);
    c.try_block(vec![reg.clone()]).unwrap();
    let id = reg.id.clone().unwrap();
    c.try_block(vec![sign(init(&alice, 2, &id, 0, 8_000, 8_000), &alice)]).unwrap(); // 80.00 UFO (2 decimals)
    c.storage.set_undo_enabled(true);
    let sth = |c: &Chain, addr: &str| c.storage.get_wallet(addr).unwrap().unwrap().balance;
    let (a0, b0) = (sth(&c, &a), sth(&c, &b));

    // rules before any order
    let e = c.try_block(vec![sign(sell(&alice, 3, &id, 9_000, 1), &alice)]).unwrap_err();
    assert!(e.contains("TokenInsufficientBalanceError"), "{e}");
    let e = c.try_block(vec![sign(buy(&bob, 1, &id, &"0".repeat(64), 1), &bob)]).unwrap_err();
    assert!(e.contains("TokenOrderNotFoundError"), "{e}");

    // ask: 1.00 UFO (100 units) at 1 STH per unit → escrow leaves alice's balance
    let ask = sign(sell(&alice, 3, &id, 100, 100_000_000), &alice);
    c.try_block(vec![ask.clone()]).unwrap();
    let oid = ask.id.clone().unwrap();
    assert_eq!(c.bal(&a, &id), 7_900);
    let o = c.storage.token_order(&oid).unwrap().unwrap();
    assert_eq!((o.remaining(), o.price, o.is_open(), o.seller.as_str()), (100, 100_000_000, true, a.as_str()));
    assert_eq!(c.storage.token_orders(&id).unwrap().len(), 1);
    assert_eq!(c.storage.wallet_orders(&a).unwrap().len(), 1);

    // buyer rules
    let e = c.try_block(vec![sign(buy(&alice, 4, &id, &oid, 1), &alice)]).unwrap_err();
    assert!(e.contains("TokenOrderSelfTradeError"), "{e}");
    let e = c.try_block(vec![sign(buy(&bob, 1, &id, &oid, 101), &bob)]).unwrap_err();
    assert!(e.contains("TokenOrderAmountError"), "{e}");
    let e = c.try_block(vec![sign(cancel(&bob, 1, &id, &oid), &bob)]).unwrap_err();
    assert!(e.contains("TokenOrderNotOwnerError"), "{e}");
    let poor = KeyPair::from_passphrase("poor").unwrap();
    c.storage.update_wallet_state(&poor.address(63).unwrap(), 1_000_000_000, 0).unwrap(); // 10 STH: fee ok, 40 STH leg not
    let e = c.try_block(vec![sign(buy(&poor, 1, &id, &oid, 40), &poor)]).unwrap_err();
    assert!(e.contains("InsufficientBalanceError"), "{e}");

    // partial fill: bob buys 40 units for 40 STH; two fills in one block, the second exceeding the remainder fails the block
    c.try_block(vec![sign(buy(&bob, 1, &id, &oid, 40), &bob)]).unwrap();
    assert_eq!(c.bal(&b, &id), 40);
    assert_eq!(sth(&c, &b), b0 - 40 * 100_000_000 - 10_000_000);
    assert_eq!(sth(&c, &a), a0 - 100_000_000 + 40 * 100_000_000, "seller got the STH leg minus her sell fee");
    let o = c.storage.token_order(&oid).unwrap().unwrap();
    assert_eq!((o.filled, o.remaining(), o.fills.len()), (40, 60, 1));
    let e = c.try_block(vec![sign(buy(&carol, 1, &id, &oid, 50), &carol), sign(buy(&bob, 2, &id, &oid, 11), &bob)]).unwrap_err();
    assert!(e.contains("TokenOrderAmountError"), "{e}");
    assert_eq!(c.bal(&k, &id), 0, "failed block left no trace");

    // the fill is recorded as a trade; rollback removes it
    let trades: Vec<_> = c.storage.token_trades(&id).map(|t| t.unwrap()).collect();
    assert_eq!(trades.len(), 1);
    assert_eq!((trades[0].buyer.as_str(), trades[0].seller.as_str(), trades[0].amount, trades[0].price, trades[0].total), (b.as_str(), a.as_str(), 40, 100_000_000, 40 * 100_000_000));
    assert_eq!(trades[0].order_id, oid);

    // rollback of the partial fill restores balances and the order
    c.rollback();
    assert_eq!(c.storage.token_trades(&id).count(), 0);
    assert_eq!((c.bal(&b, &id), sth(&c, &b)), (0, b0));
    let o = c.storage.token_order(&oid).unwrap().unwrap();
    assert_eq!((o.filled, o.remaining(), o.fills.len(), o.is_open()), (0, 100, 0, true));

    // full fill in two buys inside one block closes the order and drops it from the book; two trades, newest first
    c.try_block(vec![sign(buy(&carol, 1, &id, &oid, 50), &carol), sign(buy(&bob, 1, &id, &oid, 50), &bob)]).unwrap();
    let trades: Vec<_> = c.storage.token_trades(&id).map(|t| t.unwrap()).collect();
    assert_eq!(trades.iter().map(|t| t.buyer.as_str()).collect::<Vec<_>>(), vec![b.as_str(), k.as_str()]);
    let o = c.storage.token_order(&oid).unwrap().unwrap();
    assert_eq!((o.remaining(), o.is_open(), o.closed), (0, false, Some(c.tip.height)));
    assert!(c.storage.token_orders(&id).unwrap().is_empty());
    assert!(c.storage.wallet_orders(&a).unwrap().is_empty());
    assert_eq!((c.bal(&k, &id), c.bal(&b, &id)), (50, 50));
    let e = c.try_block(vec![sign(buy(&bob, 2, &id, &oid, 1), &bob)]).unwrap_err();
    assert!(e.contains("TokenOrderClosedError"), "{e}");
    c.rollback();
    assert!(c.storage.token_order(&oid).unwrap().unwrap().is_open());
    assert_eq!(c.storage.token_orders(&id).unwrap().len(), 1, "reopened order is back in the book");

    // cancel: remainder returns to alice; rollback of the cancel re-escrows it; rollback of the sell removes the order
    c.try_block(vec![sign(cancel(&alice, 4, &id, &oid), &alice)]).unwrap();
    assert_eq!(c.bal(&a, &id), 8_000);
    assert!(!c.storage.token_order(&oid).unwrap().unwrap().is_open());
    let e = c.try_block(vec![sign(cancel(&alice, 5, &id, &oid), &alice)]).unwrap_err();
    assert!(e.contains("TokenOrderClosedError"), "{e}");
    c.rollback();
    assert_eq!(c.bal(&a, &id), 7_900);
    assert!(c.storage.token_order(&oid).unwrap().unwrap().is_open());
    c.rollback();
    assert_eq!(c.bal(&a, &id), 8_000);
    assert!(c.storage.token_order(&oid).unwrap().is_none(), "sell rolled back → no record");
    assert!(c.storage.token_orders(&id).unwrap().is_empty());

    // mempool: a pending buy reduces what a second buyer may take; sell escrow counts as pending spend
    let ask = sign(sell(&alice, 3, &id, 100, 100_000_000), &alice);
    c.try_block(vec![ask.clone()]).unwrap();
    let oid = ask.id.clone().unwrap();
    let pool = sth_core::mempool::Mempool::new(c.storage.clone(), vec![], 100);
    let (r, errs) = pool.add_many(vec![sign(buy(&bob, 1, &id, &oid, 70), &bob), sign(buy(&carol, 1, &id, &oid, 40), &carol)]).await;
    assert_eq!(r.accept.len(), 1, "{errs:?}");
    assert!(errs.values().any(|e| e.message.contains("TokenOrderAmountError")), "{errs:?}");
    let (_, errs) = pool.add_many(vec![sign(sell(&alice, 4, &id, 7_950, 1), &alice)]).await;
    assert!(errs.values().any(|e| e.message.contains("TokenInsufficientBalanceError")), "{errs:?}");

    // API: order book + wallet orders
    let mempool = Arc::new(sth_core::mempool::Mempool::new(c.storage.clone(), vec![], 100));
    let state = Arc::new(sth_core::api::AppState::new(c.storage.clone(), mempool, vec![]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { sth_core::api::serve_listener(state, listener).await.unwrap() });
    let book: serde_json::Value = reqwest::get(format!("{base}/api/tokens/UFO/orders")).await.unwrap().json().await.unwrap();
    assert_eq!(book["meta"]["totalCount"], 1);
    assert_eq!(book["data"][0]["orderId"], oid);
    assert_eq!(book["data"][0]["remaining"], "100");
    assert_eq!(book["data"][0]["total"], (100u64 * 100_000_000).to_string());
    assert_eq!(book["data"][0]["symbol"], "UFO");
    let one: serde_json::Value = reqwest::get(format!("{base}/api/tokens/{id}/orders/{oid}")).await.unwrap().json().await.unwrap();
    assert_eq!(one["data"]["open"], true);
    let mine: serde_json::Value = reqwest::get(format!("{base}/api/wallets/{a}/orders")).await.unwrap().json().await.unwrap();
    assert_eq!(mine["data"].as_array().unwrap().len(), 1);
    let types: serde_json::Value = reqwest::get(format!("{base}/api/transactions/types")).await.unwrap().json().await.unwrap();
    assert_eq!(types["data"]["3"]["TokenSell"], 5);

    // trades API: a new fill at a different price → list newest first + one candle with OHLC
    let ask2 = sign(sell(&alice, 4, &id, 10, 200_000_000), &alice);
    c.try_block(vec![ask2.clone()]).unwrap();
    c.try_block(vec![sign(buy(&bob, 1, &id, &oid, 30), &bob), sign(buy(&carol, 1, &id, &ask2.id.clone().unwrap(), 10), &carol)]).unwrap();
    let list: serde_json::Value = reqwest::get(format!("{base}/api/tokens/UFO/trades?limit=1")).await.unwrap().json().await.unwrap();
    assert_eq!(list["meta"]["totalCount"], 2);
    assert_eq!(list["data"][0]["buyer"], k);
    assert_eq!(list["data"][0]["price"], "200000000");
    assert_eq!(list["data"][0]["total"], (10u64 * 200_000_000).to_string());
    assert_eq!(list["data"][0]["symbol"], "UFO");
    let list: serde_json::Value = reqwest::get(format!("{base}/api/tokens/UFO/trades?limit=1&page=2")).await.unwrap().json().await.unwrap();
    assert_eq!(list["data"][0]["buyer"], b);
    let candles: serde_json::Value = reqwest::get(format!("{base}/api/tokens/UFO/trades?groupBy=day&days=0")).await.unwrap().json().await.unwrap();
    let cs = candles["data"].as_array().unwrap();
    assert_eq!(cs.len(), 1, "both fills in one block → one day: {candles}");
    assert_eq!((cs[0]["open"].as_str(), cs[0]["close"].as_str(), cs[0]["high"].as_str(), cs[0]["low"].as_str()), (Some("100000000"), Some("200000000"), Some("200000000"), Some("100000000")));
    assert_eq!((cs[0]["volume"].as_str(), cs[0]["trades"].as_u64()), (Some("40"), Some(2)));
}
