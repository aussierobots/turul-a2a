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
use aws_lambda_events::apigw::ApiGatewayV2httpResponse;
use aws_lambda_events::event::sqs::{BatchItemFailure, SqsBatchResponse, SqsEvent, SqsMessage};
use aws_sdk_sqs::Client as SqsClient;
use base64::Engine;
use http_body_util::BodyExt;
use turul_a2a::durable_executor::{DurableExecutorQueue, QueueError, QueuedExecutorJob};
use turul_a2a::router::AppState;
use turul_a2a::server::spawn::{SpawnDeps, SpawnScope, run_queued_executor_job};
use turul_a2a_types::{Message, Part, Role, TaskState, TaskStatus};

use crate::LambdaA2aHandler;

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

/// Extra Lambda runners available when the `sqs` feature is on. These
/// are methods on [`LambdaA2aHandler`] so adopters stay at one level
/// of abstraction: pick the runner whose name matches the Lambda's
/// AWS triggers, then `.await?` and the adapter owns the envelope
/// dispatch.
///
/// Runner matrix:
///
/// | Lambda triggers                   | Runner                     |
/// |-----------------------------------|----------------------------|
/// | HTTP only (Function URL / APIGW)  | [`Self::run_http_only`]    |
/// | SQS only (event source mapping)   | [`Self::run_sqs_only`]     |
/// | HTTP **and** SQS on one function  | [`Self::run_http_and_sqs`] |
/// | not sure / just run it            | [`Self::run`]              |
///
/// The default `.run()` uses the HTTP+SQS classifier and handles both
/// shapes — safe for any of the three topologies above. The explicit
/// runners are strict: a non-matching event shape fails loudly, which
/// is what you want on hardened deployments or in tests.
impl LambdaA2aHandler {
    /// Run this handler as a pure SQS consumer. Strict: any non-SQS
    /// event shape fails to deserialize and the Lambda invocation
    /// errors.
    ///
    /// Appropriate for the consumer Lambda in the two-Lambda durable
    /// executor topology — it does not need
    /// `LambdaA2aServerBuilder::with_sqs_return_immediately(...)` (the
    /// worker never enqueues; it only consumes from the SQS event
    /// source mapping).
    pub async fn run_sqs_only(self) -> Result<(), lambda_runtime::Error> {
        let handler = Arc::new(self);
        lambda_runtime::run(lambda_runtime::service_fn(
            move |event: lambda_runtime::LambdaEvent<SqsEvent>| {
                let handler = Arc::clone(&handler);
                async move {
                    let (sqs_event, _ctx) = event.into_parts();
                    let resp = handler.handle_sqs(sqs_event).await;
                    Ok::<_, lambda_runtime::Error>(resp)
                }
            },
        ))
        .await
    }

    /// Run this handler as a dual HTTP+SQS Lambda. Routes each event
    /// via [`classify_event`] — HTTP events go through
    /// [`LambdaA2aHandler::handle`] with envelope conversion; SQS
    /// events go through [`LambdaA2aHandler::handle_sqs`]. Any other
    /// event shape errors.
    ///
    /// Appropriate for single-function durable-executor topologies
    /// (e.g. `ReservedConcurrency=1` demos where the same container
    /// handles the HTTP request that enqueues and the SQS trigger
    /// that consumes).
    pub async fn run_http_and_sqs(self) -> Result<(), lambda_runtime::Error> {
        let handler = Arc::new(self);
        lambda_runtime::run(lambda_runtime::service_fn(
            move |event: lambda_runtime::LambdaEvent<serde_json::Value>| {
                let handler = Arc::clone(&handler);
                async move {
                    let (value, _ctx) = event.into_parts();
                    match classify_event(&value) {
                        LambdaEvent::Sqs => {
                            let sqs_event: SqsEvent =
                                serde_json::from_value(value).map_err(|e| {
                                    lambda_runtime::Error::from(format!("invalid SQS event: {e}"))
                                })?;
                            let resp = handler.handle_sqs(sqs_event).await;
                            serde_json::to_value(resp).map_err(|e| {
                                lambda_runtime::Error::from(format!(
                                    "serialise SqsBatchResponse: {e}"
                                ))
                            })
                        }
                        LambdaEvent::Http => dispatch_http_event(&handler, value).await,
                        LambdaEvent::Unknown => Err(lambda_runtime::Error::from(
                            "unknown Lambda event shape — expected HTTP or SQS",
                        )),
                    }
                }
            },
        ))
        .await
    }

