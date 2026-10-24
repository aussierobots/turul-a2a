//! Cancellation propagation tests.
//!
//! Covers the `CancelTask` contract end to end: a cooperative executor
//! acknowledging the cancel, cross-instance propagation through the
//! storage marker and the supervisor poll, the orphan grace-and-force
//! path, both outcomes of the cancel-vs-complete race, the abort of an
//! executor that ignores the token once the grace window ends (same
//! instance and across instances, sparing an executor that already
//! committed its own terminal), a blocking send in flight while the
//! task is cancelled, the cancel error cases from
//! A2A v1.0.1 §3.1.5 on every transport, supervisor-panic cleanup, and
//! the terminal event a subscriber sees.
//!
//! Timing-sensitive tests run on tokio's paused clock: sleeps and
//! deadlines advance virtual time only when every task is idle, so
//! "returned before the grace window" is an exact statement about the
//! code path taken, not a wall-clock race.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::middleware::MiddlewareStack;
use turul_a2a::router::{AppState, build_router, core_cancel_task, core_send_message};
use turul_a2a::server::RuntimeConfig;
use turul_a2a::server::in_flight::{InFlightHandle, InFlightRegistry, SupervisorSentinel};
use turul_a2a::storage::{
    A2aAtomicStore, A2aCancellationSupervisor, A2aEventStore, A2aStorageError, A2aTaskStorage,
    InMemoryA2aStorage, TaskFilter, TaskListPage,
};
use turul_a2a::streaming::{StatusUpdatePayload, StreamEvent, TaskEventBroker};
use turul_a2a_types::{Artifact, Message, Task, TaskState, TaskStatus};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct NoopExecutor;

#[async_trait]
impl AgentExecutor for NoopExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            capabilities: Some(turul_a2a_proto::AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

const ACK_REASON: &str = "executor acknowledged cancel";

/// Cooperative executor: reports its task id, waits for the cancellation
/// token, then acknowledges with `sink.cancelled(ACK_REASON)`.
struct AckCancel {
    started: Mutex<Option<oneshot::Sender<String>>>,
}

impl AckCancel {
    fn new() -> (Self, oneshot::Receiver<String>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                started: Mutex::new(Some(tx)),
            },
            rx,
        )
    }
}

