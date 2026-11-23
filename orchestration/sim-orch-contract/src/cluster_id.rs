//! Simulation cluster identity: process id + epoch nanos (not a name hash).
//!
//! REST / serde wire form is a JSON object `{ "id", "nano" }` (#144); both
//! fields are decimal strings so full `u32` / `u128` range is preserved without
//! JSON number limits. Iceoryx service path segments use [`Display`]
//! (`"{id}-{nano}"`) so the identity stays structured in payloads
//! ([`EndpointWire`](crate::EndpointWire)) rather than a free-form `str`.

use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use iceoryx2::prelude::ZeroCopySend;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

/// Unique id for one simulation cluster instance on a host.
///
/// Layout keeps `nano` naturally aligned; `_pad` is always zero and skipped by
/// serde (public JSON is only `id` + `nano` as decimal strings).
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

    /// Owned form of the [`Display`] path segment (`"{id}-{nano}"`).
    #[must_use]
    pub fn path_key(self) -> String {
        self.to_string()
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
        write!(f, "{}-{}", self.id, self.nano)
    }
}

/// Failed parse of a [`ClusterId`] path segment (`"{id}-{nano}"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseClusterIdError;

impl fmt::Display for ParseClusterIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid ClusterId path segment (expected {id}-{nano})")
    }
}

impl std::error::Error for ParseClusterIdError {}

impl FromStr for ClusterId {
    type Err = ParseClusterIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (id, nano) = s.split_once('-').ok_or(ParseClusterIdError)?;
        let id = id.parse::<u32>().map_err(|_| ParseClusterIdError)?;
        let nano = nano.parse::<u128>().map_err(|_| ParseClusterIdError)?;
        Ok(Self::new(id, nano))
    }
}

impl Serialize for ClusterId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ClusterId", 2)?;
        state.serialize_field("id", &self.id.to_string())?;
        state.serialize_field("nano", &self.nano.to_string())?;
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
                f.write_str("ClusterId object with string id and nano")
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
                            let s: String = map.next_value()?;
                            let parsed = s.parse::<u32>().map_err(de::Error::custom)?;
                            id = Some(parsed);
                        }
                        Field::Nano => {
                            if nano.is_some() {
                                return Err(de::Error::duplicate_field("nano"));
                            }
                            let s: String = map.next_value()?;
                            let parsed = s.parse::<u128>().map_err(de::Error::custom)?;
                            nano = Some(parsed);
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
        assert_eq!(v, serde_json::json!({"id": "7", "nano": "99"}));
        let back: ClusterId = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.path_key(), "7-99");
    }

    #[test]
    fn json_preserves_full_u128_nano() {
        let nano = u128::MAX;
        let c = ClusterId::new(1, nano);
        let v = serde_json::to_value(c).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"id": "1", "nano": u128::MAX.to_string()})
        );
        let back: ClusterId = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn rejects_numeric_wire_fields() {
        assert!(serde_json::from_str::<ClusterId>(r#"{"id":7,"nano":99}"#).is_err());
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

    #[test]
    fn path_segment_round_trip() {
        let c = ClusterId::new(7, 99);
        assert_eq!(c.to_string().parse::<ClusterId>().unwrap(), c);
        assert!("7".parse::<ClusterId>().is_err());
        assert!("7-".parse::<ClusterId>().is_err());
        assert!("x-99".parse::<ClusterId>().is_err());
    }
}
