//! In-flight task tracking and supervisor-panic cleanup.
//!
//! Shared runtime substrate for ADR-010 (long-running tasks + EventSink),
//! ADR-012 (cancellation propagation), and ADR-011 (push delivery). The
//! types in this module own the invariants around spawned executors —
//! cancellation signalling, blocking-send return, abort capability,
//! cleanup on every exit path.
//!
//! # Types
//!
//! - [`InFlightRegistry`] — `RwLock<HashMap>` keyed by `(tenant, task_id)`.
//! - [`InFlightHandle`] — per-task state: cancellation token, single-fire
//!   `yielded` oneshot for blocking-send return, and the executor's
//!   [`tokio::task::JoinHandle`].
//! - [`SupervisorSentinel`] — drop-guard that cleans up the registry entry
//!   and aborts the spawned task on **all** exit paths, including panic
//!   unwind. Emits a structured tracing event at
//!   [`crate::server::obs::TARGET_SUPERVISOR_PANIC`] when the unwind is from
//!   a panic.
//!
//! # Invariants
//!
//! 1. **Cleanup always runs**: `SupervisorSentinel::Drop` removes the registry
//!    entry and aborts the spawned task whether the supervisor exited
//!    normally, returned an error, or panicked.
//! 2. **Panic is never silent**: sentinel Drop during panic unwind emits an
//!    ERROR-level tracing event with `tenant` + `task_id` fields.
//! 3. **`yielded` fires exactly once**: an `AtomicBool` CAS guards the
//!    oneshot sender. Concurrent triggers resolve to exactly one winner.
//! 4. **Registry insertion is collision-safe**: [`InFlightRegistry::try_insert`]
//!    rejects duplicate `(tenant, task_id)` keys instead of silently
//!    overwriting. Duplicate-spawn-for-same-task is a framework-level
//!    invariant violation; callers propagate the error up as an internal
//!    error rather than clobbering a live handle.
//! 5. **Sentinel Drop does not remove entries it does not own**: a sentinel
//!    for handle A must not remove a registry entry that now contains
//!    handle B — [`InFlightRegistry::remove_if_current`] uses [`Arc::ptr_eq`]
//!    to verify identity before removing. This protects against stale
//!    sentinels that outlive the logical spawn and helps make the registry
//!    robust under code paths that reuse task IDs after a prior completion.
//!
//! See `tests/runtime_substrate_tests.rs` for the invariant coverage.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::oneshot;
use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;

use turul_a2a_types::Task;

use crate::server::obs::TARGET_SUPERVISOR_PANIC;

/// Identifier for an in-flight task in the registry.
///
/// Ownership of `(tenant, task_id)` as an owned tuple rather than references
/// keeps the registry type ergonomic and matches how handlers produce keys
/// from request context.
pub type InFlightKey = (String, String);

/// Per-task state held in the in-flight registry.
///
/// Created by the server when an executor spawn begins; removed by
/// [`SupervisorSentinel::drop`] when the executor exits (normally, in error,
/// or via panic unwind).
pub struct InFlightHandle {
    /// Cancellation signal. Tripped by the `:cancel` handler or by
    /// the blocking-send soft-timeout path. The clone passed to the
    /// executor lives on `ExecutionContext::cancellation`.
    pub cancellation: CancellationToken,

    /// Yielded oneshot. Fires on the **first** commit of a terminal or
    /// interrupted task event. Consumed by the blocking `SendMessage`
    /// handler waiting to return the task to the client. Independent
    /// of `spawned` — fires as soon as the commit succeeds, not when
    /// the executor task exits.
    ///
    /// The sender is wrapped in a Mutex<Option<_>> so it can be atomically
    /// taken at fire time. The CAS on `yielded_fired` serializes calls so
    /// only one thread observes `Some(tx)`.
    yielded_tx: Mutex<Option<oneshot::Sender<Task>>>,

    /// Single-fire flag. Set true on the first successful `fire_yielded` call.
    /// The AcqRel CAS ensures concurrent triggers see exactly one winner.
    yielded_fired: AtomicBool,