#[async_trait]
impl AgentExecutor for AckCancel {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        if let Some(tx) = self.started.lock().unwrap().take() {
            let _ = tx.send(task.id().to_string());
        }
        ctx.cancellation.cancelled().await;
        ctx.events.cancelled(Some(ACK_REASON.into())).await?;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// Observes the cancellation token and answers it by completing the
/// task: the completion reaches the store while the cancel handler is
/// still inside its grace wait.
struct CompletesOnCancel;

#[async_trait]
impl AgentExecutor for CompletesOnCancel {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        ctx.cancellation.cancelled().await;
        ctx.events.complete(None).await?;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// Ignores cancellation and completes only when the test opens the gate;
/// reports what `complete()` returned.
struct GatedCompleter {
    probe: Mutex<Option<oneshot::Receiver<()>>>,
    gate: Mutex<Option<oneshot::Receiver<()>>>,
    outcome: Mutex<Option<oneshot::Sender<Result<u64, String>>>>,
}

#[async_trait]
impl AgentExecutor for GatedCompleter {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let _probe_held_in_future = self.probe.lock().unwrap().take();
        let gate = self.gate.lock().unwrap().take().expect("gate");
        let _ = gate.await;
        let result = ctx.events.complete(None).await.map_err(|e| e.to_string());
        if let Some(tx) = self.outcome.lock().unwrap().take() {
            let _ = tx.send(result);
        }
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// Ignores the cancellation token and sleeps; holds a drop probe so a
/// test can observe the executor future being dropped (aborted).
struct IgnoresToken {
    probe: Mutex<Option<oneshot::Receiver<()>>>,
}

impl IgnoresToken {
    fn new() -> (Self, oneshot::Sender<()>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                probe: Mutex::new(Some(rx)),
            },
            tx,
        )
    }
}

#[async_trait]
impl AgentExecutor for IgnoresToken {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let _probe_held_in_future = self.probe.lock().unwrap().take();
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// Raises AUTH_REQUIRED, resumes WORKING and then ignores the token. The
/// drop probe observes the executor future being dropped.
struct InterruptsThenIgnoresToken {
    probe: Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait]
impl AgentExecutor for InterruptsThenIgnoresToken {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let _probe_held_in_future = self.probe.lock().unwrap().take();
        ctx.events.require_auth(None).await?;
        ctx.events.set_status(TaskState::Working, None).await?;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// Acknowledges the cancel through its sink, then keeps running cleanup
/// well past the grace window and records that the cleanup finished.
struct AcksThenCleansUp {
    cleanup: Duration,
    cleaned_up: Arc<AtomicBool>,
}

#[async_trait]
impl AgentExecutor for AcksThenCleansUp {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        ctx.cancellation.cancelled().await;
        ctx.events.cancelled(Some(ACK_REASON.into())).await?;
        tokio::time::sleep(self.cleanup).await;
        self.cleaned_up.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// Waits until the registry holds no entry (supervisor cleanup ran).
async fn await_registry_empty(state: &AppState) {
    for _ in 0..500 {
        if state.in_flight.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("in-flight registry entry was never removed");
}

fn state_on(storage: InMemoryA2aStorage, executor: Arc<dyn AgentExecutor>) -> AppState {
    AppState {
        executor,
        task_storage: Arc::new(storage.clone()),
        push_storage: Arc::new(storage.clone()),
        event_store: Arc::new(storage.clone()),
        atomic_store: Arc::new(storage.clone()),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(MiddlewareStack::new(vec![])),
        runtime_config: RuntimeConfig::default(),
        in_flight: Arc::new(InFlightRegistry::new()),
        cancellation_supervisor: Arc::new(storage),
        push_delivery_store: None,
        push_dispatcher: None,
        durable_executor_queue: None,
    }
}

/// Build an AppState wrapping a single InMemoryA2aStorage instance with
/// the default runtime config.
fn make_state() -> (AppState, InMemoryA2aStorage) {
    let storage = InMemoryA2aStorage::new();
    (state_on(storage.clone(), Arc::new(NoopExecutor)), storage)
}

/// Shorten grace / poll-interval for tests that exercise the
/// framework-forced-CANCELED path on the real clock.
fn with_fast_grace(mut state: AppState) -> AppState {
    state.runtime_config.cancel_handler_grace = Duration::from_millis(200);
    state.runtime_config.cancel_handler_poll_interval = Duration::from_millis(10);
    state.runtime_config.cross_instance_cancel_poll_interval = Duration::from_millis(50);
    state
}

/// Seed a task in Submitted state via the task storage (bypassing the
/// atomic store so setup adds no events).
async fn seed_task(storage: &InMemoryA2aStorage, tenant: &str, owner: &str, task_id: &str) {
    let task =
        Task::new(task_id, TaskStatus::new(TaskState::Submitted)).with_context_id("ctx-cancel");
    storage
        .create_task(tenant, owner, task)
        .await
        .expect("seed_task");
}

/// Move a task to `state` through the atomic store, writing the matching
/// status-update event.
async fn transition(
    storage: &InMemoryA2aStorage,
    tenant: &str,
    owner: &str,
    task_id: &str,
    state: TaskState,
) {
    storage
        .update_task_status_with_events(
            tenant,
            task_id,
            owner,
            TaskStatus::new(state),
            vec![StreamEvent::StatusUpdate {
                status_update: StatusUpdatePayload {
                    task_id: task_id.to_string(),
                    context_id: "ctx-cancel".to_string(),
                    status: serde_json::to_value(TaskStatus::new(state)).unwrap_or_default(),
                },
            }],
        )
        .await
        .expect("transition");
}

async fn advance_to_working(
    storage: &InMemoryA2aStorage,
    tenant: &str,
    owner: &str,
    task_id: &str,
) {
    transition(storage, tenant, owner, task_id, TaskState::Working).await;
}

/// Install a live in-flight handle into `state.in_flight`. Returns the
/// cancellation token the "executor" waits on.
fn install_in_flight(state: &AppState, tenant: &str, task_id: &str) -> CancellationToken {
    let token = CancellationToken::new();
    let exposed = token.clone();
    let spawned = tokio::spawn(async move { token.cancelled().await });
    let (yielded_tx, _yielded_rx) = oneshot::channel::<Task>();
    let handle = Arc::new(InFlightHandle::new(exposed.clone(), yielded_tx, spawned));
    state
        .in_flight
        .try_insert((tenant.to_string(), task_id.to_string()), handle)
        .expect("try_insert");
    exposed
}

/// Start the state's executor through a non-blocking `SendMessage` and
/// return the new task's id.
async fn start_non_blocking(state: &AppState, owner: &str) -> String {
    let body = serde_json::json!({
        "message": {
            "messageId": uuid::Uuid::now_v7().to_string(),
            "role": "ROLE_USER",
            "parts": [{"text": "work"}],
        },
        "configuration": {"returnImmediately": true},
    })
    .to_string();
    let axum::Json(v) =
        core_send_message(state.clone(), "", owner, None, &Default::default(), body)
            .await
            .expect("non-blocking send");
    v["task"]["id"].as_str().expect("task id").to_string()
}

fn state_of(task_json: &serde_json::Value) -> &str {
    task_json["status"]["state"].as_str().unwrap_or("<missing>")
}

/// Concatenated text parts of the task's status message, if any.
fn status_text(task_json: &serde_json::Value) -> Option<String> {
    let parts = task_json["status"]["message"]["parts"].as_array()?;
    Some(
        parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Terminal status-update events persisted for a task, in sequence order.
async fn terminal_events(
    storage: &InMemoryA2aStorage,
    tenant: &str,
    task_id: &str,
) -> Vec<(u64, TaskState)> {
    storage
        .get_events_after(tenant, task_id, 0)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|(seq, e)| e.task_state().map(|s| (seq, s)))
        .filter(|(_, s)| turul_a2a_types::state_machine::is_terminal(*s))
        .collect()
}

/// Resolves once the probe's paired future has been dropped (completed
/// or aborted). The timeout is a hang guard, not a timing assertion.
async fn await_dropped(probe: oneshot::Sender<()>) {
    let mut probe = probe;
    tokio::time::timeout(Duration::from_secs(5), probe.closed())
        .await
        .expect("future holding the probe receiver was never dropped");
}

// ---------------------------------------------------------------------------
// Marker-write spy: counts `set_cancel_requested` calls the handler makes.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MarkerWriteSpy {
    inner: InMemoryA2aStorage,
    marker_writes: Arc<AtomicUsize>,
}

#[async_trait]
impl A2aTaskStorage for MarkerWriteSpy {
    fn backend_name(&self) -> &'static str {
        "marker-write-spy"
    }
    async fn create_task(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
    ) -> Result<Task, A2aStorageError> {
        self.inner.create_task(tenant, owner, task).await
    }
    async fn get_task(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        history_length: Option<i32>,
    ) -> Result<Option<Task>, A2aStorageError> {
        self.inner
            .get_task(tenant, task_id, owner, history_length)
            .await
    }
    async fn update_task(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
    ) -> Result<(), A2aStorageError> {
        self.inner.update_task(tenant, owner, task).await
    }
    async fn delete_task(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
    ) -> Result<bool, A2aStorageError> {
        self.inner.delete_task(tenant, task_id, owner).await
    }
    async fn list_tasks(&self, filter: TaskFilter) -> Result<TaskListPage, A2aStorageError> {
        self.inner.list_tasks(filter).await
    }
    async fn update_task_status(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        new_status: TaskStatus,
    ) -> Result<Task, A2aStorageError> {
        self.inner
            .update_task_status(tenant, task_id, owner, new_status)
            .await
    }
    async fn append_message(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        message: Message,
    ) -> Result<(), A2aStorageError> {
        self.inner
            .append_message(tenant, task_id, owner, message)
            .await
    }
    async fn append_artifact(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        artifact: Artifact,
        append: bool,
        last_chunk: bool,
    ) -> Result<(), A2aStorageError> {
        self.inner
            .append_artifact(tenant, task_id, owner, artifact, append, last_chunk)
            .await
    }
    async fn task_count(&self) -> Result<usize, A2aStorageError> {
        self.inner.task_count().await
    }
    async fn maintenance(&self) -> Result<(), A2aStorageError> {
        self.inner.maintenance().await
    }
    async fn set_cancel_requested(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
    ) -> Result<(), A2aStorageError> {
        self.marker_writes.fetch_add(1, Ordering::SeqCst);
        self.inner
            .set_cancel_requested(tenant, task_id, owner)
            .await
    }
}

fn spied_state() -> (AppState, InMemoryA2aStorage, Arc<AtomicUsize>) {
    let (mut state, storage) = make_state();
    let marker_writes = Arc::new(AtomicUsize::new(0));
    state.task_storage = Arc::new(MarkerWriteSpy {
        inner: storage.clone(),
        marker_writes: marker_writes.clone(),
    });
    (state, storage, marker_writes)
}

// ---------------------------------------------------------------------------
// Transport adapters: one CancelTask call over HTTP, JSON-RPC or gRPC,
// normalised to `Ok(task json)` or `Err(ErrorInfo reason)`. Each adapter
// asserts its own wire mapping for the reason it returns (A2A v1.0.1
// §5.4 mapping table), so tests can compare outcomes across transports.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Transport {
    Http,
    JsonRpc,
    #[cfg(feature = "grpc")]
    Grpc,
}

fn transports() -> Vec<Transport> {
    vec![
        Transport::Http,
        Transport::JsonRpc,
        #[cfg(feature = "grpc")]
        Transport::Grpc,
    ]
}

async fn cancel_over(
    transport: Transport,
    state: &AppState,
    task_id: &str,
    metadata: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    match transport {
        Transport::Http => {
            let body = match &metadata {
                Some(m) => serde_json::json!({ "metadata": m }).to_string(),
                None => String::new(),
            };
            let req = Request::post(format!("/tasks/{task_id}:cancel"))
                .header("content-type", "application/json")
                .header("a2a-version", "1.0")
                .body(Body::from(body))
                .unwrap();
            let resp = build_router(state.clone()).oneshot(req).await.unwrap();
            let status = resp.status().as_u16();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if status == 200 {
                return Ok(v);
            }
            let reason = v["error"]["details"][0]["reason"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let expected = match reason.as_str() {
                "TASK_NOT_CANCELABLE" => 400,
                "TASK_NOT_FOUND" => 404,
                other => panic!("unexpected HTTP error reason {other:?}: {v}"),
            };
            assert_eq!(status, expected, "HTTP status for {reason}");
            Err(reason)
        }
        Transport::JsonRpc => {
            let mut params = serde_json::json!({ "id": task_id });
            if let Some(m) = metadata {
                params["metadata"] = m;
            }
            let rpc = serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "CancelTask", "params": params,
            });
            let req = Request::post("/jsonrpc")
                .header("content-type", "application/json")
                .header("a2a-version", "1.0")
                .body(Body::from(rpc.to_string()))
                .unwrap();
            let resp = build_router(state.clone()).oneshot(req).await.unwrap();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if let Some(result) = v.get("result") {
                return Ok(result.clone());
            }
            let reason = v["error"]["data"][0]["reason"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let expected = match reason.as_str() {
                "TASK_NOT_CANCELABLE" => -32002,
                "TASK_NOT_FOUND" => -32001,
                other => panic!("unexpected JSON-RPC error reason {other:?}: {v}"),
            };
            assert_eq!(v["error"]["code"], expected, "JSON-RPC code for {reason}");
            Err(reason)
        }
        #[cfg(feature = "grpc")]
        Transport::Grpc => {
            use tonic_types::StatusExt;
            use turul_a2a_proto::grpc::A2aService;
            let mut req = serde_json::json!({ "id": task_id });
            if let Some(m) = metadata {
                req["metadata"] = m;
            }
            let req: turul_a2a_proto::CancelTaskRequest =
                serde_json::from_value(req).expect("CancelTaskRequest");
            let service = turul_a2a::grpc::GrpcService::new(state.clone());
            match service.cancel_task(tonic::Request::new(req)).await {
                Ok(resp) => Ok(serde_json::to_value(resp.into_inner()).unwrap()),
                Err(status) => {
                    let reason = status
                        .get_details_error_info()
                        .expect("ErrorInfo in gRPC status")
                        .reason;
                    let expected = match reason.as_str() {
                        "TASK_NOT_CANCELABLE" => tonic::Code::FailedPrecondition,
                        "TASK_NOT_FOUND" => tonic::Code::NotFound,
                        other => panic!("unexpected gRPC error reason {other:?}: {status:?}"),
                    };
                    assert_eq!(status.code(), expected, "gRPC code for {reason}");
                    Err(reason)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Same-instance cancel: the cooperative executor observes the token and
// acknowledges; the handler returns that terminal before its grace ends.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn same_instance_cooperative_executor_acknowledges_before_grace() {
    let storage = InMemoryA2aStorage::new();
    let (executor, _started) = AckCancel::new();
    let state = state_on(storage.clone(), Arc::new(executor));
    let task_id = start_non_blocking(&state, "owner-a").await;

    let start = tokio::time::Instant::now();
    let axum::Json(task) = core_cancel_task(state.clone(), "", "owner-a", &task_id)
        .await
        .expect("cancel OK");

    assert_eq!(state_of(&task), "TASK_STATE_CANCELED");
    assert_eq!(
        status_text(&task).as_deref(),
        Some(ACK_REASON),
        "terminal must be the executor's acknowledgement, not the framework's forced commit"
    );
    assert!(
        start.elapsed() < state.runtime_config.cancel_handler_grace,
        "handler must return on the observed terminal, before the grace window; took {:?}",
        start.elapsed()
    );
    let terminals = terminal_events(&storage, "", &task_id).await;
    assert_eq!(
        terminals.len(),
        1,
        "exactly one terminal event: {terminals:?}"
    );
    assert_eq!(terminals[0].1, TaskState::Canceled);
}

// ---------------------------------------------------------------------------
// Cross-instance cancel: instance B runs the CancelTask handler; the
// executor lives on instance A. A's supervisor poll observes B's marker,
// A's executor commits CANCELED, and B's grace wait returns that terminal.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn cross_instance_cancel_handler_on_b_observes_terminal_committed_on_a() {
    let storage = InMemoryA2aStorage::new();
    let (executor, _started) = AckCancel::new();
    let state_a = state_on(storage.clone(), Arc::new(executor));
    let state_b = state_on(storage.clone(), Arc::new(NoopExecutor));

    let task_id = start_non_blocking(&state_a, "owner-x").await;
    assert!(
        state_b.in_flight.is_empty(),
        "B must not hold the executor: no local token trip is possible on B"
    );

    let shutdown = CancellationToken::new();
    let poller = tokio::spawn(
        turul_a2a::server::in_flight::run_cross_instance_cancel_poller(
            state_a.in_flight.clone(),
            state_a.cancellation_supervisor.clone(),
            Duration::from_millis(50),
            state_a.runtime_config.cancel_handler_grace,
            shutdown.clone(),
        ),
    );

    let start = tokio::time::Instant::now();
    let axum::Json(task) = core_cancel_task(state_b.clone(), "", "owner-x", &task_id)
        .await
        .expect("cancel on B");
    shutdown.cancel();
    poller.await.expect("poller exits on shutdown");

    assert_eq!(state_of(&task), "TASK_STATE_CANCELED");
    assert_eq!(
        status_text(&task).as_deref(),
        Some(ACK_REASON),
        "B must return the terminal A's executor committed, not its own forced CANCELED"
    );
    assert!(
        start.elapsed() < state_b.runtime_config.cancel_handler_grace,
        "B's grace wait must end on A's terminal; took {:?}",
        start.elapsed()
    );
    assert_eq!(terminal_events(&storage, "", &task_id).await.len(), 1);
}

// ---------------------------------------------------------------------------
// Orphaned task (no in-flight executor anywhere): grace expires and the
// framework commits CANCELED with no agent-authored message.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn cancel_on_orphaned_task_resolves_via_grace_fallback() {
    let (state, storage) = make_state();
    seed_task(&storage, "default", "owner-o", "t-orphan").await;
    advance_to_working(&storage, "default", "owner-o", "t-orphan").await;

    let start = tokio::time::Instant::now();
    let axum::Json(task) = core_cancel_task(state.clone(), "default", "owner-o", "t-orphan")
        .await
        .expect("cancel OK");
    assert_eq!(state_of(&task), "TASK_STATE_CANCELED");
    assert!(
        start.elapsed() >= state.runtime_config.cancel_handler_grace,
        "orphan resolution waits out the full grace window; took {:?}",
        start.elapsed()
    );
    assert!(
        task["status"].get("message").is_none_or(|m| m.is_null()),
        "framework-committed CANCELED must carry no message, got: {}",
        task["status"]
    );
}

// ---------------------------------------------------------------------------
// Cancel-vs-complete, cancel wins: the executor ignores the token and is
// held past the grace window; the framework commits CANCELED and aborts
// the executor at the grace deadline, so it never reaches complete().
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn cancel_vs_complete_cancel_wins_and_executor_is_aborted_at_grace() {
    let storage = InMemoryA2aStorage::new();
    let (gate_tx, gate_rx) = oneshot::channel();
    let (outcome_tx, mut outcome_rx) = oneshot::channel::<Result<u64, String>>();
    let (probe_tx, probe_rx) = oneshot::channel::<()>();
    let executor = GatedCompleter {
        probe: Mutex::new(Some(probe_rx)),
        gate: Mutex::new(Some(gate_rx)),
        outcome: Mutex::new(Some(outcome_tx)),
    };
    let state = state_on(storage.clone(), Arc::new(executor));
    let task_id = start_non_blocking(&state, "owner-r").await;

    let axum::Json(task) = core_cancel_task(state.clone(), "", "owner-r", &task_id)
        .await
        .expect("cancel OK");
    assert_eq!(state_of(&task), "TASK_STATE_CANCELED");
    assert!(
        task["status"].get("message").is_none_or(|m| m.is_null()),
        "the framework committed this CANCELED"
    );

    await_dropped(probe_tx).await;
    assert!(
        gate_tx.send(()).is_err(),
        "the executor future was dropped, so nothing is left waiting on the gate"
    );
    await_registry_empty(&state).await;
    assert!(
        outcome_rx.try_recv().is_err(),
        "an aborted executor never reaches complete()"
    );

    let terminals = terminal_events(&storage, "", &task_id).await;
    assert_eq!(terminals.len(), 1, "no second terminal: {terminals:?}");
    assert_eq!(terminals[0].1, TaskState::Canceled);
}

// ---------------------------------------------------------------------------
// Cancel wins, late write refused: the executor's instance arms its abort
// far beyond the committing instance's grace, so the executor is still
// alive when it finally tries to complete and is refused by its closed sink.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn cancel_wins_cross_instance_late_write_is_refused() {
    let storage = InMemoryA2aStorage::new();
    let (gate_tx, gate_rx) = oneshot::channel();
    let (outcome_tx, outcome_rx) = oneshot::channel();
    let executor = GatedCompleter {
        probe: Mutex::new(None),
        gate: Mutex::new(Some(gate_rx)),
        outcome: Mutex::new(Some(outcome_tx)),
    };
    let state_a = state_on(storage.clone(), Arc::new(executor));
    let state_b = state_on(storage.clone(), Arc::new(NoopExecutor));
    let task_id = start_non_blocking(&state_a, "owner-r").await;

    let shutdown = CancellationToken::new();
    let poller = tokio::spawn(
        turul_a2a::server::in_flight::run_cross_instance_cancel_poller(
            state_a.in_flight.clone(),
            state_a.cancellation_supervisor.clone(),
            Duration::from_millis(50),
            Duration::from_secs(60),
            shutdown.clone(),
        ),
    );

    let axum::Json(task) = core_cancel_task(state_b.clone(), "", "owner-r", &task_id)
        .await
        .expect("cancel on B");
    assert_eq!(state_of(&task), "TASK_STATE_CANCELED");
    assert!(
        task["status"].get("message").is_none_or(|m| m.is_null()),
        "B committed this CANCELED"
    );

    gate_tx.send(()).expect("executor still waiting on gate");
    let late = outcome_rx.await.expect("executor reports its complete()");
    let err = late.expect_err("complete() after B's CANCELED must be refused");
    assert!(
        err.contains("EventSink is closed"),
        "late complete must fail as a closed sink, got: {err}"
    );
    shutdown.cancel();
    poller.await.expect("poller exits on shutdown");

    let terminals = terminal_events(&storage, "", &task_id).await;
    assert_eq!(terminals.len(), 1, "no second terminal: {terminals:?}");
    assert_eq!(terminals[0].1, TaskState::Canceled);
}

// ---------------------------------------------------------------------------
// Executor ignores the token: the handler returns the framework CANCELED at
// grace and the executor future is dropped.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn executor_ignoring_token_is_aborted_and_framework_cancels_at_grace() {
    let storage = InMemoryA2aStorage::new();
    let (executor, probe_tx) = IgnoresToken::new();
    let state = state_on(storage.clone(), Arc::new(executor));
    let task_id = start_non_blocking(&state, "owner-i").await;

    let start = tokio::time::Instant::now();
    let axum::Json(task) = core_cancel_task(state.clone(), "", "owner-i", &task_id)
        .await
        .expect("cancel OK");
    assert_eq!(state_of(&task), "TASK_STATE_CANCELED");
    assert!(
        task["status"].get("message").is_none_or(|m| m.is_null()),
        "framework-committed CANCELED carries no message"
    );
    assert!(start.elapsed() >= state.runtime_config.cancel_handler_grace);

    await_dropped(probe_tx).await;
    await_registry_empty(&state).await;
    let terminals = terminal_events(&storage, "", &task_id).await;
    assert_eq!(
        terminals.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
        vec![TaskState::Canceled],
        "only the framework CANCELED persists"
    );
}

// ---------------------------------------------------------------------------
// An earlier interrupted commit is not an acknowledgement of the cancel: an
// executor that raised AUTH_REQUIRED, resumed and ignores the token is still
// aborted at grace.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn executor_that_resumed_after_interrupt_is_aborted_on_cancel() {
    let storage = InMemoryA2aStorage::new();
    let (tx, rx) = oneshot::channel();
    let state = state_on(
        storage.clone(),
        Arc::new(InterruptsThenIgnoresToken {
            probe: Mutex::new(Some(rx)),
        }),
    );
    let task_id = start_non_blocking(&state, "owner-r").await;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let t = storage
            .get_task("", &task_id, "owner-r", Some(0))
            .await
            .unwrap()
            .unwrap();
        if t.status().and_then(|s| s.state().ok()) == Some(TaskState::Working) {
            break;
        }
    }

