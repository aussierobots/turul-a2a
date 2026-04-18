//! Shared runtime substrate tests.
//!
//! Covers `InFlightRegistry`, `InFlightHandle`, `SupervisorSentinel`, and the
//! `secrecy` integration. All synchronization is explicit (oneshots,
//! [`Notify`]) — no sleeps, no polling for side effects.
//!
//! Observability contract: supervisor panics emit a structured tracing event
//! at `turul_a2a::supervisor_panic`. Tests capture events via a thread-safe
//! `tracing_subscriber::Layer` so assertions run against event occurrence +
//! field values, not counter increments. This is the canonical capture
//! pattern for later phases.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use secrecy::{ExposeSecret, SecretBox};
use tokio::sync::oneshot;
use tokio::sync::Notify;  // used by normal-exit test
use tokio_util::sync::CancellationToken;
use tracing::Level;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use turul_a2a::server::in_flight::{
    InFlightHandle, InFlightKey, InFlightRegistry, InsertCollision, SupervisorSentinel,
};
use turul_a2a::server::obs::TARGET_SUPERVISOR_PANIC;
use turul_a2a_types::Task;

// ---------------------------------------------------------------------------
// Test harness — capturing tracing layer.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct CapturedEvent {
    target: String,
    level: Level,
    fields: HashMap<String, String>,
}

#[derive(Default, Clone)]
struct EventCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl EventCapture {
    fn new() -> Self {
        Self::default()
    }

    fn events(&self) -> Vec<CapturedEvent> {
        self.events.lock().unwrap().clone()
    }

    fn filter_target(&self, target: &str) -> Vec<CapturedEvent> {
        self.events()
            .into_iter()
            .filter(|e| e.target == target)
            .collect()
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = FieldVisitor(HashMap::new());
        event.record(&mut visitor);
        self.events.lock().unwrap().push(CapturedEvent {
            target: metadata.target().to_string(),
            level: *metadata.level(),
            fields: visitor.0,
        });
    }
}

struct FieldVisitor(HashMap<String, String>);

impl tracing::field::Visit for FieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_string(), format!("{value:?}"));
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

/// Global capturing subscriber — installed exactly once per test binary.
///
/// `WithSubscriber` on a spawned future does NOT reliably survive panic
/// unwind in tokio: the RAII dispatcher guard may already have unwound by
/// the time drop-guards inside the panicking frame run. A
/// `set_global_default` subscriber is active on every thread for the
/// lifetime of the process, so `tracing::error!` from inside a drop-guard
/// during panic unwind reliably delivers.
///
/// Tests filter the captured events by a unique `task_id` per test to
/// avoid cross-test contamination.
static GLOBAL_CAPTURE: OnceLock<EventCapture> = OnceLock::new();

fn global_capture() -> &'static EventCapture {
    GLOBAL_CAPTURE.get_or_init(|| {
        let capture = EventCapture::new();
        tracing_subscriber::registry()
            .with(capture.clone())
            .try_init()
            .expect("set_global_default must succeed on first call");
        capture
    })
}

// ---------------------------------------------------------------------------
// Test fixtures.
// ---------------------------------------------------------------------------

fn make_key(tenant: &str, task_id: &str) -> InFlightKey {
    (tenant.to_string(), task_id.to_string())
}

/// Build a minimal Task suitable for firing `yielded`.
fn dummy_task(task_id: &str) -> Task {
    use turul_a2a_types::{TaskState, TaskStatus};
    Task::new(task_id.to_string(), TaskStatus::new(TaskState::Submitted))
        .with_context_id("ctx-1")
}

// ---------------------------------------------------------------------------
// Tests (6 total, per plan).
// ---------------------------------------------------------------------------