    /// Executor's tokio task handle. Held in an `Option` so the
    /// supervisor task can `take()` it out and `.await` ownership.
    ///
    /// Abort capability is carried on [`Self::abort_handle`]
    /// separately — after the supervisor takes the `JoinHandle`, this
    /// field is `None` but abort is still reachable from any caller
    /// that holds the registry handle.
    spawned: Mutex<Option<JoinHandle<()>>>,

    /// Cloneable abort capability for the spawned executor task.
    ///
    /// Decoupled from [`Self::spawned`] so the blocking-send hard
    /// timeout and the `SupervisorSentinel` drop path can abort the
    /// executor without owning the `JoinHandle` — the supervisor
    /// typically already took it to await ownership. Kept in a
    /// `Mutex` so it can be replaced when
    /// [`InFlightHandle::set_spawned`] installs a real handle over an
    /// initial placeholder.
    abort_handle: Mutex<AbortHandle>,
}

impl InFlightHandle {
    /// Create a new handle. Called by the server when spawning an executor.
    pub fn new(
        cancellation: CancellationToken,
        yielded_tx: oneshot::Sender<Task>,
        spawned: JoinHandle<()>,
    ) -> Self {
        let abort_handle = spawned.abort_handle();
        Self {
            cancellation,
            yielded_tx: Mutex::new(Some(yielded_tx)),
            yielded_fired: AtomicBool::new(false),
            spawned: Mutex::new(Some(spawned)),
            abort_handle: Mutex::new(abort_handle),
        }
    }

    /// Abort the spawned executor task.
    ///
    /// Idempotent; safe to call when the task has already finished,
    /// already been aborted, or is still running. Used by the
    /// blocking-send hard-timeout path to stop an executor that is
    /// ignoring cancellation. Returns immediately — the actual task
    /// teardown (future drop, resource release) happens on the tokio
    /// runtime at the next yield point in the executor body. Callers
    /// that need to observe the teardown should await the
    /// `JoinHandle` separately (typically done by the supervisor
    /// task).
    pub fn abort(&self) {
        let guard = self.abort_handle.lock().expect("abort_handle Mutex poisoned");
        guard.abort();
    }

