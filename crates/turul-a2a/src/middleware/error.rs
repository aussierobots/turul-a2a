//! Middleware error types — transport-level only, never in A2aError.
//!
//! Wire surface governed by ADR-016. Every auth-failure variant carries an
//! [`AuthFailureKind`]; the transport boundary maps that kind to both the
//! JSON body string and (for Bearer challenges) the RFC 6750 `error=` code
//! in `WWW-Authenticate`. No validator internals, no Debug-formatted
//! enum names, no `error_description`.

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
    /// Reserved. Not produced by any current code path — scope enforcement
    /// in `BearerMiddleware::required_scopes` is not wired. Kept in the
    /// taxonomy so the wire mapping is stable when scope checks land.
    InsufficientScope,
}

impl AuthFailureKind {
    /// Stable snake_case identifier emitted as the JSON body `error` value.
    /// This string is part of the wire contract and must not change.
    pub fn body_string(self) -> &'static str {
        match self {
            Self::MissingCredential => "missing_credential",
            Self::InvalidToken => "invalid_token",
            Self::InvalidApiKey => "invalid_api_key",
            Self::EmptyPrincipal => "empty_principal",
            Self::InsufficientScope => "insufficient_scope",
        }
    }

    /// RFC 6750 `error=` code for a Bearer `WWW-Authenticate` challenge.
    /// Returns `None` when the kind does not map onto a Bearer-scheme
    /// challenge (for example, API-key-specific rejection).
    pub fn bearer_rfc6750_code(self) -> Option<&'static str> {
        match self {
            Self::MissingCredential => Some("invalid_request"),
            Self::InvalidToken | Self::EmptyPrincipal => Some("invalid_token"),
            Self::InsufficientScope => Some("insufficient_scope"),
            Self::InvalidApiKey => None,
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

    pub fn http_status(&self) -> u16 {
        match self {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_strings_are_stable() {
        assert_eq!(
            AuthFailureKind::MissingCredential.body_string(),
            "missing_credential"
        );
        assert_eq!(AuthFailureKind::InvalidToken.body_string(), "invalid_token");
        assert_eq!(
            AuthFailureKind::InvalidApiKey.body_string(),
            "invalid_api_key"
        );
        assert_eq!(
            AuthFailureKind::EmptyPrincipal.body_string(),
            "empty_principal"
        );
        assert_eq!(
            AuthFailureKind::InsufficientScope.body_string(),
            "insufficient_scope"
        );
    }

    #[test]
    fn bearer_rfc6750_mapping() {
        assert_eq!(
            AuthFailureKind::MissingCredential.bearer_rfc6750_code(),
            Some("invalid_request")
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
}
