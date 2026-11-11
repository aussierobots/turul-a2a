//! Authentication middleware for [`turul-a2a`](https://docs.rs/turul-a2a)
//! servers: [`ApiKeyMiddleware`] checks a key header against an
//! [`ApiKeyLookup`], and [`BearerMiddleware`] validates a JWT with
//! [`turul-jwt-validator`](https://docs.rs/turul-jwt-validator).
//!
//! Add this crate when an agent must authenticate callers. Both middlewares
//! set the caller's owner identity on the request, which the server uses to
//! scope tasks, and add their security scheme to the served agent card.
//! `BearerMiddleware` also takes a principal claim (default `sub`) and
//! required scopes. On AWS Lambda behind an authorizer, use
//! `LambdaAuthorizerMiddleware` from
//! [`turul-a2a-aws-lambda`](https://docs.rs/turul-a2a-aws-lambda) instead.
//!
//! # Example
//!
//! ```toml
//! [dependencies]
//! turul-a2a = "0.2"
//! turul-a2a-auth = "0.2"
//! ```
//!
//! ```rust,no_run
//! use std::collections::HashMap;
//! use std::sync::Arc;
//! use turul_a2a::A2aServer;
//! use turul_a2a_auth::{ApiKeyMiddleware, StaticApiKeyLookup};
//!
//! # fn main() -> Result<(), std::env::VarError> {
//! // Map each accepted key to the owner it authenticates.
//! let keys = HashMap::from([(std::env::var("AGENT_API_KEY")?, "alice".to_string())]);
//! let auth = ApiKeyMiddleware::new(Arc::new(StaticApiKeyLookup::new(keys)), "X-API-Key");
//!
//! // Add `.executor(..)` and `.build()` to finish the server.
//! let builder = A2aServer::builder().middleware(Arc::new(auth));
//! # let _ = builder;
//! # Ok(())
//! # }
//! ```
//!
//! # Where next
//!
//! - [`RedactedApiKeyLookup`] keeps key material out of `Debug` output
//! - [`turul_a2a::middleware`] for the `A2aMiddleware` trait behind both types
//! - [`auth-agent`](https://github.com/aussierobots/turul-a2a/tree/main/examples/auth-agent)
//!   example on GitHub

pub mod api_key;
pub mod bearer;

pub use api_key::{ApiKeyLookup, ApiKeyMiddleware, RedactedApiKeyLookup, StaticApiKeyLookup};
pub use bearer::BearerMiddleware;