    /// Fire the `yielded` signal with the given task snapshot.
    ///
    /// Returns `true` if this call won the CAS and sent the task; `false` if
    /// another thread already fired. The fire is atomic with respect to the
    /// flag — a loser's `task` argument is dropped without reaching the
    /// receiver.
    ///
    /// Used by the sink's commit hook when a terminal or interrupted
    /// state lands in the atomic store.
    pub fn fire_yielded(&self, task: Task) -> bool {
        // AcqRel on the CAS: Acq of the prior Release makes the sender's
        // state visible; Rel publishes the flag transition to later loads.
        match self.yielded_fired.compare_exchange(
            false,
            true,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // We won — take the sender and send.
                let sender = self
                    .yielded_tx
                    .lock()
                    .expect("yielded_tx Mutex poisoned")
                    .take();
                if let Some(tx) = sender {
                    // Receiver may have already dropped; ignore the SendError.
                    let _ = tx.send(task);
                }
                true
            }
            Err(_) => false,
        }
    }

    /// Query whether `yielded` has fired.
    ///
    /// Used by the blocking-send timeout path to decide between
    /// continuing to wait and force-committing a framework terminal.
    pub fn yielded_fired(&self) -> bool {
        self.yielded_fired.load(Ordering::Acquire)
    }

    /// Install the spawned JoinHandle after-the-fact.
    ///
    /// The send-path spawns the executor *after* the handle is built
    /// (because the executor body needs a [`crate::event_sink::EventSink`]
    /// that holds `Arc<InFlightHandle>`, so the handle has to exist
    /// first). Initial construction uses a placeholder noop
    /// JoinHandle; this setter swaps in the real one once
    /// `tokio::spawn(executor_body)` returns. Updates both the
    /// `JoinHandle` slot and the cloneable abort handle atomically so
    /// a subsequent [`Self::abort`] targets the real task, not the
    /// placeholder.
    pub fn set_spawned(&self, spawned: JoinHandle<()>) {
        let new_abort = spawned.abort_handle();
        *self.spawned.lock().expect("spawned Mutex poisoned") = Some(spawned);
        *self.abort_handle.lock().expect("abort_handle Mutex poisoned") = new_abort;
    }

    /// Take the spawned JoinHandle out of the handle.
    ///
    /// Used by the supervisor task body to `.await` the handle explicitly.
    /// Returns `None` if already taken. The sentinel's Drop path tolerates
    /// `None` — if the supervisor took the handle and awaited it, the
    /// sentinel has nothing to abort.
    pub fn take_spawned(&self) -> Option<JoinHandle<()>> {
        self.spawned.lock().expect("spawned Mutex poisoned").take()
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Concurrent map of in-flight tasks keyed by `(tenant, task_id)`.
///
/// Uses `std::sync::RwLock` over `HashMap` rather than `dashmap`. Rationale:
/// the access pattern is read-heavy (lookups from `:cancel`, push delivery,
/// etc.) with infrequent writes (insert-on-spawn, remove-on-exit). Lock
/// contention at typical in-flight counts is negligible; a workspace-wide
/// `dashmap` dependency is not justified at current scale. If
/// profiling later surfaces contention, this type can be swapped for
/// `DashMap` without changing any caller's API.
#[derive(Default)]
pub struct InFlightRegistry {
    map: RwLock<HashMap<InFlightKey, Arc<InFlightHandle>>>,
}

impl InFlightRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert an entry if no entry exists for this key. Returns
    /// [`InsertCollision`] if one already exists — callers treat this as an
    /// internal error at spawn time. Silent overwrites are NOT permitted:
    /// overwriting a live handle would lose track of an executor we are
    /// responsible for aborting on cleanup.
    pub fn try_insert(
        &self,
        key: InFlightKey,
        handle: Arc<InFlightHandle>,
    ) -> Result<(), InsertCollision> {
        use std::collections::hash_map::Entry;
        let mut map = self.map.write().expect("InFlightRegistry RwLock poisoned");
        match map.entry(key) {
            Entry::Occupied(occ) => Err(InsertCollision {
                key: occ.key().clone(),
                existing: occ.get().clone(),
            }),
            Entry::Vacant(vac) => {
                vac.insert(handle);
                Ok(())
            }
        }
    }

    /// Get a cloned Arc to the handle, if present.
    pub fn get(&self, key: &InFlightKey) -> Option<Arc<InFlightHandle>> {
        self.map
            .read()
            .expect("InFlightRegistry RwLock poisoned")
            .get(key)
            .cloned()
    }

    /// Remove the entry ONLY IF it still contains the given handle by
    /// [`Arc::ptr_eq`] identity. Returns `true` if the entry was removed,
    /// `false` if the key was absent or contained a different handle.
    ///
    /// This is the canonical removal path for
    /// [`SupervisorSentinel::drop`]: a stale sentinel for a prior handle
    /// must not remove a newer, live handle for the same
    /// `(tenant, task_id)`. Even with [`Self::try_insert`]'s
    /// collision-safety, a `remove`-then-`try_insert` sequence on the same
    /// key could, in principle, race with a late sentinel Drop — identity
    /// check closes that window.
    pub fn remove_if_current(&self, key: &InFlightKey, handle: &Arc<InFlightHandle>) -> bool {
        let mut map = self.map.write().expect("InFlightRegistry RwLock poisoned");
        match map.get(key) {
            Some(existing) if Arc::ptr_eq(existing, handle) => {
                map.remove(key);
                true
            }
            _ => false,
        }
    }

    /// Count of entries — diagnostic only.
    pub fn len(&self) -> usize {
        self.map
            .read()
            .expect("InFlightRegistry RwLock poisoned")
            .len()
    }

    /// True if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot of in-flight `(tenant → [task_id, ...])` groups for the
    /// supervisor's cross-instance cancel-marker poll. Copies the keys
    /// out of the lock so the poll body runs without holding the lock.
    pub(crate) fn snapshot_by_tenant(&self) -> std::collections::HashMap<String, Vec<String>> {
        let map = self
            .map
            .read()
            .expect("InFlightRegistry RwLock poisoned");
        let mut out: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (tenant, task_id) in map.keys() {
            out.entry(tenant.clone()).or_default().push(task_id.clone());
        }
        out
    }

    /// Get a handle by key. Test-friendly alias for `get` to make the
    /// cross-instance-cancel poll body explicit about the lookup intent.
    pub(crate) fn get_handle(&self, key: &InFlightKey) -> Option<Arc<InFlightHandle>> {
        self.get(key)
    }
}