    let axum::Json(task) = core_cancel_task(state.clone(), "", "owner-r", &task_id)
        .await
        .expect("cancel OK");
    assert_eq!(state_of(&task), "TASK_STATE_CANCELED");

    await_dropped(tx).await;
    await_registry_empty(&state).await;
}

// ---------------------------------------------------------------------------
// The abort fires at grace even when the CancelTask request future is
// dropped mid-grace; the CANCELED commit must not be lost with it.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn dropped_cancel_request_still_commits_canceled() {
    let storage = InMemoryA2aStorage::new();
    let (executor, probe_tx) = IgnoresToken::new();
    let state = state_on(storage.clone(), Arc::new(executor));
    let task_id = start_non_blocking(&state, "owner-d").await;
    let grace = state.runtime_config.cancel_handler_grace;

    let res = tokio::time::timeout(
        grace / 2,
        core_cancel_task(state.clone(), "", "owner-d", &task_id),
    )
    .await;
    assert!(res.is_err(), "request future dropped mid-grace");

    tokio::time::sleep(grace * 2).await;
    await_dropped(probe_tx).await;
    let terminals = terminal_events(&storage, "", &task_id).await;
    assert_eq!(
        terminals.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
        vec![TaskState::Canceled],
        "the task still reaches CANCELED"
    );
}

