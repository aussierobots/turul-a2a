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
