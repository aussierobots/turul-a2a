//! A2aMiddleware trait and SecurityContribution.

use async_trait::async_trait;

use super::context::RequestContext;
use super::error::MiddlewareError;

/// Trait that auth middleware implements.
///
/// Runs at the Tower layer before any handler or JSON-RPC dispatch.
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
