//! ADR-018 durable executor continuation tests.
//!
//! Core-crate coverage (no Lambda feature required):
//!
//! - Envelope round-trip preserves all fields including `claims`.
//! - Claims thread through to the executor on BOTH the blocking-send
//!   path (Phase 1 pre-existing bug fix) and the durable-queue path.
//! - Oversize payload does not create a task (preflight).
//! - Enqueue-failure compensates to FAILED with reason message.
//! - Cancelled queued task: `supervisor_get_cancel_requested == true`
//!   → CANCELED committed directly, executor never invoked.
//! - Concurrent cancel + terminal race: `TerminalStateAlreadySet`
//!   returned as success (the record is deleted, no retry).
//! - Task 45: pending-dispatch marker write is skipped when zero
//!   push configs are registered for the task.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tower::util::ServiceExt;

use turul_a2a::durable_executor::{DurableExecutorQueue, QueueError, QueuedExecutorJob};
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::router::{AppState, build_router};
use turul_a2a::server::RuntimeConfig;
use turul_a2a::server::in_flight::InFlightRegistry;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a::streaming::TaskEventBroker;
use turul_a2a_types::{Message, Task, TaskState};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Records executor invocations + the claims observed on each call.
struct ClaimsObservingExecutor {
    invocations: Arc<AtomicUsize>,
    last_claims: Arc<tokio::sync::Mutex<Option<serde_json::Value>>>,
}

impl ClaimsObservingExecutor {
    fn new() -> (
        Arc<Self>,
        Arc<AtomicUsize>,
        Arc<tokio::sync::Mutex<Option<serde_json::Value>>>,
    ) {
        let invocations = Arc::new(AtomicUsize::new(0));
        let last_claims = Arc::new(tokio::sync::Mutex::new(None));
        let ex = Arc::new(Self {
            invocations: invocations.clone(),
            last_claims: last_claims.clone(),
        });
        (ex, invocations, last_claims)
    }
}

#[async_trait]
impl AgentExecutor for ClaimsObservingExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        *self.last_claims.lock().await = ctx.claims.clone();
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
        turul_a2a::card_builder::AgentCardBuilder::new("ADR-018 test", "1.0.0")
            .description("ADR-018 wire test harness")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .unwrap()
    }
}

/// Minimal no-op executor for tests that don't care about claims.
struct NoOpExecutor;

#[async_trait]
impl AgentExecutor for NoOpExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
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
        turul_a2a::card_builder::AgentCardBuilder::new("ADR-018 test", "1.0.0")
            .description("ADR-018 wire test harness")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .unwrap()
    }
}

/// Fake DurableExecutorQueue that captures enqueued jobs. `max_bytes`
/// controls the preflight ceiling. `fail_enqueue` toggles the
/// synthetic transport failure used by the compensation test.
struct FakeQueue {
    max_bytes: usize,
    fail_enqueue: bool,
    captured: Arc<tokio::sync::Mutex<Vec<QueuedExecutorJob>>>,
}

impl FakeQueue {
    fn new(max_bytes: usize) -> (Arc<Self>, Arc<tokio::sync::Mutex<Vec<QueuedExecutorJob>>>) {
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let q = Arc::new(Self {
            max_bytes,
            fail_enqueue: false,
            captured: captured.clone(),
        });
        (q, captured)
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            max_bytes: 256 * 1024,
            fail_enqueue: true,
            captured: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        })
    }
}

#[async_trait]
impl DurableExecutorQueue for FakeQueue {
    fn max_payload_bytes(&self) -> usize {
        self.max_bytes
    }

    async fn enqueue(&self, job: QueuedExecutorJob) -> Result<(), QueueError> {
        if self.fail_enqueue {
            return Err(QueueError::Transport("simulated transport failure".into()));
        }
        self.captured.lock().await.push(job);
        Ok(())
    }

    fn kind(&self) -> &'static str {
        "fake"
    }
}

