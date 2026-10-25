//! Executor liveness: the lease that lets a peer fail the tasks of a dead
//! instance, and the stall watchdog that fails and aborts an executor
//! that stops making progress inside a live one.
//!
//! Lease timestamps come from the wall clock, so the lease tests run on
//! the real clock with durations of a few hundred milliseconds. The
//! stall tests only use tokio timers and run on the paused clock.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tokio::sync::oneshot;
use tower::ServiceExt;

use turul_a2a::A2aServer;
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::storage::{
    A2aAtomicStore, A2aCancellationSupervisor, A2aEventStore, A2aStorageError, A2aTaskStorage,
    ExecutorHeldTask, InMemoryA2aStorage,
};
use turul_a2a::streaming::{StatusUpdatePayload, StreamEvent};
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};

/// HTTP requests without auth middleware run as this owner on the
/// default (empty) tenant.
const TENANT: &str = "";
const OWNER: &str = "anonymous";

const LEASE_REASON: &str =
    "task failed: the server running its executor stopped renewing the executor's lease";

fn card() -> turul_a2a_proto::AgentCard {
    AgentCardBuilder::new("Liveness Agent", "1.0.0")
        .description("Agent for executor liveness tests")
        .url("http://localhost", "JSONRPC", "1.0")
        .default_input_modes(vec!["text/plain"])
        .default_output_modes(vec!["text/plain"])
        .build()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Executors
// ---------------------------------------------------------------------------

/// Runs until the process (or runtime) goes away, ignoring the token.
struct Forever {
    started: Mutex<Option<oneshot::Sender<String>>>,
}

impl Forever {
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
impl AgentExecutor for Forever {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        if let Some(tx) = self.started.lock().unwrap().take() {
            let _ = tx.send(task.id().to_string());
        }
        std::future::pending::<()>().await;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        card()
    }
}

/// Waits for the test to open a gate, then completes.
struct Gated {
    gate: Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait]
impl AgentExecutor for Gated {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let gate = self.gate.lock().unwrap().take().expect("gate");
        let _ = gate.await;
        ctx.events.complete(None).await?;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        card()
    }
}

/// First turn: records whether the task carries a lease while WORKING,
/// then raises INPUT_REQUIRED. Later turns record the same and complete.
struct LeaseProbe {
    storage: InMemoryA2aStorage,
    seen: Arc<Mutex<Vec<bool>>>,
    turns: Mutex<u32>,
}

#[async_trait]
impl AgentExecutor for LeaseProbe {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let has_lease = self
            .storage
            .supervisor_list_expired_executor_leases(i64::MAX)
            .await
            .unwrap()
            .iter()
            .any(|t| t.task_id == task.id());
        let live = !self
            .storage
            .supervisor_list_expired_executor_leases(chrono::Utc::now().timestamp_micros())
            .await
            .unwrap()
            .iter()
            .any(|t| t.task_id == task.id());
        self.seen.lock().unwrap().push(has_lease && live);
        let turn = {
            let mut turns = self.turns.lock().unwrap();
            *turns += 1;
            *turns
        };
        if turn == 1 {
            ctx.events.require_input(None).await?;
        } else {
            ctx.events.complete(None).await?;
        }
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        card()
    }
}

/// Holds a drop probe in its future and never makes progress.
struct Silent {
    probe: Mutex<Option<oneshot::Receiver<()>>>,
}

impl Silent {
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
impl AgentExecutor for Silent {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let _probe_held_in_future = self.probe.lock().unwrap().take();
        std::future::pending::<()>().await;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        card()
    }
}

/// Does silent work in `step`-long pauses, calling `heartbeat` after
/// each, for `total`, then completes.
struct Heartbeats {
    step: Duration,
    total: Duration,
}

#[async_trait]
impl AgentExecutor for Heartbeats {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let mut elapsed = Duration::ZERO;
        while elapsed < self.total {
            tokio::time::sleep(self.step).await;
            elapsed += self.step;
            ctx.events.heartbeat();
        }
        ctx.events.complete(None).await?;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        card()
    }
}