/// Test 1: basic try_insert/get/remove_if_current round-trip on the registry.
#[tokio::test]
async fn in_flight_registry_insert_remove_roundtrip() {
    let registry = Arc::new(InFlightRegistry::new());
    let key = make_key("tenant-1", "task-1");

    // Task that waits for an explicit signal then exits — no sleep.
    let (exit_tx, exit_rx) = oneshot::channel::<()>();
    let spawned = tokio::spawn(async move {
        let _ = exit_rx.await;
    });

    let (yielded_tx, _yielded_rx) = oneshot::channel::<Task>();
    let handle = Arc::new(InFlightHandle::new(
        CancellationToken::new(),
        yielded_tx,
        spawned,
    ));

    registry
        .try_insert(key.clone(), Arc::clone(&handle))
        .expect("first insert on empty key must succeed");
    assert!(registry.get(&key).is_some(), "entry should be present after insert");

    // Release the spawned task and let it resolve.
    let _ = exit_tx.send(());
    let taken = handle.take_spawned().expect("JoinHandle should still be in the handle");
    taken.await.expect("spawned task should complete cleanly");

    let removed = registry.remove_if_current(&key, &handle);
    assert!(removed, "remove_if_current should succeed for the owning handle");
    assert!(registry.get(&key).is_none(), "entry must be gone after remove");
}

/// Test 2: sentinel Drop on normal exit path removes the registry entry and
/// leaves a finished JoinHandle in its wake.
#[tokio::test]
async fn supervisor_sentinel_drops_on_normal_exit_removes_entry() {
    let registry = Arc::new(InFlightRegistry::new());
    let key = make_key("tenant-1", "task-1");

    let exit_notify = Arc::new(Notify::new());
    let exit_notify_task = Arc::clone(&exit_notify);
    let spawned = tokio::spawn(async move {
        exit_notify_task.notified().await;
    });
    let abort_handle = spawned.abort_handle();

    let (yielded_tx, _yielded_rx) = oneshot::channel::<Task>();
    let handle = Arc::new(InFlightHandle::new(
        CancellationToken::new(),
        yielded_tx,
        spawned,
    ));
    registry
        .try_insert(key.clone(), Arc::clone(&handle))
        .expect("first insert must succeed");

    // Simulate the supervisor body. The sentinel is a local; it drops when
    // this block exits, running the cleanup path.
    {
        let _sentinel = SupervisorSentinel::new(
            Arc::clone(&registry),
            key.clone(),
            Arc::clone(&handle),
        );

        // Release the spawned task and await its resolution explicitly.
        exit_notify.notify_one();
        let taken = handle.take_spawned().expect("spawned handle present");
        taken.await.expect("spawned task should complete cleanly");
    }

    assert!(registry.get(&key).is_none(), "registry entry must be removed on sentinel Drop");
    assert!(abort_handle.is_finished(), "spawned task must be finished");
}

/// Test 3: sentinel Drop on panic unwind still cleans up the registry entry
/// AND aborts the spawned task (which would otherwise hang forever).
#[tokio::test]
async fn supervisor_sentinel_drops_on_panic_still_cleans_up() {
    let registry = Arc::new(InFlightRegistry::new());
    let key = make_key("tenant-panic", "task-panic");

    // Spawned task that will never exit on its own — relies on abort.
    let (_trap_tx, trap_rx) = oneshot::channel::<()>();
    let spawned = tokio::spawn(async move {
        let _ = trap_rx.await;
    });
    let abort_handle = spawned.abort_handle();

    let (yielded_tx, _yielded_rx) = oneshot::channel::<Task>();
    let handle = Arc::new(InFlightHandle::new(
        CancellationToken::new(),
        yielded_tx,
        spawned,
    ));
    registry
        .try_insert(key.clone(), Arc::clone(&handle))
        .expect("first insert must succeed");

    let registry_for_task = Arc::clone(&registry);
    let key_for_task = key.clone();
    let handle_for_task = Arc::clone(&handle);

    // Spawn a supervisor task that panics while holding the sentinel.
    let supervisor_handle = tokio::spawn(async move {
        let _sentinel = SupervisorSentinel::new(registry_for_task, key_for_task, handle_for_task);
        panic!("simulated supervisor panic");
    });

    // Join the supervisor; its JoinError is expected (panic is caught at task boundary).
    let result = supervisor_handle.await;
    assert!(result.is_err(), "supervisor task should have panicked");
    let join_error = result.unwrap_err();
    assert!(join_error.is_panic(), "JoinError should reflect panic, not cancellation");

    // Cleanup must have run via Drop during unwind.
    assert!(
        registry.get(&key).is_none(),
        "registry entry must be removed on sentinel Drop during panic unwind"
    );

    // The spawned task was aborted by the sentinel; is_finished returns true
    // as soon as abort() is called (even before the task has actually unwound).
    assert!(
        abort_handle.is_finished(),
        "spawned task must be aborted by sentinel during panic cleanup"
    );
}

