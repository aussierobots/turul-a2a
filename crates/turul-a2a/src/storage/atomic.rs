//! Atomic task+event write boundary (ADR-009 §10).
//!
//! Guarantees that task mutations and event appends happen in a single
//! backend-owned consistency boundary. No partial commits.
//!
//! Read operations remain on `A2aTaskStorage` and `A2aEventStore`.
//! This trait handles writes where task state and events must be consistent.

use async_trait::async_trait;
use turul_a2a_types::{Task, TaskStatus};

use super::error::A2aStorageError;
use crate::streaming::StreamEvent;

/// Atomic task+event write operations.
///
/// Backend implementations own the consistency boundary:
/// - In-memory: all maps locked together for duration of operation
/// - PostgreSQL/SQLite: single database transaction
/// - DynamoDB: `TransactWriteItems`
///
/// Prevents:
/// - "event committed, task failed"
/// - "task committed, event failed"
#[async_trait]
pub trait A2aAtomicStore: Send + Sync {
    fn backend_name(&self) -> &'static str;

    /// Does this atomic store also write pending-dispatch markers atomically
    /// with terminal status commits? (ADR-013 §4.3, §6.1 — normative)
    ///
    /// `false` (default): task + event rows only. Non-push deployments never
    /// touch `a2a_push_pending_dispatches`; no marker writes, no IAM, no
    /// table to provision.
    ///
    /// `true`: after the task and event rows are written in the native
    /// transaction, the implementation iterates the events and, for each
    /// terminal `StreamEvent::StatusUpdate`, writes a row to
    /// `a2a_push_pending_dispatches` in the same transaction. Failure of
    /// the marker write rolls the whole transaction back.
    ///
    /// Opted in via a backend-specific constructor (for example
    /// `InMemoryA2aStorage::with_push_dispatch_enabled(true)`). The server
    /// and Lambda builders reject inconsistent wiring in both directions:
    /// `push_delivery_store` present with the flag off is a build error;
    /// the flag on with no `push_delivery_store` is also a build error
    /// (ADR-013 §4.3) — pending-dispatch markers are never written without
    /// a consumer.
    fn push_dispatch_enabled(&self) -> bool {
        false
    }

    /// Create a task and append initial events atomically.
    /// Returns the created task and assigned event sequences.
    async fn create_task_with_events(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
        events: Vec<StreamEvent>,
    ) -> Result<(Task, Vec<u64>), A2aStorageError>;

    /// Update a task's status and append events atomically.
    /// Validates state machine transition. Returns updated task and sequences.
    ///
    /// # Terminal-write CAS contract (ADR-010 §7.1 — normative)
    ///
    /// If the persisted task is already in a terminal state
    /// (`COMPLETED`, `FAILED`, `CANCELED`, `REJECTED`) at the time this
    /// method commits, the implementation **MUST** return
    /// [`A2aStorageError::TerminalStateAlreadySet`] and MUST NOT:
    ///
    /// - mutate the task's persisted state, or
    /// - append any events from the `events` argument to the event store.
    ///
    /// The check-and-write MUST happen in one backend-native atomic
    /// boundary so that concurrent callers racing on terminal writes
    /// resolve to exactly one winner. Acceptable mechanisms per backend:
    ///
    /// - **In-memory**: the entire operation runs under the same write
    ///   lock as the state inspection; the in-process lock is the
    ///   boundary.
    /// - **SQLite / PostgreSQL**: a conditional `UPDATE` whose `WHERE`
    ///   clause excludes terminal states. The `status_state` column
    ///   stores Rust `Debug` forms of [`turul_a2a_types::TaskState`], so
    ///   the conditional is `WHERE status_state NOT IN ('Completed',
    ///   'Failed', 'Canceled', 'Rejected')`. Affected-rows equal to zero
    ///   ⇒ terminal was already set and no task/event writes commit (the
    ///   surrounding transaction is rolled back).
    /// - **DynamoDB**: a `TransactWriteItems` with a `ConditionExpression`
    ///   on the task item's `statusState` attribute asserting it is not
    ///   one of the same four Debug-format values (`Completed`, `Failed`,
    ///   `Canceled`, `Rejected`). A `TransactionCanceledException`
    ///   referencing the task-item's condition-check failure ⇒ terminal
    ///   was already set; the transaction as a whole aborts so no events
    ///   commit.
    ///
    /// Storage column values (`"Completed"`, etc.) are distinct from the
    /// proto wire names (`"TASK_STATE_COMPLETED"`, etc.). Error reporting
    /// on [`A2aStorageError::TerminalStateAlreadySet`] converts to the
    /// wire name so log and telemetry consumers see spec-compliant
    /// identifiers. See [`crate::storage::terminal_cas`] for the helper
    /// functions.
    ///
    /// `TerminalStateAlreadySet` is **distinct** from
    /// [`A2aStorageError::InvalidTransition`]. The latter is the
    /// state-machine's illegal-transition signal (e.g. `SUBMITTED →
    /// INPUT_REQUIRED`); the former is specifically "you lost the race on
    /// a terminal write." Callers that need to distinguish the two
    /// (e.g. the executor-side [`crate::event_sink::EventSink`] error
    /// translation) match on the variant.
    ///
    /// Concurrent-write parity tests (`parity_tests::terminal_cas_*`) gate
    /// acceptance of each backend implementation.
    async fn update_task_status_with_events(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        new_status: TaskStatus,
        events: Vec<StreamEvent>,
    ) -> Result<(Task, Vec<u64>), A2aStorageError>;

