//! Deterministic id hashing and [`HashedId`] hex encoding for wire payloads.

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::str::FromStr;

use iceoryx2::prelude::ZeroCopySend;
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use xxhash_rust::xxh3::xxh3_64;

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
    /// Sentinel / empty slot used on wire when no board is selected.
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

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

/// Stable 64-bit hash of a board id / path string (UTF-8 bytes).
#[must_use]
pub fn board_id_hash(board_name: &str) -> HashedId<u64> {
    HashedId::new(sim_kernel::id_hash(board_name))
}

/// Stable 64-bit hash of a port / endpoint name (UTF-8 bytes).
#[must_use]
pub fn port_id_hash(name: &str) -> HashedId<u64> {
    HashedId::new(sim_kernel::id_hash(name))
}

/// Stable 64-bit hash of a probe name (UTF-8 bytes).
#[must_use]
pub fn probe_name_hash(name: &str) -> HashedId<u64> {
    HashedId::new(sim_kernel::id_hash(name))
}

/// Cluster-wide probe id: `xxh3(board_hash_le || 0x00 || name)`.
#[must_use]
pub fn probe_id_hash(board_hash: HashedId<u64>, name: &str) -> HashedId<u64> {
    let mut bytes = Vec::with_capacity(8 + 1 + name.len());
    bytes.extend_from_slice(&board_hash.get().to_le_bytes());
    bytes.push(0);
    bytes.extend_from_slice(name.as_bytes());
    HashedId::new(xxh3_64(&bytes))
}

/// Logical edge id string, then `xxh3_64` (same form as historical `names::edge_id`).
#[must_use]
pub fn edge_id_hash(from_board: &str, from_ep: &str, to_board: &str, to_ep: &str) -> HashedId<u64> {
    let logical = format!("{from_board}:{from_ep}->{to_board}:{to_ep}");
    HashedId::new(xxh3_64(logical.as_bytes()))
}

/// Build reverse lookup for unique id strings.
///
/// The same string may appear more than once. Distinct strings that share a
/// hash are an error.
pub fn id_hash_table<'a>(
    hash: impl Fn(&str) -> HashedId<u64>,
    ids: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<HashedId<u64>, String>, String> {
    let mut table = HashMap::new();
    for id in ids {
        let h = hash(id);

        if let Some(existing) = table.insert(h, id.to_string())
            && existing != id
        {
            return Err(format!(
                "hash collision between '{existing}' and '{id}' (hash={h})"
            ));
        }
    }
    Ok(table)
}

/// Build reverse lookup; errors if two distinct board ids collide.
pub fn board_hash_table<'a>(
    board_ids: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<HashedId<u64>, String>, String> {
    id_hash_table(board_id_hash, board_ids)
}

/// Build reverse lookup for path strings (`boards[].id` and edge route paths).
pub fn path_hash_table<'a>(
    paths: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<HashedId<u64>, String>, String> {
    id_hash_table(board_id_hash, paths)
}

/// Build reverse lookup for port / endpoint name strings.
pub fn port_hash_table<'a>(
    ports: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<HashedId<u64>, String>, String> {
    id_hash_table(port_id_hash, ports)
}

/// Build reverse lookup for logical edge-id strings.
pub fn edge_hash_table<'a>(
    edge_ids: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<HashedId<u64>, String>, String> {
    id_hash_table(|s| HashedId::new(xxh3_64(s.as_bytes())), edge_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        assert_eq!(board_id_hash("a"), board_id_hash("a"));
        assert_ne!(board_id_hash("a"), board_id_hash("b"));
        assert_eq!(port_id_hash("tx"), board_id_hash("tx"));
    }

    #[test]
    fn probe_id_includes_board_and_name() {
        let a = board_id_hash("a");
        let b = board_id_hash("b");
        assert_eq!(probe_id_hash(a, "vcc"), probe_id_hash(a, "vcc"));
        assert_ne!(probe_id_hash(a, "vcc"), probe_id_hash(b, "vcc"));
        assert_ne!(probe_id_hash(a, "vcc"), probe_id_hash(a, "gnd"));
    }

    #[test]
    fn probe_id_hash_uses_board_hash_le_bytes() {
        let board_hash = HashedId::new(0x0123_4567_89ab_cdef_u64);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&board_hash.get().to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(b"vcc");
        assert_eq!(probe_id_hash(board_hash, "vcc").get(), xxh3_64(&bytes));
    }

    #[test]
    fn table_allows_same_string_twice() {
        let table = board_hash_table(["a", "a"]).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(
            table.get(&board_id_hash("a")).map(String::as_str),
            Some("a")
        );
    }

    #[test]
    fn hashed_id_hex_round_trip() {
        let h = board_id_hash("sys/left/a");
        let text = h.to_string();
        assert_eq!(text.len(), 16);
        assert_eq!(text.parse::<HashedId<u64>>(), Ok(h));
        assert!("ABCDEF0123456789".parse::<HashedId<u64>>().is_err());
        assert!(format!("0x{text}").parse::<HashedId<u64>>().is_err());
        assert!("sys/left/a".parse::<HashedId<u64>>().is_err());
    }

    #[test]
    fn hashed_id_serde_round_trip() {
        let h = board_id_hash("mcu-a");
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, format!("\"{h}\""));
        let back: HashedId<u64> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn hashed_id_serde_rejects_bad_hex() {
        for bad in [
            "\"ABCDEF0123456789\"",
            "\"0x0123456789abcdef\"",
            "\"0123456789abcde\"",
            "\"0123456789abcdef0\"",
            "\"+123456789abcdef\"",
            "123",
        ] {
            assert!(
                serde_json::from_str::<HashedId<u64>>(bad).is_err(),
                "accepted {bad}"
            );
        }
    }

    #[test]
    fn hashed_id_is_u64_sized() {
        assert_eq!(std::mem::size_of::<HashedId<u64>>(), 8);
        assert_eq!(std::mem::align_of::<HashedId<u64>>(), 8);
    }

    #[test]
    fn edge_id_hash_matches_logical_xxh3() {
        let logical = "a:tx->b:rx";
        assert_eq!(
            edge_id_hash("a", "tx", "b", "rx").get(),
            xxh3_64(logical.as_bytes())
        );
    }

    #[test]
    fn table_rejects_distinct_string_collision() {
        let err = id_hash_table(|_| HashedId::new(1u64), ["left", "right"]).unwrap_err();
        assert!(err.contains("collision"));
        assert!(err.contains("left"));
        assert!(err.contains("right"));
    }
}