// ---------------------------------------------------------------------------
// Executor acknowledges through its sink and keeps cleaning up past the
// grace deadline: the handler returns the executor's CANCELED and the
// cleanup is not aborted.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn acknowledging_executor_cleanup_is_not_aborted_after_grace() {
    let storage = InMemoryA2aStorage::new();
    let cleaned_up = Arc::new(AtomicBool::new(false));
    let state = state_on(
        storage.clone(),
        Arc::new(AcksThenCleansUp {
            cleanup: Duration::from_secs(20),
            cleaned_up: cleaned_up.clone(),
        }),
    );
    let task_id = start_non_blocking(&state, "owner-k").await;

    let start = tokio::time::Instant::now();
    let axum::Json(task) = core_cancel_task(state.clone(), "", "owner-k", &task_id)
        .await
        .expect("cancel OK");
    assert_eq!(state_of(&task), "TASK_STATE_CANCELED");
    assert_eq!(status_text(&task).as_deref(), Some(ACK_REASON));
    assert!(start.elapsed() < state.runtime_config.cancel_handler_grace);

    tokio::time::sleep(state.runtime_config.cancel_handler_grace * 5).await;
    assert!(
        cleaned_up.load(Ordering::SeqCst),
        "cleanup after the executor's own terminal must run to completion"
    );
    assert_eq!(terminal_events(&storage, "", &task_id).await.len(), 1);
}

