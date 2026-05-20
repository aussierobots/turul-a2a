//! A2aMiddleware trait and SecurityContribution.

use async_trait::async_trait;

use super::context::RequestContext;
use super::error::MiddlewareError;

/// Trait implemented by request-scoped middleware that participates in
/// the framework's auth / authorisation / credential-forwarding plane.
///
/// Runs at the Tower layer before any handler or JSON-RPC dispatch.
/// Typical implementations:
/// - Authentication (e.g. `BearerMiddleware`, `ApiKeyMiddleware` in `turul-a2a-auth`)
/// - Trusted-header authorisation (e.g. `LambdaAuthorizerMiddleware` in `turul-a2a-aws-lambda`)
/// - Inbound credential forwarding (e.g. stashing an upstream API key or Bearer token
///   onto `ctx.identity.claims` for downstream MCP / A2A calls to consume)
///
/// Non-auth interception (metrics, rate-limiting, tracing) should be a
/// separate Tower layer composed outside this trait — implementations
/// here only get a `before_request` callback and the failure surface is
/// shaped for auth errors (401/403 + `WWW-Authenticate`).
#[async_trait]
pub trait A2aMiddleware: Send + Sync {
    /// Validate the request and populate identity on the context.
    async fn before_request(&self, ctx: &mut RequestContext) -> Result<(), MiddlewareError>;

    /// Security contribution for AgentCard auto-population.
    fn security_contribution(&self) -> SecurityContribution {
        SecurityContribution::default()
    }
}

/// What a middleware contributes to AgentCard security metadata.
///
/// Contains both scheme definitions and requirement groups.
/// Multiple `SecurityRequirement` entries = OR (alternatives).
/// Multiple schemes in one `SecurityRequirement` = AND (all required).
#[derive(Debug, Clone, Default)]
pub struct SecurityContribution {
    pub schemes: Vec<(String, turul_a2a_proto::SecurityScheme)>,
    pub requirements: Vec<turul_a2a_proto::SecurityRequirement>,
}

impl SecurityContribution {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a scheme with required scopes and a corresponding requirement.
    pub fn with_scheme(
        mut self,
        name: impl Into<String>,
        scheme: turul_a2a_proto::SecurityScheme,
        scopes: Vec<String>,
    ) -> Self {
        let name = name.into();
        self.schemes.push((name.clone(), scheme));
        let mut req_schemes = std::collections::HashMap::new();
        req_schemes.insert(name, turul_a2a_proto::StringList { list: scopes });
        self.requirements
            .push(turul_a2a_proto::SecurityRequirement {
                schemes: req_schemes,
            });
        self
    }

    pub fn is_empty(&self) -> bool {
        self.schemes.is_empty() && self.requirements.is_empty()
    }
}
