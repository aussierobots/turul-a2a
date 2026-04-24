//! ADR-018 durable executor continuation for AWS Lambda.
//!
//! Feature-gated behind `sqs`:
//!
//! - [`SqsDurableExecutorQueue`] — first-party
//!   [`turul_a2a::durable_executor::DurableExecutorQueue`] impl over
//!   AWS SQS. Reports `max_payload_bytes() = 256 * 1024` (SQS standard
//!   queue limit). Serialises [`turul_a2a::durable_executor::QueuedExecutorJob`]
//!   as JSON into `SendMessage.body`.
//! - [`LambdaA2aHandler::handle_sqs`] — SQS event-source handler.
//!   Per-record: terminal-no-op check, cancel-marker direct-CANCELED
//!   commit, then [`turul_a2a::server::spawn::run_executor_for_existing_task`].
//!   Returns `SqsBatchResponse` so one poison record does not block
//!   the batch.
//! - [`LambdaEvent`] + [`LambdaEvent::classify`] — event-shape
//!   discriminator adopters use in `main.rs` to route HTTP vs SQS
//!   events to the right handler on a single Lambda function.

#![cfg(feature = "sqs")]

use std::sync::Arc;

use async_trait::async_trait;
use aws_lambda_events::event::sqs::{BatchItemFailure, SqsBatchResponse, SqsEvent, SqsMessage};
use aws_sdk_sqs::Client as SqsClient;
use turul_a2a::durable_executor::{DurableExecutorQueue, QueueError, QueuedExecutorJob};
use turul_a2a::router::AppState;
use turul_a2a::server::spawn::{SpawnDeps, SpawnScope, run_queued_executor_job};
use turul_a2a_types::{Message, Part, Role, TaskState, TaskStatus};

/// SQS-backed [`DurableExecutorQueue`] implementation.
///
/// Reports `max_payload_bytes = 256 * 1024` (SQS standard queue
/// limit). The default `check_payload_size` from the trait is used
/// (JSON-encode + length compare). `enqueue` sends the same JSON to
/// `SendMessage.body`.
#[derive(Clone)]
pub struct SqsDurableExecutorQueue {
    client: Arc<SqsClient>,
    queue_url: String,
}

impl SqsDurableExecutorQueue {
    pub fn new(queue_url: impl Into<String>, client: Arc<SqsClient>) -> Self {
        Self {
            client,
            queue_url: queue_url.into(),
        }
    }
}

#[async_trait]
impl DurableExecutorQueue for SqsDurableExecutorQueue {
    fn max_payload_bytes(&self) -> usize {
        256 * 1024
    }

    async fn enqueue(&self, job: QueuedExecutorJob) -> Result<(), QueueError> {
        let encoded = serde_json::to_string(&job)?;
        let max = self.max_payload_bytes();
        if encoded.len() > max {
            return Err(QueueError::PayloadTooLarge {
                actual: encoded.len(),
                max,
            });
        }
        self.client
            .send_message()
            .queue_url(&self.queue_url)
            .message_body(encoded)
            .send()
            .await
            .map_err(|e| QueueError::Transport(format!("{e}")))?;
        Ok(())
    }

    fn kind(&self) -> &'static str {
        "sqs"
    }
}

