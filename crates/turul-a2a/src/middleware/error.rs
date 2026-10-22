//! Middleware error types — transport-level only, never in A2aError.
//!
//! Every auth-failure variant carries an [`AuthFailureKind`]; the transport
//! boundary maps that kind to both the `google.rpc.ErrorInfo` reason of the
//! error body and (for Bearer challenges) the RFC 6750 `error=` code in
//! `WWW-Authenticate`. No
//! validator internals, no Debug-formatted enum names, no
//! `error_description`.

/// Stable classification of an auth failure.
///
/// Deliberately coarse. Fine-grained validator discriminants
/// (expired / wrong audience / wrong issuer / unsupported alg / JWKS fetch
/// / key not found / malformed) collapse to [`InvalidToken`](Self::InvalidToken)
/// — RFC 6750 §3.1 conflates these under `invalid_token` on the wire.
///
/// Richer per-reason observability is an adopter-scoped concern handled via
/// a future hook; it is not exposed on the transport wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthFailureKind {
    /// No credential present where one was required (missing `Authorization`
    /// or the configured API-key header).
    MissingCredential,
    /// Any Bearer token validation failure, regardless of inner reason.
    InvalidToken,
    /// API-key lookup rejected the credential (unknown key or
    /// adopter-supplied lookup returned `None`).
    InvalidApiKey,
    /// Authenticated principal resolved to an empty / whitespace string.
    EmptyPrincipal,
    /// The credential is valid but lacks a required scope (Bearer
    /// `with_required_scopes`). Surfaces as 403 with an RFC 6750
    /// `insufficient_scope` challenge.
    InsufficientScope,
}

