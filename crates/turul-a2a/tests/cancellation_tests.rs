//! Cancellation propagation (ADR-012) tests.
//!
//! Covers router and storage-level cancel behaviour: same-instance
//! token trip, owner-scoped marker writes, cross-instance marker
//! supervisor reads, and the `CancelTask` handler's grace-wait +
//! forced-CANCELED CAS path. Executor-driven cancellation races that
//! require the spawn-and-track send path are called out inline and
//! covered by `tests/send_mode_tests.rs`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::middleware::MiddlewareStack;
use turul_a2a::router::{core_cancel_task, AppState};
use turul_a2a::server::in_flight::{InFlightHandle, InFlightRegistry};
use turul_a2a::server::RuntimeConfig;
use turul_a2a::storage::{A2aCancellationSupervisor, A2aTaskStorage, InMemoryA2aStorage};
use turul_a2a::streaming::{StreamEvent, TaskEventBroker};
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

struct NoopExecutor;

#[async_trait]
impl AgentExecutor for NoopExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// Build an AppState wrapping a single InMemoryA2aStorage instance. Uses
/// default runtime config — override on the returned state where a test
/// needs faster grace / polling.
fn make_state() -> (AppState, InMemoryA2aStorage) {
    let storage = InMemoryA2aStorage::new();
    let state = AppState {
        executor: Arc::new(NoopExecutor),
        task_storage: Arc::new(storage.clone()),
        push_storage: Arc::new(storage.clone()),
        event_store: Arc::new(storage.clone()),
        atomic_store: Arc::new(storage.clone()),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(MiddlewareStack::new(vec![])),
        runtime_config: RuntimeConfig::default(),
        in_flight: Arc::new(InFlightRegistry::new()),
        cancellation_supervisor: Arc::new(storage.clone()),
        push_delivery_store: None,
    };
    (state, storage)
}

/// Shorten grace / poll-interval for tests that want to exercise the
/// framework-forced-CANCELED path quickly.
fn with_fast_grace(mut state: AppState) -> AppState {
    state.runtime_config.cancel_handler_grace = Duration::from_millis(200);
    state.runtime_config.cancel_handler_poll_interval = Duration::from_millis(10);
    state.runtime_config.cross_instance_cancel_poll_interval = Duration::from_millis(50);
    state
}

/// Seed a task in Submitted state via the task storage (bypassing the
/// atomic store so we don't add extra events during setup).
async fn seed_task(storage: &InMemoryA2aStorage, tenant: &str, owner: &str, task_id: &str) {
    let task = Task::new(task_id, TaskStatus::new(TaskState::Submitted))
        .with_context_id("ctx-cancel");
    storage
        .create_task(tenant, owner, task)
        .await
        .expect("seed_task");
}

/// Advance a seeded task to Working via the atomic store (so state is
/// cancel-eligible and there is a WORKING event in the store).
async fn advance_to_working(storage: &InMemoryA2aStorage, tenant: &str, owner: &str, task_id: &str) {
    use turul_a2a::storage::A2aAtomicStore;
    storage
        .update_task_status_with_events(
            tenant,
            task_id,
            owner,
            TaskStatus::new(TaskState::Working),
            vec![StreamEvent::StatusUpdate {
                status_update: turul_a2a::streaming::StatusUpdatePayload {
                    task_id: task_id.to_string(),
                    context_id: "ctx-cancel".to_string(),
                    status: serde_json::to_value(TaskStatus::new(TaskState::Working))
                        .unwrap_or_default(),
                },
            }],
        )
        .await
        .expect("advance_to_working");
}