// ---------------------------------------------------------------------------
// Cross-instance abort: A's poller observes B's marker and aborts A's
// executor once A's grace elapses.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn cross_instance_cancel_aborts_executor_ignoring_token() {
    let storage = InMemoryA2aStorage::new();
    let (executor, probe_tx) = IgnoresToken::new();
    let state_a = state_on(storage.clone(), Arc::new(executor));
    let state_b = state_on(storage.clone(), Arc::new(NoopExecutor));
    let task_id = start_non_blocking(&state_a, "owner-x").await;

    let poll_interval = Duration::from_millis(50);
    let grace = state_a.runtime_config.cancel_handler_grace;
    let shutdown = CancellationToken::new();
    let poller = tokio::spawn(
        turul_a2a::server::in_flight::run_cross_instance_cancel_poller(
            state_a.in_flight.clone(),
            state_a.cancellation_supervisor.clone(),
            poll_interval,
            grace,
            shutdown.clone(),
        ),
    );

    let start = tokio::time::Instant::now();
    let axum::Json(task) = core_cancel_task(state_b.clone(), "", "owner-x", &task_id)
        .await
        .expect("cancel on B");
    assert_eq!(state_of(&task), "TASK_STATE_CANCELED");
    assert!(
        task["status"].get("message").is_none_or(|m| m.is_null()),
        "B committed the framework CANCELED"
    );

    await_dropped(probe_tx).await;
    assert!(
        start.elapsed() <= poll_interval + grace + Duration::from_millis(100),
        "A's executor must be dropped within poll interval + grace; took {:?}",
        start.elapsed()
    );
    await_registry_empty(&state_a).await;
    shutdown.cancel();
    poller.await.expect("poller exits on shutdown");
    assert_eq!(terminal_events(&storage, "", &task_id).await.len(), 1);
}

// ---------------------------------------------------------------------------
// Cancel-vs-complete, complete wins: the executor completes during the
// handler's grace wait; the handler returns COMPLETED and never writes
// CANCELED.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn cancel_vs_complete_complete_wins_during_grace() {
    let storage = InMemoryA2aStorage::new();
    let state = state_on(storage.clone(), Arc::new(CompletesOnCancel));
    let task_id = start_non_blocking(&state, "owner-c").await;

    let start = tokio::time::Instant::now();
    let axum::Json(task) = core_cancel_task(state.clone(), "", "owner-c", &task_id)
        .await
        .expect("a cancel resolved by another terminal is still a successful response");
    assert_eq!(state_of(&task), "TASK_STATE_COMPLETED");
    assert!(start.elapsed() < state.runtime_config.cancel_handler_grace);

    let terminals = terminal_events(&storage, "", &task_id).await;
    assert_eq!(
        terminals.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
        vec![TaskState::Completed],
        "only the executor's COMPLETED persists"
    );
}

