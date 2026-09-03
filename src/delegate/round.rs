//! Author: TechnoL0g
//!
//! Round / slot arithmetic of the DPoS consensus, identical to the legacy core:
//! `round = ceil(height / activeDelegates)`, `slot = timestamp / blocktime`, and the deterministic
//! shuffle that turns the ranked top-N into the forging order of a round.

use crate::crypto::sha256;
use crate::storage::{DelegateRank, Storage};
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundInfo {
    pub round: u64,
    pub round_height: u64,
    pub next_round: u64,
    pub max_delegates: u64,
}

pub fn round_info(height: u64, max_delegates: u64) -> RoundInfo {
    let h = height.max(1);
    let round = (h - 1) / max_delegates + 1;
    let round_height = (round - 1) * max_delegates + 1;
    let next_round = if h % max_delegates == 0 { round + 1 } else { round };
    RoundInfo { round, round_height, next_round, max_delegates }
}

pub fn slot_number(timestamp: u32, blocktime: u32) -> u64 {
    timestamp as u64 / blocktime as u64
}

pub fn slot_start(slot: u64, blocktime: u32) -> u32 {
    (slot * blocktime as u64) as u32
}

/// Legacy `shuffleDelegates`: Fisher–Yates-like swaps driven by `sha256(round)` re-hashed every 4 swaps.
/// NOTE: the legacy loop is `for (i…; i++) { for (x < 4 && i < n; i++, x++) … }` — the outer `i++`
/// runs after every inner batch, so indices 4, 9, 14, 19 are never swapped as `i`. Must be mirrored
/// exactly (verified against mainnet round 557584: 21/21 block generators).
pub fn shuffle(delegates: &mut [DelegateRank], round: u64) {
    let n = delegates.len();
    if n == 0 {
        return;
    }
    let mut seed = sha256(round.to_string().as_bytes());
    let mut i = 0usize;
    while i < n {
        let mut x = 0usize;
        while x < 4 && i < n {
            let new_index = seed[x] as usize % n;
            delegates.swap(new_index, i);
            i += 1;
            x += 1;
        }
        i += 1;
        seed = sha256(&seed);
    }
}

