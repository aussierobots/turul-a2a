//! `google.rpc.ErrorInfo` in its proto3 JSON form, as carried in A2A error
//! details (HTTP `error.details[]`, JSON-RPC `error.data[]`) and in data
//! parts that describe why a task failed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::wire::errors::ERROR_INFO_TYPE;

/// A `google.rpc.ErrorInfo`: a machine-readable `reason` within a `domain`,
/// with optional string `metadata`.
///
/// Serializes with `"@type": "type.googleapis.com/google.rpc.ErrorInfo"`
/// and omits empty `metadata`. Deserializing requires that `@type`, so
/// another detail type in the same array is never mistaken for an
/// `ErrorInfo`, and requires `reason` and `domain`, without which the
/// detail cannot be matched on; `metadata` may be absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ErrorInfoJson", into = "ErrorInfoJson")]
#[non_exhaustive]
pub struct ErrorInfo {
    /// Stable UPPER_SNAKE_CASE cause of the error within `domain`.
    pub reason: String,
    /// The logical grouping `reason` belongs to, e.g. `a2a-protocol.org`.
    pub domain: String,
    /// Additional structured details, string to string.
    pub metadata: BTreeMap<String, String>,
}

impl ErrorInfo {
    /// An `ErrorInfo` with no metadata.
    pub fn new(reason: impl Into<String>, domain: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            domain: domain.into(),
            metadata: BTreeMap::new(),
        }
    }

    /// Add one metadata entry, replacing any earlier value for `key`.
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

impl From<ErrorInfo> for serde_json::Value {
    fn from(info: ErrorInfo) -> Self {
        serde_json::to_value(info).expect("string fields always serialize")
    }
}

#[derive(Serialize, Deserialize)]
struct ErrorInfoJson {
    #[serde(rename = "@type")]
    type_url: String,
    reason: String,
    domain: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    metadata: BTreeMap<String, String>,
}

impl From<ErrorInfo> for ErrorInfoJson {
    fn from(info: ErrorInfo) -> Self {
        Self {
            type_url: ERROR_INFO_TYPE.to_string(),
            reason: info.reason,
            domain: info.domain,
            metadata: info.metadata,
        }
    }
}

impl TryFrom<ErrorInfoJson> for ErrorInfo {
    type Error = String;

    fn try_from(json: ErrorInfoJson) -> Result<Self, Self::Error> {
        if json.type_url != ERROR_INFO_TYPE {
            return Err(format!(
                "@type is {:?}, expected {ERROR_INFO_TYPE:?}",
                json.type_url
            ));
        }
        Ok(Self {
            reason: json.reason,
            domain: json.domain,
            metadata: json.metadata,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_to_google_rpc_error_info_json() {
        let bare = ErrorInfo::new("TASK_NOT_FOUND", "a2a-protocol.org");
        assert_eq!(
            serde_json::Value::from(bare),
            json!({
                "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                "reason": "TASK_NOT_FOUND",
                "domain": "a2a-protocol.org",
            })
        );

        let with_metadata = ErrorInfo::new("UPSTREAM_TIMEOUT", "example.com")
            .with_metadata("service", "orbits")
            .with_metadata("timeoutMs", "500");
        assert_eq!(
            serde_json::Value::from(with_metadata),
            json!({
                "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                "reason": "UPSTREAM_TIMEOUT",
                "domain": "example.com",
                "metadata": {"service": "orbits", "timeoutMs": "500"},
            })
        );
    }

    #[test]
    fn round_trips() {
        for info in [
            ErrorInfo::new("TASK_NOT_FOUND", "a2a-protocol.org"),
            ErrorInfo::new("UPSTREAM_TIMEOUT", "example.com").with_metadata("service", "orbits"),
        ] {
            let text = serde_json::to_string(&info).unwrap();
            assert_eq!(serde_json::from_str::<ErrorInfo>(&text).unwrap(), info);
        }
    }

    #[test]
    fn rejects_json_that_is_not_an_error_info() {
        for (case, value) in [
            (
                "other @type",
                json!({"@type": "type.googleapis.com/google.rpc.RetryInfo",
                       "reason": "R", "domain": "d"}),
            ),
            ("missing @type", json!({"reason": "R", "domain": "d"})),
            (
                "missing reason",
                json!({"@type": ERROR_INFO_TYPE, "domain": "d"}),
            ),
            (
                "missing domain",
                json!({"@type": ERROR_INFO_TYPE, "reason": "R"}),
            ),
            (
                "non-string metadata value",
                json!({"@type": ERROR_INFO_TYPE, "reason": "R", "domain": "d",
                       "metadata": {"attempts": 3}}),
            ),
        ] {
            assert!(
                serde_json::from_value::<ErrorInfo>(value).is_err(),
                "{case} must not deserialize"
            );
        }
    }
}