// ---------------------------------------------------------------------------
// Blocking send in flight + CancelTask: both calls return the executor's
// CANCELED, the send through its yielded signal and the handler through
// its grace poll.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn blocking_send_in_flight_and_cancel_both_return_canceled() {
    let storage = InMemoryA2aStorage::new();
    let (executor, started) = AckCancel::new();
    let mut state = state_on(storage.clone(), Arc::new(executor));
    state.runtime_config.blocking_task_timeout = Duration::from_secs(10);

    let send_state = state.clone();
    let send = tokio::spawn(async move {
        let body = serde_json::json!({
            "message": {
                "messageId": uuid::Uuid::now_v7().to_string(),
                "role": "ROLE_USER",
                "parts": [{"text": "block"}],
            }
        })
        .to_string();
        let start = tokio::time::Instant::now();
        let result =
            core_send_message(send_state, "", "owner-b", None, &Default::default(), body).await;
        (result, start.elapsed())
    });

    let task_id = started.await.expect("executor started");
    let cancel_start = tokio::time::Instant::now();
    let axum::Json(cancelled) = core_cancel_task(state.clone(), "", "owner-b", &task_id)
        .await
        .expect("cancel OK");
    let cancel_elapsed = cancel_start.elapsed();

    let (send_result, send_elapsed) = send.await.expect("send task");
    let axum::Json(sent) = send_result.expect("blocking send OK");

    assert_eq!(state_of(&cancelled), "TASK_STATE_CANCELED");
    assert_eq!(status_text(&cancelled).as_deref(), Some(ACK_REASON));
    assert_eq!(state_of(&sent["task"]), "TASK_STATE_CANCELED");
    assert_eq!(status_text(&sent["task"]).as_deref(), Some(ACK_REASON));
    assert!(cancel_elapsed < state.runtime_config.cancel_handler_grace);
    assert!(
        send_elapsed < state.runtime_config.blocking_task_timeout,
        "blocking send must return on the cancel, not its own timeout; took {send_elapsed:?}"
    );
    assert_eq!(terminal_events(&storage, "", &task_id).await.len(), 1);
}

// ---------------------------------------------------------------------------
// Cancel-vs-cancel idempotency.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_vs_cancel_idempotency() {
    let (state, storage) = make_state();
    seed_task(&storage, "default", "owner-i", "t-idem").await;
    advance_to_working(&storage, "default", "owner-i", "t-idem").await;

    let state = with_fast_grace(state);

    let s1 = state.clone();
    let s2 = state.clone();
    let (r1, r2) = tokio::join!(
        core_cancel_task(s1, "default", "owner-i", "t-idem"),
        core_cancel_task(s2, "default", "owner-i", "t-idem"),
    );

    let axum::Json(r1) = r1.expect("first cancel must succeed");
    let axum::Json(r2) = r2.expect("second cancel must succeed (idempotent)");
    for r in [&r1, &r2] {
        assert_eq!(state_of(r), "TASK_STATE_CANCELED", "both return CANCELED");
    }

    let terminals = terminal_events(&storage, "default", "t-idem").await;
    assert_eq!(
        terminals.len(),
        1,
        "exactly one CANCELED event must land; got {terminals:?}"
    );
}

// ---------------------------------------------------------------------------
// Cancel on a terminal task: TaskNotCancelable for every terminal state
// (A2A v1.0.1 §3.1.5 — "already completed, failed, or canceled"), and the
// handler never attempts the marker write.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_on_every_terminal_state_is_not_cancelable_and_writes_no_marker() {
    let (state, storage, marker_writes) = spied_state();
    for terminal in [
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Canceled,
        TaskState::Rejected,
    ] {
        let task_id = format!("t-term-{terminal:?}");
        seed_task(&storage, "default", "owner-t", &task_id).await;
        advance_to_working(&storage, "default", "owner-t", &task_id).await;
        transition(&storage, "default", "owner-t", &task_id, terminal).await;

        match core_cancel_task(state.clone(), "default", "owner-t", &task_id).await {
            Err(A2aError::TaskNotCancelable { task_id: id }) => assert_eq!(id, task_id),
            other => panic!("{terminal:?}: expected TaskNotCancelable, got {other:?}"),
        }
    }
    assert_eq!(
        marker_writes.load(Ordering::SeqCst),
        0,
        "a terminal task must be rejected before the cancel marker is written"
    );
}

#[tokio::test]
async fn cancel_terminal_task_is_not_cancelable_on_every_transport() {
    let (state, storage) = make_state();
    for transport in transports() {
        for terminal in [
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::Rejected,
        ] {
            let task_id = format!("t-wire-{transport:?}-{terminal:?}");
            seed_task(&storage, "", "anonymous", &task_id).await;
            advance_to_working(&storage, "", "anonymous", &task_id).await;
            transition(&storage, "", "anonymous", &task_id, terminal).await;

            let outcome = cancel_over(transport, &state, &task_id, None).await;
            assert_eq!(
                outcome,
                Err("TASK_NOT_CANCELABLE".to_string()),
                "{transport:?} / {terminal:?}"
            );
        }
    }
}

/// A2A v1.0.1 §3.1.5 lists "already canceled" as not cancelable: the
/// first cancel succeeds, the second is rejected — on every transport.
#[tokio::test]
async fn second_cancel_after_canceled_is_not_cancelable_on_every_transport() {
    let (state, storage) = make_state();
    let state = with_fast_grace(state);
    for transport in transports() {
        let task_id = format!("t-twice-{transport:?}");
        seed_task(&storage, "", "anonymous", &task_id).await;
        advance_to_working(&storage, "", "anonymous", &task_id).await;

        let first = cancel_over(transport, &state, &task_id, None)
            .await
            .unwrap_or_else(|e| panic!("{transport:?}: first cancel failed: {e}"));
        assert_eq!(state_of(&first), "TASK_STATE_CANCELED", "{transport:?}");
        assert_eq!(first["id"], task_id.as_str(), "{transport:?}");

        let second = cancel_over(transport, &state, &task_id, None).await;
        assert_eq!(
            second,
            Err("TASK_NOT_CANCELABLE".to_string()),
            "{transport:?}"
        );
    }
}

#[tokio::test]
async fn cancel_unknown_task_is_not_found_on_every_transport() {
    let (state, _storage) = make_state();
    for transport in transports() {
        let outcome = cancel_over(transport, &state, "t-does-not-exist", None).await;
        assert_eq!(outcome, Err("TASK_NOT_FOUND".to_string()), "{transport:?}");
    }
}

/// Interrupted states are not terminal: INPUT_REQUIRED and AUTH_REQUIRED
/// tasks are cancelable.
#[tokio::test]
async fn cancel_on_interrupted_states_succeeds() {
    let (state, storage) = make_state();
    let state = with_fast_grace(state);
    for interrupted in [TaskState::InputRequired, TaskState::AuthRequired] {
        let task_id = format!("t-int-{interrupted:?}");
        seed_task(&storage, "default", "owner-n", &task_id).await;
        advance_to_working(&storage, "default", "owner-n", &task_id).await;
        transition(&storage, "default", "owner-n", &task_id, interrupted).await;

        let axum::Json(task) = core_cancel_task(state.clone(), "default", "owner-n", &task_id)
            .await
            .unwrap_or_else(|e| panic!("{interrupted:?}: cancel failed: {e:?}"));
        assert_eq!(state_of(&task), "TASK_STATE_CANCELED", "{interrupted:?}");
    }
}

