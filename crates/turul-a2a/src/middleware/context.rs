//! Request context and auth identity types.

use std::collections::HashMap;
use std::fmt;

/// Authentication identity — type-level distinction, not a sentinel string.
///
/// Per ADR-007: auth state is an enum, not `owner != "anonymous"`.
#[derive(Clone, Default)]
pub enum AuthIdentity {
    /// No auth middleware configured or no credentials provided.
    #[default]
    Anonymous,
    /// Authenticated principal with owner and optional claims.
    Authenticated {
        owner: String,
        claims: Option<serde_json::Value>,
    },
}

impl AuthIdentity {
    pub fn is_authenticated(&self) -> bool {
        matches!(self, Self::Authenticated { .. })
    }

    /// Owner string for storage calls. Anonymous returns "anonymous".
    pub fn owner(&self) -> &str {
        match self {
            Self::Anonymous => "anonymous",
            Self::Authenticated { owner, .. } => owner,
        }
    }

    /// Claims from JWT validation (None for API key auth or anonymous).
    pub fn claims(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Anonymous => None,
            Self::Authenticated { claims, .. } => claims.as_ref(),
        }
    }
}

/// `Debug` is manually implemented to redact `claims` — JWT claims can
/// carry PII-class data (email, roles, custom fields) and must not leak
/// through default-derived Debug output (ADR-016 §2.4).
impl fmt::Debug for AuthIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Anonymous => write!(f, "Anonymous"),
            Self::Authenticated { owner, claims } => f
                .debug_struct("Authenticated")
                .field("owner", owner)
                .field(
                    "claims",
                    &match claims {
                        Some(_) => "<redacted>",
                        None => "<none>",
                    },
                )
                .finish(),
        }
    }
}

/// Request context threaded through middleware and handlers.
#[derive(Clone)]
pub struct RequestContext {
    pub bearer_token: Option<String>,
    pub headers: http::HeaderMap,
    pub identity: AuthIdentity,
    pub extensions: HashMap<String, serde_json::Value>,
}

impl RequestContext {
    pub fn new() -> Self {
        Self {
            bearer_token: None,
            headers: http::HeaderMap::new(),
            identity: AuthIdentity::default(),
            extensions: HashMap::new(),
        }
    }
}

impl Default for RequestContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Default-deny Debug redaction (ADR-016 §2.4).
///
/// - `bearer_token`: `Some(<redacted>)` when present.
/// - `headers`: header names always printed; values printed only for a
///   fixed allowlist of known-safe headers; all others redacted.
///   `Authorization`, `Cookie`, `X-API-Key`, and any adopter-defined
///   credential header are redacted by default because they are not in
///   the allowlist.
/// - `identity`: delegates to the redacted `AuthIdentity::Debug`.
/// - `extensions`: map keys printed, values redacted.
impl fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Redacted;
        impl fmt::Debug for Redacted {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "<redacted>")
            }
        }

        struct HeadersDbg<'a>(&'a http::HeaderMap);
        impl<'a> fmt::Debug for HeadersDbg<'a> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let mut m = f.debug_map();
                for (name, value) in self.0.iter() {
                    if is_allowlisted_header(name.as_str()) {
                        m.entry(&name.as_str(), &value.to_str().unwrap_or("<non-utf8>"));
                    } else {
                        m.entry(&name.as_str(), &"<redacted>");
                    }
                }
                m.finish()
            }
        }

        struct ExtensionsDbg<'a>(&'a HashMap<String, serde_json::Value>);
        impl<'a> fmt::Debug for ExtensionsDbg<'a> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let mut m = f.debug_map();
                for key in self.0.keys() {
                    m.entry(key, &"<redacted>");
                }
                m.finish()
            }
        }

        let token_field: &dyn fmt::Debug = match &self.bearer_token {
            Some(_) => &Some(Redacted),
            None => &None::<Redacted>,
        };

        f.debug_struct("RequestContext")
            .field("bearer_token", token_field)
            .field("headers", &HeadersDbg(&self.headers))
            .field("identity", &self.identity)
            .field("extensions", &ExtensionsDbg(&self.extensions))
            .finish()
    }
}

