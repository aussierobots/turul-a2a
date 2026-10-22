//! Send-mode coverage.
//!
//! These tests drive `core_send_message` end-to-end against an
//! in-memory backend with a live EventSink and the spawn-and-track
//! machinery. They prove:
//!
//! - A long-running sink-driven executor completes correctly through
//!   the blocking send path.
//! - Direct-task-mutation executors that never touch `ctx.events`
//!   still reach a durable terminal via the framework's
//!   post-execute detection rule — and that terminal goes through
//!   the CAS-guarded `update_task_status_with_events`, NOT the old
//!   `update_task_with_events` bypass.
//! - An executor returning `Err` yields a `FAILED` task with the error
//!   text, not a silently-stuck task.
//! - `REJECTED` via `sink.reject(...)` persists as `TASK_STATE_REJECTED`
//!   distinct from FAILED.
//! - Blocking send's two-deadline timeout: the cooperative path returns
//!   inside the cooperative window, the abort fallback returns at the
//!   hard deadline with FAILED, and a terminal committed by another
//!   writer during the window wins over the framework's FAILED via
//!   `TerminalStateAlreadySet`.
//! - Blocking send returns as soon as the task reaches an interrupted
//!   state (INPUT_REQUIRED / AUTH_REQUIRED), on HTTP and JSON-RPC.
//! - `returnImmediately = true` returns the task before the executor
//!   reaches terminal.
//!
//! Timing assertions run on tokio's paused clock, so each bound names
//! the code path taken rather than racing the wall clock.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::middleware::MiddlewareStack;
use turul_a2a::router::{AppState, core_send_message};
use turul_a2a::server::RuntimeConfig;
use turul_a2a::server::in_flight::InFlightRegistry;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a::streaming::TaskEventBroker;
use turul_a2a_types::{Artifact, Message, Part, Role, Task, TaskState};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn make_state(executor: Arc<dyn AgentExecutor>) -> AppState {
    state_on(InMemoryA2aStorage::new(), executor)
}

fn state_on(storage: InMemoryA2aStorage, executor: Arc<dyn AgentExecutor>) -> AppState {
    let storage = Arc::new(storage);
    AppState {
        executor,
        task_storage: storage.clone(),
        push_storage: storage.clone(),
        event_store: storage.clone(),
        atomic_store: storage.clone(),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(MiddlewareStack::new(vec![])),
        runtime_config: RuntimeConfig::default(),
        in_flight: Arc::new(InFlightRegistry::new()),
        cancellation_supervisor: storage,
        push_delivery_store: None,
        push_dispatcher: None,
        durable_executor_queue: None,
    }
}

fn fast_timeout_state(
    executor: Arc<dyn AgentExecutor>,
    soft: Duration,
    grace: Duration,
) -> AppState {
    let mut state = make_state(executor);
    state.runtime_config.blocking_task_timeout = soft;
    state.runtime_config.timeout_abort_grace = grace;
    state
}

fn send_request_body(text: &str) -> String {
    serde_json::json!({
        "message": {
            "messageId": uuid::Uuid::now_v7().to_string(),
            "role": "ROLE_USER",
            "parts": [{"text": text, "mediaType": "text/plain"}],
        }
    })
    .to_string()
}

fn send_request_body_return_immediately(text: &str) -> String {
    serde_json::json!({
        "message": {
            "messageId": uuid::Uuid::now_v7().to_string(),
            "role": "ROLE_USER",
            "parts": [{"text": text, "mediaType": "text/plain"}],
        },
        "configuration": {
            "returnImmediately": true,
        }
    })
    .to_string()
}

fn task_from_response(json: serde_json::Value) -> Task {
    let task_v = json.get("task").expect("response has task").clone();
    serde_json::from_value(task_v).expect("task deserializes")
}

fn agent_text(text: &str) -> Message {
    Message::new(
        uuid::Uuid::now_v7().to_string(),
        Role::Agent,
        vec![Part::text(text)],
    )
}

// ---------------------------------------------------------------------------
// Long-running sink-driven executor completes
// ---------------------------------------------------------------------------

struct SinkDrivenCompleter {
    delay_ms: u64,
}