/// Raises INPUT_REQUIRED and then idles, still running.
struct InterruptsThenIdles {
    idle: Duration,
}

#[async_trait]
impl AgentExecutor for InterruptsThenIdles {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        ctx.events.require_input(None).await?;
        tokio::time::sleep(self.idle).await;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        card()
    }
}

/// Raises INPUT_REQUIRED, resumes WORKING on its own, then goes silent.
struct InterruptsResumesThenSilent;

#[async_trait]
impl AgentExecutor for InterruptsResumesThenSilent {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        ctx.events.require_input(None).await?;
        ctx.events.set_status(TaskState::Working, None).await?;
        std::future::pending::<()>().await;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        card()
    }
}

// ---------------------------------------------------------------------------
// Supervisor spy: records every lease renewal the server issues, with the
// task's state at the moment of the call.
// ---------------------------------------------------------------------------

/// `(task ids, expires_at_micros, state of the first id at call time)`.
type Renewal = (Vec<String>, i64, Option<TaskState>);

#[derive(Clone)]
struct LeaseSpy {
    inner: InMemoryA2aStorage,
    renewals: Arc<Mutex<Vec<Renewal>>>,
    /// Task ids of every call that cleared leases (`None` expiry).
    clears: Arc<Mutex<Vec<Vec<String>>>>,
}

