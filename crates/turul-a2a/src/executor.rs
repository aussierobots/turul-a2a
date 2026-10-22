//! AgentExecutor trait — the user-facing contract for agent behavior.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use turul_a2a_types::{Message, Task};

use crate::error::A2aError;
use crate::event_sink::EventSink;

/// Context available to the executor during message processing.
///
/// Provides auth identity, tenant/task metadata, cooperative cancellation,
/// and an [`EventSink`] for durable event emission.
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
    /// Durable event sink. Executors that want to drive task
    /// lifecycle themselves — progress artifacts, interrupted states,
    /// explicit terminal — call the methods on this sink.
    ///
    /// Production send paths install a live sink that writes through the
    /// atomic store and fires the framework's blocking-send awaiter on
    /// terminal or interrupted commit.
    /// [`ExecutionContext::anonymous`] returns a detached sink — emits
    /// return `A2aError::Internal`. Executor unit tests that don't stand
    /// up a server can use it as-is; they simply cannot drive durable
    /// lifecycle events without real storage.
    pub events: EventSink,
    /// `SendMessageConfiguration.accepted_output_modes` from the request
    /// that started this execution: the media types the client is
    /// prepared to accept for response parts (a2a.proto
    /// `SendMessageConfiguration`, "Agents SHOULD use this to tailor
    /// their output"). Empty when the client sent none.
    pub accepted_output_modes: Vec<String>,
    /// `SendMessageRequest.metadata` from the request that started this
    /// execution, as a JSON object. Distinct from `Message.metadata`,
    /// which the executor reads from the message itself. `None` when the
    /// client sent none.
    pub request_metadata: Option<serde_json::Value>,
    /// Extension URIs negotiated for the request that started this
    /// execution: those the client activated (the `A2A-Extensions`
    /// header / gRPC metadata) that this agent advertises in
    /// `AgentCard.capabilities.extensions`, sorted. Empty when none were
    /// activated. An executor whose behaviour depends on an extension
    /// applies it only when the URI is listed here.
    pub activated_extensions: Vec<String>,
}

impl ExecutionContext {
    /// Create a context for anonymous/unauthenticated execution.
    ///
    /// The [`events`] sink is [`EventSink::detached`] — emits return
    /// `A2aError::Internal`. Use this for executor unit tests that don't
    /// need durable event persistence. Production code constructs
    /// `ExecutionContext` with a live sink wired to the framework's
    /// atomic store.
    ///
    /// [`events`]: Self#structfield.events
    pub fn anonymous(task_id: &str) -> Self {
        Self {
            owner: "anonymous".to_string(),
            tenant: None,
            task_id: task_id.to_string(),
            context_id: None,
            claims: None,
            cancellation: CancellationToken::new(),
            events: EventSink::detached(),
            accepted_output_modes: Vec::new(),
            request_metadata: None,
            activated_extensions: Vec::new(),
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
    ///
    /// Served only when [`Self::agent_card`] declares
    /// `capabilities.extendedAgentCard = true`; otherwise
    /// `GetExtendedAgentCard` is an `UnsupportedOperationError`. Declared
    /// but `None` here is `ExtendedAgentCardNotConfiguredError`.
    fn extended_agent_card(
        &self,
        _claims: Option<&serde_json::Value>,
    ) -> Option<turul_a2a_proto::AgentCard> {
        None
    }
}
