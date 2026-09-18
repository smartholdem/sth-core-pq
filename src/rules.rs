//! Author: TechnoL0g
//!
//! Stateful transaction rules that need the sender's wallet (legacy `TransactionHandler.throwIfCannotBeApplied`):
//! second-signature enforcement, exactly as Node.js nodes apply it.

use crate::crypto::verify_transaction_second_signature;
use crate::models::{tx_type, Transaction};

/// Second-signature rule for one transaction given the sender's registered second public key.
/// Errors carry the legacy error names so API / log messages match the old core.
pub fn check_second_signature(tx: &Transaction, second_public_key: Option<&str>) -> Result<(), String> {
    if tx.is_pq() {
        // v3: the legacy key is proven inside the second-signature blocks (check_pq)
        return Ok(());
    }
    match second_public_key {
        Some(pk) => {
            if tx.type_group == crate::models::TYPE_GROUP_CORE && tx.type_ == tx_type::SECOND_SIGNATURE {
                return Err("SecondSignatureAlreadyRegisteredError: wallet already has a second signature".into());
            }
            match verify_transaction_second_signature(tx, pk) {
                Ok(true) => Ok(()),
                Ok(false) if tx.second_signature_any().is_none() => {
                    Err("MissingSecondSignatureError: transaction requires a second signature".into())
                }
                Ok(false) => Err("InvalidSecondSignatureError: second signature is invalid".into()),
                Err(e) => Err(format!("InvalidSecondSignatureError: {e}")),
            }
        }
        None if tx.second_signature_any().is_some() => {
            Err("UnexpectedSecondSignatureError: wallet has no second signature".into())
        }
        None => Ok(()),
    }
}

/// Second public key a `secondSignature` registration installs (33-byte compressed secp256k1, hex).
pub fn registered_second_key(tx: &Transaction) -> Result<Option<&str>, String> {
    if tx.type_group != crate::models::TYPE_GROUP_CORE || tx.type_ != tx_type::SECOND_SIGNATURE || tx.is_pq() {
        return Ok(None);
    }
    let pk = tx
        .asset
        .as_ref()
        .and_then(|a| a.signature.as_ref())
        .map(|s| s.public_key.as_str())
        .ok_or("SecondSignatureRegistration without asset.signature.publicKey")?;
    let valid = pk.len() == 66 && (pk.starts_with("02") || pk.starts_with("03")) && hex::decode(pk).is_ok();
    if !valid {
        return Err(format!("SecondSignatureRegistration: invalid public key {pk}"));
    }
    Ok(Some(pk))
}

// ------------------------------------------------------------------ smart objects (sObjects)