#[async_trait]
impl AgentExecutor for SinkDrivenCompleter {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        // Emit an artifact after a small delay, then complete.
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        let artifact = Artifact::new("a-progress", vec![Part::text("intermediate")]);
        ctx.events.emit_artifact(artifact, false, false).await?;
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        ctx.events.complete(Some(agent_text("done"))).await?;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

#[tokio::test(start_paused = true)]
async fn blocking_send_long_running_sink_driven_completes() {
    let state = make_state(Arc::new(SinkDrivenCompleter { delay_ms: 50 }));
    let start = tokio::time::Instant::now();
    let axum::Json(v) = core_send_message(
        state.clone(),
        "",
        "owner-1",
        None,
        &Default::default(),
        send_request_body("hi"),
    )
    .await
    .expect("blocking send completes");
    let elapsed = start.elapsed();
    let task = task_from_response(v);
    assert!(
        elapsed >= Duration::from_millis(100),
        "blocking send must wait out both executor delays: {elapsed:?}"
    );
    assert_eq!(
        task.status().unwrap().state().unwrap(),
        TaskState::Completed
    );
    assert_eq!(task.artifacts().len(), 1, "emitted artifact should persist");
}

// ---------------------------------------------------------------------------
// Direct-task-mutation executor still works
// (detection rule routes the terminal through CAS — no
// update_task_with_events bypass).
// ---------------------------------------------------------------------------

struct DirectMutationCompleter;

#[async_trait]
impl AgentExecutor for DirectMutationCompleter {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        // Never touches ctx.events — appends artifact + mutates status.
        task.append_artifact(Artifact::new("a-legacy", vec![Part::text("legacy output")]));
        task.complete();
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// Direct-task-mutation executor that mutates an existing artifact
/// in place (the streaming-chunk pattern expressed through `&mut
/// Task`). The detection rule must observe the part-count growth
/// and emit an `append=true` artifact update so the persisted task
/// carries the extended parts — not only the initial snapshot.
struct DirectMutationChunkExtender;

#[async_trait]
impl AgentExecutor for DirectMutationChunkExtender {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        // Add the initial artifact (counts as a new ID at a new index).
        task.append_artifact(Artifact::new("a-chunk", vec![Part::text("chunk-1")]));
        // Then extend the SAME artifact with more parts — direct
        // mutation of the existing artifact's parts list.
        task.merge_artifact(
            Artifact::new("a-chunk", vec![Part::text("chunk-2")]),
            true,
            false,
        );
        task.merge_artifact(
            Artifact::new("a-chunk", vec![Part::text("chunk-3")]),
            true,
            true,
        );
        task.complete();
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

#[tokio::test]
async fn blocking_send_direct_task_mutation_extended_artifact_is_persisted() {
    let state = make_state(Arc::new(DirectMutationChunkExtender));
    let axum::Json(v) = core_send_message(
        state.clone(),
        "",
        "owner-ext",
        None,
        &Default::default(),
        send_request_body("x"),
    )
    .await
    .expect("blocking send on direct-mutation chunk extender");
    let task = task_from_response(v);
    assert_eq!(
        task.status().unwrap().state().unwrap(),
        TaskState::Completed
    );
    // All three parts must appear on the single artifact — the
    // detection rule's index diff catches the part-count growth and
    // emits the tail as append=true, so storage preserves chunks 2
    // and 3 instead of only the initial chunk-1.
    assert_eq!(
        task.artifacts().len(),
        1,
        "append=true chunks must extend the same artifact, not create duplicates"
    );
    assert_eq!(
        task.artifacts()[0].parts.len(),
        3,
        "all three chunks must be persisted"
    );
}

#[tokio::test]
async fn blocking_send_direct_task_mutation_still_terminates_via_cas() {
    let state = make_state(Arc::new(DirectMutationCompleter));
    let axum::Json(v) = core_send_message(
        state.clone(),
        "",
        "owner-legacy",
        None,
        &Default::default(),
        send_request_body("x"),
    )
    .await
    .expect("blocking send on direct-task-mutation executor");
    let task = task_from_response(v);
    assert_eq!(
        task.status().unwrap().state().unwrap(),
        TaskState::Completed
    );
    // The detection-rule artifact publish went through the sink —
    // it should be in the persisted task.
    assert_eq!(task.artifacts().len(), 1);
    assert_eq!(task.artifacts()[0].artifact_id, "a-legacy");
}

// ---------------------------------------------------------------------------
// Executor returning Err → framework FAILED
// ---------------------------------------------------------------------------

struct ErrReturner;

#[async_trait]
impl AgentExecutor for ErrReturner {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        Err(A2aError::Internal(
            "boom — upstream service unavailable".into(),
        ))
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

#[tokio::test]
async fn blocking_send_executor_err_commits_failed() {
    let state = make_state(Arc::new(ErrReturner));
    let axum::Json(v) = core_send_message(
        state.clone(),
        "",
        "owner-err",
        None,
        &Default::default(),
        send_request_body("x"),
    )
    .await
    .expect("blocking send returns the FAILED task, not an Err");
    let task = task_from_response(v);
    assert_eq!(task.status().unwrap().state().unwrap(), TaskState::Failed);
    // The framework's agent-authored failure message carries the
    // executor's error text.
    let pb_status = task.status().unwrap();
    let msg = pb_status
        .as_proto()
        .message
        .as_ref()
        .expect("status message present");
    let msg_text: String = msg
        .parts
        .iter()
        .filter_map(|p| match &p.content {
            Some(turul_a2a_proto::part::Content::Text(t)) => Some(t.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        msg_text.contains("boom"),
        "FAILED message should include executor error text: {msg_text}"
    );
}

// ---------------------------------------------------------------------------
// `sink.reject` → TASK_STATE_REJECTED (distinct
// from FAILED on the wire — proto enum value 7, not 4).
// ---------------------------------------------------------------------------

struct PolicyRejecter;

#[async_trait]
impl AgentExecutor for PolicyRejecter {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        ctx.events
            .reject(Some("policy: not permitted".into()))
            .await?;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

#[tokio::test]
async fn blocking_send_executor_rejects_via_sink_reject() {
    let state = make_state(Arc::new(PolicyRejecter));
    let axum::Json(v) = core_send_message(
        state.clone(),
        "",
        "owner-r",
        None,
        &Default::default(),
        send_request_body("x"),
    )
    .await
    .expect("blocking send returns the REJECTED task");
    let task = task_from_response(v);
    let state_val = task.status().unwrap().state().unwrap();
    assert_eq!(
        state_val,
        TaskState::Rejected,
        "REJECTED is distinct from FAILED"
    );

    // Wire-level check: proto enum integer value 7 for REJECTED.
    let wire: turul_a2a_proto::TaskState =
        task.status().unwrap().as_proto().state.try_into().unwrap();
    assert_eq!(wire as i32, 7, "proto integer value matches spec");
}

// ---------------------------------------------------------------------------
// Cooperative two-deadline: executor observes
// cancellation within grace window and emits CANCELED.
// ---------------------------------------------------------------------------

struct CooperativeCancelLoop;

#[async_trait]
impl AgentExecutor for CooperativeCancelLoop {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        // Loop checking cancellation every 20ms for up to 5s.
        for _ in 0..250 {
            if ctx.cancellation.is_cancelled() {
                ctx.events
                    .cancelled(Some("cooperative: token observed".into()))
                    .await?;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        ctx.events
            .fail(Some("cancellation never arrived".into()))
            .await?;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

#[tokio::test(start_paused = true)]
async fn blocking_send_two_deadline_cooperative_returns_canceled() {
    let soft = Duration::from_millis(200);
    let grace = Duration::from_millis(500);
    let state = fast_timeout_state(Arc::new(CooperativeCancelLoop), soft, grace);
    let start = tokio::time::Instant::now();
    let axum::Json(v) = core_send_message(
        state,
        "",
        "owner-coop",
        None,
        &Default::default(),
        send_request_body("x"),
    )
    .await
    .expect("blocking send returns within grace window");
    let elapsed = start.elapsed();
    let task = task_from_response(v);
    assert_eq!(task.status().unwrap().state().unwrap(), TaskState::Canceled);
    assert!(
        elapsed >= soft && elapsed < soft + grace,
        "cooperative path returns inside the cooperative window, got {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Abort fallback: executor ignores cancellation
// and would sleep past the hard deadline. Framework force-commits
// FAILED and aborts the spawned JoinHandle.
// ---------------------------------------------------------------------------

struct IgnoresCancellation;

const IGNORES_CANCELLATION_SLEEP: Duration = Duration::from_secs(10);

#[async_trait]
impl AgentExecutor for IgnoresCancellation {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        // Deliberately ignore cancellation. Sleep past hard deadline.
        tokio::time::sleep(IGNORES_CANCELLATION_SLEEP).await;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// Drop-probe executor. Takes a oneshot `Sender<()>` out of the
/// struct and holds it inside the execute future. When the future
/// drops — whether via normal return, error, or abort — the Sender
/// drops, and the test's Receiver resolves (with `RecvError`, which
/// we don't care to distinguish). This is cleaner than
/// `Notify::notify_waiters()` because it does not require the
/// waiter to be polled/registered at the moment of fire.
struct IgnoresCancellationWithDropProbe {
    tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

#[async_trait]
impl AgentExecutor for IgnoresCancellationWithDropProbe {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        // Move the Sender into the future body. The Sender's Drop is
        // what the test waits on: its `Receiver::await` resolves
        // (Err) when this Sender drops.
        let _tx_held_in_future = self.tx.lock().expect("tx mutex").take();
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

#[tokio::test(start_paused = true)]
async fn blocking_send_two_deadline_abort_fallback_returns_failed() {
    let soft = Duration::from_millis(150);
    let grace = Duration::from_millis(200);
    let state = fast_timeout_state(Arc::new(IgnoresCancellation), soft, grace);
    let start = tokio::time::Instant::now();
    let axum::Json(v) = core_send_message(
        state,
        "",
        "owner-abort",
        None,
        &Default::default(),
        send_request_body("x"),
    )
    .await
    .expect("blocking send force-commits FAILED");
    let elapsed = start.elapsed();
    let task = task_from_response(v);
    assert_eq!(task.status().unwrap().state().unwrap(), TaskState::Failed);
    assert!(
        elapsed >= soft + grace && elapsed < IGNORES_CANCELLATION_SLEEP,
        "FAILED is committed at the hard deadline, without waiting for the executor; got {elapsed:?}"
    );
    let pb_status = task.status().unwrap();
    let msg = pb_status
        .as_proto()
        .message
        .as_ref()
        .expect("timeout message present");
    let msg_text: String = msg
        .parts
        .iter()
        .filter_map(|p| match &p.content {
            Some(turul_a2a_proto::part::Content::Text(t)) => Some(t.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        msg_text.contains("timed out") || msg_text.contains("hard deadline"),
        "FAILED message should mention timeout: {msg_text}"
    );
}

/// Regression gate: the framework hard-timeout path MUST actually
/// abort a cancellation-ignoring executor — not just return FAILED to
/// the client while the zombie future keeps running on the tokio
/// runtime. The abort capability is carried on a cloneable
/// `AbortHandle`, decoupled from the supervisor's `JoinHandle`
/// ownership, so the timeout path can reach the task regardless of
/// whether the supervisor already took the JoinHandle.
///
/// Verified via a `Drop`-flag guard inside the executor body: if the
/// abort reaches the task, the `tokio::time::sleep(30s)` yield point
/// wakes and the future drops, flipping the flag.
#[tokio::test(start_paused = true)]
async fn blocking_send_hard_timeout_actually_aborts_executor() {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let executor = Arc::new(IgnoresCancellationWithDropProbe {
        tx: std::sync::Mutex::new(Some(tx)),
    });
    let state = fast_timeout_state(
        executor,
        Duration::from_millis(150),
        Duration::from_millis(150),
    );

    let axum::Json(_v) = core_send_message(
        state,
        "",
        "owner-abort-probe",
        None,
        &Default::default(),
        send_request_body("x"),
    )
    .await
    .expect("blocking send force-commits FAILED");

    // The Sender lives inside the executor future. When the future
    // drops (via abort), the Sender drops, and `rx` resolves —
    // either Ok(()) on explicit send (we never send) or Err(_) on
    // Sender-dropped. Both outcomes prove the future was dropped.
    // On the paused clock the 2s timeout fires long before the
    // executor's 30s sleep: if abort doesn't reach the task, the
    // Sender is still live inside a zombie future and the timeout wins.
    let drop_observed = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect(
            "executor future was never dropped — abort did not reach the task. \
             The hard-timeout path is leaving zombie executors behind.",
        );
    // `drop_observed` is Ok(()) on explicit send (never happens in this
    // executor) or Err(RecvError) on Sender-drop. Either outcome proves
    // the future was dropped; we do not need to distinguish.
    let _ = drop_observed;
}

// ---------------------------------------------------------------------------
// The hard deadline's FAILED loses the terminal CAS. The
// executor ignores cancellation; during the cooperative window a
// CancelTask on another instance (sharing the store) force-commits
// CANCELED. That commit does not fire this instance's yielded signal,
// so the blocking send reaches the hard deadline, its FAILED write gets
// `TerminalStateAlreadySet`, and it returns the persisted CANCELED. The
// executor overran the hard deadline either way, so it is aborted.
// ---------------------------------------------------------------------------

/// Reports when it observes the cancellation token, then ignores it.
/// Holds `alive` inside its future so the test can see it dropped.
struct ObservesThenIgnoresCancel {
    observed: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
    alive: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[async_trait]
impl AgentExecutor for ObservesThenIgnoresCancel {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let _alive = self.alive.lock().expect("alive mutex").take();
        ctx.cancellation.cancelled().await;
        if let Some(tx) = self.observed.lock().expect("observed mutex").take() {
            let _ = tx.send(task.id().to_string());
        }
        std::future::pending::<()>().await;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

#[tokio::test(start_paused = true)]
async fn blocking_send_hard_deadline_loses_cas_to_concurrent_cancel_returns_canceled() {
    let soft = Duration::from_millis(80);
    let grace = Duration::from_millis(80);
    let storage = InMemoryA2aStorage::new();
    let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
    let (mut alive_tx, alive_rx) = tokio::sync::oneshot::channel::<()>();
    let executor = Arc::new(ObservesThenIgnoresCancel {
        observed: std::sync::Mutex::new(Some(observed_tx)),
        alive: std::sync::Mutex::new(Some(alive_rx)),
    });
    let mut state_a = state_on(storage.clone(), executor);
    state_a.runtime_config.blocking_task_timeout = soft;
    state_a.runtime_config.timeout_abort_grace = grace;

    // Instance B shares the store; its cancel grace ends inside A's
    // cooperative window.
    let mut state_b = state_on(storage.clone(), Arc::new(SlowCompleter));
    state_b.runtime_config.cancel_handler_grace = grace / 4;
    state_b.runtime_config.cancel_handler_poll_interval = grace / 8;

    let start = tokio::time::Instant::now();
    let send = tokio::spawn(async move {
        core_send_message(
            state_a,
            "",
            "owner-race",
            None,
            &Default::default(),
            send_request_body("x"),
        )
        .await
    });

    let task_id = observed_rx.await.expect("soft deadline trips the token");
    let axum::Json(cancelled) =
        turul_a2a::router::core_cancel_task(state_b, "", "owner-race", &task_id)
            .await
            .expect("cancel on B force-commits CANCELED");
    assert_eq!(cancelled["status"]["state"], "TASK_STATE_CANCELED");
    assert!(
        start.elapsed() < soft + grace,
        "B's CANCELED lands before A's hard deadline"
    );

    let axum::Json(v) = send
        .await
        .expect("send task")
        .expect("blocking send returns the persisted terminal");
    let elapsed = start.elapsed();
    let task = task_from_response(v);
    assert_eq!(
        task.status().unwrap().state().unwrap(),
        TaskState::Canceled,
        "the earlier CANCELED wins; the hard deadline's FAILED is discarded"
    );
    assert!(
        elapsed >= soft + grace,
        "the send returned through the hard-deadline path, got {elapsed:?}"
    );

    use turul_a2a::storage::A2aEventStore;
    let terminals: Vec<TaskState> = storage
        .get_events_after("", &task_id, 0)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|(_, e)| e.task_state())
        .filter(|s| turul_a2a_types::state_machine::is_terminal(*s))
        .collect();
    assert_eq!(
        terminals,
        vec![TaskState::Canceled],
        "no FAILED ever persists"
    );

    // The executor never returns on its own; only the hard-deadline abort
    // drops its future. On the paused clock the timeout is a hang guard.
    tokio::time::timeout(Duration::from_secs(2), alive_tx.closed())
        .await
        .expect("executor that overran the hard deadline must be aborted");
}

// ---------------------------------------------------------------------------
// `returnImmediately = true` returns a non-terminal task
// ---------------------------------------------------------------------------

struct SlowCompleter;

const SLOW_COMPLETER_DELAY: Duration = Duration::from_millis(300);

#[async_trait]
impl AgentExecutor for SlowCompleter {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        tokio::time::sleep(SLOW_COMPLETER_DELAY).await;
        ctx.events.complete(None).await?;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

#[tokio::test(start_paused = true)]
async fn non_blocking_send_return_immediately_returns_before_terminal() {
    let state = make_state(Arc::new(SlowCompleter));
    let start = tokio::time::Instant::now();
    let axum::Json(v) = core_send_message(
        state.clone(),
        "",
        "owner-ni",
        None,
        &Default::default(),
        send_request_body_return_immediately("x"),
    )
    .await
    .expect("non-blocking send returns fast");
    let elapsed = start.elapsed();
    let task = task_from_response(v);
    assert!(
        elapsed < SLOW_COMPLETER_DELAY,
        "returnImmediately=true must not block for the executor: {elapsed:?}"
    );
    let state_val = task.status().unwrap().state().unwrap();
    assert!(
        matches!(state_val, TaskState::Submitted | TaskState::Working),
        "returned task should be pre-terminal, got {state_val:?}"
    );

    // Poll to verify the executor eventually reaches COMPLETED in the
    // background (proves the spawn-and-track lifecycle worked).
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let persisted = state
            .task_storage
            .get_task("", task.id(), "owner-ni", None)
            .await
            .unwrap()
            .unwrap();
        if persisted.status().unwrap().state().unwrap() == TaskState::Completed {
            return;
        }
    }
    panic!("task never reached COMPLETED");
}

// ---------------------------------------------------------------------------
// Blocking send returns on an interrupted state (A2A v1.0.1 §3.2.2: a
// blocking send MUST wait until the task reaches a terminal state or an
// interrupted state, INPUT_REQUIRED / AUTH_REQUIRED). The executor stays
// alive after the interrupt, so only the interrupt can end the wait; on
// the paused clock a send that waited for anything else would reach the
// blocking timeout and come back FAILED.
// ---------------------------------------------------------------------------

/// Raises an interrupted state through the sink, then stays running.
struct InterruptsAndWaits {
    state: TaskState,
}

#[async_trait]
impl AgentExecutor for InterruptsAndWaits {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        match self.state {
            TaskState::InputRequired => {
                ctx.events
                    .require_input(Some(agent_text("which file?")))
                    .await?;
            }
            TaskState::AuthRequired => {
                ctx.events
                    .require_auth(Some(agent_text("sign in to continue")))
                    .await?;
            }
            other => panic!("not an interrupted state: {other:?}"),
        }
        std::future::pending::<()>().await;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// Blocking `SendMessage` over the HTTP+JSON or JSON-RPC binding; returns
/// the `task` from the response.
async fn blocking_send_over(state: AppState, jsonrpc: bool) -> serde_json::Value {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let send: serde_json::Value = serde_json::from_str(&send_request_body("help")).unwrap();
    let (path, body) = if jsonrpc {
        let rpc = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": send,
        });
        ("/jsonrpc", rpc)
    } else {
        ("/message:send", send)
    };
    let req = http::Request::post(path)
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = turul_a2a::router::build_router(state)
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let response = if jsonrpc { &v["result"] } else { &v };
    response
        .get("task")
        .unwrap_or_else(|| panic!("response carries a task: {v}"))
        .clone()
}

#[tokio::test(start_paused = true)]
async fn blocking_send_returns_on_interrupted_state_over_http_and_jsonrpc() {
    for interrupted in [TaskState::InputRequired, TaskState::AuthRequired] {
        for jsonrpc in [false, true] {
            let state = make_state(Arc::new(InterruptsAndWaits { state: interrupted }));
            let timeout = state.runtime_config.blocking_task_timeout;
            let start = tokio::time::Instant::now();
            let task = blocking_send_over(state, jsonrpc).await;
            let elapsed = start.elapsed();
            let binding = if jsonrpc { "JSON-RPC" } else { "HTTP" };

            let wire_state = match interrupted {
                TaskState::InputRequired => "TASK_STATE_INPUT_REQUIRED",
                _ => "TASK_STATE_AUTH_REQUIRED",
            };
            assert_eq!(
                task["status"]["state"], wire_state,
                "{binding}: blocking send must return the interrupted task: {task}"
            );
            assert!(
                task["status"]["message"]["parts"][0]["text"].is_string(),
                "{binding}: the interrupt's message is part of the latest status: {task}"
            );
            assert!(
                elapsed < timeout,
                "{binding}: returned on the interrupt, not the blocking timeout ({elapsed:?})"
            );
        }
    }
}
