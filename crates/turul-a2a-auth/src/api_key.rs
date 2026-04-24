//! API Key auth middleware.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use turul_a2a::middleware::{
    A2aMiddleware, AuthFailureKind, AuthIdentity, MiddlewareError, RequestContext,
    SecurityContribution,
};

/// Trait for resolving API key to owner identity.
/// Returns `Some(owner)` if the key is valid, `None` if invalid.
#[async_trait]
pub trait ApiKeyLookup: Send + Sync {
    async fn lookup(&self, key: &str) -> Option<String>;
}

/// Static in-memory API key lookup.
pub struct StaticApiKeyLookup {
    keys: HashMap<String, String>, // key → owner
}

impl StaticApiKeyLookup {
    pub fn new(keys: HashMap<String, String>) -> Self {
        Self { keys }
    }
}

#[async_trait]
impl ApiKeyLookup for StaticApiKeyLookup {
    async fn lookup(&self, key: &str) -> Option<String> {
        self.keys.get(key).cloned()
    }
}

/// First-party `ApiKeyLookup` reference implementation with a redacted
/// `Debug` that never exposes key material (ADR-016 §2.5).
///
/// Internal storage uses a plain `HashMap<String, String>` — keys remain
/// in-process strings but are unreachable via `Debug`, `Display`, or
/// `Serialize`. This is the simplest shape that satisfies the ADR
/// invariant; adopter implementations may reach for more elaborate
/// containers (`secrecy::SecretString` with newtype, etc.) if their
/// deployment requires stronger guarantees.
///
/// ```
/// use std::collections::HashMap;
/// use turul_a2a_auth::RedactedApiKeyLookup;
///
/// let mut keys = HashMap::new();
/// keys.insert("api-key-abc".to_string(), "owner-alice".to_string());
/// let lookup = RedactedApiKeyLookup::new(keys);
///
/// let dbg = format!("{lookup:?}");
/// assert!(!dbg.contains("api-key-abc"), "keys must never print: {dbg}");
/// assert!(dbg.contains("RedactedApiKeyLookup"));
/// ```
pub struct RedactedApiKeyLookup {
    keys: HashMap<String, String>,
}

impl RedactedApiKeyLookup {
    pub fn new(keys: HashMap<String, String>) -> Self {
        Self { keys }
    }

    /// Number of keys currently registered. Safe to expose — reveals no
    /// key material.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

impl fmt::Debug for RedactedApiKeyLookup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedactedApiKeyLookup")
            .field("len", &self.keys.len())
            .finish()
    }
}

#[async_trait]
impl ApiKeyLookup for RedactedApiKeyLookup {
    async fn lookup(&self, key: &str) -> Option<String> {
        self.keys.get(key).cloned()
    }
}

/// API Key auth middleware.
///
/// Extracts API key from a configurable header and validates via `ApiKeyLookup`.
/// Rejects empty/whitespace owner values from lookup (symmetrical with Bearer).
pub struct ApiKeyMiddleware {
    lookup: Arc<dyn ApiKeyLookup>,
    header_name: String,
}

impl ApiKeyMiddleware {
    pub fn new(lookup: Arc<dyn ApiKeyLookup>, header_name: impl Into<String>) -> Self {
        Self {
            lookup,
            header_name: header_name.into(),
        }
    }
}

#[async_trait]
impl A2aMiddleware for ApiKeyMiddleware {
    async fn before_request(&self, ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
        let key = ctx
            .headers
            .get(&self.header_name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let key = match key {
            Some(k) if !k.is_empty() => k,
            _ => {
                return Err(MiddlewareError::Unauthenticated(
                    AuthFailureKind::MissingCredential,
                ));
            }
        };

        let owner = self
            .lookup
            .lookup(&key)
            .await
            .ok_or(MiddlewareError::Unauthenticated(
                AuthFailureKind::InvalidApiKey,
            ))?;

        // Reject empty/whitespace owners — symmetrical with Bearer principal extraction
        if owner.trim().is_empty() {
            return Err(MiddlewareError::Unauthenticated(
                AuthFailureKind::EmptyPrincipal,
            ));
        }

        ctx.identity = AuthIdentity::Authenticated {
            owner,
            claims: None, // API key auth has no JWT claims
        };
        Ok(())
    }

    fn security_contribution(&self) -> SecurityContribution {
        SecurityContribution::new().with_scheme(
            "apiKey",
            turul_a2a_proto::SecurityScheme {
                scheme: Some(
                    turul_a2a_proto::security_scheme::Scheme::ApiKeySecurityScheme(
                        turul_a2a_proto::ApiKeySecurityScheme {
                            description: String::new(),
                            location: "header".into(),
                            name: self.header_name.clone(),
                        },
                    ),
                ),
            },
            vec![],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lookup() -> Arc<dyn ApiKeyLookup> {
        let mut keys = HashMap::new();
        keys.insert("valid-key".to_string(), "user-from-key".to_string());
        keys.insert("empty-owner-key".to_string(), "".to_string());
        keys.insert("whitespace-key".to_string(), "  ".to_string());
        Arc::new(StaticApiKeyLookup::new(keys))
    }

    fn middleware() -> ApiKeyMiddleware {
        ApiKeyMiddleware::new(test_lookup(), "X-API-Key")
    }

    #[tokio::test]
    async fn valid_key_sets_authenticated_identity() {
        let mw = middleware();
        let mut ctx = RequestContext::new();
        ctx.headers
            .insert("x-api-key", "valid-key".parse().unwrap());
        mw.before_request(&mut ctx).await.unwrap();
        assert!(ctx.identity.is_authenticated());
        assert_eq!(ctx.identity.owner(), "user-from-key");
        assert!(ctx.identity.claims().is_none(), "API key has no claims");
    }

    #[tokio::test]
    async fn missing_key_returns_unauthenticated() {
        let mw = middleware();
        let mut ctx = RequestContext::new();
        let err = mw.before_request(&mut ctx).await.unwrap_err();
        assert!(matches!(err, MiddlewareError::Unauthenticated(_)));
    }

    #[tokio::test]
    async fn invalid_key_returns_unauthenticated() {
        let mw = middleware();
        let mut ctx = RequestContext::new();
        ctx.headers.insert("x-api-key", "bad-key".parse().unwrap());
        let err = mw.before_request(&mut ctx).await.unwrap_err();
        assert!(matches!(err, MiddlewareError::Unauthenticated(_)));
    }

    #[tokio::test]
    async fn empty_owner_from_lookup_rejected() {
        let mw = middleware();
        let mut ctx = RequestContext::new();
        ctx.headers
            .insert("x-api-key", "empty-owner-key".parse().unwrap());
        let err = mw.before_request(&mut ctx).await.unwrap_err();
        assert!(matches!(err, MiddlewareError::Unauthenticated(_)));
    }

    #[tokio::test]
    async fn whitespace_owner_from_lookup_rejected() {
        let mw = middleware();
        let mut ctx = RequestContext::new();
        ctx.headers
            .insert("x-api-key", "whitespace-key".parse().unwrap());
        let err = mw.before_request(&mut ctx).await.unwrap_err();
        assert!(matches!(err, MiddlewareError::Unauthenticated(_)));
    }

    #[test]
    fn security_contribution_returns_api_key_scheme() {
        let mw = middleware();
        let contrib = mw.security_contribution();
        assert_eq!(contrib.schemes.len(), 1);
        assert_eq!(contrib.schemes[0].0, "apiKey");
        assert_eq!(contrib.requirements.len(), 1);
    }
}