/// Format-level checks of an sObject transaction (no wallet state): schema, static fee, amount.
/// `name`/`ipfsData` limits follow the legacy core schemas (SHIP-13 legacy rules). With milestone
/// `sobjV2` the pointer field `ntfryData` is any UTF-8 text up to 255 bytes and type-5 names must be tickers.
pub fn check_sobj_format(tx: &Transaction, ms: &crate::config::Milestone) -> Result<crate::models::SmartObjectAsset, String> {
    let e = tx.sobj_asset().ok_or("SmartObject: missing or malformed asset")?;
    if tx.amount != 0 {
        return Err("SmartObject: amount must be 0".into());
    }
    let fee = crate::models::sobj::static_fee(e.action).ok_or_else(|| format!("SmartObject: unknown action {}", e.action))? + tx.pq_surcharge(ms.pq.fee_per_byte);
    if tx.fee != fee {
        return Err(format!("StaticFeeMismatchError: sObject fee must be exactly {fee}"));
    }
    use crate::models::sobj::{ACTION_BUY, ACTION_SELL, ACTION_TRANSFER};
    let transfer = e.action == ACTION_TRANSFER;
    let market = matches!(e.action, ACTION_TRANSFER | ACTION_SELL | ACTION_BUY);
    if market && !ms.sobj_v2 {
        return Err("SmartObjectTransferNotActiveError: sObject transfer / sell / buy need milestone sobjV2".into());
    }
    if e.recipient_id.is_some() != transfer {
        return Err("SmartObject: recipientId is only valid for transfer (action 3)".into());
    }
    if e.price.is_some() != (e.action == ACTION_SELL) {
        return Err("SmartObject: price is only valid for sell (action 4)".into());
    }
    if let Some(r) = &e.recipient_id {
        if crate::crypto::address_to_bytes(r).is_err() {
            return Err("SmartObject: invalid recipientId".into());
        }
    }
    let name_ok = |n: &str| (1..=40).contains(&n.len()) && n.chars().all(|c| c.is_ascii_alphanumeric() || "_!@$&.-".contains(c));
    let data_ok = |s: &str| {
        if ms.sobj_v2 {
            (1..=255).contains(&s.len()) && !s.chars().any(char::is_control)
        } else {
            (1..=128).contains(&s.len()) && bs58::decode(s).into_vec().is_ok()
        }
    };
    if let Some(d) = &e.data.ntfry_data {
        if !data_ok(d) {
            return Err("SmartObject: invalid ntfryData".into());
        }
    }
    match e.action {
        crate::models::sobj::ACTION_REGISTER => {
            if e.registration_id.is_some() {
                return Err("SmartObject: register must not carry registrationId".into());
            }
            match &e.data.name {
                Some(n) if name_ok(n) => {}
                _ => return Err("SmartObject: register requires a valid name".into()),
            }
            if ms.sobj_v2 && e.type_ == crate::models::token::TICKER_SOBJ_TYPE && !ticker_ok(e.data.name.as_deref().unwrap_or_default()) {
                return Err("SmartObjectTickerInvalidError: type-5 name must match ^[A-Z0-9]{3,10}$".into());
            }
        }
        _ => {
            match &e.registration_id {
                Some(id) if id.len() == 64 && hex::decode(id).is_ok() => {}
                _ => return Err("SmartObject: update/resign require registrationId".into()),
            }
            if e.data.name.is_some() {
                return Err("SmartObject: name cannot be changed".into());
            }
            if (e.action == crate::models::sobj::ACTION_RESIGN || market) && e.data.ntfry_data.is_some() {
                return Err("SmartObject: resign / transfer / sell / buy must not carry data".into());
            }
        }
    }
    Ok(e)
}