/// Error returned by [`InFlightRegistry::try_insert`] when the key is
/// already occupied. The existing handle is returned so the caller can
/// log or inspect it for diagnostics; the caller MUST NOT proceed as if
/// the insert succeeded.
pub struct InsertCollision {
    pub key: InFlightKey,
    pub existing: Arc<InFlightHandle>,
}

// Manual Debug — `InFlightHandle` holds non-Debug fields (oneshot sender,
// JoinHandle) so we can't derive. Print identity-only info: Arc pointer
// and key. That's enough for log diagnostics and doesn't risk leaking
// any state from within the handle.
impl fmt::Debug for InsertCollision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InsertCollision")
            .field("key", &self.key)
            .field("existing_ptr", &Arc::as_ptr(&self.existing))
            .finish()
    }
}

impl fmt::Display for InsertCollision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (tenant, task_id) = &self.key;
        write!(
            f,
            "in-flight registry collision for tenant={tenant}, task_id={task_id}; \
             a spawn for this key is already active"
        )
    }
}

impl std::error::Error for InsertCollision {}

// ---------------------------------------------------------------------------
// SupervisorSentinel — drop-guard for panic-safe cleanup
// ---------------------------------------------------------------------------

/// Cleanup sentinel that runs on every exit path of the supervisor task.
///
/// Constructed at the top of a supervisor task body; its `Drop` impl fires
/// when the supervisor returns normally, returns an error, or panics (via
/// unwind). Guarantees:
///
/// - **Spawned task aborted** if still running.
/// - **Registry entry removed** for `(tenant, task_id)`.
/// - **Panic is not silent**: when `std::thread::panicking()` is true, emits
///   a structured tracing event at [`TARGET_SUPERVISOR_PANIC`] with fields
///   `tenant` + `task_id`.
///
/// # Important
///
/// The sentinel does NOT fire the `yielded` oneshot. Firing `yielded` from a
/// panicking context risks delivering a corrupt task snapshot to a blocking
/// caller. Blocking-send's `yielded` awaiter falls through to its own
/// `blocking_task_timeout` if the supervisor panics mid-execution — the
/// timeout path is itself sentinel-guarded.
pub struct SupervisorSentinel {
    registry: Arc<InFlightRegistry>,
    key: InFlightKey,
    handle: Arc<InFlightHandle>,
}

impl SupervisorSentinel {
    /// Create a sentinel. Hold as a local in the supervisor task body; Drop
    /// runs on scope exit.
    pub fn new(
        registry: Arc<InFlightRegistry>,
        key: InFlightKey,
        handle: Arc<InFlightHandle>,
    ) -> Self {
        Self { registry, key, handle }
    }
}