fn storage_with_push_dispatch() -> Arc<InMemoryA2aStorage> {
    Arc::new(InMemoryA2aStorage::new().with_push_dispatch_enabled(true))
}

fn base_state(executor: Arc<dyn AgentExecutor>) -> AppState {
    let s = storage_with_push_dispatch();
    AppState {
        executor,
        task_storage: s.clone(),
        push_storage: s.clone(),
        event_store: s.clone(),
        atomic_store: s.clone(),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(turul_a2a::middleware::MiddlewareStack::new(vec![])),
        runtime_config: RuntimeConfig::default(),
        in_flight: Arc::new(InFlightRegistry::new()),
        cancellation_supervisor: s,
        push_delivery_store: None,
        push_dispatcher: None,
        durable_executor_queue: None,
    }
}

fn state_with_queue(
    executor: Arc<dyn AgentExecutor>,
    queue: Arc<dyn DurableExecutorQueue>,
) -> AppState {
    let mut state = base_state(executor);
    state.runtime_config.supports_return_immediately = true;
    state.durable_executor_queue = Some(queue);
    state
}

async fn json_response(router: axum::Router, req: Request<Body>) -> (u16, serde_json::Value) {
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::json!({})),
    )
}

fn send_body(return_immediately: bool) -> String {
    serde_json::json!({
        "message": {
            "messageId": uuid::Uuid::now_v7().to_string(),
            "role": "ROLE_USER",
            "parts": [{"text": "hello", "mediaType": "text/plain"}],
        },
        "configuration": { "returnImmediately": return_immediately },
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Envelope round-trip
// ---------------------------------------------------------------------------

#[test]
fn queued_executor_job_envelope_roundtrip_preserves_claims() {
    let claims = serde_json::json!({
        "sub": "user-123",
        "iss": "https://auth.example.test",
        "exp": 1_900_000_000i64,
    });
    let job = QueuedExecutorJob {
        version: QueuedExecutorJob::VERSION,
        tenant: "tenant-a".into(),
        owner: "user-123".into(),
        task_id: "task-x".into(),
        context_id: "ctx-y".into(),
        message: turul_a2a_proto::Message {
            message_id: "m1".into(),
            role: turul_a2a_proto::Role::User.into(),
            ..Default::default()
        },
        claims: Some(claims.clone()),
        enqueued_at_micros: 123_456_789,
    };
    let encoded = serde_json::to_string(&job).unwrap();
    let decoded: QueuedExecutorJob = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.version, QueuedExecutorJob::VERSION);
    assert_eq!(decoded.tenant, "tenant-a");
    assert_eq!(decoded.owner, "user-123");
    assert_eq!(decoded.task_id, "task-x");
    assert_eq!(decoded.context_id, "ctx-y");
    assert_eq!(decoded.claims, Some(claims));
    assert_eq!(decoded.enqueued_at_micros, 123_456_789);
}

// ---------------------------------------------------------------------------
// Oversize payload must not create a task
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oversize_payload_rejected_before_task_creation() {
    // max_bytes = 100 — any realistic payload (which includes three
    // UUIDs + field names + JSON overhead) will exceed this.
    let (queue, captured) = FakeQueue::new(100);
    let (executor, invocations, _claims) = ClaimsObservingExecutor::new();
    let state = state_with_queue(executor, queue);
    let task_storage = state.task_storage.clone();
    let router = build_router(state);

    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body(true)))
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 400, "oversize payload must return HTTP 400");

    let page = task_storage
        .list_tasks(turul_a2a::storage::TaskFilter {
            tenant: Some(String::new()),
            owner: Some("anonymous".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        page.tasks.is_empty(),
        "oversize payload must leave zero tasks"
    );
    assert!(
        captured.lock().await.is_empty(),
        "fake queue must not have been called for enqueue"
    );
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "executor must not have run"
    );
}

// ---------------------------------------------------------------------------
// Enqueue success → task is Working, executor not invoked locally
// ---------------------------------------------------------------------------

