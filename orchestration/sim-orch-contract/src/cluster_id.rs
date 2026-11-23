//! Simulation cluster identity: process id + epoch nanos (not a name hash).
//!
//! REST / serde wire form is a JSON object `{ "id", "nano" }` (#144); `nano` is
//! a JSON number narrowed to `u64` on the wire (epoch nanos fit). Iceoryx
//! service path segments use [`ClusterId::path_key`] (`"{id}-{nano}"`) so the
//! identity stays structured in payloads ([`EndpointWire`](crate::EndpointWire))
//! rather than a free-form `str`.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use iceoryx2::prelude::ZeroCopySend;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

/// Unique id for one simulation cluster instance on a host.
///
/// Layout keeps `nano` naturally aligned; `_pad` is always zero and skipped by
/// serde (public JSON is only `id` + `nano`).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, ZeroCopySend)]
pub struct ClusterId {
    pub nano: u128,
    pub id: u32,
    _pad: [u8; 12],
}

impl ClusterId {
    /// Construct from process id and epoch nanos.
    #[must_use]
    pub const fn new(id: u32, nano: u128) -> Self {
        Self {
            nano,
            id,
            _pad: [0; 12],
        }
    }

    /// Same information source as legacy `new_cluster_key()` (`pid` + `as_nanos`).
    #[must_use]
    pub fn generate() -> Self {
        let nano = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self::new(std::process::id(), nano)
    }

    /// Deterministic iceoryx / filesystem path segment (`"{id}-{nano}"`).
    #[must_use]
    pub fn path_key(self) -> String {
        format!("{}-{}", self.id, self.nano)
    }
}

impl fmt::Debug for ClusterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClusterId")
            .field("id", &self.id)
            .field("nano", &self.nano)
            .finish()
    }
}

impl fmt::Display for ClusterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.path_key())
    }
}

impl Serialize for ClusterId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // JSON numbers are not full u128; epoch nanos fit in u64. Keep the
        // in-memory field as u128 for path_key / iceoryx layout.
        let nano = u64::try_from(self.nano).map_err(|_| {
            serde::ser::Error::custom("cluster nano exceeds u64 (JSON wire limit)")
        })?;
        let mut state = serializer.serialize_struct("ClusterId", 2)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("nano", &nano)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ClusterId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            Id,
            Nano,
        }

        struct ClusterIdVisitor;

        impl<'de> Visitor<'de> for ClusterIdVisitor {
            type Value = ClusterId;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("ClusterId object with id and nano")
            }

            fn visit_map<V: MapAccess<'de>>(self, mut map: V) -> Result<ClusterId, V::Error> {
                let mut id = None;
                let mut nano = None;
                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Id => {
                            if id.is_some() {
                                return Err(de::Error::duplicate_field("id"));
                            }
                            id = Some(map.next_value()?);
                        }
                        Field::Nano => {
                            if nano.is_some() {
                                return Err(de::Error::duplicate_field("nano"));
                            }
                            let n: u64 = map.next_value()?;
                            nano = Some(u128::from(n));
                        }
                    }
                }
                let id = id.ok_or_else(|| de::Error::missing_field("id"))?;
                let nano = nano.ok_or_else(|| de::Error::missing_field("nano"))?;
                Ok(ClusterId::new(id, nano))
            }
        }

        deserializer.deserialize_struct("ClusterId", &["id", "nano"], ClusterIdVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_object_wire() {
        let c = ClusterId::new(7, 99);
        let v = serde_json::to_value(c).unwrap();
        assert_eq!(v, serde_json::json!({"id": 7, "nano": 99}));
        let back: ClusterId = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.path_key(), "7-99");
    }

    #[test]
    fn rejects_string_wire() {
        assert!(serde_json::from_str::<ClusterId>("\"7-99\"").is_err());
    }

    #[test]
    fn layout_is_packed_for_zerocopy() {
        use std::mem::{align_of, offset_of, size_of};
        assert_eq!(align_of::<ClusterId>(), 16);
        assert_eq!(size_of::<ClusterId>(), 32);
        assert_eq!(offset_of!(ClusterId, nano), 0);
        assert_eq!(offset_of!(ClusterId, id), 16);
    }

    #[test]
    fn path_key_has_no_path_separators() {
        let c = ClusterId::new(u32::MAX, u64::MAX as u128);
        let key = c.path_key();
        assert!(!key.contains('/'));
        assert!(!key.contains('\0'));
        assert_eq!(key, format!("{}-{}", u32::MAX, u64::MAX));
    }
}