/// Test 4 (observability contract): sentinel Drop during panic unwind emits a
/// structured event at `turul_a2a::supervisor_panic` with ERROR level and
/// `task_id` / `tenant` fields.
///
/// Capture strategy: a `set_global_default` subscriber is installed once per
/// test binary via [`global_capture`]. This is reliable during panic unwind
/// (unlike `WithSubscriber`, where the RAII dispatcher guard may already have
/// unwound by the time drop-guards in the panicking frame run). Cross-test
/// isolation is achieved by filtering captured events by a unique `task_id`
/// prefix per test.
///
/// This is the canonical pattern for subsequent phases' observability tests.
#[tokio::test]
async fn supervisor_panic_emits_structured_event() {
    let capture = global_capture();

    // Unique task_id prefix guarantees no cross-test contamination in the
    // shared global capture.
    let tenant = "tenant-obs-unique-b8a3";
    let task_id = "task-obs-unique-b8a3";

    let registry = Arc::new(InFlightRegistry::new());
    let key = make_key(tenant, task_id);

    let (_trap_tx, trap_rx) = oneshot::channel::<()>();
    let spawned = tokio::spawn(async move {
        let _ = trap_rx.await;
    });

    let (yielded_tx, _yielded_rx) = oneshot::channel::<Task>();
    let handle = Arc::new(InFlightHandle::new(
        CancellationToken::new(),
        yielded_tx,
        spawned,
    ));
    registry
        .try_insert(key.clone(), Arc::clone(&handle))
        .expect("first insert must succeed");

    let registry_for_task = Arc::clone(&registry);
    let key_for_task = key.clone();
    let handle_for_task = Arc::clone(&handle);

    let supervisor_handle = tokio::spawn(async move {
        let _sentinel = SupervisorSentinel::new(registry_for_task, key_for_task, handle_for_task);
        panic!("observable panic");
    });

    let result = supervisor_handle.await;
    assert!(result.is_err(), "supervisor should have panicked");

    // Filter by both target and our unique task_id for isolation from
    // parallel tests using the same global subscriber.
    let panic_events: Vec<_> = capture
        .filter_target(TARGET_SUPERVISOR_PANIC)
        .into_iter()
        .filter(|e| e.fields.get("task_id").map(String::as_str) == Some(task_id))
        .collect();

    assert_eq!(
        panic_events.len(),
        1,
        "exactly one supervisor_panic event for task_id={task_id} expected, got {}",
        panic_events.len(),
    );

    let evt = &panic_events[0];
    assert_eq!(evt.level, Level::ERROR, "supervisor_panic should be ERROR level");
    assert_eq!(
        evt.fields.get("tenant").map(String::as_str),
        Some(tenant),
        "tenant field must match"
    );
    assert_eq!(
        evt.fields.get("task_id").map(String::as_str),
        Some(task_id),
        "task_id field must match"
    );

    // Invariant check: no secret-looking substrings in this event's fields.
    // No secret data flows through the supervisor-panic event today;
    // this template applies to push-delivery observability events as
    // they land.
    for value in evt.fields.values() {
        assert!(
            !value.contains("BEGIN PRIVATE KEY"),
            "captured field value unexpectedly contains PEM header: {value}"
        );
    }
}