    /// Full replacement update of a task and append events atomically.
    /// Returns assigned event sequences.
    ///
    /// # Terminal-preservation CAS contract (ADR-010 §7.1 extension — normative)
    ///
    /// If the **persisted** task is already in a terminal state
    /// (`COMPLETED`, `FAILED`, `CANCELED`, `REJECTED`) at the time this
    /// method commits, the implementation **MUST** return
    /// [`A2aStorageError::TerminalStateAlreadySet`] and MUST NOT mutate
    /// the task row nor append any events. This applies regardless of
    /// the status field on the incoming `task` argument — a full-task
    /// replacement must not overwrite a concurrently-committed terminal.
    ///
    /// Rationale: `EventSink::emit_artifact` reads the current task,
    /// mutates a local copy, then calls this method to persist. Between
    /// the read and the write, another writer (the cancel handler's
    /// force-commit, the executor's own terminal on a different task
    /// future, or the framework's hard-deadline commit) may have
    /// committed a terminal. A naive full-replacement would silently
    /// roll back that terminal and defeat the CAS that
    /// [`Self::update_task_status_with_events`] so carefully enforces.
    ///
    /// Per-backend enforcement:
    ///
    /// - **In-memory**: the terminal check runs under the same write
    ///   lock as the replacement — inspect the stored task's current
    ///   status before overwriting; if terminal, return the error.
    /// - **SQLite / PostgreSQL**: the conditional `UPDATE`'s `WHERE`
    ///   clause excludes terminal `status_state` values (same set as
    ///   [`crate::storage::terminal_cas::DEBUG_TERMINAL_STATES`]).
    ///   Affected-rows = 0 on an existing task ⇒ terminal already set;
    ///   the surrounding transaction rolls back so no events commit.
    /// - **DynamoDB**: the task-item's `Update` carries a
    ///   `ConditionExpression` asserting `statusState NOT IN
    ///   (:completed, :failed, :canceled, :rejected)`. A
    ///   `TransactionCanceledException` citing the condition-check
    ///   failure ⇒ terminal already set; the transaction as a whole
    ///   aborts so no events commit.
    ///
    /// This clause is distinct from terminal transition validation —
    /// [`Self::update_task_status_with_events`] rejects a terminal-to-
    /// terminal write attempt; this method rejects any write (including
    /// an innocuous artifact-only update) performed against a task
    /// whose persisted state is terminal. Together the two clauses
    /// ensure that once a terminal is persisted, it is immutable from
    /// every write path in the trait.
    async fn update_task_with_events(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
        events: Vec<StreamEvent>,
    ) -> Result<Vec<u64>, A2aStorageError>;
}
