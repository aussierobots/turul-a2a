//! Bearer/JWT auth middleware.

use std::sync::Arc;

use async_trait::async_trait;
use turul_a2a::middleware::{
    A2aMiddleware, AuthFailureKind, AuthIdentity, MiddlewareError, RequestContext,
    SecurityContribution,
};
use turul_jwt_validator::JwtValidator;

/// Bearer token auth middleware using JWT validation.
///
/// Extracts owner from a configurable JWT claim (default: "sub").
/// Rejects empty/missing principals.
pub struct BearerMiddleware {
    validator: Arc<JwtValidator>,
    /// JWT claim to extract as owner (default: "sub")
    principal_claim: String,
    /// Required scopes (empty = no scope requirement)
    required_scopes: Vec<String>,
}

impl BearerMiddleware {
    pub fn new(validator: Arc<JwtValidator>) -> Self {
        Self {
            validator,
            principal_claim: "sub".into(),
            required_scopes: vec![],
        }
    }

    pub fn with_principal_claim(mut self, claim: impl Into<String>) -> Self {
        self.principal_claim = claim.into();
        self
    }

    pub fn with_required_scopes(mut self, scopes: Vec<String>) -> Self {
        self.required_scopes = scopes;
        self
    }
}

#[async_trait]
impl A2aMiddleware for BearerMiddleware {
    async fn before_request(&self, ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
        let token = ctx
            .bearer_token
            .as_deref()
            .ok_or(MiddlewareError::HttpChallenge(
                AuthFailureKind::MissingCredential,
            ))?;

        // ADR-016 §2.1: every validator failure collapses to `InvalidToken`.
        // The original validator error is intentionally discarded — leaking
        // it through `error_description` would expose JWKS URLs, jsonwebtoken
        // internals, or token fragments on the public response header.
        let claims = self
            .validator
            .validate(token)
            .await
            .map_err(|_| MiddlewareError::HttpChallenge(AuthFailureKind::InvalidToken))?;

        // Extract principal from configured claim
        let owner = if self.principal_claim == "sub" {
            claims.sub.clone()
        } else {
            claims
                .extra
                .get(&self.principal_claim)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };

        // Reject empty/whitespace principal
        if owner.trim().is_empty() {
            return Err(MiddlewareError::Unauthenticated(
                AuthFailureKind::EmptyPrincipal,
            ));
        }

        let claims_json = serde_json::to_value(&claims).ok();

        ctx.identity = AuthIdentity::Authenticated {
            owner,
            claims: claims_json,
        };
        Ok(())
    }

    fn security_contribution(&self) -> SecurityContribution {
        SecurityContribution::new().with_scheme(
            "bearer",
            turul_a2a_proto::SecurityScheme {
                scheme: Some(
                    turul_a2a_proto::security_scheme::Scheme::HttpAuthSecurityScheme(
                        turul_a2a_proto::HttpAuthSecurityScheme {
                            description: String::new(),
                            scheme: "Bearer".into(),
                            bearer_format: "JWT".into(),
                        },
                    ),
                ),
            },
            self.required_scopes.clone(),
        )
    }
}