/// Test 5: the `yielded` signal fires exactly once under concurrent
/// triggers. Uses an `AtomicBool` CAS inside `InFlightHandle::fire_yielded`
/// as the single source of truth. Synchronization is via
/// [`tokio::sync::Barrier`] — deterministic rendezvous with no sleeps and
/// no spawn-race against `Notify::notify_waiters`.
#[tokio::test]
async fn yielded_oneshot_fires_once_under_concurrent_triggers() {
    const N_TRIGGERS: usize = 16;

    let (yielded_tx, mut yielded_rx) = oneshot::channel::<Task>();
    let handle = Arc::new(InFlightHandle::new(
        CancellationToken::new(),
        yielded_tx,
        tokio::spawn(async {}),
    ));

    // Barrier: N trigger tasks + 1 main thread = N+1 parties. All block
    // at `.wait().await` until the last party arrives, then all release
    // simultaneously. No spawn-race: the main thread only calls `.wait()`
    // after kicking off every spawn, so all spawns have already polled
    // their own `.wait()` by the time the main thread is blocked.
    let barrier = Arc::new(tokio::sync::Barrier::new(N_TRIGGERS + 1));
    let results = Arc::new(Mutex::new(Vec::with_capacity(N_TRIGGERS)));

    let mut joins = Vec::with_capacity(N_TRIGGERS);
    for i in 0..N_TRIGGERS {
        let handle = Arc::clone(&handle);
        let barrier = Arc::clone(&barrier);
        let results = Arc::clone(&results);
        joins.push(tokio::spawn(async move {
            barrier.wait().await;
            let task = dummy_task(&format!("task-{i}"));
            let did_fire = handle.fire_yielded(task);
            results.lock().unwrap().push(did_fire);
        }));
    }

    // The main thread is the (N+1)th party. Arrival here releases everyone.
    barrier.wait().await;

    for join in joins {
        join.await.expect("trigger task should not panic");
    }

    let did_fire_count = results.lock().unwrap().iter().filter(|v| **v).count();
    assert_eq!(
        did_fire_count, 1,
        "exactly one fire_yielded call must win the CAS; got {did_fire_count}"
    );

    // Recv must succeed exactly once with one of the tasks.
    let received = yielded_rx
        .try_recv()
        .expect("yielded oneshot must have been fired exactly once");
    assert!(
        received.id().starts_with("task-"),
        "received task id should be one of task-0..task-{N_TRIGGERS}, got {}",
        received.id()
    );

    // Subsequent attempt on the sender side is a no-op.
    let task = dummy_task("task-after");
    assert!(!handle.fire_yielded(task), "after-terminal attempt must NOT fire");
}

/// Test 6 (secret handling): `secrecy::SecretBox<String>` redacts in Debug
/// and Display. Validates the invariant that future phases (push delivery,
/// etc.) can rely on: formatting a secret never leaks the underlying value.
#[tokio::test]
async fn secrecy_newtype_never_leaks_in_debug_or_display() {
    let sentinel_value = "SECRET-VALUE-DO-NOT-LEAK-48a9f7b3";
    let secret: SecretBox<String> = SecretBox::new(Box::new(sentinel_value.to_string()));

    // Debug
    let debug_formatted = format!("{secret:?}");
    assert!(
        !debug_formatted.contains(sentinel_value),
        "SecretBox::Debug must not contain the underlying value, got: {debug_formatted}"
    );
    assert!(
        debug_formatted.contains("REDACTED") || debug_formatted.contains("[REDACTED"),
        "SecretBox::Debug should contain a redaction marker, got: {debug_formatted}"
    );

    // Explicit expose still works — this is the intended escape hatch.
    assert_eq!(
        secret.expose_secret(),
        sentinel_value,
        "expose_secret should return the underlying value"
    );

    // Inside a struct that uses default-derived Debug. The `#[allow(dead_code)]`
    // pragma is because the fields are only used via Debug formatting — the
    // compiler can't tell that Debug "reads" them. The test assertion on the
    // Debug output is what proves both the redaction invariant (token hidden)
    // and the normal-field visibility (name present).
    #[derive(Debug)]
    #[allow(dead_code)]
    struct Wrapper {
        name: String,
        token: SecretBox<String>,
    }

    let wrapper = Wrapper {
        name: "webhook-1".to_string(),
        token: SecretBox::new(Box::new(sentinel_value.to_string())),
    };

    let debug_wrapper = format!("{wrapper:?}");
    assert!(
        !debug_wrapper.contains(sentinel_value),
        "Wrapper::Debug must not leak the wrapped secret, got: {debug_wrapper}"
    );
    assert!(
        debug_wrapper.contains("webhook-1"),
        "non-secret fields should still appear in Debug, got: {debug_wrapper}"
    );
}