impl AuthFailureKind {
    /// Stable `ErrorInfo.reason` for this kind (upper snake case). Part of
    /// the wire contract on every transport; must not change.
    pub fn reason(self) -> &'static str {
        match self {
            Self::MissingCredential => "MISSING_CREDENTIAL",
            Self::InvalidToken => "INVALID_TOKEN",
            Self::InvalidApiKey => "INVALID_API_KEY",
            Self::EmptyPrincipal => "EMPTY_PRINCIPAL",
            Self::InsufficientScope => "INSUFFICIENT_SCOPE",
        }
    }

    /// RFC 6750 `error=` code for a Bearer `WWW-Authenticate` challenge.
    ///
    /// Returns `None` for a missing credential — RFC 6750 Section 3.1: "If
    /// the request lacks any authentication information ... the resource
    /// server SHOULD NOT include an error code or other error information"
    /// — and for kinds that are not Bearer failures (API-key rejection).
    pub fn bearer_rfc6750_code(self) -> Option<&'static str> {
        match self {
            Self::InvalidToken | Self::EmptyPrincipal => Some("invalid_token"),
            Self::InsufficientScope => Some("insufficient_scope"),
            Self::MissingCredential | Self::InvalidApiKey => None,
        }
    }

    /// `WWW-Authenticate` value for a Bearer challenge of this kind, or
    /// `None` when the kind is not a Bearer failure. A missing credential
    /// gets the bare challenge `Bearer realm="a2a"`; every other Bearer
    /// failure adds its fixed RFC 6750 `error=` code. `error_description`
    /// is never emitted, so no validator detail reaches the header.
    pub fn bearer_challenge(self) -> Option<String> {
        match self {
            Self::MissingCredential => Some(r#"Bearer realm="a2a""#.to_string()),
            other => other
                .bearer_rfc6750_code()
                .map(|code| format!(r#"Bearer realm="a2a", error="{code}""#)),
        }
    }
}

/// Middleware errors produce HTTP responses directly via the Tower layer.
/// These never become A2aError variants or JSON-RPC error objects.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum MiddlewareError {
    /// 401 — credential missing or rejected, no Bearer challenge.
    Unauthenticated(AuthFailureKind),
    /// 401 with a Bearer `WWW-Authenticate` challenge. The challenge header
    /// is constructed at the transport boundary from the embedded kind.
    HttpChallenge(AuthFailureKind),
    /// 403 — credentials valid but insufficient permissions.
    Forbidden(AuthFailureKind),
    /// 500 — internal middleware failure (fatal, short-circuits AnyOf).
    Internal(String),
}

impl MiddlewareError {
    /// Precedence for AnyOfMiddleware failure aggregation.
    /// Higher = takes priority when selecting which error to surface.
    pub(crate) fn precedence(&self) -> u8 {
        match self {
            Self::Forbidden(_) => 3,
            Self::HttpChallenge(_) => 2,
            Self::Unauthenticated(_) => 1,
            Self::Internal(_) => 0, // should never participate in aggregation (short-circuits)
        }
    }

    /// HTTP status for the failure.
    ///
    /// `HttpChallenge(InsufficientScope)` returns 403, not 401, per
    /// RFC 6750 §3 ("the resource server SHOULD respond with the HTTP
    /// 403 (Forbidden) status code"). The variant remains `HttpChallenge`
    /// so the transport layer still emits the `WWW-Authenticate: Bearer …
    /// error="insufficient_scope"` header that RFC 6750 requires for
    /// scope rejection. All other `HttpChallenge` kinds stay 401.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::HttpChallenge(AuthFailureKind::InsufficientScope) => 403,
            Self::Unauthenticated(_) | Self::HttpChallenge(_) => 401,
            Self::Forbidden(_) => 403,
            Self::Internal(_) => 500,
        }
    }

    /// Auth-failure kind, if any. Returns `None` for `Internal`.
    pub fn kind(&self) -> Option<AuthFailureKind> {
        match self {
            Self::Unauthenticated(k) | Self::HttpChallenge(k) | Self::Forbidden(k) => Some(*k),
            Self::Internal(_) => None,
        }
    }

    /// `ErrorInfo.reason` surfaced on every transport (HTTP+JSON and
    /// JSON-RPC bodies, gRPC status details).
    ///
    /// `Internal` collapses to the fixed `"INTERNAL_ERROR"` — the inner
    /// `String` payload is intentionally NOT exposed on the wire (would
    /// leak validator internals, DB errors, etc.).
    pub fn reason(&self) -> &'static str {
        match self.kind() {
            Some(kind) => kind.reason(),
            None => "INTERNAL_ERROR",
        }
    }

    /// Human-readable description for the error body. Fixed per kind so
    /// nothing from the validator or the request leaks.
    pub fn message(&self) -> &'static str {
        match self.kind() {
            Some(AuthFailureKind::MissingCredential) => "authentication credentials are required",
            Some(AuthFailureKind::InvalidToken) => "the bearer token is invalid",
            Some(AuthFailureKind::InvalidApiKey) => "the API key is invalid",
            Some(AuthFailureKind::EmptyPrincipal) => "the credential does not identify a principal",
            Some(AuthFailureKind::InsufficientScope) => "the credential lacks a required scope",
            None => "authentication failed due to an internal error",
        }
    }

    /// google.rpc.Code name for the HTTP error body.
    fn status_name(&self) -> &'static str {
        match self.http_status() {
            401 => "UNAUTHENTICATED",
            403 => "PERMISSION_DENIED",
            _ => "INTERNAL",
        }
    }

    /// The stable kind as a google.rpc.ErrorInfo detail: `reason` is the
    /// kind in upper snake case, `domain` is the turul auth domain.
    fn error_info(&self) -> serde_json::Value {
        use turul_a2a_types::wire::errors;
        serde_json::json!({
            "@type": errors::ERROR_INFO_TYPE,
            "reason": self.reason(),
            "domain": errors::AUTH_ERROR_DOMAIN,
        })
    }

    /// HTTP+JSON error body (spec: every error response carries a code and
    /// a message): `{"error": {code, status, message, details}}`.
    pub fn http_body(&self) -> serde_json::Value {
        serde_json::json!({
            "error": {
                "code": self.http_status(),
                "status": self.status_name(),
                "message": self.message(),
                "details": [self.error_info()],
            }
        })
    }

    /// JSON-RPC error envelope for a request to the JSON-RPC endpoint (the
    /// request id is unknown at this point, so it is null).
    pub fn jsonrpc_body(&self) -> serde_json::Value {
        use turul_a2a_types::wire::errors;
        let code = match self.http_status() {
            401 => errors::JSONRPC_UNAUTHENTICATED,
            403 => errors::JSONRPC_PERMISSION_DENIED,
            _ => errors::JSONRPC_INTERNAL_ERROR,
        };
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": serde_json::Value::Null,
            "error": {
                "code": code,
                "message": self.message(),
                "data": [self.error_info()],
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_stable() {
        assert_eq!(
            AuthFailureKind::MissingCredential.reason(),
            "MISSING_CREDENTIAL"
        );
        assert_eq!(AuthFailureKind::InvalidToken.reason(), "INVALID_TOKEN");
        assert_eq!(AuthFailureKind::InvalidApiKey.reason(), "INVALID_API_KEY");
        assert_eq!(AuthFailureKind::EmptyPrincipal.reason(), "EMPTY_PRINCIPAL");
        assert_eq!(
            AuthFailureKind::InsufficientScope.reason(),
            "INSUFFICIENT_SCOPE"
        );
        assert_eq!(
            MiddlewareError::Internal("db down".into()).reason(),
            "INTERNAL_ERROR"
        );
    }

    #[test]
    fn bearer_rfc6750_mapping() {
        assert_eq!(
            AuthFailureKind::MissingCredential.bearer_rfc6750_code(),
            None,
            "RFC 6750 Section 3.1: no error code when credentials are absent"
        );
        assert_eq!(
            AuthFailureKind::InvalidToken.bearer_rfc6750_code(),
            Some("invalid_token")
        );
        assert_eq!(
            AuthFailureKind::EmptyPrincipal.bearer_rfc6750_code(),
            Some("invalid_token")
        );
        assert_eq!(
            AuthFailureKind::InsufficientScope.bearer_rfc6750_code(),
            Some("insufficient_scope")
        );
        assert_eq!(AuthFailureKind::InvalidApiKey.bearer_rfc6750_code(), None);
    }

    #[test]
    fn bearer_challenge_values() {
        assert_eq!(
            AuthFailureKind::MissingCredential
                .bearer_challenge()
                .as_deref(),
            Some(r#"Bearer realm="a2a""#)
        );
        assert_eq!(
            AuthFailureKind::InvalidToken.bearer_challenge().as_deref(),
            Some(r#"Bearer realm="a2a", error="invalid_token""#)
        );
        assert_eq!(
            AuthFailureKind::EmptyPrincipal
                .bearer_challenge()
                .as_deref(),
            Some(r#"Bearer realm="a2a", error="invalid_token""#)
        );
        assert_eq!(
            AuthFailureKind::InsufficientScope
                .bearer_challenge()
                .as_deref(),
            Some(r#"Bearer realm="a2a", error="insufficient_scope""#)
        );
        assert_eq!(AuthFailureKind::InvalidApiKey.bearer_challenge(), None);
    }

    #[test]
    fn http_status_codes() {
        assert_eq!(
            MiddlewareError::Unauthenticated(AuthFailureKind::MissingCredential).http_status(),
            401
        );
        assert_eq!(
            MiddlewareError::HttpChallenge(AuthFailureKind::InvalidToken).http_status(),
            401
        );
        assert_eq!(
            MiddlewareError::Forbidden(AuthFailureKind::InsufficientScope).http_status(),
            403
        );
        assert_eq!(MiddlewareError::Internal("x".into()).http_status(), 500);
    }

    #[test]
    fn http_challenge_insufficient_scope_returns_403() {
        // RFC 6750 §3: scope rejection must be 403 with a Bearer challenge.
        assert_eq!(
            MiddlewareError::HttpChallenge(AuthFailureKind::InsufficientScope).http_status(),
            403
        );
    }
}