impl Drop for SupervisorSentinel {
    fn drop(&mut self) {
        // Distinguish panic unwind from normal exit. During unwind,
        // `std::thread::panicking()` returns true on the thread processing
        // the panic. Tokio tasks unwind on panic and drop their locals on
        // the worker thread running the task, so this reliably detects the
        // panic case.
        let panicking = std::thread::panicking();

        // 1. Abort this sentinel's executor task. Load-bearing
        //    panic-safety guarantee: a panicked supervisor cannot
        //    leave a live executor behind. Using the cloneable
        //    AbortHandle means we never race the supervisor's
        //    `take_spawned().await` — the abort call works even when
        //    the supervisor has already taken ownership of the
        //    JoinHandle. Abort is idempotent: safe to call on a task
        //    that has already finished.
        self.handle.abort();

        // 2. Remove the registry entry ONLY if it still contains our
        //    handle. A stale sentinel must not remove a newer handle that
        //    happens to share the same `(tenant, task_id)` key. Uses
        //    Arc::ptr_eq via `remove_if_current`.
        self.registry.remove_if_current(&self.key, &self.handle);

        // 3. If the unwind was a panic, emit the observability signal. Note:
        //    tracing macros during panic unwind are safe — tracing's
        //    Dispatch acquires no locks that could be poisoned, and the
        //    current subscriber is looked up via thread-local / global
        //    default that remains live until the test binary exits.
        if panicking {
            let (tenant, task_id) = &self.key;
            tracing::error!(
                target: TARGET_SUPERVISOR_PANIC,
                tenant = %tenant,
                task_id = %task_id,
                "supervisor task panicked; cleanup ran via SupervisorSentinel Drop"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-instance cancel poller (ADR-012 §3/§4)
// ---------------------------------------------------------------------------

/// Run a cross-instance cancel-marker poll loop until `shutdown` is cancelled.
///
/// Tick interval is `interval`. On each tick:
/// 1. Snapshot the current in-flight registry into `(tenant → [task_id…])`.
/// 2. For each tenant, call
///    [`crate::storage::A2aCancellationSupervisor::supervisor_list_cancel_requested`]
///    to get the subset with the marker set.
/// 3. Trip `cancellation` on each matching handle.
///
/// This bridges the cross-instance case: a `:cancel` handler on another
/// instance writes the marker via
/// [`crate::storage::A2aTaskStorage::set_cancel_requested`]; this poller
/// on the executor's instance observes it and tokens the executor on
/// this side.
///
/// Storage-layer errors are logged at WARN and the loop continues on
/// the next tick — a transient storage failure must not kill
/// cancellation propagation entirely.
///
/// Intended to be spawned by the server runtime at startup and shut
/// down via the `shutdown` token when the server exits.
pub async fn run_cross_instance_cancel_poller(
    registry: std::sync::Arc<InFlightRegistry>,
    supervisor: std::sync::Arc<dyn crate::storage::A2aCancellationSupervisor>,
    interval: std::time::Duration,
    shutdown: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.cancelled() => {
                tracing::debug!(
                    target: "turul_a2a::cross_instance_cancel_poll",
                    "cross-instance cancel poller shutting down"
                );
                return;
            }
        }
        poll_once(&registry, supervisor.as_ref()).await;
    }
}

/// Single poll pass. Public so integration tests can drive one tick
/// deterministically without waiting on a wall-clock interval. Not part
/// of the stable public surface — may change without a semver bump.
///
/// This re-export retains the doc for discoverability; the internal
/// name stays `poll_once` for brevity.
pub async fn poll_once_for_tests(
    registry: &InFlightRegistry,
    supervisor: &dyn crate::storage::A2aCancellationSupervisor,
) {
    poll_once(registry, supervisor).await
}

/// Internal single-poll pass (not exposed; tests use `poll_once_for_tests`).
async fn poll_once(
    registry: &InFlightRegistry,
    supervisor: &dyn crate::storage::A2aCancellationSupervisor,
) {
    let groups = registry.snapshot_by_tenant();
    for (tenant, task_ids) in groups {
        match supervisor
            .supervisor_list_cancel_requested(&tenant, &task_ids)
            .await
        {
            Ok(marked) => {
                for task_id in marked {
                    let key = (tenant.clone(), task_id);
                    if let Some(handle) = registry.get_handle(&key) {
                        handle.cancellation.cancel();
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "turul_a2a::cross_instance_cancel_poll_error",
                    tenant = %tenant,
                    error = %e,
                    "cross-instance cancel poll failed; will retry next tick"
                );
            }
        }
    }
}
