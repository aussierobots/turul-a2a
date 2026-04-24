//! Ergonomic Rust types for the A2A Protocol v1.0.
//!
//! This crate wraps the prost-generated proto types from [`turul_a2a_proto`]
//! with idiomatic Rust APIs, state machine enforcement on [`TaskState`], and
//! builder helpers on [`Task`], [`Message`], and [`Artifact`].
//!
//! All public types are `#[non_exhaustive]` — additive changes will not break
//! downstream code.
//!
//! # Primary entry points
//!
//! - [`Task`], [`TaskStatus`], [`TaskState`] — the core task object and its lifecycle
//! - [`Message`], [`Part`], [`Role`] — conversational payloads
//! - [`Artifact`] — outputs produced during task execution
//! - [`wire`] — JSON-RPC method constants and SSE event shapes
//!
//! Raw proto access is available via [`proto`] for advanced use.

pub mod artifact;
pub mod error;
pub mod message;
pub mod pbjson;
pub mod state_machine;
pub mod task;
pub mod wire;

/// Re-export proto types for advanced users.
pub mod proto {
    pub use turul_a2a_proto::*;
}

pub use artifact::Artifact;
pub use error::A2aTypeError;
pub use message::{Message, Part, Role};
pub use task::{Task, TaskState, TaskStatus};
