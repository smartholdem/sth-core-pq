//! Author: TechnoL0g
//!
//! Serde helpers: core serialises `BigNumber` fields (amount, fee, nonce, ...) as
//! decimal strings, but tolerates plain JSON numbers on input.

use serde::de::{self, Deserializer, Unexpected, Visitor};
use serde::Serializer;
use std::fmt;

struct U64Visitor;

impl<'de> Visitor<'de> for U64Visitor {
    type Value = u64;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a u64 or a decimal string")
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
        Ok(v)
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
        u64::try_from(v).map_err(|_| E::invalid_value(Unexpected::Signed(v), &self))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<u64, E> {
        if v.fract() == 0.0 && v >= 0.0 && v <= u64::MAX as f64 {
            Ok(v as u64)
        } else {
            Err(E::invalid_value(Unexpected::Float(v), &self))
        }
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
        v.parse::<u64>().map_err(|_| E::invalid_value(Unexpected::Str(v), &self))
    }
}

struct I64Visitor;

impl<'de> Visitor<'de> for I64Visitor {
    type Value = i64;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an i64 or a decimal string")
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<i64, E> {
        i64::try_from(v).map_err(|_| E::invalid_value(Unexpected::Unsigned(v), &self))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<i64, E> {
        Ok(v)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<i64, E> {
        v.parse::<i64>().map_err(|_| E::invalid_value(Unexpected::Str(v), &self))
    }
}

/// `u64` <-> decimal string.
pub mod string_u64 {
    use super::*;

    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        d.deserialize_any(U64Visitor)
    }
}

/// `i64` <-> decimal string.
pub mod string_i64 {
    use super::*;

    pub fn serialize<S: Serializer>(v: &i64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
        d.deserialize_any(I64Visitor)
    }
}

/// `Option<u64>` <-> decimal string (or null).
pub mod opt_string_u64 {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(n) => s.serialize_str(&n.to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        struct OptVisitor;
        impl<'de> Visitor<'de> for OptVisitor {
            type Value = Option<u64>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an optional u64 or decimal string")
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
                d.deserialize_any(U64Visitor).map(Some)
            }
        }
        d.deserialize_option(OptVisitor)
    }
}