    /// Default Lambda runner (with `sqs` feature on). Same dispatch
    /// as [`Self::run_http_and_sqs`] — routes HTTP and SQS events via
    /// the classifier. Safe for Lambdas that receive either or both.
    pub async fn run(self) -> Result<(), lambda_runtime::Error> {
        self.run_http_and_sqs().await
    }
}

/// Convert a raw Lambda HTTP event (API Gateway / Function URL / ALB)
/// into a `lambda_http::Request`, dispatch through
/// `handler.handle(...)`, then build an API Gateway v2 / Function URL
/// response JSON.
///
/// Kept `pub(crate)` — the only caller is
/// [`LambdaA2aHandler::run_http_and_sqs`].
/// This boilerplate is what `lambda_http::run` normally hides; we
/// open-code it once because the dual-event runner needs
/// `lambda_runtime::run` (generic) rather than `lambda_http::run`
/// (HTTP-only).
///
/// Body encoding: text for `text/*`, `application/json`,
/// `application/xml`, `application/javascript`, or anything with
/// `charset=`; base64 otherwise.
pub(crate) async fn dispatch_http_event(
    handler: &LambdaA2aHandler,
    value: serde_json::Value,
) -> Result<serde_json::Value, lambda_runtime::Error> {
    let lambda_req: lambda_http::request::LambdaRequest = serde_json::from_value(value)
        .map_err(|e| lambda_runtime::Error::from(format!("invalid HTTP event: {e}")))?;
    let req: lambda_http::Request = lambda_req.into();

    let resp = handler
        .handle(req)
        .await
        .map_err(|e| lambda_runtime::Error::from(format!("handler error: {e}")))?;
    let (parts, body) = resp.into_parts();

    let bytes = body
        .collect()
        .await
        .map_err(|e| lambda_runtime::Error::from(format!("body collect: {e}")))?
        .to_bytes();

    let ct = parts
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_ascii_lowercase();
    let is_text = ct.starts_with("text/")
        || ct.starts_with("application/json")
        || ct.starts_with("application/xml")
        || ct.starts_with("application/javascript")
        || ct.contains("charset=");

    let (body_str, is_base64) = if bytes.is_empty() {
        (None, false)
    } else if is_text {
        match std::str::from_utf8(&bytes) {
            Ok(s) => (Some(s.to_string()), false),
            Err(_) => (
                Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
                true,
            ),
        }
    } else {
        (
            Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
            true,
        )
    };

    let mut api_resp = ApiGatewayV2httpResponse::default();
    api_resp.status_code = parts.status.as_u16() as i64;
    api_resp.headers = parts.headers;
    api_resp.body = body_str.map(aws_lambda_events::encodings::Body::Text);
    api_resp.is_base64_encoded = is_base64;

    serde_json::to_value(api_resp)
        .map_err(|e| lambda_runtime::Error::from(format!("serialise HTTP response: {e}")))
}

