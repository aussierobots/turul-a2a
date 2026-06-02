//! Backend-agnostic retention (TTL) configuration.
//!
//! Drives age-based cleanup of expired events and tasks across every
//! storage backend. A non-zero TTL makes [`super::A2aEventStore::cleanup_expired`]
//! delete rows older than the configured window; `0` disables expiry for
//! that row class.
//!
//! Age-based expiry is state-independent: a task past `task_ttl_seconds`
//! is reaped regardless of its `TaskState`. Setting a non-zero task TTL
//! can therefore reap long-running live tasks — choose a window larger
//! than the longest expected task lifetime, or leave it `0`.
//!
//! Cleanup deletes in bounded batches of `cleanup_batch_size` rows,
//! committing each batch, so a large backlog does not hold one
//! long-running transaction (SQL) or the write lock for the whole sweep
//! (in-memory). A single `cleanup_expired` call still drains everything
//! eligible; it just does so incrementally.

/// Default number of rows deleted per committed cleanup batch.
pub const DEFAULT_CLEANUP_BATCH_SIZE: usize = 1000;

/// TTL window for expired events and tasks, plus the cleanup batch size.
///
/// `0` for either TTL means "no expiry" for that row class — the default
/// for SQL and in-memory backends. DynamoDB does not use this type; it
/// reaps via engine-native TTL configured on its own `DynamoDbConfig`.
///
/// Construct with [`RetentionConfig::new`] and refine the batch size with
/// [`RetentionConfig::with_batch_size`]; the struct is `non_exhaustive`
/// so future knobs can be added without breaking call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RetentionConfig {
    /// Maximum task age in seconds before it is eligible for cleanup.
    /// `0` disables task expiry. Measured against the task's
    /// `updated_at` (last status/content write).
    pub task_ttl_seconds: u64,
    /// Maximum event age in seconds before it is eligible for cleanup.
    /// `0` disables event expiry. Measured against the event's
    /// `created_at` (append time).
    pub event_ttl_seconds: u64,
    /// Rows deleted per committed batch during cleanup. Bounds the size
    /// of each delete transaction and lock hold; cleanup loops over
    /// batches until the backlog is drained. Clamped to at least 1.
    pub cleanup_batch_size: usize,
}

impl RetentionConfig {
    /// No expiry for either events or tasks. The default for SQL and
    /// in-memory backends — cleanup deletes nothing until a TTL is set.
    pub const DISABLED: RetentionConfig = RetentionConfig {
        task_ttl_seconds: 0,
        event_ttl_seconds: 0,
        cleanup_batch_size: DEFAULT_CLEANUP_BATCH_SIZE,
    };

    /// Retention with the given TTLs (seconds; `0` disables that class)
    /// and the default cleanup batch size.
    pub const fn new(task_ttl_seconds: u64, event_ttl_seconds: u64) -> Self {
        Self {
            task_ttl_seconds,
            event_ttl_seconds,
            cleanup_batch_size: DEFAULT_CLEANUP_BATCH_SIZE,
        }
    }

    /// Override the cleanup batch size. Values below 1 are treated as 1
    /// by the backends at sweep time.
    pub fn with_batch_size(mut self, cleanup_batch_size: usize) -> Self {
        self.cleanup_batch_size = cleanup_batch_size;
        self
    }

    /// Cleanup batch size, clamped to at least 1.
    pub(crate) fn batch(&self) -> usize {
        self.cleanup_batch_size.max(1)
    }
}

impl Default for RetentionConfig {
    /// No expiry. Backends that want bounded retention by default
    /// (DynamoDB) configure it on their own config.
    fn default() -> Self {
        Self::DISABLED
    }
}
