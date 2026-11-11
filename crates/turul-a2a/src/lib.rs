//! Server framework for the [A2A Protocol](https://a2a-protocol.org/) v1.0:
//! implement one [`AgentExecutor`](executor::AgentExecutor) and
//! [`A2aServer`] serves it over HTTP+JSON, JSON-RPC and Server-Sent Events,
//! with task state and streaming events kept in a pluggable store.
//!
//! This is the main crate for building an A2A agent. Task and message types
//! come from [`turul-a2a-types`](https://docs.rs/turul-a2a-types); callers
//! use [`turul-a2a-client`](https://docs.rs/turul-a2a-client).
//!
//! # Example
//!
//! ```toml
//! [dependencies]
//! turul-a2a = "0.2"
//! turul-a2a-proto = "0.2"
//! async-trait = "0.1"
//! tokio = { version = "1", features = ["full"] }
//! ```
//!
//! ```rust,no_run
//! use turul_a2a::prelude::*;
//!
//! struct Echo;
//!
//! #[async_trait::async_trait]
//! impl AgentExecutor for Echo {
//!     async fn execute(&self, task: &mut Task, message: &Message, _ctx: &ExecutionContext)
//!         -> Result<(), A2aError>
//!     {
//!         task.push_text_artifact("echo", "Echo", message.joined_text());
//!         task.complete();
//!         Ok(())
//!     }
//!
//!     fn agent_card(&self) -> turul_a2a_proto::AgentCard {
//!         AgentCardBuilder::new("Echo", "0.1.0")
//!             .description("Echoes messages back")
//!             .url("http://localhost:3000/jsonrpc", "JSONRPC", "1.0")
//!             .default_input_modes(vec!["text/plain"])
//!             .default_output_modes(vec!["text/plain"])
//!             .build()
//!             .expect("valid agent card")
//!     }
//! }
//!
//! # async fn run() -> Result<(), A2aError> {
//! A2aServer::builder().executor(Echo).bind(([0, 0, 0, 0], 3000)).build()?.run().await
//! # }
//! ```
//!
//! # Storage
//!
//! Task state and events are committed together through
//! [`storage::A2aAtomicStore`]; the [`streaming`] broker only wakes local
//! subscribers, so instances sharing one backend see each other's tasks and
//! events. Subscribing to an already-terminal task returns
//! `UnsupportedOperationError` (A2A v1.0 section 3.1.6); use `GetTask`.
//!
//! # Features
//!
//! | Feature | Enables |
//! |---|---|
//! | `in-memory` (default) | volatile in-process storage |
//! | `sqlite` | SQLite backend |
//! | `postgres` | PostgreSQL backend |
//! | `dynamodb` | DynamoDB backend with TTL |
//! | `grpc` | gRPC transport (`grpc-reflection` and `grpc-health` build on it) |
//! | `compat-v03` | a2a-sdk 0.3.x wire shim: method-name normalisation, root `POST` route, optional `A2A-Version` header |
//!
//! # Where next
//!
//! - [`executor::ExecutionContext`] for caller identity, cancellation and
//!   the durable event sink
//! - [`middleware`] to add authentication, with
//!   [`turul-a2a-auth`](https://docs.rs/turul-a2a-auth)
//! - [`turul-a2a-aws-lambda`](https://docs.rs/turul-a2a-aws-lambda) to run
//!   the same executor on AWS Lambda
//! - [Examples](https://github.com/aussierobots/turul-a2a/tree/main/examples)
//!   on GitHub

mod capability;
pub mod card_builder;
#[cfg(feature = "compat-v03")]
pub mod compat_v03;
pub mod durable_executor;
pub mod error;
pub mod event_sink;
pub mod executor;
#[cfg(feature = "grpc")]
pub mod grpc;
pub mod jsonrpc;
pub mod middleware;
pub mod prelude;
#[doc(hidden)]
pub mod profile_dispatch;

/// Stable adopter-facing constants for A2A profile extensions
/// implemented in this crate. The underlying dispatcher lives in
/// `profile_dispatch` (kept doc-hidden because its parse/validate
/// helpers are internal plumbing for the router / JSON-RPC / gRPC
/// transports). Re-export new profile URIs and header names here as
/// further profiles are added.
pub mod profiles {
    pub use crate::profile_dispatch::{
        A2A_EXTENSIONS_HEADER, SKILL_ID_METADATA_KEY, SKILL_INVOCATION_PROFILE_V1,
        SKILL_PARAMS_METADATA_KEY,
    };
}
pub mod push;
pub mod router;
pub mod server;
pub mod storage;
pub mod streaming;

pub use server::{A2aServer, BoundA2aServer};