#[cfg(test)]
mod dispatch_http_event_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use turul_a2a::executor::{AgentExecutor, ExecutionContext};
    use turul_a2a::server::RuntimeConfig;
    use turul_a2a::server::in_flight::InFlightRegistry;
    use turul_a2a::storage::InMemoryA2aStorage;
    use turul_a2a::streaming::TaskEventBroker;
    use turul_a2a_types::Task;

    struct NoOpExecutor(Arc<AtomicUsize>);

    #[async_trait]
    impl AgentExecutor for NoOpExecutor {
        async fn execute(
            &self,
            task: &mut Task,
            _msg: &Message,
            _ctx: &ExecutionContext,
        ) -> Result<(), turul_a2a::error::A2aError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let mut proto = task.as_proto().clone();
            proto.status = Some(turul_a2a_proto::TaskStatus {
                state: turul_a2a_proto::TaskState::Completed.into(),
                message: None,
                timestamp: None,
            });
            *task = Task::try_from(proto).unwrap();
            Ok(())
        }
        fn agent_card(&self) -> turul_a2a_proto::AgentCard {
            turul_a2a_proto::AgentCard {
                name: "test-agent".to_string(),
                ..Default::default()
            }
        }
    }

    fn build_handler() -> LambdaA2aHandler {
        let s = Arc::new(InMemoryA2aStorage::new());
        let state = AppState {
            executor: Arc::new(NoOpExecutor(Arc::new(AtomicUsize::new(0)))),
            task_storage: s.clone(),
            push_storage: s.clone(),
            event_store: s.clone(),
            atomic_store: s.clone(),
            event_broker: TaskEventBroker::new(),
            middleware_stack: Arc::new(turul_a2a::middleware::MiddlewareStack::new(vec![])),
            runtime_config: RuntimeConfig::default(),
            in_flight: Arc::new(InFlightRegistry::new()),
            cancellation_supervisor: s.clone(),
            push_delivery_store: None,
            push_dispatcher: None,
            durable_executor_queue: None,
        };
        let router = turul_a2a::router::build_router(state.clone());
        LambdaA2aHandler { router, state }
    }

    fn apigw_v2_agent_card_get() -> serde_json::Value {
        serde_json::json!({
            "version": "2.0",
            "routeKey": "GET /.well-known/agent-card.json",
            "rawPath": "/.well-known/agent-card.json",
            "rawQueryString": "",
            "headers": {"accept": "application/json", "a2a-version": "1.0"},
            "requestContext": {
                "accountId": "000000000000",
                "apiId": "api",
                "domainName": "fake.execute-api",
                "domainPrefix": "fake",
                "http": {
                    "method": "GET",
                    "path": "/.well-known/agent-card.json",
                    "protocol": "HTTP/1.1",
                    "sourceIp": "127.0.0.1",
                    "userAgent": "test"
                },
                "requestId": "rid",
                "routeKey": "GET /.well-known/agent-card.json",
                "stage": "$default",
                "time": "01/Jan/2026:00:00:00 +0000",
                "timeEpoch": 1_735_689_600_000i64
            },
            "isBase64Encoded": false
        })
    }

    #[tokio::test]
    async fn dispatch_http_event_agent_card_returns_text_json() {
        let handler = build_handler();
        let event = apigw_v2_agent_card_get();
        let resp_json = dispatch_http_event(&handler, event).await.expect("ok");
        let resp: ApiGatewayV2httpResponse = serde_json::from_value(resp_json).unwrap();
        assert_eq!(resp.status_code, 200);
        assert!(!resp.is_base64_encoded, "JSON body must not be base64");
        let body = match resp.body {
            Some(aws_lambda_events::encodings::Body::Text(s)) => s,
            other => panic!("expected text body, got {other:?}"),
        };
        assert!(
            body.contains("\"name\""),
            "agent card shape in body: {body}"
        );
    }

    #[tokio::test]
    async fn dispatch_http_event_rejects_unknown_shape() {
        let handler = build_handler();
        let event = serde_json::json!({"not": "an HTTP event"});
        let err = dispatch_http_event(&handler, event)
            .await
            .expect_err("unknown shape must error");
        let s = format!("{err}");
        assert!(s.contains("invalid HTTP event"), "error msg: {s}");
    }
}
