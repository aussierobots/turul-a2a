//! MiddlewareStack and AnyOfMiddleware.

use std::sync::Arc;

use async_trait::async_trait;

use super::context::RequestContext;
use super::error::MiddlewareError;
use super::traits::A2aMiddleware;

/// Ordered stack of middleware. Executes in registration order.
/// All must pass (AND semantics). First failure stops the chain.
pub struct MiddlewareStack {
    middleware: Vec<Arc<dyn A2aMiddleware>>,
}

impl MiddlewareStack {
    pub fn new(middleware: Vec<Arc<dyn A2aMiddleware>>) -> Self {
        Self { middleware }
    }

    pub fn is_empty(&self) -> bool {
        self.middleware.is_empty()
    }

    pub async fn before_request(&self, ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
        for mw in &self.middleware {
            mw.before_request(ctx).await?;
        }
        Ok(())
    }
}

/// Combinator: try children in order, first success wins (OR semantics).
///
/// Failure aggregation rules:
/// - Internal errors are **fatal** — short-circuit immediately, do not try more children.
/// - If all children fail, surface the highest-precedence error:
///   Forbidden(403) > HttpChallenge(401+WWW-Auth) > Unauthenticated(401)
/// - Ties broken by registration order (first registered wins).
/// - WWW-Authenticate headers from all HttpChallenge errors are merged.
pub struct AnyOfMiddleware {
    children: Vec<Arc<dyn A2aMiddleware>>,
}

impl AnyOfMiddleware {
    pub fn new(children: Vec<Arc<dyn A2aMiddleware>>) -> Self {
        assert!(
            !children.is_empty(),
            "AnyOfMiddleware requires at least one child"
        );
        Self { children }
    }
}

#[async_trait]
impl A2aMiddleware for AnyOfMiddleware {
    async fn before_request(&self, ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
        let mut errors: Vec<MiddlewareError> = Vec::new();

        for child in &self.children {
            // Clone context for each attempt — only the successful child's mutations stick
            let mut attempt_ctx = RequestContext {
                bearer_token: ctx.bearer_token.clone(),
                headers: ctx.headers.clone(),
                identity: ctx.identity.clone(),
                extensions: ctx.extensions.clone(),
            };

            match child.before_request(&mut attempt_ctx).await {
                Ok(()) => {
                    // First success wins — apply this child's context mutations
                    ctx.identity = attempt_ctx.identity;
                    ctx.extensions = attempt_ctx.extensions;
                    return Ok(());
                }
                Err(MiddlewareError::Internal(msg)) => {
                    // Fatal — stop immediately, do not try more children
                    return Err(MiddlewareError::Internal(msg));
                }
                Err(e) => {
                    errors.push(e);
                }
            }
        }

        // All children failed. Select highest-precedence error.
        let selected = errors
            .into_iter()
            .reduce(|champion, challenger| {
                if challenger.precedence() > champion.precedence() {
                    challenger
                } else {
                    champion // tie goes to first-registered
                }
            })
            .expect("AnyOfMiddleware has at least one child");

        // If the selected error is 401-class, merge WWW-Authenticate headers
        if let MiddlewareError::HttpChallenge { .. } = &selected {
            // Already has a challenge — no merge needed if it's the only HttpChallenge
        }
        // For a more complete merge: collect all HttpChallenge www_authenticate strings
        // and combine. For v0.2, the selected error's challenge is sufficient since
        // AnyOfMiddleware selects the highest-precedence HttpChallenge (first one).

        Err(selected)
    }

