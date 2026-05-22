//! Skill-invocation dispatcher profile — wire-bookkeeping for the
//! `A2A-Extensions` header.
//!
//! This module is wire-only:
//!
//! - [`parse_a2a_extensions`] decodes a comma-separated request header
//!   value into a set of URIs.
//! - [`validate_activation`] reconciles activated URIs with the agent
//!   card's advertised extensions, returning the intersection or an
//!   `UnsupportedOperationError` when a `required = true` extension is
//!   advertised but not activated by the client.
//! - [`response_header_value`] formats a non-empty intersection back
//!   into a header value for echo on the response.
//!
//! Payload routing — reading `Message.metadata["a2a.skillId"]` /
//! `["a2a.skillParams"]` and dispatching to a specific skill — is
//! adopter code inside the `AgentExecutor`. The framework only
//! standardises the activation contract on the wire.
//!
//! The transport layer (HTTP + JSON-RPC + gRPC) invokes this module
//! before the executor runs and echoes activated URIs on the response.

use std::collections::HashSet;

use crate::error::A2aError;

/// Canonical URI for the v1 skill-invocation dispatcher profile.
///
/// Servers advertise this URI in
/// `AgentCard.capabilities.extensions[].uri` to opt into the contract,
/// and clients activate it by sending the URI in the
/// `A2A-Extensions` request header.
pub const SKILL_INVOCATION_PROFILE_V1: &str =
    "https://turul.dev/a2a/extensions/skill-invocation/v1";

/// Parse the value of an inbound `A2A-Extensions` request header into
/// a set of activated URIs.
///
/// The header is comma-separated per the A2A spec; whitespace around
/// each segment is trimmed; empty segments are dropped. A `None` header
/// or a header containing only whitespace yields an empty set.
///
/// Set semantics are used (not `Vec`) because activation is membership-
/// based, not order-sensitive, and duplicate URIs in the header are
/// equivalent to a single activation.
pub fn parse_a2a_extensions(header_value: Option<&str>) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(raw) = header_value else {
        return out;
    };
    for segment in raw.split(',') {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.insert(trimmed.to_string());
    }
    out
}

/// Reconcile activated URIs against the agent's advertised extensions.
///
/// Returns the intersection (URIs both advertised by the server and
/// activated by the client). When the server advertises any extension
/// with `required = true` whose URI is *not* present in `activated`,
/// returns `UnsupportedOperationError`.
///
/// Unknown URIs in `activated` (not advertised by the server) are
/// silently ignored, mirroring A2A spec guidance: a server is not
/// obligated to error on an unknown extension unless that extension is
/// marked required (in which case the requirement is on the server's
/// side, not the client's).
pub fn validate_activation(
    activated: &HashSet<String>,
    advertised: &[turul_a2a_proto::AgentExtension],
) -> Result<HashSet<String>, A2aError> {
    // First, ensure every required extension advertised by this server
    // appears in the activation set. If not, this request cannot be
    // served — the server has declared a hard prerequisite the client
    // did not meet.
    for ext in advertised {
        if ext.required && !activated.contains(&ext.uri) {
            return Err(A2aError::UnsupportedOperation {
                message: format!(
                    "Required extension '{}' not activated by client (send A2A-Extensions header)",
                    ext.uri
                ),
            });
        }
    }

    // Intersection: keep only URIs the server has actually advertised.
    let advertised_uris: HashSet<&str> = advertised.iter().map(|e| e.uri.as_str()).collect();
    let intersection: HashSet<String> = activated
        .iter()
        .filter(|uri| advertised_uris.contains(uri.as_str()))
        .cloned()
        .collect();
    Ok(intersection)
}

/// Format a non-empty set of activated URIs into a comma-separated
/// header value suitable for echo on the response. Returns `None` for
/// an empty set so the caller can skip the header entirely.
///
/// URIs are emitted in sorted order to keep the header value stable
/// across requests and easy to assert on in tests; A2A clients treat
/// the field as set-valued, so ordering is not part of the wire
/// contract.
pub fn response_header_value(activated: &HashSet<String>) -> Option<String> {
    if activated.is_empty() {
        return None;
    }
    let mut sorted: Vec<&str> = activated.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    Some(sorted.join(", "))
}

/// HTTP header name used for both activation (request) and echo
/// (response). HTTP is case-insensitive; gRPC metadata requires
/// lowercase, so we store the canonical lowercase form here and let
/// HTTP handlers compare case-insensitively.
pub const A2A_EXTENSIONS_HEADER: &str = "a2a-extensions";

/// Convenience: extract the activated set from an axum `HeaderMap`.
pub fn activated_from_headers(headers: &axum::http::HeaderMap) -> HashSet<String> {
    let raw = headers
        .get(A2A_EXTENSIONS_HEADER)
        .or_else(|| headers.get("A2A-Extensions"))
        .and_then(|v| v.to_str().ok());
    parse_a2a_extensions(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(uri: &str, required: bool) -> turul_a2a_proto::AgentExtension {
        turul_a2a_proto::AgentExtension {
            uri: uri.into(),
            description: String::new(),
            required,
            params: None,
        }
    }

    #[test]
    fn parser_empty_and_whitespace() {
        assert!(parse_a2a_extensions(None).is_empty());
        assert!(parse_a2a_extensions(Some("")).is_empty());
        assert!(parse_a2a_extensions(Some("   ,  ,")).is_empty());
    }

    #[test]
    fn parser_trims_and_splits() {
        let set = parse_a2a_extensions(Some(" a , b , a "));
        assert_eq!(set.len(), 2);
        assert!(set.contains("a"));
        assert!(set.contains("b"));
    }

    #[test]
    fn validate_required_missing_errors() {
        let advertised = vec![ext("uri-1", true)];
        let activated = HashSet::new();
        assert!(matches!(
            validate_activation(&activated, &advertised),
            Err(A2aError::UnsupportedOperation { .. })
        ));
    }

    #[test]
    fn validate_required_present_ok() {
        let advertised = vec![ext("uri-1", true)];
        let mut activated = HashSet::new();
        activated.insert("uri-1".to_string());
        let result = validate_activation(&activated, &advertised).unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn validate_unknown_uri_ignored() {
        let advertised = vec![ext("uri-1", false)];
        let mut activated = HashSet::new();
        activated.insert("uri-2".to_string());
        let result = validate_activation(&activated, &advertised).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn header_value_stable_sort() {
        let mut set = HashSet::new();
        set.insert("b".to_string());
        set.insert("a".to_string());
        assert_eq!(response_header_value(&set).unwrap(), "a, b");
    }
}