impl LeaseSpy {
    fn new(inner: InMemoryA2aStorage) -> Self {
        Self {
            inner,
            renewals: Arc::new(Mutex::new(Vec::new())),
            clears: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn renewals(&self) -> Vec<Renewal> {
        self.renewals.lock().unwrap().clone()
    }

    fn clears(&self) -> Vec<Vec<String>> {
        self.clears.lock().unwrap().clone()
    }

    /// Poll until a clear naming `task_id` has been issued.
    async fn wait_for_clear(&self, task_id: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !self.clears().iter().any(|c| c == &[task_id.to_string()]) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "lease of {task_id} was never cleared"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[async_trait]
impl A2aCancellationSupervisor for LeaseSpy {
    fn backend_name(&self) -> &'static str {
        A2aTaskStorage::backend_name(&self.inner)
    }
    async fn supervisor_get_cancel_requested(
        &self,
        tenant: &str,
        task_id: &str,
    ) -> Result<bool, A2aStorageError> {
        self.inner
            .supervisor_get_cancel_requested(tenant, task_id)
            .await
    }
    async fn supervisor_list_cancel_requested(
        &self,
        tenant: &str,
        task_ids: &[String],
    ) -> Result<Vec<String>, A2aStorageError> {
        self.inner
            .supervisor_list_cancel_requested(tenant, task_ids)
            .await
    }
    async fn supervisor_renew_executor_leases(
        &self,
        tenant: &str,
        task_ids: &[String],
        expires_at_micros: Option<i64>,
    ) -> Result<(), A2aStorageError> {
        let state = match task_ids.first() {
            Some(id) => self
                .inner
                .get_task(tenant, id, OWNER, Some(0))
                .await
                .unwrap()
                .and_then(|t| t.status().and_then(|s| s.state().ok())),
            None => None,
        };
        match expires_at_micros {
            Some(expiry) => self
                .renewals
                .lock()
                .unwrap()
                .push((task_ids.to_vec(), expiry, state)),
            None => self.clears.lock().unwrap().push(task_ids.to_vec()),
        }
        self.inner
            .supervisor_renew_executor_leases(tenant, task_ids, expires_at_micros)
            .await
    }
    async fn supervisor_list_expired_executor_leases(
        &self,
        now_micros: i64,
    ) -> Result<Vec<ExecutorHeldTask>, A2aStorageError> {
        self.inner
            .supervisor_list_expired_executor_leases(now_micros)
            .await
    }
}

/// Atomic store whose status commits to CANCELED fail, as a framework
/// terminal commit lost to a storage error would.
#[derive(Clone)]
struct RefusesCanceled {
    inner: InMemoryA2aStorage,
}

#[async_trait]
impl A2aAtomicStore for RefusesCanceled {
    fn backend_name(&self) -> &'static str {
        A2aAtomicStore::backend_name(&self.inner)
    }
    async fn create_task_with_events(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
        events: Vec<StreamEvent>,
    ) -> Result<(Task, Vec<u64>), A2aStorageError> {
        self.inner
            .create_task_with_events(tenant, owner, task, events)
            .await
    }
    async fn update_task_status_with_events(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        new_status: TaskStatus,
        events: Vec<StreamEvent>,
    ) -> Result<(Task, Vec<u64>), A2aStorageError> {
        if new_status.state().ok() == Some(TaskState::Canceled) {
            return Err(A2aStorageError::DatabaseError("injected".into()));
        }
        self.inner
            .update_task_status_with_events(tenant, task_id, owner, new_status, events)
            .await
    }
    async fn update_task_with_events(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
        events: Vec<StreamEvent>,
    ) -> Result<Vec<u64>, A2aStorageError> {
        self.inner
            .update_task_with_events(tenant, owner, task, events)
            .await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn send_body(task_id: Option<&str>, return_immediately: bool) -> serde_json::Value {
    let mut message = serde_json::json!({
        "messageId": uuid::Uuid::now_v7().to_string(),
        "role": "ROLE_USER",
        "parts": [{"text": "go", "mediaType": "text/plain"}],
    });
    if let Some(id) = task_id {
        message["taskId"] = id.into();
    }
    serde_json::json!({
        "message": message,
        "configuration": {"returnImmediately": return_immediately},
    })
}

async fn request(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("A2A-Version", "1.0")
        .header("content-type", "application/json")
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn send(
    router: &axum::Router,
    task_id: Option<&str>,
    return_immediately: bool,
) -> serde_json::Value {
    let (status, body) = request(
        router,
        "POST",
        "/message:send",
        Some(send_body(task_id, return_immediately)),
    )
    .await;
    assert_eq!(status, 200, "send failed: {body}");
    body["task"].clone()
}

async fn get_task(storage: &InMemoryA2aStorage, task_id: &str) -> Task {
    storage
        .get_task(TENANT, task_id, OWNER, None)
        .await
        .unwrap()
        .expect("task exists")
}

fn state_of(task: &Task) -> TaskState {
    task.status().and_then(|s| s.state().ok()).unwrap()
}

fn status_text(task: &Task) -> Option<String> {
    let status = serde_json::to_value(task).unwrap()["status"].clone();
    let parts = status["message"]["parts"].as_array()?;
    Some(
        parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn status_role(task: &Task) -> Option<String> {
    serde_json::to_value(task).unwrap()["status"]["message"]["role"]
        .as_str()
        .map(str::to_string)
}

/// Poll until the task reaches `want`; panics after `within`.
async fn wait_for_state(
    storage: &InMemoryA2aStorage,
    task_id: &str,
    want: TaskState,
    within: Duration,
) -> Task {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let task = get_task(storage, task_id).await;
        if state_of(&task) == want {
            return task;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "task {task_id} never reached {want:?}; is {:?}",
            state_of(&task)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn terminal_states(storage: &InMemoryA2aStorage, task_id: &str) -> Vec<TaskState> {
    storage
        .get_events_after(TENANT, task_id, 0)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|(_, e)| e.task_state())
        .filter(|s| turul_a2a_types::state_machine::is_terminal(*s))
        .collect()
}

fn status_event(task_id: &str, state: TaskState) -> StreamEvent {
    StreamEvent::StatusUpdate {
        status_update: StatusUpdatePayload {
            task_id: task_id.to_string(),
            context_id: format!("ctx-{task_id}"),
            status: serde_json::to_value(TaskStatus::new(state)).unwrap(),
        },
    }
}

/// Seed a task and drive it to `state` through the atomic store, as a
/// server that later died would have left it.
async fn seed(storage: &InMemoryA2aStorage, id: &str, state: TaskState) {
    let task =
        Task::new(id, TaskStatus::new(TaskState::Submitted)).with_context_id(format!("ctx-{id}"));
    storage.create_task(TENANT, OWNER, task).await.unwrap();
    let path: &[TaskState] = match state {
        TaskState::Submitted => &[],
        TaskState::Working => &[TaskState::Working],
        other => &[TaskState::Working, other],
    };
    for next in path {
        storage
            .update_task_status_with_events(
                TENANT,
                id,
                OWNER,
                TaskStatus::new(*next),
                vec![status_event(id, *next)],
            )
            .await
            .unwrap();
    }
}

async fn give_lease(storage: &InMemoryA2aStorage, id: &str, expires_at_micros: i64) {
    storage
        .supervisor_renew_executor_leases(TENANT, &[id.to_string()], Some(expires_at_micros))
        .await
        .unwrap();
}

fn now_micros() -> i64 {
    chrono::Utc::now().timestamp_micros()
}

// ---------------------------------------------------------------------------
// Lease written before WORKING
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lease_is_written_before_working_for_new_task_and_continuation() {
    let storage = InMemoryA2aStorage::new();
    let spy = LeaseSpy::new(storage.clone());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = A2aServer::builder()
        .executor(LeaseProbe {
            storage: storage.clone(),
            seen: seen.clone(),
            turns: Mutex::new(0),
        })
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage.clone())
        .cancellation_supervisor(spy.clone())
        .executor_lease(Duration::from_secs(60))
        .build()
        .unwrap()
        .into_router();

    let first = send(&router, None, false).await;
    assert_eq!(first["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
    let task_id = first["id"].as_str().unwrap().to_string();
    let second = send(&router, Some(&task_id), false).await;
    assert_eq!(second["status"]["state"], "TASK_STATE_COMPLETED");

    let renewals = spy.renewals();
    assert_eq!(renewals.len(), 2, "one lease write per turn: {renewals:?}");
    assert_eq!(
        renewals[0].2,
        Some(TaskState::Submitted),
        "a new task's lease is written before its WORKING commit"
    );
    assert_eq!(
        renewals[1].2,
        Some(TaskState::InputRequired),
        "a continuation's lease is written before its WORKING commit"
    );
    assert!(renewals.iter().all(|r| r.0 == vec![task_id.clone()]));
    assert_eq!(
        *seen.lock().unwrap(),
        vec![true, true],
        "the task carries a live lease while WORKING on every turn"
    );
}

#[tokio::test]
async fn no_lease_is_written_when_disabled() {
    let storage = InMemoryA2aStorage::new();
    let spy = LeaseSpy::new(storage.clone());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = A2aServer::builder()
        .executor(LeaseProbe {
            storage: storage.clone(),
            seen: seen.clone(),
            turns: Mutex::new(0),
        })
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage.clone())
        .cancellation_supervisor(spy.clone())
        .build()
        .unwrap()
        .into_router();

    let first = send(&router, None, false).await;
    let task_id = first["id"].as_str().unwrap().to_string();
    send(&router, Some(&task_id), false).await;

    assert!(spy.renewals().is_empty(), "{:?}", spy.renewals());
    assert_eq!(*seen.lock().unwrap(), vec![false, false]);
    assert!(
        storage
            .supervisor_list_expired_executor_leases(i64::MAX)
            .await
            .unwrap()
            .is_empty(),
        "no task carries a lease"
    );
}

// ---------------------------------------------------------------------------
// Renewal, served through into_router
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lease_expiry_advances_while_the_executor_runs_and_stops_after_it_exits() {
    let ttl = Duration::from_millis(300);
    let storage = InMemoryA2aStorage::new();
    let spy = LeaseSpy::new(storage.clone());
    let (gate_tx, gate_rx) = oneshot::channel();
    let router = A2aServer::builder()
        .executor(Gated {
            gate: Mutex::new(Some(gate_rx)),
        })
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage.clone())
        .cancellation_supervisor(spy.clone())
        .executor_lease(ttl)
        .build()
        .unwrap()
        .into_router();

    let task = send(&router, None, true).await;
    let task_id = task["id"].as_str().unwrap().to_string();

    // The turn-start write plus renewals every ttl / 3.
    tokio::time::sleep(ttl * 2).await;
    let running = spy.renewals();
    assert!(
        running.len() >= 4,
        "renewals while the executor runs: {running:?}"
    );
    assert!(
        running.windows(2).all(|w| w[0].1 < w[1].1),
        "each renewal pushes the expiry further out: {running:?}"
    );
    assert!(running.iter().all(|r| r.0 == vec![task_id.clone()]));

    gate_tx.send(()).unwrap();
    wait_for_state(
        &storage,
        &task_id,
        TaskState::Completed,
        Duration::from_secs(5),
    )
    .await;
    // A renewal already in flight when the executor exits may still land.
    tokio::time::sleep(ttl).await;
    let settled = spy.renewals().len();
    tokio::time::sleep(ttl * 2).await;
    assert_eq!(
        spy.renewals().len(),
        settled,
        "renewals stop once the executor has left the registry"
    );
}

#[tokio::test]
async fn lease_is_cleared_when_the_executor_exits_after_raising_input_required() {
    let storage = InMemoryA2aStorage::new();
    let spy = LeaseSpy::new(storage.clone());
    let router = A2aServer::builder()
        .executor(InterruptsThenIdles {
            idle: Duration::ZERO,
        })
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage.clone())
        .cancellation_supervisor(spy.clone())
        .executor_lease(Duration::from_secs(60))
        .build()
        .unwrap()
        .into_router();

    let task = send(&router, None, false).await;
    assert_eq!(task["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
    spy.wait_for_clear(task["id"].as_str().unwrap()).await;
}

#[tokio::test]
async fn aborted_executor_keeps_its_lease_and_the_reaper_fails_the_task() {
    let ttl = Duration::from_millis(300);
    let storage = InMemoryA2aStorage::new();
    let (executor, started) = Forever::new();
    let router = A2aServer::builder()
        .executor(executor)
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(RefusesCanceled {
            inner: storage.clone(),
        })
        .cancellation_supervisor(storage.clone())
        .executor_lease(ttl)
        .cancel_handler_grace(Duration::from_millis(100))
        .cancel_abort_grace(Duration::from_millis(20))
        .build()
        .unwrap()
        .into_router();

    let task = send(&router, None, true).await;
    let task_id = task["id"].as_str().unwrap().to_string();
    started.await.unwrap();

    // The executor ignores the token and is aborted; the framework's
    // CANCELED commit then fails, leaving the task WORKING.
    let _ = request(
        &router,
        "POST",
        &format!("/tasks/{task_id}:cancel"),
        Some(serde_json::json!({})),
    )
    .await;

    // Clearing the lease on abort would leave the task executor-held and
    // unreapable; the kept lease expires and the reaper fails it.
    let failed = wait_for_state(
        &storage,
        &task_id,
        TaskState::Failed,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(status_text(&failed).as_deref(), Some(LEASE_REASON));
}

#[tokio::test]
async fn continuation_on_a_server_without_the_lease_is_not_failed_by_a_lease_peer() {
    let ttl = Duration::from_millis(200);
    let storage = InMemoryA2aStorage::new();
    let spy = LeaseSpy::new(storage.clone());
    let leased = A2aServer::builder()
        .executor(InterruptsThenIdles {
            idle: Duration::ZERO,
        })
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage.clone())
        .cancellation_supervisor(spy.clone())
        .executor_lease(ttl)
        .build()
        .unwrap()
        .into_router();
    let first = send(&leased, None, false).await;
    let task_id = first["id"].as_str().unwrap().to_string();
    spy.wait_for_clear(&task_id).await;

    let (executor, _started) = Forever::new();
    let unleased = A2aServer::builder()
        .executor(executor)
        .storage(storage.clone())
        .build()
        .unwrap()
        .into_router();
    let resumed = send(&unleased, Some(&task_id), true).await;
    assert_eq!(resumed["status"]["state"], "TASK_STATE_WORKING");

    // Several ttls of the lease-enabled peer's reaper.
    tokio::time::sleep(ttl * 5).await;
    assert_eq!(
        state_of(&get_task(&storage, &task_id).await),
        TaskState::Working
    );
}

// ---------------------------------------------------------------------------
// Reaper, served through into_router
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reaper_fails_expired_lease_task_and_skips_everything_else() {
    let ttl = Duration::from_millis(200);
    let storage = InMemoryA2aStorage::new();
    let expired = now_micros() - 1_000_000;

    seed(&storage, "t-working", TaskState::Working).await;
    seed(&storage, "t-submitted", TaskState::Submitted).await;
    seed(&storage, "t-unleased", TaskState::Working).await;
    seed(&storage, "t-input", TaskState::InputRequired).await;
    seed(&storage, "t-auth", TaskState::AuthRequired).await;
    seed(&storage, "t-completed", TaskState::Completed).await;
    seed(&storage, "t-live", TaskState::Working).await;
    for id in [
        "t-working",
        "t-submitted",
        "t-input",
        "t-auth",
        "t-completed",
    ] {
        give_lease(&storage, id, expired).await;
    }
    give_lease(&storage, "t-live", now_micros() + 3_600_000_000).await;

    let (executor, _started) = Forever::new();
    let _router = A2aServer::builder()
        .executor(executor)
        .storage(storage.clone())
        .executor_lease(ttl)
        .build()
        .unwrap()
        .into_router();

    for id in ["t-working", "t-submitted"] {
        let failed = wait_for_state(&storage, id, TaskState::Failed, Duration::from_secs(5)).await;
        assert_eq!(status_text(&failed).as_deref(), Some(LEASE_REASON));
        assert_eq!(status_role(&failed).as_deref(), Some("ROLE_AGENT"));
        assert_eq!(
            terminal_states(&storage, id).await,
            vec![TaskState::Failed],
            "exactly one FAILED status update"
        );
    }

    // Several more reaper ticks: nothing else changes.
    tokio::time::sleep(ttl * 3).await;
    for (id, state) in [
        ("t-unleased", TaskState::Working),
        ("t-input", TaskState::InputRequired),
        ("t-auth", TaskState::AuthRequired),
        ("t-completed", TaskState::Completed),
        ("t-live", TaskState::Working),
    ] {
        assert_eq!(state_of(&get_task(&storage, id).await), state, "{id}");
    }
    assert_eq!(
        terminal_states(&storage, "t-completed").await,
        vec![TaskState::Completed]
    );
}

#[tokio::test]
async fn reaper_clears_the_lease_of_the_task_it_fails() {
    let storage = InMemoryA2aStorage::new();
    let spy = LeaseSpy::new(storage.clone());
    seed(&storage, "t-dead", TaskState::Working).await;
    give_lease(&storage, "t-dead", now_micros() - 1_000_000).await;

    let (executor, _started) = Forever::new();
    let _router = A2aServer::builder()
        .executor(executor)
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage.clone())
        .cancellation_supervisor(spy.clone())
        .executor_lease(Duration::from_millis(200))
        .build()
        .unwrap()
        .into_router();

    wait_for_state(
        &storage,
        "t-dead",
        TaskState::Failed,
        Duration::from_secs(5),
    )
    .await;
    spy.wait_for_clear("t-dead").await;
}

// ---------------------------------------------------------------------------
// Two servers, one store: a live executor is never reaped by its peer; a
// dead instance's task is failed within two ttls.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn peer_never_reaps_a_live_executor_and_fails_a_dead_instances_task() {
    let ttl = Duration::from_secs(1);
    let storage = InMemoryA2aStorage::new();

    // Server A runs on its own runtime so that dropping the runtime kills
    // its executor, renewal loop and registry together, as a process
    // exit would.
    let rt_a = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (executor, started) = Forever::new();
    let storage_a = storage.clone();
    rt_a.spawn(async move {
        let router = A2aServer::builder()
            .executor(executor)
            .storage(storage_a)
            .executor_lease(ttl)
            .build()
            .unwrap()
            .into_router();
        send(&router, None, true).await;
        // Keep A's server state alive for the life of the runtime.
        std::future::pending::<()>().await;
    });
    let task_id = started.await.expect("executor started on A");

    let (b_executor, _unused) = Forever::new();
    let bound = A2aServer::builder()
        .executor(b_executor)
        .storage(storage.clone())
        .executor_lease(ttl)
        .bind(([127, 0, 0, 1], 0u16))
        .build()
        .unwrap()
        .bind()
        .await
        .unwrap();
    let base = format!("http://{}", bound.local_addr());
    let serving = tokio::spawn(bound.run());
    common::wait_http_ready(&base).await;

    let http = reqwest::Client::new();
    let get = || async {
        http.get(format!("{base}/tasks/{task_id}"))
            .header("A2A-Version", "1.0")
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()
    };

    // B's reaper runs for several ttls while A's executor is alive.
    let watch_until = tokio::time::Instant::now() + ttl * 3;
    while tokio::time::Instant::now() < watch_until {
        assert_eq!(
            get().await["status"]["state"],
            "TASK_STATE_WORKING",
            "a live executor's task must not be reaped by a peer"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A dies: its runtime, executor and renewals are gone.
    rt_a.shutdown_background();
    let died = tokio::time::Instant::now();
    let failed = loop {
        let task = get().await;
        if task["status"]["state"] == "TASK_STATE_FAILED" {
            break task;
        }
        assert!(
            died.elapsed() < ttl * 2 + Duration::from_secs(1),
            "B did not fail the dead instance's task in time: {task}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        failed["status"]["message"]["parts"][0]["text"],
        LEASE_REASON
    );
    assert_eq!(
        terminal_states(&storage, &task_id).await,
        vec![TaskState::Failed]
    );

    serving.abort();
    let _ = serving.await;
}

// ---------------------------------------------------------------------------
// Builder validation
// ---------------------------------------------------------------------------

fn builder() -> turul_a2a::server::A2aServerBuilder {
    let (executor, _started) = Forever::new();
    A2aServer::builder().executor(executor)
}

#[test]
fn build_without_a_runtime_rejects_lease_and_stall_timeout() {
    for configured in [
        builder().executor_lease(Duration::from_secs(1)),
        builder().executor_stall_timeout(Duration::from_secs(1)),
    ] {
        let err = configured.build().err().expect("no runtime: build fails");
        assert!(
            err.to_string().contains("tokio runtime"),
            "unexpected error: {err}"
        );
    }
    builder()
        .build()
        .expect("a server without liveness options needs no runtime");
}

#[tokio::test]
async fn build_rejects_zero_lease_ttl_and_zero_stall_timeout() {
    let err = builder()
        .executor_lease(Duration::ZERO)
        .build()
        .err()
        .expect("zero ttl rejected");
    assert!(err.to_string().contains("executor_lease"), "{err}");
    let err = builder()
        .executor_stall_timeout(Duration::ZERO)
        .build()
        .err()
        .expect("zero stall timeout rejected");
    assert!(err.to_string().contains("executor_stall_timeout"), "{err}");

    builder()
        .executor_lease(Duration::from_secs(1))
        .executor_stall_timeout(Duration::from_secs(1))
        .build()
        .expect("non-zero options inside a runtime build");
}

// ---------------------------------------------------------------------------
// Stall watchdog (paused clock)
// ---------------------------------------------------------------------------

const STALL: Duration = Duration::from_millis(400);

fn stall_router(
    executor: impl AgentExecutor + 'static,
    storage: &InMemoryA2aStorage,
) -> axum::Router {
    A2aServer::builder()
        .executor(executor)
        .storage(storage.clone())
        .executor_stall_timeout(STALL)
        .timeout_abort_grace(Duration::from_millis(100))
        .blocking_task_timeout(Duration::from_secs(60))
        .build()
        .unwrap()
        .into_router()
}

#[tokio::test(start_paused = true)]
async fn silent_executor_is_failed_and_then_aborted() {
    let storage = InMemoryA2aStorage::new();
    let (executor, probe) = Silent::new();
    let router = stall_router(executor, &storage);

    let task = send(&router, None, true).await;
    let task_id = task["id"].as_str().unwrap().to_string();

    let failed = wait_for_state(
        &storage,
        &task_id,
        TaskState::Failed,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        status_text(&failed).as_deref(),
        Some("task failed: its executor made no progress for 400ms")
    );
    assert_eq!(status_role(&failed).as_deref(), Some("ROLE_AGENT"));
    assert_eq!(
        terminal_states(&storage, &task_id).await,
        vec![TaskState::Failed]
    );

    let mut probe = probe;
    tokio::time::timeout(Duration::from_secs(5), probe.closed())
        .await
        .expect("the stalled executor was never aborted");
}

#[tokio::test(start_paused = true)]
async fn blocking_send_on_a_silent_executor_returns_the_stall_failure() {
    let storage = InMemoryA2aStorage::new();
    let (executor, _probe) = Silent::new();
    let router = stall_router(executor, &storage);

    let start = tokio::time::Instant::now();
    let task = send(&router, None, false).await;
    assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
    assert_eq!(
        task["status"]["message"]["parts"][0]["text"],
        "task failed: its executor made no progress for 400ms"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "the send returns on the stall failure, not its own timeout; took {:?}",
        start.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn executor_that_heartbeats_is_not_failed() {
    let storage = InMemoryA2aStorage::new();
    let router = stall_router(
        Heartbeats {
            step: Duration::from_millis(150),
            total: Duration::from_secs(3),
        },
        &storage,
    );

    let task = send(&router, None, false).await;
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
}

#[tokio::test(start_paused = true)]
async fn executor_idle_after_input_required_is_not_failed() {
    let storage = InMemoryA2aStorage::new();
    let router = stall_router(
        InterruptsThenIdles {
            idle: Duration::from_secs(5),
        },
        &storage,
    );

    let task = send(&router, None, false).await;
    assert_eq!(task["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
    let task_id = task["id"].as_str().unwrap().to_string();

    tokio::time::sleep(STALL * 5).await;
    assert_eq!(
        state_of(&get_task(&storage, &task_id).await),
        TaskState::InputRequired
    );
}

#[tokio::test(start_paused = true)]
async fn executor_that_resumed_working_after_input_required_and_went_silent_is_failed() {
    let storage = InMemoryA2aStorage::new();
    let router = stall_router(InterruptsResumesThenSilent, &storage);

    let task = send(&router, None, false).await;
    assert_eq!(task["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
    let task_id = task["id"].as_str().unwrap().to_string();

    let failed = wait_for_state(
        &storage,
        &task_id,
        TaskState::Failed,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        status_text(&failed).as_deref(),
        Some("task failed: its executor made no progress for 400ms")
    );
}

#[tokio::test(start_paused = true)]
async fn cancelled_executor_is_not_failed_by_the_watchdog() {
    let storage = InMemoryA2aStorage::new();
    let (executor, _probe) = Silent::new();
    let router = A2aServer::builder()
        .executor(executor)
        .storage(storage.clone())
        .executor_stall_timeout(STALL)
        .cancel_handler_grace(STALL * 3)
        .cancel_abort_grace(STALL * 10)
        .build()
        .unwrap()
        .into_router();

    let task = send(&router, None, true).await;
    let task_id = task["id"].as_str().unwrap().to_string();

    // The cancel trips the token well inside the stall timeout and holds
    // the outcome until the handler's grace expires, past the timeout.
    let (status, cancelled) = request(
        &router,
        "POST",
        &format!("/tasks/{task_id}:cancel"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(cancelled["status"]["state"], "TASK_STATE_CANCELED");
    assert_eq!(
        terminal_states(&storage, &task_id).await,
        vec![TaskState::Canceled],
        "the watchdog wrote no second terminal"
    );
}