/// Helper: install a live in-flight handle into state.in_flight. Returns
/// the handle and the CancellationToken clone the caller can observe.
fn install_in_flight(
    state: &AppState,
    tenant: &str,
    task_id: &str,
) -> (Arc<InFlightHandle>, CancellationToken, tokio::task::AbortHandle) {
    let token = CancellationToken::new();
    let exposed = token.clone();
    // spawned task: waits on cancellation — represents a live executor
    // that will exit when tokened.
    let spawned = tokio::spawn(async move { token.cancelled().await });
    let abort = spawned.abort_handle();
    let (yielded_tx, _yielded_rx) = oneshot::channel::<Task>();
    let handle = Arc::new(InFlightHandle::new(exposed.clone(), yielded_tx, spawned));
    state
        .in_flight
        .try_insert((tenant.to_string(), task_id.to_string()), Arc::clone(&handle))
        .expect("try_insert");
    (handle, exposed, abort)
}

// ---------------------------------------------------------------------------
// Test 1: same-instance cancel trips the in-flight cancellation token.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_instance_cancel_trips_executor_token() {
    let (state, storage) = make_state();
    seed_task(&storage, "default", "owner-a", "t-1").await;
    advance_to_working(&storage, "default", "owner-a", "t-1").await;

    let (_handle, token, _abort) = install_in_flight(&state, "default", "t-1");

    // State starts untripped.
    assert!(!token.is_cancelled(), "precondition");

    // Kick off core_cancel_task in the background; it will grace-wait
    // then force-commit CANCELED (because no executor is actually
    // responding to the token in this test — we only verify the token
    // was tripped).
    let state_clone = with_fast_grace(state.clone());
    let cancel_handle =
        tokio::spawn(async move { core_cancel_task(state_clone, "default", "owner-a", "t-1").await });

    // Poll for the token being tripped. Deterministic via yield_now; the
    // :cancel handler trips the token BEFORE entering its grace loop.
    let tripped = {
        let mut tripped = false;
        for _ in 0..32 {
            if token.is_cancelled() {
                tripped = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        tripped
    };
    assert!(tripped, "local in-flight token must be tripped by :cancel");

    // Let the cancel handler complete (grace expires, framework commits
    // CANCELED). Response is a terminal Task.
    let result = cancel_handle.await.expect("cancel task").expect("cancel OK");
    let state_str = result.get("status").and_then(|s| s.get("state")).and_then(|v| v.as_str());
    assert_eq!(state_str, Some("TASK_STATE_CANCELED"), "terminal is CANCELED");
}

// ---------------------------------------------------------------------------
// Test 2: cross-instance cancel — instance B writes the marker via
// storage, instance A's supervisor poll trips A's local token.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cross_instance_cancel_via_storage_marker() {
    // Shared storage = shared DB; two AppStates, distinct in_flight registries.
    let storage = InMemoryA2aStorage::new();
    let make_state_for_storage = |storage: InMemoryA2aStorage| AppState {
        executor: Arc::new(NoopExecutor),
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
    };
    let state_a = make_state_for_storage(storage.clone());
    let state_b = make_state_for_storage(storage.clone());

    seed_task(&storage, "default", "owner-x", "t-xinst").await;
    advance_to_working(&storage, "default", "owner-x", "t-xinst").await;

    // Instance A has the live in-flight handle for the executor.
    let (_handle_a, token_a, _abort_a) = install_in_flight(&state_a, "default", "t-xinst");

    // Instance B receives the cancel: writes the marker. It will NOT
    // find the task in its own in-flight registry, so it does NOT trip
    // any local token — propagation relies on A's supervisor poll.
    state_b
        .task_storage
        .set_cancel_requested("default", "t-xinst", "owner-x")
        .await
        .expect("set_cancel_requested on B");

    // A's local token is not yet tripped — B's write is storage-only.
    assert!(
        !token_a.is_cancelled(),
        "A's token must not trip purely from B's storage write"
    );

    // Manually drive one supervisor poll tick on instance A. This is
    // deterministic; in production the poller runs on an interval.
    turul_a2a::server::in_flight::poll_once_for_tests(
        &state_a.in_flight,
        state_a.cancellation_supervisor.as_ref(),
    )
    .await;

    // A's token must now be tripped.
    assert!(
        token_a.is_cancelled(),
        "A's token must be tripped after supervisor poll observes marker written by B"
    );
}

