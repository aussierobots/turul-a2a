//! A2A Protocol v1.0 server framework.
//!
//! `turul-a2a` provides the server side of the [A2A
//! Protocol](https://github.com/a2aproject/A2A): a typed
//! [`executor::AgentExecutor`] trait, HTTP + JSON-RPC transports, Server-Sent
//! Events streaming, and a storage abstraction with four backends
//! (in-memory, SQLite, PostgreSQL, DynamoDB) — all proto-normative per
//! `proto/a2a.proto`.
//!
//! # Quick start
//!
//! See the `examples/echo-agent` crate for a runnable end-to-end example.
//! A minimal executor implements [`executor::AgentExecutor`] and is passed to
//! [`A2aServer::builder()`]:
//!
//! ```text
//! let server = A2aServer::builder()
//!     .executor(MyExecutor)
//!     .bind(([0, 0, 0, 0], 3000))
//!     .build()?;
//! server.run().await?;
//! ```
//!
//! # Feature flags
//!
//! - `in-memory` (default) — volatile in-process storage
//! - `sqlite` — SQLx SQLite backend (atomic task+event writes)
//! - `postgres` — SQLx PostgreSQL backend
//! - `dynamodb` — AWS DynamoDB backend with TTL
//! - `compat-v03` — opt-in compatibility shim for a2a-sdk 0.3.x clients
//!   (method name normalization, root POST route, optional `A2A-Version`
//!   header). Canonical builds are v1.0 strict.
//!
//! # Durable event coordination
//!
//! Task state and streaming events are written atomically via
//! [`storage::A2aAtomicStore`]. The in-process event broker is a local
//! wake-up signal; the storage backend is the source of truth. Subscribers
//! attached while a task is non-terminal receive replay of prior events and
//! then live delivery, supporting `Last-Event-ID` reconnection across
//! instances when the same backend is used (e.g., shared DynamoDB or
//! PostgreSQL). Subscribing to an already-terminal task returns
//! `UnsupportedOperationError` per A2A v1.0 §3.1.6 — use [`storage`] or
//! `GetTask` to retrieve the final state.
//!
//! # Multi-instance streaming
//!
//! The in-process [`streaming`] broker fans out to clients on the same
//! process only. Cross-instance streaming relies on the shared durable event
//! store for replay. For horizontally scaled deployments, configure all
//! instances against the same backend.

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
pub mod push;
pub mod router;
pub mod server;
pub mod storage;
pub mod streaming;

pub use server::A2aServer;
