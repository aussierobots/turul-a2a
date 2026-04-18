//! Atomic task+event write boundary (ADR-009 §10).
//!
//! Guarantees that task mutations and event appends happen in a single
//! backend-owned consistency boundary. No partial commits.
//!
//! Read operations remain on `A2aTaskStorage` and `A2aEventStore`.
//! This trait handles writes where task state and events must be consistent.

use async_trait::async_trait;
use turul_a2a_types::{Task, TaskStatus};

use crate::streaming::StreamEvent;
use super::error::A2aStorageError;

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
    ///   clause excludes terminal states (equivalent to `WHERE
    ///   status_state NOT IN ('TASK_STATE_COMPLETED', …)`). Affected-rows
    ///   equal to zero ⇒ terminal was already set and no task/event writes
    ///   committed (the surrounding transaction is rolled back).
    /// - **DynamoDB**: a `TransactWriteItems` with a `ConditionExpression`
    ///   on the task item's `statusState` attribute asserting it is a
    ///   non-terminal. `TransactionCanceledException` with the task-item's
    ///   condition-check failure ⇒ terminal was already set.
    ///
    /// `TerminalStateAlreadySet` is **distinct** from
    /// [`A2aStorageError::InvalidTransition`]. The latter is the
    /// state-machine's illegal-transition signal (e.g. `SUBMITTED →
    /// INPUT_REQUIRED`); the former is specifically "you lost the race on
    /// a terminal write." Callers that need to distinguish the two (e.g.
    /// `EventSink` translation in phase D) match on the variant.
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
    async fn update_task_with_events(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
        events: Vec<StreamEvent>,
    ) -> Result<Vec<u64>, A2aStorageError>;
}