/// Event-shape discriminator for a dual-mode Lambda function that
/// handles both HTTP and SQS events on a single function (ADR-018
/// §"Event dispatch").
///
/// Adopters' `main.rs` receives a raw `serde_json::Value` and uses
/// [`classify_event`] to route:
///
/// ```ignore
/// lambda_runtime::run(lambda_runtime::service_fn(
///     move |event: lambda_runtime::LambdaEvent<serde_json::Value>| {
///         let handler = handler.clone();
///         async move {
///             let (value, _ctx) = event.into_parts();
///             match turul_a2a_aws_lambda::classify_event(&value) {
///                 turul_a2a_aws_lambda::LambdaEvent::Sqs => {
///                     let sqs: aws_lambda_events::event::sqs::SqsEvent =
///                         serde_json::from_value(value)?;
///                     let resp = handler.handle_sqs(sqs).await;
///                     Ok(serde_json::to_value(resp)?)
///                 }
///                 turul_a2a_aws_lambda::LambdaEvent::Http => {
///                     // Hand off to lambda_http for the HTTP envelope.
///                     // Typical pattern: use a second service_fn for HTTP
///                     // wrapped in lambda_http::run at a higher level.
///                     Err("http handled by lambda_http layer".into())
///                 }
///                 turul_a2a_aws_lambda::LambdaEvent::Unknown => {
///                     Err("unknown Lambda event shape".into())
///                 }
///             }
///         }
///     }
/// ))
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LambdaEvent {
    /// HTTP event (API Gateway REST, HTTP API, Function URL, ALB).
    Http,
    /// SQS trigger event from the durable executor queue.
    Sqs,
    /// Unrecognised shape (DynamoDB stream, EventBridge scheduler,
    /// custom invocation payloads, etc.).
    Unknown,
}

/// Classify a raw Lambda event JSON shape.
///
/// Heuristic:
/// - SQS event JSON has a top-level `"Records"` array whose entries
///   contain `"eventSource": "aws:sqs"`.
/// - HTTP event JSON has one of `httpMethod`,
///   `requestContext.http.method`, or `routeKey`.
/// - Anything else is `Unknown`.
pub fn classify_event(event: &serde_json::Value) -> LambdaEvent {
    if is_sqs_event_shape(event) {
        return LambdaEvent::Sqs;
    }
    if is_http_event_shape(event) {
        return LambdaEvent::Http;
    }
    LambdaEvent::Unknown
}

fn is_sqs_event_shape(event: &serde_json::Value) -> bool {
    event
        .get("Records")
        .and_then(|r| r.as_array())
        .and_then(|arr| arr.first())
        .and_then(|rec| rec.get("eventSource"))
        .and_then(|s| s.as_str())
        .map(|s| s == "aws:sqs")
        .unwrap_or(false)
}

fn is_http_event_shape(event: &serde_json::Value) -> bool {
    if event.get("httpMethod").is_some() {
        return true;
    }
    if let Some(req_ctx) = event.get("requestContext") {
        if req_ctx.pointer("/http/method").is_some() {
            return true;
        }
    }
    if event.get("routeKey").is_some() {
        return true;
    }
    false
}

/// Drive one SQS batch through the durable-continuation path.
///
/// Public so `LambdaA2aHandler::handle_sqs` can delegate; adopters
/// should not call this directly — use the handler wrapper.
///
/// Per ADR-018 §SQS invocation:
///
/// 1. Deserialize body; unknown envelope version → batch-item failure.
/// 2. Load task; not found → batch-item failure.
/// 3. Already terminal → success (idempotent no-op).
/// 4. Cancel marker set → commit CANCELED directly via atomic store
///    (executor never invoked); `TerminalStateAlreadySet` → success.
/// 5. Run executor via `run_executor_for_existing_task`.
/// 6. Executor error → batch-item failure.
pub async fn drive_sqs_batch(state: &AppState, event: SqsEvent) -> SqsBatchResponse {
    let mut failures = Vec::new();
    for record in event.records {
        if let Err(id) = drive_sqs_record(state, record).await {
            let mut f = BatchItemFailure::default();
            f.item_identifier = id;
            failures.push(f);
        }
    }
    let mut resp = SqsBatchResponse::default();
    resp.batch_item_failures = failures;
    resp
}