// ---------------------------------------------------------------------------
// Collision-safety tests for InFlightRegistry key insertion.
// ---------------------------------------------------------------------------

/// Helper: construct a handle wrapping a tokio task that waits on the given
/// receiver then exits. Returns the handle, its abort handle, and the
/// sender that releases the task.
fn make_waiting_handle() -> (Arc<InFlightHandle>, tokio::task::AbortHandle, oneshot::Sender<()>) {
    let (exit_tx, exit_rx) = oneshot::channel::<()>();
    let spawned = tokio::spawn(async move {
        let _ = exit_rx.await;
    });
    let abort_handle = spawned.abort_handle();
    let (yielded_tx, _yielded_rx) = oneshot::channel::<Task>();
    let handle = Arc::new(InFlightHandle::new(
        CancellationToken::new(),
        yielded_tx,
        spawned,
    ));
    (handle, abort_handle, exit_tx)
}

/// Test 7: duplicate `try_insert` for the same key is rejected and does
/// NOT overwrite the existing handle. Both the returned error and a
/// subsequent `get()` reference the original handle via `Arc::ptr_eq`.
#[tokio::test]
async fn try_insert_rejects_duplicate_without_overwrite() {
    let registry = Arc::new(InFlightRegistry::new());
    let key = make_key("tenant-dup", "task-dup");

    let (handle_a, abort_a, exit_a) = make_waiting_handle();
    let (handle_b, abort_b, exit_b) = make_waiting_handle();

    registry
        .try_insert(key.clone(), Arc::clone(&handle_a))
        .expect("first insert must succeed");

    let collision: InsertCollision = registry
        .try_insert(key.clone(), Arc::clone(&handle_b))
        .expect_err("duplicate insert for same key must fail");

    // Returned existing handle is the original A, not B.
    assert!(
        Arc::ptr_eq(&collision.existing, &handle_a),
        "collision.existing must be the original handle, not the rejected one"
    );
    assert_eq!(collision.key, key, "collision reports the conflicting key");

    // Registry still contains A — B was rejected and not inserted.
    let current = registry.get(&key).expect("entry should still be present");
    assert!(
        Arc::ptr_eq(&current, &handle_a),
        "registry must still hold the original handle; duplicate did not overwrite"
    );

    // Error message is well-formed (useful in server startup panics / logs).
    let msg = collision.to_string();
    assert!(msg.contains("tenant-dup"), "error should mention tenant");
    assert!(msg.contains("task-dup"), "error should mention task_id");

    // Cleanup: release both spawned tasks so the test doesn't leak them.
    let _ = exit_a.send(());
    let _ = exit_b.send(());
    if let Some(h) = handle_a.take_spawned() {
        let _ = h.await;
    }
    if let Some(h) = handle_b.take_spawned() {
        let _ = h.await;
    }
    drop(abort_a);
    drop(abort_b);
}

