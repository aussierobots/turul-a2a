//! Lambda authorizer middleware — reads trusted x-authorizer-* headers.

use async_trait::async_trait;
use turul_a2a::middleware::{A2aMiddleware, AuthIdentity, MiddlewareError, RequestContext};

use crate::adapter::AUTHORIZER_HEADER_PREFIX;

/// Mapping configuration for Lambda authorizer context.
#[derive(Debug, Clone)]
pub struct AuthorizerMapping {
    /// Authorizer field that maps to owner (default: "sub")
    pub owner_field: String,
    /// Whether to collect all authorizer fields as claims
    pub include_claims: bool,
}

impl Default for AuthorizerMapping {
    fn default() -> Self {
        Self {
            owner_field: "sub".into(),
            include_claims: true,
        }
    }
}

/// Middleware that reads trusted authorizer context from x-authorizer-* headers.
///
/// These headers are injected by the Lambda adapter from `requestContext.authorizer`
/// after stripping any client-supplied headers with the same prefix (anti-spoofing).
pub struct LambdaAuthorizerMiddleware {
    mapping: AuthorizerMapping,
}

impl LambdaAuthorizerMiddleware {
    pub fn new(mapping: AuthorizerMapping) -> Self {
        Self { mapping }
    }
}

#[async_trait]
impl A2aMiddleware for LambdaAuthorizerMiddleware {
    async fn before_request(&self, ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
        let owner_header = format!("{AUTHORIZER_HEADER_PREFIX}{}", self.mapping.owner_field);
        let owner = ctx
            .headers
            .get(owner_header.as_str())
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        match owner {
            Some(owner) if !owner.trim().is_empty() => {
                let claims = if self.mapping.include_claims {
                    Some(collect_authorizer_claims(&ctx.headers))
                } else {
                    None
                };
                ctx.identity = AuthIdentity::Authenticated { owner, claims };
                Ok(())
            }
            _ => Err(MiddlewareError::Unauthenticated(
                "Missing authorizer context".into(),
            )),
        }
    }
}

/// Collect all x-authorizer-* headers into a JSON object.
fn collect_authorizer_claims(headers: &http::HeaderMap) -> serde_json::Value {
    let mut claims = serde_json::Map::new();
    for (key, value) in headers.iter() {
        let key_str = key.as_str();
        if let Some(field) = key_str.strip_prefix(AUTHORIZER_HEADER_PREFIX) {
            if let Ok(val) = value.to_str() {
                claims.insert(
                    field.to_string(),
                    serde_json::Value::String(val.to_string()),
                );
            }
        }
    }
    serde_json::Value::Object(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn authorizer_middleware_extracts_owner() {
        let mw = LambdaAuthorizerMiddleware::new(AuthorizerMapping::default());
        let mut ctx = RequestContext::new();
        ctx.headers.insert(
            http::header::HeaderName::from_static("x-authorizer-sub"),
            "user-123".parse().unwrap(),
        );

        mw.before_request(&mut ctx).await.unwrap();
        assert!(ctx.identity.is_authenticated());
        assert_eq!(ctx.identity.owner(), "user-123");
    }

    #[tokio::test]
    async fn authorizer_middleware_custom_owner_field() {
        let mw = LambdaAuthorizerMiddleware::new(AuthorizerMapping {
            owner_field: "userId".into(),
            include_claims: false,
        });
        let mut ctx = RequestContext::new();
        ctx.headers.insert(
            http::header::HeaderName::from_static("x-authorizer-userid"),
            "custom-user".parse().unwrap(),
        );

        mw.before_request(&mut ctx).await.unwrap();
        assert_eq!(ctx.identity.owner(), "custom-user");
    }

    #[tokio::test]
    async fn authorizer_middleware_rejects_missing_owner() {
        let mw = LambdaAuthorizerMiddleware::new(AuthorizerMapping::default());
        let ctx = &mut RequestContext::new();
        let err = mw.before_request(ctx).await.unwrap_err();
        assert!(matches!(err, MiddlewareError::Unauthenticated(_)));
    }

    #[tokio::test]
    async fn authorizer_middleware_rejects_empty_owner() {
        let mw = LambdaAuthorizerMiddleware::new(AuthorizerMapping::default());
        let mut ctx = RequestContext::new();
        ctx.headers.insert(
            http::header::HeaderName::from_static("x-authorizer-sub"),
            "".parse().unwrap(),
        );
        let err = mw.before_request(&mut ctx).await.unwrap_err();
        assert!(matches!(err, MiddlewareError::Unauthenticated(_)));
    }

    #[tokio::test]
    async fn authorizer_middleware_collects_claims() {
        let mw = LambdaAuthorizerMiddleware::new(AuthorizerMapping {
            owner_field: "sub".into(),
            include_claims: true,
        });
        let mut ctx = RequestContext::new();
        ctx.headers.insert(
            http::header::HeaderName::from_static("x-authorizer-sub"),
            "user-1".parse().unwrap(),
        );
        ctx.headers.insert(
            http::header::HeaderName::from_static("x-authorizer-role"),
            "admin".parse().unwrap(),
        );

        mw.before_request(&mut ctx).await.unwrap();
        let claims = ctx.identity.claims().unwrap();
        assert_eq!(claims["sub"], "user-1");
        assert_eq!(claims["role"], "admin");
    }
}
