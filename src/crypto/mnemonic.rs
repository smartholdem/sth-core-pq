//! Author: TechnoL0g
//!
//! BIP-39 English mnemonics (12 words = 128 bits of entropy + 4-bit checksum). SmartHoldem wallets use the mnemonic
//! sentence itself as the passphrase (`KeyPair::from_passphrase` = sha256 of the text), so no seed derivation is needed here.

use super::hash::sha256;
use crate::error::{Error, Result};

const WORDS: &str = include_str!("bip39_english.txt");

fn wordlist() -> Vec<&'static str> {
    WORDS.lines().map(str::trim).filter(|w| !w.is_empty()).collect()
}

/// Entropy (16 / 20 / 24 / 28 / 32 bytes) → mnemonic sentence.
pub fn mnemonic_from_entropy(entropy: &[u8]) -> Result<String> {
    if !matches!(entropy.len(), 16 | 20 | 24 | 28 | 32) {
        return Err(Error::Config(format!("mnemonic entropy must be 16..=32 bytes, got {}", entropy.len())));
    }
    let words = wordlist();
    let checksum = sha256(entropy);
    let mut bits: Vec<bool> = entropy.iter().flat_map(|b| (0..8).rev().map(move |i| b >> i & 1 == 1)).collect();
    bits.extend((0..entropy.len() * 8 / 32).map(|i| checksum[i / 8] >> (7 - i % 8) & 1 == 1));
    Ok(bits.chunks(11).map(|c| words[c.iter().fold(0usize, |acc, b| acc << 1 | *b as usize)]).collect::<Vec<_>>().join(" "))
}

/// Fresh 12-word mnemonic from the OS random generator.
pub fn generate_mnemonic() -> Result<String> {
    let mut entropy = [0u8; 16];
    getrandom::getrandom(&mut entropy).map_err(|e| Error::Config(format!("os random generator: {e}")))?;
    mnemonic_from_entropy(&entropy)
}

/// True when every word is in the list and the checksum matches (12–24 words).
pub fn validate_mnemonic(sentence: &str) -> bool {
    let words = wordlist();
    let parts: Vec<&str> = sentence.split_whitespace().collect();
    if !matches!(parts.len(), 12 | 15 | 18 | 21 | 24) {
        return false;
    }
    let mut bits = Vec::with_capacity(parts.len() * 11);
    for p in &parts {
        let Ok(idx) = words.binary_search(&p.to_lowercase().as_str()) else { return false };
        bits.extend((0..11).rev().map(|i| idx >> i & 1 == 1));
    }
    let ent_bits = parts.len() * 11 * 32 / 33;
    let entropy: Vec<u8> = bits[..ent_bits].chunks(8).map(|c| c.iter().fold(0u8, |acc, b| acc << 1 | *b as u8)).collect();
    mnemonic_from_entropy(&entropy).map(|m| m == parts.join(" ").to_lowercase()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bip39_vectors() {
        assert_eq!(mnemonic_from_entropy(&[0u8; 16]).unwrap(), "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about");
        assert_eq!(mnemonic_from_entropy(&hex::decode("7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f").unwrap()).unwrap(), "legal winner thank year wave sausage worth useful legal winner thank yellow");
        assert_eq!(mnemonic_from_entropy(&hex::decode("80808080808080808080808080808080").unwrap()).unwrap(), "letter advice cage absurd amount doctor acoustic avoid letter advice cage above");
        assert_eq!(mnemonic_from_entropy(&[0xff; 16]).unwrap(), "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong");
        assert_eq!(wordlist().len(), 2048);
    }

    #[test]
    fn generate_and_validate() {
        let m = generate_mnemonic().unwrap();
        assert_eq!(m.split(' ').count(), 12);
        assert!(validate_mnemonic(&m));
        assert!(validate_mnemonic("zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong"));
        assert!(!validate_mnemonic("zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo"), "bad checksum");
        assert!(!validate_mnemonic("not a mnemonic at all"));
        assert_ne!(generate_mnemonic().unwrap(), m);
    }
}