/// `CancelTaskRequest.metadata` carries no framework-defined behaviour
/// (a2a.proto: "a flexible key-value map for passing additional context
/// or parameters"). Every transport accepts it and cancels exactly as
/// without it.
#[tokio::test]
async fn cancel_metadata_is_accepted_on_every_transport() {
    let (state, storage) = make_state();
    let state = with_fast_grace(state);
    let metadata = serde_json::json!({"reason": "user-initiated", "attempt": 1});
    for transport in transports() {
        let task_id = format!("t-meta-{transport:?}");
        seed_task(&storage, "", "anonymous", &task_id).await;
        advance_to_working(&storage, "", "anonymous", &task_id).await;

        let task = cancel_over(transport, &state, &task_id, Some(metadata.clone()))
            .await
            .unwrap_or_else(|e| panic!("{transport:?}: cancel with metadata failed: {e}"));
        assert_eq!(state_of(&task), "TASK_STATE_CANCELED", "{transport:?}");
    }
}

// ---------------------------------------------------------------------------
// Cancel by wrong owner: 404 TaskNotFound (anti-enumeration), no marker.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_by_wrong_owner_returns_404_and_sets_no_marker() {
    let (state, storage, marker_writes) = spied_state();
    seed_task(&storage, "default", "owner-real", "t-wo").await;
    advance_to_working(&storage, "default", "owner-real", "t-wo").await;

    match core_cancel_task(state, "default", "owner-impostor", "t-wo").await {
        Err(A2aError::TaskNotFound { task_id }) => assert_eq!(task_id, "t-wo"),
        other => panic!("expected TaskNotFound, got {other:?}"),
    }
    assert_eq!(marker_writes.load(Ordering::SeqCst), 0);
    assert!(
        !storage
            .supervisor_get_cancel_requested("default", "t-wo")
            .await
            .unwrap(),
        "the owner's live task must not carry a cancel marker"
    );
}

// ---------------------------------------------------------------------------
// Marker write is conditional on non-terminal.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn marker_write_conditional_on_non_terminal() {
    let (_state, storage) = make_state();
    seed_task(&storage, "default", "owner-c", "t-cond").await;

    storage
        .set_cancel_requested("default", "t-cond", "owner-c")
        .await
        .expect("marker set on SUBMITTED");
    assert!(
        storage
            .supervisor_get_cancel_requested("default", "t-cond")
            .await
            .unwrap()
    );

    storage
        .update_task_status_with_events(
            "default",
            "t-cond",
            "owner-c",
            TaskStatus::new(TaskState::Working),
            vec![],
        )
        .await
        .unwrap();
    storage
        .update_task_status_with_events(
            "default",
            "t-cond",
            "owner-c",
            TaskStatus::new(TaskState::Completed),
            vec![],
        )
        .await
        .unwrap();

    match storage
        .set_cancel_requested("default", "t-cond", "owner-c")
        .await
    {
        Err(A2aStorageError::TerminalState { task_id, state }) => {
            assert_eq!(task_id, "t-cond");
            assert_eq!(state, TaskState::Completed);
        }
        other => panic!("expected TerminalState on terminal marker write, got {other:?}"),
    }

    assert!(
        !storage
            .supervisor_get_cancel_requested("default", "t-cond")
            .await
            .unwrap()
    );
}

// ---------------------------------------------------------------------------
// Cross-instance poll load: one batch query per tenant per tick,
// independent of the number of in-flight tasks.
// ---------------------------------------------------------------------------

struct CountingSupervisor {
    inner: InMemoryA2aStorage,
    list_calls: Mutex<Vec<String>>,
    get_calls: AtomicUsize,
}

#[async_trait]
impl A2aCancellationSupervisor for CountingSupervisor {
    fn backend_name(&self) -> &'static str {
        "counting-supervisor"
    }
    async fn supervisor_get_cancel_requested(
        &self,
        tenant: &str,
        task_id: &str,
    ) -> Result<bool, A2aStorageError> {
        self.get_calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .supervisor_get_cancel_requested(tenant, task_id)
            .await
    }
    async fn supervisor_list_cancel_requested(
        &self,
        tenant: &str,
        task_ids: &[String],
    ) -> Result<Vec<String>, A2aStorageError> {
        self.list_calls.lock().unwrap().push(tenant.to_string());
        self.inner
            .supervisor_list_cancel_requested(tenant, task_ids)
            .await
    }
    async fn supervisor_list_executor_held_tasks(
        &self,
    ) -> Result<Vec<turul_a2a::storage::ExecutorHeldTask>, A2aStorageError> {
        self.inner.supervisor_list_executor_held_tasks().await
    }
}

#[tokio::test(start_paused = true)]
async fn cross_instance_poll_issues_one_query_per_tenant_per_tick() {
    const TASKS_PER_TENANT: usize = 50;
    const TENANTS: [&str; 2] = ["tenant-a", "tenant-b"];

    let (state, storage) = make_state();
    let mut tokens = Vec::new();
    for tenant in TENANTS {
        for i in 0..TASKS_PER_TENANT {
            let task_id = format!("load-{i}");
            seed_task(&storage, tenant, "owner-l", &task_id).await;
            advance_to_working(&storage, tenant, "owner-l", &task_id).await;
            if i % 3 == 0 {
                storage
                    .set_cancel_requested(tenant, &task_id, "owner-l")
                    .await
                    .unwrap();
            }
            tokens.push((i % 3 == 0, install_in_flight(&state, tenant, &task_id)));
        }
    }
    assert_eq!(state.in_flight.len(), TENANTS.len() * TASKS_PER_TENANT);

    let supervisor = Arc::new(CountingSupervisor {
        inner: storage.clone(),
        list_calls: Mutex::new(Vec::new()),
        get_calls: AtomicUsize::new(0),
    });
    let interval = Duration::from_secs(1);
    let shutdown = CancellationToken::new();
    let poller = tokio::spawn(
        turul_a2a::server::in_flight::run_cross_instance_cancel_poller(
            state.in_flight.clone(),
            supervisor.clone(),
            interval,
            Duration::from_secs(5),
            shutdown.clone(),
        ),
    );

    // Three ticks fire at 1s, 2s and 3s of virtual time.
    tokio::time::sleep(interval * 3 + interval / 2).await;
    shutdown.cancel();
    poller.await.expect("poller exits on shutdown");

    let mut calls = supervisor.list_calls.lock().unwrap().clone();
    assert_eq!(
        calls.len(),
        3 * TENANTS.len(),
        "one batch query per tenant per tick for {} in-flight tasks, got {calls:?}",
        state.in_flight.len()
    );
    calls.sort();
    calls.dedup();
    assert_eq!(calls, TENANTS.to_vec());
    assert_eq!(supervisor.get_calls.load(Ordering::SeqCst), 0);

    for (marked, token) in &tokens {
        assert_eq!(
            token.is_cancelled(),
            *marked,
            "exactly the marked tasks' tokens are tripped"
        );
    }
}