/// Forging order for `round` from the current ranking (or the stored round snapshot).
pub fn forging_order(storage: &Storage, round: u64, max_delegates: usize) -> Result<Vec<DelegateRank>> {
    let mut list = match storage.get_round(round)? {
        Some(l) => l,
        None => storage.active_delegates(max_delegates)?,
    };
    shuffle(&mut list, round);
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_and_slots() {
        assert_eq!(round_info(1, 21).round, 1);
        assert_eq!(round_info(21, 21).round, 1);
        assert_eq!(round_info(21, 21).next_round, 2);
        assert_eq!(round_info(22, 21).round, 2);
        assert_eq!(round_info(22, 21).round_height, 22);
        assert_eq!(slot_number(95101456, 8), 11887682);
        assert_eq!(slot_start(11887682, 8), 95101456);
    }

    #[test]
    fn shuffle_is_deterministic_and_a_permutation() {
        let mk = || (0..21).map(|i| DelegateRank { public_key: format!("{i:02}"), votes: 100 - i as u64 }).collect::<Vec<_>>();
        let (mut a, mut b) = (mk(), mk());
        shuffle(&mut a, 557_000);
        shuffle(&mut b, 557_000);
        assert_eq!(a, b);
        assert_ne!(a, mk());
        let mut keys: Vec<_> = a.iter().map(|d| d.public_key.clone()).collect();
        keys.sort();
        assert_eq!(keys, mk().iter().map(|d| d.public_key.clone()).collect::<Vec<_>>());
    }

    /// Mainnet round 557584 (heights 11,709,244–11,709,264): ranked top-21 from
    /// `/api/rounds/557584/delegates` (votes desc, public key asc) → generator of slot `s` is `order[s % 21]`.
    #[test]
    fn shuffle_matches_mainnet_round_557584() {
        const RANKED: [&str; 21] = [
            "02399ed18f1cc75e6b2d9f8c6b89fb4ff94cf73591983f0f78669800e2eaafaf5a",
            "03ac459c15e32cd9f8189e76322d7f6461349d3eeaaddbda23ab8221800a11f688",
            "02be9e3d6fc7e89020a2da88e953907718cdd8baf7532299a20cd2cf37f31221f2",
            "03d67017411e92a8e93d7b0318e2748f013c130d6e4e01b1248da0ee0959ee9df8",
            "02f6b90fc96cc7e2196b55aae3e6f6e3223ede190e56cff3828b381b167c61905e",
            "021ba22768ca16b4e7bb8999b2c2869442492d485dded47377ae72b0e21b194bf8",
            "0292ea75d5e9f76e8c7586844129df43c738ac527eeb7fc5b4ff84abb1ec38ff5c",
            "03ca0639e64376c50f8251b00edc708ece1ab551618c71279b499ffd9a94e37e4f",
            "0263c031bc2b0593aee600974cd030a0a3f02802ed0d1b5a583868006b4054b84d",
            "023bff30613a00d39f9190c6d8460fd4bf01efbe0949955c62b0dbad547047d16a",
            "03e1e3a75c91a7a4b62cfa5380c6b8670245469defe5c3c169e2f7603517e37486",
            "02c9ba5a3a36ed409102fe7d25240f93ea1013e15e4ec67d56e5bbf5acffae5f9e",
            "035231cc2fccf7fba239b8abb2611def73f0c0a01598164909181eebd544ce114d",
            "02800fe25c005535be15f1a092aba211b69095f0a782224d71a541c14f3a186ef1",
            "02930a1801c7213a206d4e5edeb70a23260e8ce970082db29d7a144f4c737cc264",
            "03fc2ba1d5dca63d4bcd70b9055e19fa0e484d435217bc25113b6233fc0d32eb70",
            "0344efe631ac747d1031202dcb1e6253436caaa4da75ae133a7b7e9890bd36f20b",
            "02ff02a2c814a0a5f133267e3e18c83f3bd152a00846668b79584d5b6653659dd5",
            "03518ae86e5547b6371708a70588c1c29676a81922995bf3e79c4fe2acf7e13c04",
            "02dc5f9bd41dcd8072ec1e5a98877349302d55e0bd3e81988f9e6e91fc4f986fff",
            "03264d1c9b941f18c387f5f1be8cc6a2df84fff6228a148c0449c80472ecd17d9d",
        ];
        // observed generators by slot % 21 (slot 11 was empty on mainnet: nicholasflamel ran this node)
        const BY_SLOT: [&str; 21] = [
            "0292ea75d5e9f76e8c7586844129df43c738ac527eeb7fc5b4ff84abb1ec38ff5c", // clearable
            "035231cc2fccf7fba239b8abb2611def73f0c0a01598164909181eebd544ce114d", // europa
            "03ac459c15e32cd9f8189e76322d7f6461349d3eeaaddbda23ab8221800a11f688", // etidorhpa
            "03518ae86e5547b6371708a70588c1c29676a81922995bf3e79c4fe2acf7e13c04", // axai
            "021ba22768ca16b4e7bb8999b2c2869442492d485dded47377ae72b0e21b194bf8", // reptilian
            "02f6b90fc96cc7e2196b55aae3e6f6e3223ede190e56cff3828b381b167c61905e", // paracelsus
            "0263c031bc2b0593aee600974cd030a0a3f02802ed0d1b5a583868006b4054b84d", // edwardkelley
            "03fc2ba1d5dca63d4bcd70b9055e19fa0e484d435217bc25113b6233fc0d32eb70", // elementachimae
            "03e1e3a75c91a7a4b62cfa5380c6b8670245469defe5c3c169e2f7603517e37486", // olympusmons
            "023bff30613a00d39f9190c6d8460fd4bf01efbe0949955c62b0dbad547047d16a", // boerhave
            "02be9e3d6fc7e89020a2da88e953907718cdd8baf7532299a20cd2cf37f31221f2", // johndee
            "02c9ba5a3a36ed409102fe7d25240f93ea1013e15e4ec67d56e5bbf5acffae5f9e", // nicholasflamel
            "03d67017411e92a8e93d7b0318e2748f013c130d6e4e01b1248da0ee0959ee9df8", // geber
            "02399ed18f1cc75e6b2d9f8c6b89fb4ff94cf73591983f0f78669800e2eaafaf5a", // albertpoisson
            "02930a1801c7213a206d4e5edeb70a23260e8ce970082db29d7a144f4c737cc264", // sthtothemoon
            "03ca0639e64376c50f8251b00edc708ece1ab551618c71279b499ffd9a94e37e4f", // quantum
            "03264d1c9b941f18c387f5f1be8cc6a2df84fff6228a148c0449c80472ecd17d9d", // albertusmagnus
            "02800fe25c005535be15f1a092aba211b69095f0a782224d71a541c14f3a186ef1", // cryptoishere
            "02ff02a2c814a0a5f133267e3e18c83f3bd152a00846668b79584d5b6653659dd5", // titan
            "02dc5f9bd41dcd8072ec1e5a98877349302d55e0bd3e81988f9e6e91fc4f986fff", // franztausend
            "0344efe631ac747d1031202dcb1e6253436caaa4da75ae133a7b7e9890bd36f20b", // technolog
        ];
        let mut list: Vec<DelegateRank> = RANKED.iter().enumerate().map(|(i, pk)| DelegateRank { public_key: pk.to_string(), votes: 1000 - i as u64 }).collect();
        shuffle(&mut list, 557_584);
        let got: Vec<&str> = list.iter().map(|d| d.public_key.as_str()).collect();
        assert_eq!(got, BY_SLOT.to_vec());
    }
}