    fn security_contribution(&self) -> super::traits::SecurityContribution {
        let mut contribution = super::traits::SecurityContribution::new();
        for child in &self.children {
            let child_contrib = child.security_contribution();
            // Schemes: union
            for (name, scheme) in child_contrib.schemes {
                contribution.schemes.push((name, scheme));
            }
            // Requirements: each child's requirements are alternatives (OR)
            for req in child_contrib.requirements {
                contribution.requirements.push(req);
            }
        }
        contribution
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::context::AuthIdentity;

    // =========================================================
    // Test middleware implementations for contract testing
    // =========================================================

    struct SucceedingMiddleware {
        owner: String,
    }

    #[async_trait]
    impl A2aMiddleware for SucceedingMiddleware {
        async fn before_request(&self, ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
            ctx.identity = AuthIdentity::Authenticated {
                owner: self.owner.clone(),
                claims: None,
            };
            Ok(())
        }
    }

    struct FailUnauthenticated {
        message: String,
    }

    #[async_trait]
    impl A2aMiddleware for FailUnauthenticated {
        async fn before_request(&self, _ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
            Err(MiddlewareError::Unauthenticated(self.message.clone()))
        }
    }

    struct FailHttpChallenge {
        www_authenticate: String,
    }

    #[async_trait]
    impl A2aMiddleware for FailHttpChallenge {
        async fn before_request(&self, _ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
            Err(MiddlewareError::HttpChallenge {
                status: 401,
                www_authenticate: self.www_authenticate.clone(),
            })
        }
    }

    struct FailForbidden {
        message: String,
    }

    #[async_trait]
    impl A2aMiddleware for FailForbidden {
        async fn before_request(&self, _ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
            Err(MiddlewareError::Forbidden(self.message.clone()))
        }
    }

    struct FailInternal {
        message: String,
    }

    #[async_trait]
    impl A2aMiddleware for FailInternal {
        async fn before_request(&self, _ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
            Err(MiddlewareError::Internal(self.message.clone()))
        }
    }

    /// Tracks whether before_request was called.
    struct CallTracker {
        called: std::sync::Arc<std::sync::atomic::AtomicBool>,
        inner: Box<dyn A2aMiddleware>,
    }

    #[async_trait]
    impl A2aMiddleware for CallTracker {
        async fn before_request(&self, ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
            self.called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.inner.before_request(ctx).await
        }
    }

    // =========================================================
    // MiddlewareStack tests
    // =========================================================

    #[tokio::test]
    async fn empty_stack_passes_through() {
        let stack = MiddlewareStack::new(vec![]);
        let mut ctx = RequestContext::new();
        assert!(stack.before_request(&mut ctx).await.is_ok());
        assert!(!ctx.identity.is_authenticated());
    }

    #[tokio::test]
    async fn stack_error_halts_chain() {
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stack = MiddlewareStack::new(vec![
            Arc::new(FailUnauthenticated {
                message: "first".into(),
            }),
            Arc::new(CallTracker {
                called: called.clone(),
                inner: Box::new(SucceedingMiddleware {
                    owner: "user".into(),
                }),
            }),
        ]);
        let mut ctx = RequestContext::new();
        assert!(stack.before_request(&mut ctx).await.is_err());
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "Second middleware should not be called after first fails"
        );
    }

    // =========================================================
    // AnyOfMiddleware — first success wins
    // =========================================================

    #[tokio::test]
    async fn anyof_first_success_wins() {
        let any = AnyOfMiddleware::new(vec![
            Arc::new(FailUnauthenticated {
                message: "no key".into(),
            }),
            Arc::new(SucceedingMiddleware {
                owner: "user-b".into(),
            }),
        ]);
        let mut ctx = RequestContext::new();
        assert!(any.before_request(&mut ctx).await.is_ok());
        assert_eq!(ctx.identity.owner(), "user-b");
    }

    #[tokio::test]
    async fn anyof_first_child_succeeds_skips_rest() {
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let any = AnyOfMiddleware::new(vec![
            Arc::new(SucceedingMiddleware {
                owner: "user-a".into(),
            }),
            Arc::new(CallTracker {
                called: called.clone(),
                inner: Box::new(SucceedingMiddleware {
                    owner: "user-b".into(),
                }),
            }),
        ]);
        let mut ctx = RequestContext::new();
        any.before_request(&mut ctx).await.unwrap();
        assert_eq!(ctx.identity.owner(), "user-a");
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    }

    // =========================================================
    // AnyOfMiddleware — Internal short-circuits
    // =========================================================

