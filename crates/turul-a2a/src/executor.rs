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
    /// The caller identity from middleware; `"anonymous"` when no
    /// middleware authenticated the request.
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

/// What [`AgentExecutor::validate_message`] knows about a send before
/// the task is created or changed.
///
/// `#[non_exhaustive]` allows adding fields in future versions.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ValidationContext<'a> {
    /// The caller identity from middleware; `"anonymous"` when no
    /// middleware authenticated the request.
    pub owner: &'a str,
    /// Tenant scope. `None` on default (un-tenanted) routes.
    pub tenant: Option<&'a str>,
    /// The task the message targets: a new id for a new task, or the
    /// existing task's id for a follow-up.
    pub task_id: &'a str,
    /// Context ID for this conversation.
    pub context_id: &'a str,
    /// `true` when the message continues an existing INPUT_REQUIRED or
    /// AUTH_REQUIRED task.
    pub is_continuation: bool,
    /// Auth claims from middleware (JWT claims, API key metadata, etc.).
    pub claims: Option<&'a serde_json::Value>,
    /// As [`ExecutionContext::accepted_output_modes`].
    pub accepted_output_modes: &'a [String],
    /// As [`ExecutionContext::request_metadata`].
    pub request_metadata: Option<&'a serde_json::Value>,
    /// As [`ExecutionContext::activated_extensions`].
    pub activated_extensions: &'a [String],
}

impl<'a> ValidationContext<'a> {
    /// A context for a new task sent anonymously, for unit-testing a
    /// validator without a server.
    pub fn anonymous(task_id: &'a str) -> Self {
        Self {
            owner: "anonymous",
            tenant: None,
            task_id,
            context_id: "",
            is_continuation: false,
            claims: None,
            accepted_output_modes: &[],
            request_metadata: None,
            activated_extensions: &[],
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
    ///
    /// # Errors
    ///
    /// Returns an error when the executor could not process the message; the
    /// framework then commits the task as `FAILED` with the error message,
    /// unless the executor already committed a terminal through its sink.
    async fn execute(
        &self,
        task: &mut Task,
        message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError>;

    /// Check a message before it is accepted.
    ///
    /// Called on `SendMessage` and `SendStreamingMessage`, on every
    /// transport, after the framework's own request checks and before
    /// anything is written: no task is created or changed, nothing is
    /// enqueued and [`Self::execute`] is not called until this returns
    /// `Ok`. An `Err` is returned to the caller as the response; return
    /// [`A2aError::InvalidRequest`] for invalid input (HTTP 400, JSON-RPC
    /// `-32602`, gRPC `INVALID_ARGUMENT`), naming the field and why.
    ///
    /// Keep it cheap and free of side effects: it runs on the request
    /// path, and a request it passes may still be refused afterwards.
    /// The default accepts every message.
    ///
    /// # Errors
    ///
    /// Returns the error to refuse the message with; nothing has been written
    /// at that point.
    async fn validate_message(
        &self,
        _message: &Message,
        _ctx: &ValidationContext<'_>,
    ) -> Result<(), A2aError> {
        Ok(())
    }

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