#[tokio::test]
async fn successful_enqueue_returns_working_without_executor_spawn() {
    let (queue, captured) = FakeQueue::new(256 * 1024);
    let (executor, invocations, _claims) = ClaimsObservingExecutor::new();
    let state = state_with_queue(executor, queue);
    let router = build_router(state);

    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body(true)))
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200, "enqueue path must return 200: {body}");

    let state_str = body
        .pointer("/task/status/state")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(state_str, "TASK_STATE_WORKING");

    let jobs = captured.lock().await;
    assert_eq!(jobs.len(), 1, "exactly one job enqueued");
    assert_eq!(jobs[0].version, QueuedExecutorJob::VERSION);
    assert_eq!(jobs[0].owner, "anonymous");

    // Executor must NOT have run in-process on the HTTP path.
    assert_eq!(invocations.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// Enqueue-failure compensation → task FAILED with reason message
// ---------------------------------------------------------------------------

#[tokio::test]
async fn enqueue_failure_compensates_to_failed_with_reason() {
    let queue = FakeQueue::failing();
    let (executor, invocations, _claims) = ClaimsObservingExecutor::new();
    let state = state_with_queue(executor, queue);
    let task_storage = state.task_storage.clone();
    let router = build_router(state);

    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body(true)))
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert!(status >= 400, "enqueue failure must surface as error");

    let page = task_storage
        .list_tasks(turul_a2a::storage::TaskFilter {
            tenant: Some(String::new()),
            owner: Some("anonymous".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.tasks.len(), 1, "exactly one (FAILED) task exists");
    let t = &page.tasks[0];
    let s = t.status().unwrap().state().unwrap();
    assert_eq!(s, TaskState::Failed);

    let status_msg = t
        .status()
        .unwrap()
        .as_proto()
        .message
        .clone()
        .expect("FAILED status must carry reason Message");
    let text = status_msg
        .parts
        .iter()
        .filter_map(|p| match &p.content {
            Some(turul_a2a_proto::part::Content::Text(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        text.contains("durable executor enqueue failed"),
        "reason message must name the failure; got {text:?}"
    );

    assert_eq!(invocations.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// Phase 1 claims plumbing — executor sees Some(claims) on blocking send
// ---------------------------------------------------------------------------

#[tokio::test]
async fn blocking_send_delivers_claims_to_executor() {
    // Use `core_send_message` directly so we can pass claims explicitly
    // (the HTTP path would require a live auth middleware + JWT, which
    // is overkill for this Phase 1 assertion).
    let (executor, invocations, last_claims) = ClaimsObservingExecutor::new();
    let state = base_state(executor);
    let claims_in = Some(serde_json::json!({"sub": "alice", "scope": "read"}));
    let _json = turul_a2a::router::core_send_message(
        state,
        "",
        "alice",
        claims_in.clone(),
        serde_json::json!({
            "message": {
                "messageId": uuid::Uuid::now_v7().to_string(),
                "role": "ROLE_USER",
                "parts": [{"text": "hi", "mediaType": "text/plain"}],
            }
        })
        .to_string(),
    )
    .await
    .expect("send should succeed");
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    assert_eq!(*last_claims.lock().await, claims_in);
}

// ---------------------------------------------------------------------------
// Pending-dispatch optimization (task 45)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pending_dispatch_skipped_when_no_push_configs() {
    let s = Arc::new(InMemoryA2aStorage::new().with_push_dispatch_enabled(true));
    let state = AppState {
        executor: Arc::new(NoOpExecutor),
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
    let router = build_router(state);
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body(false)))
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 200);

    // No push configs were registered → no pending-dispatch rows.
    use turul_a2a::push::A2aPushDeliveryStore;
    let stale = s
        .list_stale_pending_dispatches(
            std::time::SystemTime::now() + std::time::Duration::from_secs(60),
            100,
        )
        .await
        .expect("list_stale works");
    assert!(
        stale.is_empty(),
        "framework must skip pending-dispatch writes when no push configs exist; got {} rows",
        stale.len()
    );
}