/// Wallet-aware sObject rules (SHIP-13; mirrors the legacy core handler `throwIfCannotBeApplied`).
/// `name_taken` answers "is (name, type) already registered network-wide"; `pending` = records written earlier in the batch.
pub fn check_sobj(
    tx: &Transaction,
    ms: &crate::config::Milestone,
    wallet: Option<&crate::storage::WalletState>,
    pending: &std::collections::BTreeMap<String, crate::storage::SmartObject>,
    name_taken: impl Fn(&str, u8) -> bool,
    lookup: impl Fn(&str) -> Option<(String, crate::storage::SmartObject)>,
    token_supply: impl Fn(&str) -> Option<u64>,
) -> Result<(), String> {
    let e = check_sobj_format(tx, ms)?;
    let id = tx.id.as_deref().ok_or("SmartObject: transaction without id")?;
    let record = |reg: &str| pending.get(reg).cloned().or_else(|| wallet.and_then(|w| w.sobjects.get(reg).cloned()));
    if e.action == crate::models::sobj::ACTION_BUY {
        // the record lives in the seller's wallet, not the buyer's
        let reg = e.registration_id.as_deref().unwrap_or_default();
        let (owner, rec) = lookup(reg).ok_or("SmartObjectNotRegisteredError")?;
        if rec.resigned {
            return Err("SmartObjectAlreadyResignedError".into());
        }
        if rec.type_ != e.type_ || rec.sub_type != e.sub_type {
            return Err("SmartObjectWrongTypeError".into());
        }
        let price = rec.price.filter(|p| *p > 0).ok_or("SmartObjectNotForSaleError: no open sale order")?;
        if wallet.is_some_and(|w| w.address == owner) {
            return Err("SmartObjectTransferToSelfError: you already own this sObject".into());
        }
        let balance = wallet.map(|w| w.balance).unwrap_or(0);
        if balance < 0 || (balance as u64) < price.saturating_add(tx.fee) {
            return Err(format!("SmartObjectInsufficientBalanceError: price {price} + fee {} needed", tx.fee));
        }
        return Ok(());
    }
    match e.action {
        crate::models::sobj::ACTION_REGISTER => {
            if record(id).is_some() {
                return Err("SmartObjectAlreadyRegisteredError".into());
            }
            let name = e.data.name.as_deref().unwrap_or_default();
            if name_taken(name, e.type_) {
                return Err("SmartObjectNameAlreadyRegisteredError".into());
            }
            if e.type_ == crate::models::sobj::TYPE_DELEGATE {
                match wallet.and_then(|w| w.username.as_deref()) {
                    None => return Err("SmartObjectSenderIsNotDelegateError".into()),
                    Some(u) if u != name => return Err("SmartObjectNameDoesNotMatchDelegateError".into()),
                    _ => {}
                }
            }
        }
        _ => {
            let reg = e.registration_id.as_deref().unwrap_or_default();
            let Some(rec) = record(reg) else { return Err("SmartObjectNotRegisteredError".into()) };
            if rec.resigned {
                return Err("SmartObjectAlreadyResignedError".into());
            }
            if rec.type_ != e.type_ {
                return Err("SmartObjectWrongTypeError".into());
            }
            if e.action == crate::models::sobj::ACTION_TRANSFER {
                let recipient = e.recipient_id.as_deref().unwrap_or_default();
                if wallet.is_some_and(|w| w.address == recipient) {
                    return Err("SmartObjectTransferToSelfError".into());
                }
            }
            if matches!(e.action, crate::models::sobj::ACTION_TRANSFER | crate::models::sobj::ACTION_SELL) && rec.type_ == crate::models::sobj::TYPE_DELEGATE {
                return Err("SmartObjectDelegateNotTransferableError: delegate sObjects follow the delegate key".into());
            }
            if rec.sub_type != e.sub_type {
                return Err("SmartObjectWrongSubTypeError".into());
            }
            // resign guard: a type-5 registry with a live token cannot be orphaned
            if e.action == crate::models::sobj::ACTION_RESIGN && rec.type_ == crate::models::token::TICKER_SOBJ_TYPE && token_supply(reg).is_some_and(|s| s > 0) {
                return Err("TokenSmartObjectStillActiveError: the token has live supply — burn it to 0 before resigning its registry".into());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Native tokens (typeGroup 3) — docs/SPEC-TOKENS-NATIVE.md §3–4
// ---------------------------------------------------------------------------------------------

/// Stateless token checks: exact static fee, amount 0, no vendorField, field ranges. Returns the parsed asset.
pub fn check_token_format(tx: &Transaction, ms: &crate::config::Milestone) -> Result<crate::models::TokenAsset, String> {
    use crate::models::token;
    let a = tx.token_asset().ok_or("Token: missing or malformed asset.token")?;
    if tx.amount != 0 {
        return Err("Token: amount must be 0".into());
    }
    if tx.vendor_field.as_deref().is_some_and(|v| !v.is_empty()) {
        return Err("Token: vendorField is not allowed (use memo)".into());
    }
    if a.id.len() != 64 || !a.id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Token: id must be a 32-byte hex registration id".into());
    }
    let f = &ms.token_fees;
    let expected_fee = match tx.type_ {
        token::INIT => {
            let (d, fl, init, cap) = (a.decimals.unwrap_or(0), a.flags.unwrap_or(0), a.initial_supply.unwrap_or(0), a.supply_cap.unwrap_or(0));
            if d > token::MAX_DECIMALS {
                return Err("TokenDecimalsInvalidError: decimals must be 0..=8".into());
            }
            if fl & !(token::FLAG_MINTABLE | token::FLAG_BURNABLE | token::FLAG_FROZEN_CAP) != 0 {
                return Err("TokenFlagsInvalidError: reserved flag bits set".into());
            }
            if init == 0 || init > cap {
                return Err("TokenSupplyInvalidError: 0 < initialSupply <= supplyCap".into());
            }
            if fl & token::FLAG_MINTABLE == 0 && init != cap {
                return Err("TokenSupplyInvalidError: non-mintable token must have initialSupply == supplyCap".into());
            }
            f.init
        }
        token::TRANSFER => {
            let items = a.transfers.as_deref().unwrap_or_default();
            if items.is_empty() || items.len() > ms.token_transfer_max_recipients as usize {
                return Err(format!("TokenTransferRecipientsError: 1..={} recipients", ms.token_transfer_max_recipients));
            }
            if items.iter().any(|i| i.amount == 0) {
                return Err("TokenAmountInvalidError: amount must be > 0".into());
            }
            if items.iter().map(|i| i.amount as u128).sum::<u128>() > u64::MAX as u128 {
                return Err("TokenAmountInvalidError: total overflows u64".into());
            }
            if a.memo.as_deref().is_some_and(|m| m.len() > token::MAX_MEMO) {
                return Err("TokenMemoTooLongError: memo must be <= 64 bytes".into());
            }
            f.transfer + f.transfer_per_recipient * (items.len() as u64 - 1)
        }
        token::MINT => {
            if a.amount.unwrap_or(0) == 0 || a.recipient_id.is_none() {
                return Err("TokenAmountInvalidError: mint needs amount > 0 and recipientId".into());
            }
            f.mint
        }
        token::META => {
            check_token_meta(a.meta.as_ref().ok_or("TokenMetaInvalidError: missing asset.token.meta")?)?;
            f.meta
        }
        _ => {
            if a.amount.unwrap_or(0) == 0 {
                return Err("TokenAmountInvalidError: burn amount must be > 0".into());
            }
            f.burn
        }
    };
    let expected_fee = expected_fee + tx.pq_surcharge(ms.pq.fee_per_byte);
    if tx.fee != expected_fee {
        return Err(format!("StaticFeeMismatchError: token fee must be exactly {expected_fee} (milestone tokenFees), transaction pays {} — the forging node runs other tokenFees / core version", tx.fee));
    }
    Ok(a)
}

/// Ticker rule for sObject type 5 names once tokens are live.
pub fn ticker_ok(name: &str) -> bool {
    (3..=10).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// TokenMeta manifest limits: text fields without control characters, logo = SVG (text, no scripts) or PNG ≤ 8 KB.
pub fn check_token_meta(m: &crate::models::TokenMeta) -> Result<(), String> {
    use crate::models::token;
    let text_ok = |s: &str, max: usize| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control);
    if !text_ok(&m.name, token::META_MAX_NAME) {
        return Err(format!("TokenMetaInvalidError: name must be 1..={} bytes", token::META_MAX_NAME));
    }
    if m.description.as_deref().is_some_and(|d| !text_ok(d, token::META_MAX_DESCRIPTION)) {
        return Err(format!("TokenMetaInvalidError: description must be 1..={} bytes", token::META_MAX_DESCRIPTION));
    }
    if m.website.as_deref().is_some_and(|w| !text_ok(w, token::META_MAX_WEBSITE) || w.contains(char::is_whitespace)) {
        return Err(format!("TokenMetaInvalidError: website must be 1..={} bytes without spaces", token::META_MAX_WEBSITE));
    }
    match (m.logo_type.as_deref(), m.logo.as_deref()) {
        (None, None) => Ok(()),
        (Some(t @ ("svg" | "png")), Some(_)) => {
            let bytes = m.logo_bytes().ok_or("TokenMetaInvalidError: logo must be base64")?;
            if bytes.is_empty() || bytes.len() > token::META_MAX_LOGO {
                return Err(format!("TokenLogoTooLargeError: logo must be 1..={} bytes", token::META_MAX_LOGO));
            }
            if t == "png" {
                if !bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
                    return Err("TokenLogoInvalidError: not a PNG file".into());
                }
            } else {
                let text = std::str::from_utf8(&bytes).map_err(|_| "TokenLogoInvalidError: SVG must be UTF-8")?;
                let lower = text.to_ascii_lowercase();
                let head = lower.trim_start();
                if !(head.starts_with("<svg") || head.starts_with("<?xml")) || !lower.contains("<svg") {
                    return Err("TokenLogoInvalidError: not an SVG document".into());
                }
                if lower.contains("<script") || lower.contains("javascript:") || lower.contains("<foreignobject") {
                    return Err("TokenLogoInvalidError: SVG must not contain scripts".into());
                }
            }
            Ok(())
        }
        _ => Err("TokenMetaInvalidError: logoType must be svg|png and come together with logo".into()),
    }
}

/// View of token state needed by the stateful rules (storage + pending changes of the batch/mempool).
pub trait TokenView {
    /// Owner address of an initialized token.
    fn token_owner(&self, id: &str) -> Option<String>;
    fn token_state(&self, id: &str) -> Option<crate::storage::TokenState>;
    fn token_balance(&self, address: &str, id: &str) -> u64;
    /// Ticker sObject (type 5) by registration id: (owner address, name, resigned).
    fn ticker_sobj(&self, id: &str) -> Option<(String, String, bool)>;
}

/// Wallet-aware token checks (legacy `throwIfCannotBeApplied` analogue). `sender` is the tx sender address.
pub fn check_token(tx: &Transaction, a: &crate::models::TokenAsset, sender: &str, view: &dyn TokenView) -> Result<(), String> {
    use crate::models::token;
    match tx.type_ {
        token::INIT => {
            let (owner, name, resigned) = view.ticker_sobj(&a.id).ok_or("TokenSmartObjectNotFoundError: no type-5 sObject with this id")?;
            if resigned {
                return Err("TokenSmartObjectResignedError".into());
            }
            if owner != sender {
                return Err("TokenNotSmartObjectOwnerError: only the sObject registrant can initialize the token".into());
            }
            if !ticker_ok(&name) {
                return Err("TokenSymbolInvalidError: ticker must match ^[A-Z0-9]{3,10}$".into());
            }
            if view.token_state(&a.id).is_some() {
                return Err("TokenAlreadyInitializedError".into());
            }
        }
        token::META => {
            view.token_state(&a.id).ok_or("TokenNotFoundError")?;
            if view.token_owner(&a.id).as_deref() != Some(sender) {
                return Err("TokenNotOwnerError: only the owner can publish the manifest".into());
            }
        }

        token::TRANSFER => {
            view.token_state(&a.id).ok_or("TokenNotFoundError")?;
            let total: u64 = a.transfers.as_deref().unwrap_or_default().iter().map(|i| i.amount).sum();
            if view.token_balance(sender, &a.id) < total {
                return Err("TokenInsufficientBalanceError".into());
            }
        }
        token::MINT => {
            let st = view.token_state(&a.id).ok_or("TokenNotFoundError")?;
            if view.token_owner(&a.id).as_deref() != Some(sender) {
                return Err("TokenNotOwnerError: only the owner can mint".into());
            }
            if st.flags & token::FLAG_MINTABLE == 0 {
                return Err("TokenNotMintableError".into());
            }
            if st.supply.saturating_add(a.amount.unwrap_or(0)) > st.supply_cap {
                return Err("TokenSupplyCapExceededError".into());
            }
        }
        _ => {
            let st = view.token_state(&a.id).ok_or("TokenNotFoundError")?;
            if st.flags & token::FLAG_BURNABLE == 0 {
                return Err("TokenNotBurnableError".into());
            }
            if view.token_balance(sender, &a.id) < a.amount.unwrap_or(0) {
                return Err("TokenInsufficientBalanceError".into());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Quantum Shield stage B — v3 transactions (docs/SPEC-PQ-V3.md §4–6)
// ---------------------------------------------------------------------------------------------

/// Stateless v3 checks: activation, known algorithms, block sizes, ascending order, fee surcharge.
/// Error names are the `ERR_PQ_*` codes of the spec.
pub fn check_pq_format(tx: &Transaction, ms: &crate::config::Milestone) -> Result<(), String> {
    use crate::crypto::pq;
    if !tx.is_pq() {
        return Ok(());
    }
    if !ms.pq.active {
        return Err("ERR_PQ_NOT_ACTIVE: version 3 transactions need milestone pq.active".into());
    }
    let blocks = tx.pq_blocks();
    if blocks.len() > 2 {
        return Err("ERR_PQ_LENGTH: at most two second-signature blocks".into());
    }
    for (i, b) in blocks.iter().enumerate() {
        let want = match b.algorithm {
            0 => 64,
            pq::ALG_ML_DSA_44 => pq::SIG_LEN,
            a => return Err(format!("ERR_PQ_ALGORITHM: unknown second-signature algorithm {a}")),
        };
        if b.signature.len() != want * 2 || hex::decode(&b.signature).is_err() {
            return Err(format!("ERR_PQ_LENGTH: algorithm {} signature must be {want} bytes", b.algorithm));
        }
        if i > 0 && blocks[i - 1].algorithm > b.algorithm {
            return Err("ERR_PQ_ORDER: second-signature blocks must be in ascending algorithm order".into());
        }
    }
    if tx.second_signature_any().is_some() {
        return Err("ERR_PQ_LENGTH: v3 carries secondSignatures blocks, not secondSignature".into());
    }
    if tx.type_group == crate::models::TYPE_GROUP_CORE && tx.type_ == tx_type::SECOND_SIGNATURE {
        let a = tx.asset.as_ref().and_then(|a| a.signature.as_ref()).ok_or("ERR_PQ_ALGORITHM: missing asset.signature")?;
        if a.algorithm != Some(pq::ALG_ML_DSA_44) {
            return Err("ERR_PQ_ALGORITHM: asset.signature.algorithm must be 1 (ML-DSA-44)".into());
        }
        if a.public_key.len() != pq::PK_LEN * 2 || hex::decode(&a.public_key).is_err() {
            return Err(format!("ERR_PQ_LENGTH: ML-DSA-44 public key must be {} bytes", pq::PK_LEN));
        }
    }
    // core types: static fee of the type + surcharge (sObject / token types check their exact fee + surcharge themselves)
    if tx.type_group == crate::models::TYPE_GROUP_CORE {
        let min = ms.static_fee_for_type(tx.type_) + tx.pq_surcharge(ms.pq.fee_per_byte);
        if tx.fee < min {
            return Err(format!("ERR_PQ_FEE: version 3 fee must be at least static fee + PQ surcharge = {min}"));
        }
    }
    Ok(())
}

/// Wallet state the v3 rules need (storage + pending changes of the batch / pool).
#[derive(Debug, Clone, Default)]
pub struct PqWalletView<'a> {
    /// Legacy secp256k1 second public key, if registered.
    pub second_public_key: Option<&'a str>,
    /// Registered PQ key (wallet is "PQ-locked").
    pub pq_key: Option<&'a crate::crypto::pq::PqKey>,
    /// Stage-A commitment, if published.
    pub commitment: Option<&'a crate::crypto::pq::PqCommitment>,
}

/// Wallet-aware v3 rules (§5.1 / §5.2 / §6.5). Returns the PQ key a v3 registration installs.
/// `activation` = height of the first `pq.active` milestone (commitment grace window).
pub fn check_pq(
    tx: &Transaction,
    ms: &crate::config::Milestone,
    height: u64,
    activation: Option<u64>,
    w: PqWalletView<'_>,
) -> Result<Option<crate::crypto::pq::PqKey>, String> {
    use crate::crypto::pq;
    if !tx.is_pq() {
        if w.pq_key.is_some() {
            return Err("ERR_PQ_SECOND_SIGNATURE_REQUIRED: wallet is PQ-locked, send a version 3 transaction with its ML-DSA block".into());
        }
        return Ok(None);
    }
    check_pq_format(tx, ms)?;
    let m2 = crate::crypto::transaction_pq_message(tx).map_err(|e| format!("ERR_PQ_SECOND_SIGNATURE_INVALID: {e}"))?;
    let blocks = tx.pq_blocks();
    // verify one block against a key of its algorithm
    let ok = |b: &crate::models::PqSignatureBlock, alg: u8, key_hex: &str| -> bool {
        if b.algorithm != alg {
            return false;
        }
        match alg {
            0 => crate::crypto::verify_signature(&m2, &b.signature, key_hex).unwrap_or(false),
            _ => match (hex::decode(key_hex), hex::decode(&b.signature)) {
                (Ok(pk), Ok(sig)) => pq::verify(&pk, &m2, &sig).unwrap_or(false),
                _ => false,
            },
        }
    };
    let registering = tx.type_group == crate::models::TYPE_GROUP_CORE && tx.type_ == tx_type::SECOND_SIGNATURE;
    if registering {
        let a = tx.asset.as_ref().and_then(|a| a.signature.as_ref()).ok_or("ERR_PQ_ALGORITHM: missing asset.signature")?;
        let new_pk = hex::decode(&a.public_key).map_err(|_| "ERR_PQ_LENGTH: public key is not hex")?;
        if let (Some(c), Some(act)) = (w.commitment, activation) {
            if height < act.saturating_add(ms.pq.commitment_grace) && c.commitment != hex::encode(pq::commitment_hash(&new_pk)) {
                return Err("ERR_PQ_COMMITMENT_MISMATCH: the key differs from the stage-A commitment (grace window)".into());
            }
        }
        // proof of the current second key comes first, the new key signs last
        let proof: Option<(u8, &str)> = match (w.pq_key, w.second_public_key) {
            (Some(k), _) => Some((k.algorithm, k.public_key.as_str())),
            (None, Some(legacy)) => Some((0, legacy)),
            (None, None) => None,
        };
        let expected = 1 + proof.is_some() as usize;
        if blocks.len() != expected {
            return Err(match proof {
                Some(_) => "ERR_PQ_LEGACY_PROOF_REQUIRED: registration must carry [current key, new key] blocks".into(),
                None => "ERR_PQ_SECOND_SIGNATURE_REQUIRED: registration must carry exactly one block signed by the new key".into(),
            });
        }
        if let Some((alg, key)) = proof {
            if !ok(&blocks[0], alg, key) {
                return Err("ERR_PQ_LEGACY_PROOF_REQUIRED: first block must be a valid signature of the current second key".into());
            }
        }
        if !ok(&blocks[expected - 1], pq::ALG_ML_DSA_44, &a.public_key) {
            return Err("ERR_PQ_SECOND_SIGNATURE_INVALID: last block must be signed by the new ML-DSA-44 key".into());
        }
        return Ok(Some(pq::PqKey { algorithm: pq::ALG_ML_DSA_44, public_key: a.public_key.clone(), since: height }));
    }
    match (w.pq_key, w.second_public_key) {
        (Some(k), _) => {
            if blocks.len() != 1 || blocks[0].algorithm != k.algorithm {
                return Err("ERR_PQ_SECOND_SIGNATURE_REQUIRED: exactly one block of the registered PQ algorithm".into());
            }
            if !ok(&blocks[0], k.algorithm, &k.public_key) {
                return Err("ERR_PQ_SECOND_SIGNATURE_INVALID".into());
            }
        }
        (None, Some(legacy)) => {
            if blocks.len() != 1 || !ok(&blocks[0], 0, legacy) {
                return Err("ERR_PQ_SECOND_SIGNATURE_INVALID: legacy second signature block (algorithm 0) required".into());
            }
        }
        (None, None) => {
            if !blocks.is_empty() {
                return Err("UnexpectedSecondSignatureError: wallet has no second signature".into());
            }
        }
    }
    Ok(None)
}