async fn drive_sqs_record(state: &AppState, record: SqsMessage) -> Result<(), String> {
    let identifier = record
        .message_id
        .clone()
        .unwrap_or_else(|| "<no-message-id>".to_string());
    let body = match record.body.as_deref() {
        Some(b) => b,
        None => {
            tracing::error!(item = %identifier, "SQS record has no body");
            return Err(identifier);
        }
    };
    let job: QueuedExecutorJob = match serde_json::from_str(body) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(item = %identifier, error = %e, "failed to deserialise QueuedExecutorJob");
            return Err(identifier);
        }
    };
    if job.version != QueuedExecutorJob::VERSION {
        tracing::error!(
            item = %identifier,
            version = job.version,
            expected = QueuedExecutorJob::VERSION,
            "unknown envelope version"
        );
        return Err(identifier);
    }

    // Load task — 404 is DLQ-grade.
    let task = match state
        .task_storage
        .get_task(&job.tenant, &job.task_id, &job.owner, None)
        .await
    {
        Ok(Some(t)) => t,
        Ok(None) => {
            tracing::error!(
                item = %identifier,
                tenant = %job.tenant,
                task_id = %job.task_id,
                "task not found on SQS dequeue"
            );
            return Err(identifier);
        }
        Err(e) => {
            tracing::error!(item = %identifier, error = %e, "get_task failed on SQS dequeue");
            return Err(identifier);
        }
    };

    // Terminal already → idempotent no-op success.
    if let Some(status) = task.status() {
        if let Ok(s) = status.state() {
            use turul_a2a_types::state_machine::is_terminal;
            if is_terminal(s) {
                tracing::debug!(
                    item = %identifier,
                    state = ?s,
                    "task already terminal; skipping executor invocation"
                );
                return Ok(());
            }
        }
    }

    // Cancel marker → commit CANCELED directly, never invoke executor.
    let cancel_requested = state
        .cancellation_supervisor
        .supervisor_get_cancel_requested(&job.tenant, &job.task_id)
        .await
        .unwrap_or(false);
    if cancel_requested {
        let reason = Message::new(
            uuid::Uuid::now_v7().to_string(),
            Role::Agent,
            vec![Part::text("canceled before durable executor dispatch")],
        );
        let canceled_status = TaskStatus::new(TaskState::Canceled).with_message(reason);
        let canceled_event = turul_a2a::streaming::StreamEvent::StatusUpdate {
            status_update: turul_a2a::streaming::StatusUpdatePayload {
                task_id: job.task_id.clone(),
                context_id: job.context_id.clone(),
                status: serde_json::to_value(&canceled_status).unwrap_or_default(),
            },
        };
        match state
            .atomic_store
            .update_task_status_with_events(
                &job.tenant,
                &job.task_id,
                &job.owner,
                canceled_status,
                vec![canceled_event],
            )
            .await
        {
            Ok(_) => {
                tracing::info!(
                    item = %identifier,
                    tenant = %job.tenant,
                    task_id = %job.task_id,
                    "ADR-018: canceled before dispatch — CANCELED committed, executor never invoked"
                );
                state.event_broker.notify(&job.task_id).await;
                return Ok(());
            }
            Err(turul_a2a::storage::A2aStorageError::TerminalStateAlreadySet { .. }) => {
                tracing::debug!(
                    item = %identifier,
                    "task reached terminal concurrently with cancel — success"
                );
                return Ok(());
            }
            Err(e) => {
                tracing::error!(
                    item = %identifier,
                    error = %e,
                    "ADR-018 canceled compensation failed; batch-item retry"
                );
                return Err(identifier);
            }
        }
    }

    // Build deps + scope and delegate to the shared
    // `run_queued_executor_job` which handles sink construction and
    // post-execute detection. Any executor failure surfaces through
    // the sink as a FAILED terminal — from the SQS handler's
    // perspective the record was processed successfully.
    let deps = SpawnDeps {
        executor: state.executor.clone(),
        task_storage: state.task_storage.clone(),
        atomic_store: state.atomic_store.clone(),
        event_broker: state.event_broker.clone(),
        in_flight: state.in_flight.clone(),
        push_dispatcher: state.push_dispatcher.clone(),
    };
    let scope = SpawnScope {
        tenant: job.tenant.clone(),
        owner: job.owner.clone(),
        task_id: job.task_id.clone(),
        context_id: job.context_id.clone(),
        message: match Message::try_from(job.message.clone()) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(item = %identifier, error = %e, "invalid message in SQS job");
                return Err(identifier);
            }
        },
        claims: job.claims.clone(),
    };
    run_queued_executor_job(deps, scope).await;
    Ok(())
}
