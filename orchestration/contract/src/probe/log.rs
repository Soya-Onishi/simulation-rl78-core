//! On-disk probe log JSON records (`$SIM_CSV_DIR/<Cluster ID>/probelog.csv`).
//!
//! Shape matches #141: `{ "vt", "kind", "data" }` with `kind` as the serde tag.

use serde::{Deserialize, Serialize};

fn is_false(v: &bool) -> bool {
    !*v
}

/// One JSON object per spilled probe sample (#141).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ProbeLogRecord {
    Serial {
        /// Virtual time in nanoseconds.
        vt: u64,
        /// Opaque JSON value from `Record::value`.
        data: serde_json::Value,
    },
    Analog {
        vt: u64,
        data: serde_json::Value,
    },
    Digital {
        vt: u64,
        data: DigitalProbeData,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DigitalProbeData {
    /// Reference voltage.
    pub v: f64,
    /// Present and `true` only when high-impedance; omit when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hiz: bool,
    /// Hi duty ratio (0..=100). Constant Hi=100, Lo=0.
    pub ratio: u8,
    /// PWM period in ns; omitted for constant levels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digital_round_trip_and_kind_tag() {
        let rec = ProbeLogRecord::Digital {
            vt: 100,
            data: DigitalProbeData {
                v: 3.3,
                hiz: true,
                ratio: 50,
                length: Some(1000),
            },
        };
        let json = serde_json::to_value(&rec).unwrap();
        assert_eq!(json["kind"], "digital");
        assert_eq!(json["data"]["hiz"], true);
        let back: ProbeLogRecord = serde_json::from_value(json).unwrap();
        assert_eq!(back, rec);
    }

    #[test]
    fn digital_omits_hiz_when_false() {
        let rec = ProbeLogRecord::Digital {
            vt: 1,
            data: DigitalProbeData {
                v: 5.0,
                hiz: false,
                ratio: 100,
                length: None,
            },
        };
        let json = serde_json::to_value(&rec).unwrap();
        assert!(json["data"].get("hiz").is_none());
        assert!(json["data"].get("length").is_none());
        let back: ProbeLogRecord = serde_json::from_value(json).unwrap();
        assert_eq!(back, rec);
    }

    #[test]
    fn serial_value_not_stolen_by_digital() {
        let rec = ProbeLogRecord::Serial {
            vt: 7,
            data: serde_json::json!({"raw": [1, 2, 3]}),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: ProbeLogRecord = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ProbeLogRecord::Serial { .. }));
    }

    #[test]
    fn rejects_kind_data_mismatch() {
        let bad = r#"{"kind":"digital","vt":1,"data":"not-an-object"}"#;
        assert!(serde_json::from_str::<ProbeLogRecord>(bad).is_err());
    }
}
