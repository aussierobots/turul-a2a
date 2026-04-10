pub mod artifact;
pub mod error;
pub mod message;
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