/// Fixed allowlist of headers whose values are non-sensitive. Everything
/// outside this list is redacted — including `Authorization`, `Cookie`,
/// and any adopter-defined credential header. Case-insensitive (HTTP
/// header names are case-insensitive per RFC 7230 §3.2).
fn is_allowlisted_header(name: &str) -> bool {
    const ALLOWLIST: &[&str] = &[
        "content-type",
        "content-length",
        "accept",
        "user-agent",
        "host",
    ];
    let lower = name.to_ascii_lowercase();
    ALLOWLIST.iter().any(|h| *h == lower)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue};

    #[test]
    fn anonymous_debug_is_plain_string() {
        let s = format!("{:?}", AuthIdentity::Anonymous);
        assert_eq!(s, "Anonymous");
    }

    #[test]
    fn authenticated_debug_shows_owner_but_redacts_claims() {
        let identity = AuthIdentity::Authenticated {
            owner: "user-123".into(),
            claims: Some(serde_json::json!({
                "sub": "user-123",
                "email": "secret@example.com",
                "roles": ["admin"]
            })),
        };
        let s = format!("{identity:?}");
        assert!(s.contains("user-123"), "owner should be visible: {s}");
        assert!(s.contains("<redacted>"), "claims should be redacted: {s}");
        assert!(
            !s.contains("secret@example.com"),
            "email must not leak: {s}"
        );
        assert!(!s.contains("admin"), "role must not leak: {s}");
    }

    #[test]
    fn authenticated_debug_with_no_claims_shows_none_marker() {
        let identity = AuthIdentity::Authenticated {
            owner: "u".into(),
            claims: None,
        };
        let s = format!("{identity:?}");
        assert!(s.contains("<none>"), "absent claims should be marked: {s}");
    }

    fn make_request_context() -> RequestContext {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer eyJabcdef.secret-jwt-body.signature"),
        );
        headers.insert("x-api-key", HeaderValue::from_static("super-secret-key"));
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            http::header::COOKIE,
            HeaderValue::from_static("session=sensitive"),
        );
        headers.insert("x-custom-auth", HeaderValue::from_static("custom-secret"));

        let mut extensions = HashMap::new();
        extensions.insert("api_key".to_string(), serde_json::json!("another-secret"));

        RequestContext {
            bearer_token: Some("eyJabcdef.secret-jwt-body.signature".into()),
            headers,
            identity: AuthIdentity::Authenticated {
                owner: "user-123".into(),
                claims: Some(serde_json::json!({"sub": "user-123"})),
            },
            extensions,
        }
    }

    #[test]
    fn request_context_debug_redacts_bearer_token() {
        let s = format!("{:?}", make_request_context());
        assert!(
            !s.contains("secret-jwt-body"),
            "bearer token must not leak: {s}"
        );
        assert!(!s.contains("eyJabcdef"), "bearer token must not leak: {s}");
    }

    #[test]
    fn request_context_debug_redacts_sensitive_headers() {
        let s = format!("{:?}", make_request_context());
        assert!(
            !s.contains("super-secret-key"),
            "X-API-Key must not leak: {s}"
        );
        assert!(
            !s.contains("session=sensitive"),
            "Cookie must not leak: {s}"
        );
        assert!(
            !s.contains("custom-secret"),
            "adopter-defined credential must not leak: {s}"
        );
    }

    #[test]
    fn request_context_debug_shows_allowlisted_header_values() {
        let s = format!("{:?}", make_request_context());
        assert!(
            s.contains("application/json"),
            "Content-Type value should be visible: {s}"
        );
    }

    #[test]
    fn request_context_debug_shows_header_names() {
        let s = format!("{:?}", make_request_context());
        assert!(
            s.contains("x-api-key"),
            "header name should be visible: {s}"
        );
        assert!(
            s.contains("x-custom-auth"),
            "header name should be visible: {s}"
        );
    }

    #[test]
    fn request_context_debug_redacts_extension_values() {
        let s = format!("{:?}", make_request_context());
        assert!(
            s.contains("api_key"),
            "extension key should be visible: {s}"
        );
        assert!(
            !s.contains("another-secret"),
            "extension value must not leak: {s}"
        );
    }

    #[test]
    fn allowlist_is_case_insensitive() {
        assert!(is_allowlisted_header("Content-Type"));
        assert!(is_allowlisted_header("CONTENT-TYPE"));
        assert!(is_allowlisted_header("content-type"));
        assert!(!is_allowlisted_header("Authorization"));
    }
}
