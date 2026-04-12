//! AgentExecutor trait — the user-facing contract for agent behavior.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use turul_a2a_types::{Message, Task};

use crate::error::A2aError;

/// Context available to the executor during message processing.
///
/// Provides auth identity, tenant/task metadata, and cooperative cancellation.
/// `#[non_exhaustive]` allows adding fields in future versions.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ExecutionContext {
    /// The authenticated owner (from middleware). Empty string for anonymous.
    pub owner: String,
    /// Tenant scope. `None` on default (un-tenanted) routes.
    pub tenant: Option<String>,
    /// The task being processed.
    pub task_id: String,
    /// Context ID for this conversation. `None` if not yet assigned.
    pub context_id: Option<String>,
    /// Auth claims from middleware (JWT claims, API key metadata, etc.).
    pub claims: Option<serde_json::Value>,
    /// Cooperative cancellation token. Check `cancellation.is_cancelled()`
    /// in long-running loops to respect client cancellation.
    pub cancellation: CancellationToken,
}

impl ExecutionContext {
    /// Create a context for anonymous/unauthenticated execution.
    pub fn anonymous(task_id: &str) -> Self {
        Self {
            owner: "anonymous".to_string(),
            tenant: None,
            task_id: task_id.to_string(),
            context_id: None,
            claims: None,
            cancellation: CancellationToken::new(),
        }
    }
}

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
    ///
    /// `ctx` provides auth identity, tenant, and cancellation support.
    /// Executors that don't need context can ignore it with `_ctx`.
    async fn execute(
        &self,
        task: &mut Task,
        message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError>;

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
