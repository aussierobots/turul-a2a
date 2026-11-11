//! Ergonomic Rust types for the A2A Protocol v1.0: [`Task`], [`Message`],
//! [`Part`] and [`Artifact`] wrappers over the generated messages in
//! [`turul_a2a_proto`], with a task state machine and builder helpers.
//!
//! Use it directly when you only need to build or inspect A2A payloads. The
//! server framework [`turul-a2a`](https://docs.rs/turul-a2a) and the
//! [`turul-a2a-client`](https://docs.rs/turul-a2a-client) crate both build on
//! it. The generated messages stay reachable through [`proto`] and the
//! `as_proto` / `into_proto` accessors.
//!
//! # Example
//!
//! ```toml
//! [dependencies]
//! turul-a2a-types = "0.2"
//! ```
//!
//! ```rust
//! use turul_a2a_types::{Message, Part, Role, Task, TaskState, TaskStatus};
//!
//! let message = Message::new("m-1", Role::User, vec![Part::text("hello")]);
//! assert_eq!(message.joined_text(), "hello");
//!
//! let mut task = Task::new("t-1", TaskStatus::new(TaskState::Working));
//! task.push_text_artifact("a-1", "Reply", "hi");
//! task.complete();
//! assert!(task.status().is_some());
//!
//! assert!(TaskState::Working.can_transition_to(TaskState::Completed));
//! assert!(!TaskState::Completed.can_transition_to(TaskState::Working));
//! ```
//!
//! # Where next
//!
//! - [`wire`] for JSON-RPC method names and other wire constants
//! - [`state_machine`] for the allowed [`TaskState`] transitions
//! - [`turul-a2a`](https://docs.rs/turul-a2a) to serve an agent that uses
//!   these types
//! - [Examples](https://github.com/aussierobots/turul-a2a/tree/main/examples)
//!   on GitHub

pub mod artifact;
pub mod error;
pub mod error_info;
pub mod message;
pub mod pbjson;
pub mod push_config;
pub mod state_machine;
pub mod task;
pub mod wire;

/// Re-export proto types for advanced users.
pub mod proto {
    pub use turul_a2a_proto::*;
}

pub use artifact::Artifact;
pub use error::A2aTypeError;
pub use error_info::ErrorInfo;
pub use message::{Message, Part, Role};
pub use push_config::{PushAuth, PushConfig, PushConfigBuilder, PushConfigPage};
pub use task::{Task, TaskState, TaskStatus};