// ---------------------------------------------------------------------------
// Supervisor panic: the sentinel removes the registry entry and aborts
// the executor; the server stays healthy; the task is left non-terminal
// and a later CancelTask resolves it through the orphan path.
//
// The production supervisor body has no panic site of its own, so the
// test runs a supervisor-shaped task (sentinel held, then panic) over a
// real task and a real in-flight handle.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervisor_panic_cleans_up_and_task_stays_cancelable() {
    let (state, storage) = make_state();
    let state = with_fast_grace(state);
    let (tenant, owner, task_id) = ("", "anonymous", "t-panic");
    seed_task(&storage, tenant, owner, task_id).await;
    advance_to_working(&storage, tenant, owner, task_id).await;
    seed_task(&storage, tenant, owner, "t-bystander").await;

    let (probe_tx, probe_rx) = oneshot::channel::<()>();
    let executor = tokio::spawn(async move {
        let _probe = probe_rx;
        std::future::pending::<()>().await
    });
    let (yielded_tx, _yielded_rx) = oneshot::channel::<Task>();
    let handle = Arc::new(InFlightHandle::new(
        CancellationToken::new(),
        yielded_tx,
        executor,
    ));
    let key = (tenant.to_string(), task_id.to_string());
    state
        .in_flight
        .try_insert(key.clone(), handle.clone())
        .unwrap();

    let registry = state.in_flight.clone();
    let sup_key = key.clone();
    let supervisor = tokio::spawn(async move {
        let _sentinel = SupervisorSentinel::new(registry, sup_key, handle);
        panic!("supervisor panicked mid-run");
    });
    let join = supervisor.await;
    assert!(join.is_err_and(|e| e.is_panic()), "supervisor panicked");

    assert!(
        state.in_flight.get(&key).is_none(),
        "registry entry removed"
    );
    await_dropped(probe_tx).await;

    let router = build_router(state.clone());
    let get = |id: &str| {
        Request::get(format!("/tasks/{id}"))
            .header("a2a-version", "1.0")
            .body(Body::empty())
            .unwrap()
    };

    let resp = router.clone().oneshot(get("t-bystander")).await.unwrap();
    assert_eq!(
        resp.status(),
        200,
        "server keeps serving unrelated requests"
    );

    let resp = router.clone().oneshot(get(task_id)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        state_of(&snapshot),
        "TASK_STATE_WORKING",
        "nothing committed a terminal for the aborted executor"
    );

    let task = cancel_over(Transport::Http, &state, task_id, None)
        .await
        .expect("orphan cancel succeeds");
    assert_eq!(state_of(&task), "TASK_STATE_CANCELED");
    assert!(task["status"].get("message").is_none_or(|m| m.is_null()));
}

// ---------------------------------------------------------------------------
// SSE subscriber sees terminal CANCELED. The subscriber is attached (its
// Task snapshot read) before the cancel; it then receives the CANCELED
// status update as its last event, and the persisted CANCELED sequence is
// above every earlier event for the task.
// ---------------------------------------------------------------------------

/// Read the next complete SSE `data:` payload from `body`, or `None`
/// when the stream ends.
async fn next_sse_data(body: &mut Body, buf: &mut String) -> Option<serde_json::Value> {
    loop {
        if let Some(end) = buf.find("\n\n") {
            let event: String = buf.drain(..end + 2).collect();
            let data: String = event
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(str::trim_start)
                .collect();
            if data.is_empty() {
                continue; // keep-alive comment
            }
            return Some(serde_json::from_str(&data).expect("SSE data is JSON"));
        }
        let frame = body.frame().await?.expect("body frame");
        if let Ok(bytes) = frame.into_data() {
            buf.push_str(std::str::from_utf8(&bytes).expect("utf-8 SSE"));
        }
    }
}

#[tokio::test]
async fn streaming_subscriber_sees_terminal_canceled() {
    let (state, storage) = make_state();
    let state = with_fast_grace(state);
    let (tenant, owner, task_id) = ("", "anonymous", "t-sse-cancel");
    seed_task(&storage, tenant, owner, task_id).await;
    advance_to_working(&storage, tenant, owner, task_id).await;

    let subscribe_req = Request::get(format!("/tasks/{task_id}:subscribe"))
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = build_router(state.clone())
        .oneshot(subscribe_req)
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "subscribe must 200 on non-terminal");
    let mut body = resp.into_body();
    let mut buf = String::new();

    let first = next_sse_data(&mut body, &mut buf)
        .await
        .expect("stream opens with the Task");
    assert_eq!(state_of(&first["task"]), "TASK_STATE_WORKING");

    let prior_seq = storage
        .get_events_after(tenant, task_id, 0)
        .await
        .unwrap()
        .last()
        .map(|(seq, _)| *seq)
        .expect("WORKING event persisted");

    let axum::Json(task) = core_cancel_task(state.clone(), tenant, owner, task_id)
        .await
        .expect("cancel OK");
    assert_eq!(state_of(&task), "TASK_STATE_CANCELED");

    let (terminal, end) = tokio::time::timeout(Duration::from_secs(5), async {
        let terminal = next_sse_data(&mut body, &mut buf).await;
        let end = next_sse_data(&mut body, &mut buf).await;
        (terminal, end)
    })
    .await
    .expect("stream must deliver the terminal and close");
    let terminal = terminal.expect("CANCELED status update delivered");
    assert_eq!(
        state_of(&terminal["statusUpdate"]),
        "TASK_STATE_CANCELED",
        "next event after the snapshot is the CANCELED status update: {terminal}"
    );
    assert!(end.is_none(), "stream closes after the terminal: {end:?}");

    let after_prior = storage
        .get_events_after(tenant, task_id, prior_seq)
        .await
        .unwrap();
    assert_eq!(after_prior.len(), 1, "{after_prior:?}");
    let (cancel_seq, cancel_event) = &after_prior[0];
    assert!(*cancel_seq > prior_seq);
    assert_eq!(cancel_event.task_state(), Some(TaskState::Canceled));
}

// The trait split keeps `supervisor_get_cancel_requested` off
// `Arc<dyn A2aTaskStorage>`; the compile_fail doctest on
// `A2aCancellationSupervisor` (crates/turul-a2a/src/storage/traits.rs)
// enforces it.