// ---------------------------------------------------------------------------
// Test 3: orphaned task (no in-flight) resolves through grace + fallback.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_on_orphaned_task_resolves_via_grace_fallback() {
    let (state, storage) = make_state();
    seed_task(&storage, "default", "owner-o", "t-orphan").await;
    advance_to_working(&storage, "default", "owner-o", "t-orphan").await;

    // NOTE: no in-flight handle installed — this is the "orphaned task"
    // case. The handler will grace-wait (no token to trip, no cooperative
    // response) and then force-commit CANCELED via the atomic store.

    let state_fast = with_fast_grace(state);
    let result =
        core_cancel_task(state_fast, "default", "owner-o", "t-orphan").await.expect("cancel OK");
    let state_str = result.get("status").and_then(|s| s.get("state")).and_then(|v| v.as_str());
    assert_eq!(state_str, Some("TASK_STATE_CANCELED"), "orphan resolved to CANCELED");

    // Framework-committed terminal has message = None per ADR-012 §8.
    let message = result.get("status").and_then(|s| s.get("message"));
    assert!(
        message.is_none() || message == Some(&serde_json::Value::Null),
        "framework-committed CANCELED must carry message = None, got: {message:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: cancel-vs-cancel idempotency.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_vs_cancel_idempotency() {
    let (state, storage) = make_state();
    seed_task(&storage, "default", "owner-i", "t-idem").await;
    advance_to_working(&storage, "default", "owner-i", "t-idem").await;

    let state = with_fast_grace(state);

    // Two concurrent cancels.
    let s1 = state.clone();
    let s2 = state.clone();
    let (r1, r2) = tokio::join!(
        core_cancel_task(s1, "default", "owner-i", "t-idem"),
        core_cancel_task(s2, "default", "owner-i", "t-idem"),
    );

    let r1 = r1.expect("first cancel must succeed");
    let r2 = r2.expect("second cancel must succeed (idempotent)");
    for r in [&r1, &r2] {
        let s = r.get("status").and_then(|s| s.get("state")).and_then(|v| v.as_str());
        assert_eq!(s, Some("TASK_STATE_CANCELED"), "both return CANCELED");
    }

    // Event store has exactly one CANCELED event (the single-terminal-writer
    // invariant from ADR-010 §7.1 enforces this at the atomic-store layer).
    use turul_a2a::storage::A2aEventStore;
    let events = storage.get_events_after("default", "t-idem", 0).await.unwrap();
    let canceled_count = events
        .iter()
        .filter(|(_, e)| matches!(e, StreamEvent::StatusUpdate { status_update } if status_update
                .status
                .get("state")
                .and_then(|v| v.as_str()) == Some("TASK_STATE_CANCELED")))
        .count();
    assert_eq!(
        canceled_count, 1,
        "exactly one CANCELED event must land; got {canceled_count}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: cancel on terminal task returns 409.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_on_terminal_returns_409() {
    let (state, storage) = make_state();
    seed_task(&storage, "default", "owner-t", "t-term").await;
    advance_to_working(&storage, "default", "owner-t", "t-term").await;
    // Transition to COMPLETED via atomic store.
    use turul_a2a::storage::A2aAtomicStore;
    storage
        .update_task_status_with_events(
            "default",
            "t-term",
            "owner-t",
            TaskStatus::new(TaskState::Completed),
            vec![StreamEvent::StatusUpdate {
                status_update: turul_a2a::streaming::StatusUpdatePayload {
                    task_id: "t-term".into(),
                    context_id: "ctx-cancel".into(),
                    status: serde_json::to_value(TaskStatus::new(TaskState::Completed))
                        .unwrap_or_default(),
                },
            }],
        )
        .await
        .unwrap();

    let result = core_cancel_task(state, "default", "owner-t", "t-term").await;
    match result {
        Err(A2aError::TaskNotCancelable { task_id }) => {
            assert_eq!(task_id, "t-term");
        }
        other => panic!("expected TaskNotCancelable, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 6: cancel by wrong owner returns 404 (TaskNotFound, anti-enumeration).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_by_wrong_owner_returns_404() {
    let (state, storage) = make_state();
    seed_task(&storage, "default", "owner-real", "t-wo").await;
    advance_to_working(&storage, "default", "owner-real", "t-wo").await;

    let result = core_cancel_task(state, "default", "owner-impostor", "t-wo").await;
    match result {
        Err(A2aError::TaskNotFound { task_id }) => {
            assert_eq!(task_id, "t-wo");
        }
        other => panic!("expected TaskNotFound, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 7: marker write is conditional on non-terminal.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn marker_write_conditional_on_non_terminal() {
    let (_state, storage) = make_state();
    seed_task(&storage, "default", "owner-c", "t-cond").await;

    // Succeed on non-terminal (SUBMITTED).
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

    // Transition to COMPLETED.
    use turul_a2a::storage::A2aAtomicStore;
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

    // Now marker write must reject.
    let result = storage.set_cancel_requested("default", "t-cond", "owner-c").await;
    match result {
        Err(turul_a2a::storage::A2aStorageError::TerminalState(s)) => {
            assert_eq!(s, TaskState::Completed);
        }
        other => panic!("expected TerminalState on terminal marker write, got {other:?}"),
    }

    // Supervisor reads must now return false (task is terminal).
    assert!(
        !storage
            .supervisor_get_cancel_requested("default", "t-cond")
            .await
            .unwrap()
    );
}

// ---------------------------------------------------------------------------
// Test 8: supervisor panic cleanup via sentinel — reuses the
// SupervisorSentinel invariant under cancellation context. If a
// supervisor task wrapping a cancellation workflow panics, its
// Drop-guard runs cleanup.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervisor_panic_cleanup_via_sentinel_under_cancellation() {
    let registry = Arc::new(InFlightRegistry::new());
    let key = ("tenant-p".to_string(), "t-p".to_string());

    let cancellation = CancellationToken::new();
    let (_trap_tx, trap_rx) = oneshot::channel::<()>();
    let spawned = tokio::spawn(async move { let _ = trap_rx.await; });
    let abort = spawned.abort_handle();
    let (yielded_tx, _yielded_rx) = oneshot::channel::<Task>();
    let handle = Arc::new(InFlightHandle::new(cancellation.clone(), yielded_tx, spawned));
    registry.try_insert(key.clone(), Arc::clone(&handle)).unwrap();

    let reg_for_task = Arc::clone(&registry);
    let key_for_task = key.clone();
    let handle_for_task = Arc::clone(&handle);

    // Supervisor task that would trip cancellation during its work — then
    // panics before completing.
    let supervisor = tokio::spawn(async move {
        let _sentinel = turul_a2a::server::in_flight::SupervisorSentinel::new(
            reg_for_task,
            key_for_task,
            handle_for_task,
        );
        cancellation.cancel();
        panic!("supervisor panicked mid-cancel");
    });
    let _ = supervisor.await;

    assert!(registry.get(&key).is_none(), "registry cleaned");
    assert!(abort.is_finished(), "spawned handle aborted");
}

// ---------------------------------------------------------------------------
// Test 9: SSE subscriber sees terminal CANCELED.
//
// Subscribe while the task is WORKING, call core_cancel_task, assert
// the stream delivers the CANCELED terminal event (ADR-012 §11 test
// item #11). The test does not rely on a spawned executor: the
// `CancelTask` handler writes the CANCELED event via
// `A2aAtomicStore::update_task_status_with_events` (terminal-CAS
// path) and notifies the event broker, while `core_subscribe_to_task`
// reads from the same durable store and closes on terminal events.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_subscriber_sees_terminal_canceled() {
    use axum::body::Body;
    use http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let (state, storage) = make_state();
    // Match the router's defaults: untenanted route uses empty tenant,
    // and no-middleware requests land as AuthIdentity::Anonymous → owner
    // == "anonymous". Seeding the task under those values lets the test
    // drive the subscribe route via build_router without setting up a
    // test middleware stack.
    let tenant = "";
    let owner = "anonymous";
    let task_id = "t-sse-cancel";

    // Set up a non-terminal task so subscribe is permitted.
    seed_task(&storage, tenant, owner, task_id).await;
    advance_to_working(&storage, tenant, owner, task_id).await;

    // Open the subscribe stream via the router (full HTTP path).
    let router = turul_a2a::router::build_router(state.clone());
    let subscribe_req = Request::get(format!("/tasks/{task_id}:subscribe"))
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .expect("subscribe request");
    let subscribe_resp = router
        .clone()
        .oneshot(subscribe_req)
        .await
        .expect("subscribe oneshot");
    assert_eq!(subscribe_resp.status(), 200, "subscribe must 200 on non-terminal");

    // Kick off a body-reader task that collects SSE frames until the
    // stream closes (terminal delivery closes it cleanly per ADR-009 §4).
    // Use an upper-bound timeout so a bug doesn't hang the test forever.
    let body = subscribe_resp.into_body();
    let collect = async move {
        let collected = body.collect().await.expect("collect body").to_bytes();
        String::from_utf8_lossy(&collected).to_string()
    };

    // Give the subscribe handler a chance to emit its initial Task
    // snapshot before we trip the cancel. Yield once so the spawned
    // SSE task runs; this is deterministic on a multi-threaded runtime.
    let collector = tokio::spawn(collect);
    tokio::task::yield_now().await;

    // Fire :cancel via core_cancel_task. With default grace (5s) the
    // handler blocks until it force-commits CANCELED. Speed it up with
    // short grace so the test is snappy.
    let mut fast_state = state.clone();
    fast_state.runtime_config.cancel_handler_grace = Duration::from_millis(100);
    fast_state.runtime_config.cancel_handler_poll_interval = Duration::from_millis(10);
    let cancel_result =
        turul_a2a::router::core_cancel_task(fast_state, tenant, owner, task_id).await;
    let task_json = cancel_result.expect("cancel OK");
    let persisted_state = task_json
        .get("status")
        .and_then(|s| s.get("state"))
        .and_then(|v| v.as_str());
    assert_eq!(persisted_state, Some("TASK_STATE_CANCELED"));

    // Wait for the stream to close (terminal delivery triggers close).
    let body_text = tokio::time::timeout(Duration::from_secs(3), collector)
        .await
        .expect("subscribe body must close within 3s of terminal commit")
        .expect("collector task");

    // The stream SHOULD contain the CANCELED status-update event. SSE
    // body is `data: <json>\n\n` frames; we search for the proto wire
    // name since that is spec-compliant and stable.
    assert!(
        body_text.contains("TASK_STATE_CANCELED"),
        "subscribe stream must include the terminal CANCELED event; got body:\n{body_text}"
    );
}

// ---------------------------------------------------------------------------
// Compile-time guard: `supervisor_get_cancel_requested` MUST NOT be
// reachable via `Arc<dyn A2aTaskStorage>`. Enforced structurally by the
// trait split. The compile_fail doctest lives on the trait itself in
// `crates/turul-a2a/src/storage/traits.rs`; `cargo test --doc -p turul-a2a`
// verifies the invariant. Integration-test doctests don't execute, so the
// assertion is authored where it belongs — on the trait's documentation.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Executor-driven cancellation races (cancel-vs-complete deterministic
// winner, cooperative-timeout-driven CANCELED, hard-timeout-driven
// FAILED, last-moment executor-wins-CAS) require the spawn-and-track
// send path to produce a real concurrent executor. Those scenarios
// live in `tests/send_mode_tests.rs` alongside the other send-mode
// coverage.
//
// Cross-instance poll-load characterisation (sustained N=100+ in-flight
// tasks polled against a shared marker store) is a scalability
// concern, not a correctness invariant. Not captured in this file;
// revisit if production deployments surface marker-poll contention.
// ---------------------------------------------------------------------------
