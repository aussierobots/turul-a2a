//! Middleware error types — transport-level only, never in A2aError.

/// Middleware errors produce HTTP responses directly via the Tower layer.
/// These never become A2aError variants or JSON-RPC error objects.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum MiddlewareError {
    /// 401 — no credentials or invalid credentials.
    Unauthenticated(String),
    /// 401 with WWW-Authenticate challenge header.
    HttpChallenge {
        status: u16,
        www_authenticate: String,
    },
    /// 403 — credentials valid but insufficient permissions.
    Forbidden(String),
    /// 500 — internal middleware failure (fatal, short-circuits AnyOf).
    Internal(String),
}

impl MiddlewareError {
    /// Precedence for AnyOfMiddleware failure aggregation.
    /// Higher = takes priority when selecting which error to surface.
    pub(crate) fn precedence(&self) -> u8 {
        match self {
            Self::Forbidden(_) => 3,
            Self::HttpChallenge { .. } => 2,
            Self::Unauthenticated(_) => 1,
            Self::Internal(_) => 0, // should never participate in aggregation (short-circuits)
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            Self::Unauthenticated(_) | Self::HttpChallenge { .. } => 401,
            Self::Forbidden(_) => 403,
            Self::Internal(_) => 500,
        }
    }
}
