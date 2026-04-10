//! AgentExecutor trait — the user-facing contract for agent behavior.

use async_trait::async_trait;
use turul_a2a_types::{Message, Task};

use crate::error::A2aError;

/// Trait that users implement to define agent behavior.
///
/// The server calls `execute` when a message arrives. The executor
/// updates the task's status, appends artifacts, etc.
#[async_trait]
pub trait AgentExecutor: Send + Sync {
    /// Process a message against a task.
    ///
    /// The task is mutable — update its status, append messages/artifacts.
    /// Return `Ok(())` on success. The server persists the updated task.
    async fn execute(&self, task: &mut Task, message: &Message) -> Result<(), A2aError>;

    /// Return the public agent card (unauthenticated discovery).
    fn agent_card(&self) -> turul_a2a_proto::AgentCard;

    /// Return the extended agent card for authenticated callers.
    /// Return `None` if extended card is not supported.
    fn extended_agent_card(
        &self,
        _claims: Option<&serde_json::Value>,
    ) -> Option<turul_a2a_proto::AgentCard> {
        None
    }
}
