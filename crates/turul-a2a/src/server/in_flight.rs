//! In-flight task tracking and supervisor-panic cleanup.
//!
//! This module is the **shared runtime substrate** for ADR-010 (long-running
//! tasks + EventSink), ADR-012 (cancellation propagation), and ADR-011 (push
//! delivery). Phase A introduces the types and invariants; later phases wire
//! up the executor, router, and delivery code against them.
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
//!
//! See `tests/runtime_substrate_tests.rs` for the invariant coverage.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;
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
    /// Cancellation signal. Tripped by the `:cancel` handler (phase C) or by
    /// the blocking-send timeout path (phase D). The clone passed to the
    /// executor lives on `ExecutionContext::cancellation`.
    pub cancellation: CancellationToken,

    /// Yielded oneshot. Fires on the **first** commit of a terminal or
    /// interrupted task event. Consumer: blocking `SendMessage` handler
    /// (phase D). Independent of `spawned` — fires as soon as the commit
    /// succeeds, not when the executor task exits.
    ///
    /// The sender is wrapped in a Mutex<Option<_>> so it can be atomically
    /// taken at fire time. The CAS on `yielded_fired` serializes calls so
    /// only one thread observes `Some(tx)`.
    yielded_tx: Mutex<Option<oneshot::Sender<Task>>>,

    /// Single-fire flag. Set true on the first successful `fire_yielded` call.
    /// The AcqRel CAS ensures concurrent triggers see exactly one winner.
    yielded_fired: AtomicBool,

    /// Executor's tokio task handle. Used by the supervisor for cleanup and,
    /// in later phases, for blocking-send hard-timeout abort.
    ///
    /// `Mutex<Option<_>>` so the supervisor can `take()` for awaiting while
    /// the sentinel still has a shot at aborting on panic.
    spawned: Mutex<Option<JoinHandle<()>>>,
}

impl InFlightHandle {
    /// Create a new handle. Called by the server when spawning an executor.
    pub fn new(
        cancellation: CancellationToken,
        yielded_tx: oneshot::Sender<Task>,
        spawned: JoinHandle<()>,
    ) -> Self {
        Self {
            cancellation,
            yielded_tx: Mutex::new(Some(yielded_tx)),
            yielded_fired: AtomicBool::new(false),
            spawned: Mutex::new(Some(spawned)),
        }
    }

    /// Fire the `yielded` signal with the given task snapshot.
    ///
    /// Returns `true` if this call won the CAS and sent the task; `false` if
    /// another thread already fired. The fire is atomic with respect to the
    /// flag — a loser's `task` argument is dropped without reaching the
    /// receiver.
    ///
    /// Used by the commit hook in phase D when a terminal or interrupted
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
    /// Used by phase D's blocking-send timeout path to decide between waiting
    /// and forcing a framework terminal.
    pub fn yielded_fired(&self) -> bool {
        self.yielded_fired.load(Ordering::Acquire)
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
/// `dashmap` dependency is not justified for phase A. If profiling in phase
/// D or E surfaces contention, this type can be swapped for `DashMap`
/// without changing any caller's API.
#[derive(Default)]
pub struct InFlightRegistry {
    map: RwLock<HashMap<InFlightKey, Arc<InFlightHandle>>>,
}

impl InFlightRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert an entry. Overwrites any previous entry for the same key —
    /// which should not happen in practice (spawns have unique `task_id`).
    pub fn insert(&self, key: InFlightKey, handle: Arc<InFlightHandle>) {
        self.map
            .write()
            .expect("InFlightRegistry RwLock poisoned")
            .insert(key, handle);
    }

    /// Get a cloned Arc to the handle, if present.
    pub fn get(&self, key: &InFlightKey) -> Option<Arc<InFlightHandle>> {
        self.map
            .read()
            .expect("InFlightRegistry RwLock poisoned")
            .get(key)
            .cloned()
    }

    /// Remove and return the handle, if present. Idempotent.
    pub fn remove(&self, key: &InFlightKey) -> Option<Arc<InFlightHandle>> {
        self.map
            .write()
            .expect("InFlightRegistry RwLock poisoned")
            .remove(key)
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
}

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
/// timeout path is itself sentinel-guarded in phase D.
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

        // 1. Abort the spawned task if the supervisor's normal path didn't
        //    take and await it. This is the load-bearing panic-safety
        //    guarantee — a panicked supervisor cannot leave a live executor.
        if let Some(handle) = self.handle.take_spawned() {
            if !handle.is_finished() {
                handle.abort();
            }
        }

        // 2. Remove the registry entry. Idempotent if the supervisor's
        //    normal path already removed it.
        self.registry.remove(&self.key);

        // 3. If the unwind was a panic, emit the observability signal. Note:
        //    tracing macros during panic unwind are safe — tracing's
        //    Dispatch acquires no locks that could be poisoned, and the
        //    current subscriber is looked up via thread-local / WithSubscriber
        //    context that remains live until the future's state is fully
        //    dropped.
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
