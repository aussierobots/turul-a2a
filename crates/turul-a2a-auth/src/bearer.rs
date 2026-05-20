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

        // every validator failure collapses to `InvalidToken`.
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

        // Authorization: enforce required_scopes against claims.scope.
        // Returns HttpChallenge(InsufficientScope) so the transport layer
        // emits the RFC 6750 §3 `WWW-Authenticate: Bearer … error="insufficient_scope"`
        // header at 403 status (special-cased in `MiddlewareError::http_status`).
        // The `scp` claim variant (array form, used by some IdPs) is not
        // supported; only the OAuth 2.0 `scope` claim (space-delimited per
        // RFC 6749 §3.3).
        enforce_required_scopes(&self.required_scopes, claims.scope.as_deref())?;

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

/// Check that every `required` scope is present as an exact token in
/// `claims_scope` (OAuth 2.0 `scope` claim, RFC 6749 §3.3). Tokens are
/// delimited by a single SP character (`0x20`), per the ABNF
/// `scope = scope-token *( SP scope-token )`. Tabs, newlines, and other
/// whitespace are NOT delimiters — a claim of `"read\twrite"` contains
/// the single token `"read\twrite"`, not `"read"` and `"write"`.
/// Substring matches do not count.
///
/// Returns `Ok` when `required` is empty regardless of `claims_scope`.
/// Otherwise returns `HttpChallenge(InsufficientScope)` so the transport
/// layer emits 403 + `WWW-Authenticate: Bearer … error="insufficient_scope"`.
fn enforce_required_scopes(
    required: &[String],
    claims_scope: Option<&str>,
) -> Result<(), MiddlewareError> {
    if required.is_empty() {
        return Ok(());
    }
    let scope_str = claims_scope.unwrap_or("");
    // Strict SP split per RFC 6749 §3.3 ABNF. Empty tokens (from leading/
    // trailing/repeated SP) end up in the set but cannot match any named
    // required scope, so they are harmless without an explicit filter.
    let present: std::collections::HashSet<&str> = scope_str.split(' ').collect();
    if required.iter().all(|s| present.contains(s.as_str())) {
        Ok(())
    } else {
        Err(MiddlewareError::HttpChallenge(
            AuthFailureKind::InsufficientScope,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_required_scopes_passes_even_when_claim_missing() {
        assert!(enforce_required_scopes(&[], None).is_ok());
        assert!(enforce_required_scopes(&[], Some("")).is_ok());
        assert!(enforce_required_scopes(&[], Some("read write")).is_ok());
    }

    #[test]
    fn required_subset_of_present_passes() {
        let required = vec!["a2a.read".to_string()];
        assert!(enforce_required_scopes(&required, Some("a2a.read a2a.write")).is_ok());
    }

    #[test]
    fn missing_scope_claim_fails_insufficient_scope() {
        let required = vec!["a2a.read".to_string()];
        let err = enforce_required_scopes(&required, None).unwrap_err();
        assert!(matches!(
            err,
            MiddlewareError::HttpChallenge(AuthFailureKind::InsufficientScope)
        ));
    }

    #[test]
    fn empty_or_whitespace_scope_claim_fails() {
        let required = vec!["a2a.read".to_string()];
        for claim in ["", "   ", "\t\n "] {
            let err = enforce_required_scopes(&required, Some(claim)).unwrap_err();
            assert!(
                matches!(
                    err,
                    MiddlewareError::HttpChallenge(AuthFailureKind::InsufficientScope)
                ),
                "claim {claim:?} should fail"
            );
        }
    }

    #[test]
    fn partial_overlap_fails() {
        let required = vec!["a2a.read".to_string(), "a2a.admin".to_string()];
        let err = enforce_required_scopes(&required, Some("a2a.read a2a.write")).unwrap_err();
        assert!(matches!(
            err,
            MiddlewareError::HttpChallenge(AuthFailureKind::InsufficientScope)
        ));
    }

    #[test]
    fn exact_token_match_only_no_substring() {
        // required="read" must NOT match claim "read_write" via substring.
        let required = vec!["read".to_string()];
        let err = enforce_required_scopes(&required, Some("read_write")).unwrap_err();
        assert!(matches!(
            err,
            MiddlewareError::HttpChallenge(AuthFailureKind::InsufficientScope)
        ));
        // Exact token in a multi-scope claim still works.
        assert!(enforce_required_scopes(&required, Some("read_write read")).is_ok());
    }

    #[test]
    fn tab_is_not_a_scope_delimiter() {
        // RFC 6749 §3.3 ABNF: scope = scope-token *( SP scope-token ).
        // A tab between "read" and "write" makes the whole string a single
        // token "read\twrite" — not two tokens.
        let required = vec!["read".to_string()];
        let err = enforce_required_scopes(&required, Some("read\twrite")).unwrap_err();
        assert!(matches!(
            err,
            MiddlewareError::HttpChallenge(AuthFailureKind::InsufficientScope)
        ));
        // Same for newline.
        let err = enforce_required_scopes(&required, Some("read\nwrite")).unwrap_err();
        assert!(matches!(
            err,
            MiddlewareError::HttpChallenge(AuthFailureKind::InsufficientScope)
        ));
    }
}