/// Test 8 (the core collision-safety invariant): a stale sentinel for
/// handle A must NOT remove a newer handle B that now occupies the same
/// `(tenant, task_id)` key. Sequence:
///
/// 1. Insert handle A for key K.
/// 2. Remove A via `remove_if_current`.
/// 3. Insert handle B for key K (same key, different Arc).
/// 4. Drop a sentinel that was constructed with A as its handle.
/// 5. Assert B is still in the registry.
///
/// Without the `Arc::ptr_eq` identity check in `remove_if_current`, step 4
/// would remove B by key, leaving B untracked.
#[tokio::test]
async fn stale_sentinel_does_not_remove_newer_handle_for_same_key() {
    let registry = Arc::new(InFlightRegistry::new());
    let key = make_key("tenant-reuse", "task-reuse");

    let (handle_a, abort_a, exit_a) = make_waiting_handle();
    let (handle_b, abort_b, exit_b) = make_waiting_handle();

    // Step 1: insert A.
    registry
        .try_insert(key.clone(), Arc::clone(&handle_a))
        .expect("A insert must succeed");

    // Step 2: remove A from the registry BUT keep the handle and construct
    // a sentinel that references it (simulating a stale sentinel that
    // outlives the registry entry — e.g. the supervisor's sentinel is
    // still on the stack but the registry has already cycled).
    let removed_a = registry.remove_if_current(&key, &handle_a);
    assert!(removed_a, "A must be removable while it is the current entry");

    // Step 3: install B under the same key.
    registry
        .try_insert(key.clone(), Arc::clone(&handle_b))
        .expect("B insert must succeed after A was removed");

    // Step 4: drop a sentinel that holds A. Inside its Drop, it will call
    // `remove_if_current(&key, &handle_a)`. Since the registry now holds
    // B (Arc::ptr_eq == false), the call must be a no-op on the registry.
    {
        let _stale_sentinel = SupervisorSentinel::new(
            Arc::clone(&registry),
            key.clone(),
            Arc::clone(&handle_a),
        );
        // scope exit → Drop → remove_if_current(&key, &handle_a) → no-op
    }

    // Step 5: B must still be present.
    let current = registry
        .get(&key)
        .expect("B must still be in the registry; stale sentinel must not have removed it");
    assert!(
        Arc::ptr_eq(&current, &handle_b),
        "registry must still hold B after the stale sentinel dropped"
    );

    // Cleanup.
    let _ = exit_a.send(());
    let _ = exit_b.send(());
    if let Some(h) = handle_a.take_spawned() {
        let _ = h.await;
    }
    if let Some(h) = handle_b.take_spawned() {
        let _ = h.await;
    }
    drop(abort_a);
    drop(abort_b);
}

/// Test 9: a stale sentinel still aborts its OWN spawned task even though
/// it does not remove the registry entry (because that entry now contains
/// a different handle). Invariant: each sentinel is responsible for its
/// own JoinHandle — independent of whether it still "owns" the registry
/// entry at Drop time.
#[tokio::test]
async fn stale_sentinel_still_aborts_own_spawned_handle() {
    let registry = Arc::new(InFlightRegistry::new());
    let key = make_key("tenant-abort", "task-abort");

    let (handle_a, abort_a, _keep_exit_a) = make_waiting_handle();
    let (handle_b, abort_b, exit_b) = make_waiting_handle();

    registry
        .try_insert(key.clone(), Arc::clone(&handle_a))
        .expect("A insert must succeed");
    assert!(registry.remove_if_current(&key, &handle_a));
    registry
        .try_insert(key.clone(), Arc::clone(&handle_b))
        .expect("B insert must succeed");

    assert!(
        !abort_a.is_finished(),
        "precondition: A's spawned task should still be running"
    );

    // Dropping a sentinel wrapped around A triggers A's abort path, even
    // though the registry entry now belongs to B.
    {
        let _stale_sentinel = SupervisorSentinel::new(
            Arc::clone(&registry),
            key.clone(),
            Arc::clone(&handle_a),
        );
    }

    // `abort()` schedules a cancel wake for the aborted task; the task's
    // `is_finished` flips once the task's next poll observes cancellation.
    // We yield the runtime a bounded number of times to let that propagate.
    // Deterministic in practice: one yield suffices on the default
    // multi-threaded runtime, but we allow a few more for safety in
    // debug builds / slow CI.
    for _ in 0..16 {
        if abort_a.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        abort_a.is_finished(),
        "stale sentinel must abort its own spawned task (even after 16 yields)"
    );

    // B remains untouched: still in registry, spawned task still running.
    assert!(
        registry.get(&key).is_some(),
        "B must still be in the registry"
    );
    assert!(
        !abort_b.is_finished(),
        "B's spawned task must NOT be aborted by A's sentinel"
    );

    // Cleanup: release B and await its natural exit.
    let _ = exit_b.send(());
    if let Some(h) = handle_b.take_spawned() {
        let _ = h.await;
    }
    // A was aborted — its take_spawned returns the already-aborted handle.
    if let Some(h) = handle_a.take_spawned() {
        let _ = h.await; // JoinError::cancelled — swallowed.
    }
}
