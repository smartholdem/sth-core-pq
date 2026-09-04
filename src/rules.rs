//! Author: TechnoL0g
//!
//! Stateful transaction rules that need the sender's wallet (legacy `TransactionHandler.throwIfCannotBeApplied`):
//! second-signature enforcement, exactly as Node.js nodes apply it.

use crate::crypto::verify_transaction_second_signature;
use crate::models::{tx_type, Transaction};

/// Second-signature rule for one transaction given the sender's registered second public key.
/// Errors carry the legacy error names so API / log messages match the old core.
pub fn check_second_signature(tx: &Transaction, second_public_key: Option<&str>) -> Result<(), String> {
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
    if tx.type_group != crate::models::TYPE_GROUP_CORE || tx.type_ != tx_type::SECOND_SIGNATURE {
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

// ------------------------------------------------------------------ AIP-36 entities

/// Format-level checks of an entity transaction (no wallet state): schema, static fee, amount.
/// `name`/`ipfsData` limits follow `core-magistrate-crypto` `entity-schemas.ts`.
pub fn check_entity_format(tx: &Transaction) -> Result<crate::models::EntityAsset, String> {
    let e = tx.entity_asset().ok_or("Entity: missing or malformed asset")?;
    if tx.amount != 0 {
        return Err("Entity: amount must be 0".into());
    }
    let fee = crate::models::entity::static_fee(e.action).ok_or_else(|| format!("Entity: unknown action {}", e.action))?;
    if tx.fee != fee {
        return Err(format!("StaticFeeMismatchError: entity fee must be exactly {fee}"));
    }
    let name_ok = |n: &str| (1..=40).contains(&n.len()) && n.chars().all(|c| c.is_ascii_alphanumeric() || "_!@$&.-".contains(c));
    let ipfs_ok = |s: &str| (1..=128).contains(&s.len()) && bs58::decode(s).into_vec().is_ok();
    if let Some(ipfs) = &e.data.ipfs_data {
        if !ipfs_ok(ipfs) {
            return Err("Entity: invalid ipfsData".into());
        }
    }
    match e.action {
        crate::models::entity::ACTION_REGISTER => {
            if e.registration_id.is_some() {
                return Err("Entity: register must not carry registrationId".into());
            }
            match &e.data.name {
                Some(n) if name_ok(n) => {}
                _ => return Err("Entity: register requires a valid name".into()),
            }
        }
        _ => {
            match &e.registration_id {
                Some(id) if id.len() == 64 && hex::decode(id).is_ok() => {}
                _ => return Err("Entity: update/resign require registrationId".into()),
            }
            if e.data.name.is_some() {
                return Err("Entity: name cannot be changed".into());
            }
            if e.action == crate::models::entity::ACTION_RESIGN && e.data.ipfs_data.is_some() {
                return Err("Entity: resign must not carry data".into());
            }
        }
    }
    Ok(e)
}

/// Wallet-aware entity rules (legacy `EntityTransactionHandler.throwIfCannotBeApplied`).
/// `name_taken` answers "is (name, type) already registered network-wide"; `pending` = records written earlier in the batch.
pub fn check_entity(
    tx: &Transaction,
    wallet: Option<&crate::storage::WalletState>,
    pending: &std::collections::BTreeMap<String, crate::storage::EntityRecord>,
    name_taken: impl Fn(&str, u8) -> bool,
) -> Result<(), String> {
    let e = check_entity_format(tx)?;
    let id = tx.id.as_deref().ok_or("Entity: transaction without id")?;
    let record = |reg: &str| pending.get(reg).cloned().or_else(|| wallet.and_then(|w| w.entities.get(reg).cloned()));
    match e.action {
        crate::models::entity::ACTION_REGISTER => {
            if record(id).is_some() {
                return Err("EntityAlreadyRegisteredError".into());
            }
            let name = e.data.name.as_deref().unwrap_or_default();
            if name_taken(name, e.type_) {
                return Err("EntityNameAlreadyRegisteredError".into());
            }
            if e.type_ == crate::models::entity::TYPE_DELEGATE {
                match wallet.and_then(|w| w.username.as_deref()) {
                    None => return Err("EntitySenderIsNotDelegateError".into()),
                    Some(u) if u != name => return Err("EntityNameDoesNotMatchDelegateError".into()),
                    _ => {}
                }
            }
        }
        _ => {
            let reg = e.registration_id.as_deref().unwrap_or_default();
            let Some(rec) = record(reg) else { return Err("EntityNotRegisteredError".into()) };
            if rec.resigned {
                return Err("EntityAlreadyResignedError".into());
            }
            if rec.type_ != e.type_ {
                return Err("EntityWrongTypeError".into());
            }
            if rec.sub_type != e.sub_type {
                return Err("EntityWrongSubTypeError".into());
            }
        }
    }
    Ok(())
}
