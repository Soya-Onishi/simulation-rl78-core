//! Opaque hashed identifiers for wire / path use (`HashedId<u64>` = 16 hex digits).

use std::fmt;
use std::hash::Hash;
use std::str::FromStr;

use iceoryx2::prelude::ZeroCopySend;
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// Encode / decode a value as a fixed lowercase hex string (no `0x` prefix).
pub trait Hexable: Copy + Eq + Hash + Sized {
    /// Public wire form (for `u64`: exactly 16 lowercase hex digits).
    fn to_hex(self) -> String;
    /// Parse the public wire form; reject other spellings.
    fn from_hex(text: &str) -> Option<Self>;
}

impl Hexable for u64 {
    fn to_hex(self) -> String {
        format!("{self:016x}")
    }

    fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 16 || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        u64::from_str_radix(text, 16).ok()
    }
}

/// Hashed or opaque id stored numerically in-process; serde / [`Display`] use hex.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, ZeroCopySend)]
pub struct HashedId<T: Hexable + ZeroCopySend>(T);

impl<T: Hexable + ZeroCopySend> HashedId<T> {
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> T {
        self.0
    }
}

impl HashedId<u64> {
    /// Sentinel / empty slot used on wire when no id is selected.
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl<T: Hexable + ZeroCopySend> From<T> for HashedId<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: Hexable + ZeroCopySend> fmt::Debug for HashedId<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HashedId({})", self.0.to_hex())
    }
}

impl<T: Hexable + ZeroCopySend> fmt::Display for HashedId<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_hex())
    }
}

impl<T: Hexable + ZeroCopySend> FromStr for HashedId<T> {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        T::from_hex(s).map(Self::new).ok_or(())
    }
}

impl<T: Hexable + ZeroCopySend> Serialize for HashedId<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_hex())
    }
}

impl<'de, T: Hexable + ZeroCopySend> Deserialize<'de> for HashedId<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct HexVisitor<T>(std::marker::PhantomData<T>);

        impl<'de, T: Hexable + ZeroCopySend> Visitor<'de> for HexVisitor<T> {
            type Value = HashedId<T>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a lowercase hex hashed id")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                T::from_hex(v)
                    .map(HashedId::new)
                    .ok_or_else(|| E::custom("invalid hashed id hex"))
            }
        }

        deserializer.deserialize_str(HexVisitor(std::marker::PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let id = HashedId::new(0xdead_beef_u64);
        assert_eq!(id.to_string(), "00000000deadbeef");
        assert_eq!(id, "00000000deadbeef".parse().unwrap());
    }
}
