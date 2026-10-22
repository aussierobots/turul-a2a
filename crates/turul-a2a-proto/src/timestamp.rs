//! `google.protobuf.Timestamp` for the A2A types, with the proto3 JSON
//! mapping.
//!
//! The proto3 JSON mapping (<https://protobuf.dev/programming-guides/json/>)
//! writes a Timestamp as RFC 3339 whose "generated output will always be
//! Z-normalized and uses 0, 3, 6 or 9 fractional digits"; offsets other
//! than "Z" are accepted on input. Every `google.protobuf.Timestamp` field
//! of the generated A2A messages has this type, so every transport and
//! every payload built from those messages writes the same form.

use chrono::{DateTime, SecondsFormat, Utc};

/// A point in time: seconds and nanoseconds since the Unix epoch, wire
/// compatible with `google.protobuf.Timestamp`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, ::prost::Message)]
pub struct Timestamp {
    /// Seconds since 1970-01-01T00:00:00Z.
    #[prost(int64, tag = "1")]
    pub seconds: i64,
    /// Non-negative fraction of a second, `0..=999_999_999`.
    #[prost(int32, tag = "2")]
    pub nanos: i32,
}

impl From<DateTime<Utc>> for Timestamp {
    fn from(at: DateTime<Utc>) -> Self {
        Self {
            seconds: at.timestamp(),
            nanos: at.timestamp_subsec_nanos() as i32,
        }
    }
}

impl TryFrom<Timestamp> for DateTime<Utc> {
    type Error = &'static str;

    /// Fails when `nanos` is outside `0..=999_999_999` or the instant is
    /// outside the range `chrono` represents.
    fn try_from(ts: Timestamp) -> Result<Self, Self::Error> {
        let nanos = u32::try_from(ts.nanos)
            .ok()
            .filter(|n| *n < 1_000_000_000)
            .ok_or("Timestamp nanos outside 0..=999999999")?;
        DateTime::from_timestamp(ts.seconds, nanos).ok_or("Timestamp out of range")
    }
}

impl serde::Serialize for Timestamp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let at = DateTime::<Utc>::try_from(*self).map_err(serde::ser::Error::custom)?;
        // `AutoSi` prints 0, 3, 6 or 9 fractional digits; `true` writes "Z".
        serializer.serialize_str(&at.to_rfc3339_opts(SecondsFormat::AutoSi, true))
    }
}

impl<'de> serde::Deserialize<'de> for Timestamp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        DateTime::parse_from_rfc3339(&text)
            .map(|at| Self::from(at.with_timezone(&Utc)))
            .map_err(|e| serde::de::Error::custom(format!("invalid Timestamp {text:?}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(seconds: i64, nanos: i32) -> Timestamp {
        Timestamp { seconds, nanos }
    }

    #[test]
    fn serialises_z_normalised_with_0_3_6_or_9_fraction_digits() {
        for (nanos, text) in [
            (0, "2023-11-14T22:13:20Z"),
            (120_000_000, "2023-11-14T22:13:20.120Z"),
            (5_000, "2023-11-14T22:13:20.000005Z"),
            (123_456_789, "2023-11-14T22:13:20.123456789Z"),
        ] {
            assert_eq!(
                serde_json::to_value(ts(1_700_000_000, nanos)).unwrap(),
                serde_json::json!(text)
            );
        }
    }

    #[test]
    fn accepts_any_rfc3339_offset() {
        for text in [
            "2023-11-14T22:13:20.5Z",
            "2023-11-14T22:13:20.5+00:00",
            "2023-11-15T03:13:20.5+05:00",
        ] {
            let parsed: Timestamp = serde_json::from_value(serde_json::json!(text)).unwrap();
            assert_eq!(parsed, ts(1_700_000_000, 500_000_000), "{text}");
        }
        assert!(serde_json::from_value::<Timestamp>(serde_json::json!("yesterday")).is_err());
    }

    #[test]
    fn out_of_range_nanos_do_not_serialise() {
        for nanos in [-1, 1_000_000_000] {
            assert!(serde_json::to_value(ts(0, nanos)).is_err(), "{nanos}");
        }
    }

    /// The generated messages carry this type, so a `TaskStatus` writes
    /// its timestamp in the mapped form.
    #[test]
    fn task_status_timestamp_uses_the_mapping() {
        let status = crate::TaskStatus {
            state: crate::TaskState::Working.into(),
            message: None,
            timestamp: Some(ts(1_700_000_000, 0)),
        };
        assert_eq!(
            serde_json::to_value(&status).unwrap()["timestamp"],
            "2023-11-14T22:13:20Z"
        );
    }
}
