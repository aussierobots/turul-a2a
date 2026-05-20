//! Auth-oriented request middleware: authentication, authorisation, and
//! inbound credential forwarding.
//!
//! [`A2aMiddleware`] runs at the Tower layer before any handler or
//! JSON-RPC dispatch and is composed via [`MiddlewareStack`] (AND
//! semantics) or [`AnyOfMiddleware`] (OR with documented precedence).
//! Concrete implementations live in `turul-a2a-auth` (`BearerMiddleware`,
//! `ApiKeyMiddleware`) and `turul-a2a-aws-lambda`
//! (`LambdaAuthorizerMiddleware`).
//!
//! Non-auth interception — metrics, rate-limiting, tracing, structured
//! logging — should be a separate Tower layer composed outside this
//! module. This trait's surface (`before_request` only, failure shaped
//! for 401 / 403 + `WWW-Authenticate`) is deliberately auth-oriented.
//!
//! See ADR-007 (auth middleware architecture) and ADR-016 (stable auth
//! failure wire surface) for the design rationale.

pub mod bearer;
pub mod context;
pub mod error;
pub mod layer;
pub mod stack;
pub mod traits;
pub mod transport;

pub use context::{AuthIdentity, RequestContext};
pub use error::{AuthFailureKind, MiddlewareError};
pub use layer::AuthLayer;
pub use stack::{AnyOfMiddleware, MiddlewareStack};
pub use traits::{A2aMiddleware, SecurityContribution};

/// Paths that bypass auth and transport-compliance validation. Today this
/// is just the discovery endpoint — extended cards remain auth-required
/// per A2A spec. Both `AuthLayer` and `TransportComplianceLayer` consult
/// this list.
pub(crate) const BYPASS_PATHS: &[&str] = &["/.well-known/agent-card.json"];

pub(crate) fn is_bypass_path(path: &str) -> bool {
    BYPASS_PATHS.contains(&path)
}
