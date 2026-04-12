//! Server/agent-authoring prelude.
//!
//! ```rust
//! use turul_a2a::prelude::*;
//! ```

pub use crate::card_builder::AgentCardBuilder;
pub use crate::error::A2aError;
pub use crate::executor::{AgentExecutor, ExecutionContext};
pub use crate::server::A2aServer;

// Core types from turul-a2a-types (re-exported for agent authors)
pub use turul_a2a_types::{Artifact, Message, Part, Task, TaskState, TaskStatus};
