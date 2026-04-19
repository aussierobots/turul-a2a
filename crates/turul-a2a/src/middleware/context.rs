//! Request context and auth identity types.

use std::collections::HashMap;

/// Authentication identity — type-level distinction, not a sentinel string.
///
/// Per ADR-007: auth state is an enum, not `owner != "anonymous"`.
#[derive(Debug, Clone)]
#[derive(Default)]
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


/// Request context threaded through middleware and handlers.
#[derive(Debug, Clone)]
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
