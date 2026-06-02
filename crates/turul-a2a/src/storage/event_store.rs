//! Durable event store trait for cross-instance streaming coordination.
//!
//! Event store and task store MUST share the same backend: a single
//! storage instance implements both `A2aTaskStorage` and
//! `A2aEventStore`. The builder rejects split configurations.

use async_trait::async_trait;

use super::error::A2aStorageError;
use crate::streaming::StreamEvent;

/// Durable event store for streaming coordination.
///
/// Source of truth for task events. The in-process broker is a local
/// optimization for attached clients — this trait provides correctness.
///
/// Events are tenant-scoped and monotonically ordered per task.
#[async_trait]
pub trait A2aEventStore: Send + Sync {
    fn backend_name(&self) -> &'static str;

    /// Append an event for a task. Returns the assigned sequence number.
    /// The store assigns the sequence atomically.
    async fn append_event(
        &self,
        tenant: &str,
        task_id: &str,
        event: StreamEvent,
    ) -> Result<u64, A2aStorageError>;

    /// Get all events for a task after a given sequence number.
    /// Returns events in sequence order. Tenant-scoped.
    async fn get_events_after(
        &self,
        tenant: &str,
        task_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, StreamEvent)>, A2aStorageError>;

    /// Get the latest event sequence number for a task.
    /// Returns 0 if no events exist. Tenant-scoped.
    async fn latest_sequence(&self, tenant: &str, task_id: &str) -> Result<u64, A2aStorageError>;

    /// Delete all expired events and tasks based on the configured TTL.
    ///
    /// Age-based, state-independent expiry: events older than the
    /// configured event TTL (measured from each event's `created_at`)
    /// and tasks older than the configured task TTL (measured from each
    /// task's `updated_at`) are deleted regardless of `TaskState` — a
    /// terminal task and a long-running live task past the window are
    /// reaped alike.
    ///
    /// Returns the total count of deleted rows (events + tasks combined).
    /// Idempotent: safe to call repeatedly. When a backend has no TTL
    /// configured (`0`), the corresponding row class is never deleted.
    ///
    /// DynamoDB returns `Ok(0)`: it reaps expired items via native TTL
    /// attributes, so application-level cleanup is bypassed there.
    async fn cleanup_expired(&self) -> Result<u64, A2aStorageError>;
}