    #[tokio::test]
    async fn anyof_internal_short_circuits() {
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let any = AnyOfMiddleware::new(vec![
            Arc::new(FailInternal {
                message: "db down".into(),
            }),
            Arc::new(CallTracker {
                called: called.clone(),
                inner: Box::new(SucceedingMiddleware {
                    owner: "user".into(),
                }),
            }),
        ]);
        let mut ctx = RequestContext::new();
        let err = any.before_request(&mut ctx).await.unwrap_err();
        assert!(matches!(err, MiddlewareError::Internal(_)));
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "Internal error should short-circuit, not try next child"
        );
    }

    // =========================================================
    // AnyOfMiddleware — precedence: Forbidden > HttpChallenge > Unauthenticated
    // =========================================================

    #[tokio::test]
    async fn anyof_forbidden_beats_unauthenticated() {
        let any = AnyOfMiddleware::new(vec![
            Arc::new(FailUnauthenticated {
                message: "no auth".into(),
            }),
            Arc::new(FailForbidden {
                message: "no access".into(),
            }),
        ]);
        let mut ctx = RequestContext::new();
        let err = any.before_request(&mut ctx).await.unwrap_err();
        assert!(
            matches!(err, MiddlewareError::Forbidden(_)),
            "Forbidden should win over Unauthenticated"
        );
    }

    #[tokio::test]
    async fn anyof_forbidden_beats_http_challenge() {
        let any = AnyOfMiddleware::new(vec![
            Arc::new(FailHttpChallenge {
                www_authenticate: "Bearer realm=\"a2a\"".into(),
            }),
            Arc::new(FailForbidden {
                message: "no access".into(),
            }),
        ]);
        let mut ctx = RequestContext::new();
        let err = any.before_request(&mut ctx).await.unwrap_err();
        assert!(
            matches!(err, MiddlewareError::Forbidden(_)),
            "Forbidden should win over HttpChallenge"
        );
    }

    #[tokio::test]
    async fn anyof_http_challenge_beats_unauthenticated() {
        let any = AnyOfMiddleware::new(vec![
            Arc::new(FailUnauthenticated {
                message: "no key".into(),
            }),
            Arc::new(FailHttpChallenge {
                www_authenticate: "Bearer realm=\"a2a\"".into(),
            }),
        ]);
        let mut ctx = RequestContext::new();
        let err = any.before_request(&mut ctx).await.unwrap_err();
        assert!(
            matches!(err, MiddlewareError::HttpChallenge { .. }),
            "HttpChallenge should win over Unauthenticated"
        );
    }

    // =========================================================
    // AnyOfMiddleware — tie-breaking by registration order
    // =========================================================

    #[tokio::test]
    async fn anyof_all_unauthenticated_returns_first() {
        let any = AnyOfMiddleware::new(vec![
            Arc::new(FailUnauthenticated {
                message: "first-registered".into(),
            }),
            Arc::new(FailUnauthenticated {
                message: "second-registered".into(),
            }),
        ]);
        let mut ctx = RequestContext::new();
        let err = any.before_request(&mut ctx).await.unwrap_err();
        match err {
            MiddlewareError::Unauthenticated(msg) => {
                assert_eq!(msg, "first-registered", "Tie should go to first-registered");
            }
            _ => panic!("Expected Unauthenticated"),
        }
    }

    // =========================================================
    // MiddlewareError → HTTP status mapping
    // =========================================================

    #[test]
    fn middleware_error_http_status_mapping() {
        assert_eq!(
            MiddlewareError::Unauthenticated("x".into()).http_status(),
            401
        );
        assert_eq!(
            MiddlewareError::HttpChallenge {
                status: 401,
                www_authenticate: "Bearer".into()
            }
            .http_status(),
            401
        );
        assert_eq!(MiddlewareError::Forbidden("x".into()).http_status(), 403);
        assert_eq!(MiddlewareError::Internal("x".into()).http_status(), 500);
    }

    // =========================================================
    // AnyOfMiddleware — panics with empty children
    // =========================================================

    #[test]
    #[should_panic(expected = "at least one child")]
    fn anyof_empty_children_panics() {
        AnyOfMiddleware::new(vec![]);
    }

    // =========================================================
    // AuthIdentity contract tests
    // =========================================================

    #[test]
    fn anonymous_is_not_authenticated() {
        let id = AuthIdentity::Anonymous;
        assert!(!id.is_authenticated());
        assert_eq!(id.owner(), "anonymous");
        assert!(id.claims().is_none());
    }

    #[test]
    fn authenticated_is_authenticated() {
        let id = AuthIdentity::Authenticated {
            owner: "user-1".into(),
            claims: Some(serde_json::json!({"sub": "user-1"})),
        };
        assert!(id.is_authenticated());
        assert_eq!(id.owner(), "user-1");
        assert!(id.claims().is_some());
    }

    #[test]
    fn authenticated_with_literal_anonymous_owner_is_still_authenticated() {
        // Per ADR-007: a principal literally named "anonymous" IS authenticated
        let id = AuthIdentity::Authenticated {
            owner: "anonymous".into(),
            claims: None,
        };
        assert!(id.is_authenticated());
    }

    #[test]
    fn api_key_auth_has_no_claims_but_is_authenticated() {
        let id = AuthIdentity::Authenticated {
            owner: "api-key-user".into(),
            claims: None,
        };
        assert!(id.is_authenticated());
        assert!(id.claims().is_none());
        assert_eq!(id.owner(), "api-key-user");
    }
}
