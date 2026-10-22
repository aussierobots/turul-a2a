use chrono::{DateTime, Utc};
use turul_a2a_types::{Task, TaskState};

use super::error::A2aStorageError;

/// Page size when a listing request leaves it unset (a2a.proto
/// `ListTasksRequest.page_size`: "If unspecified, at most 50 tasks will be
/// returned").
pub const DEFAULT_PAGE_SIZE: i32 = 50;
/// Largest page a listing returns (a2a.proto `ListTasksRequest.page_size`:
/// "The maximum value is 100"). The minimum is 1.
pub const MAX_PAGE_SIZE: i32 = 100;

/// Page size for a push-config listing.
/// `ListTaskPushNotificationConfigsRequest.page_size` has no protocol
/// bounds: 0 or less (0 is the proto3 unset value) means the default, and
/// larger requests are capped at [`MAX_PAGE_SIZE`] — the field is "the
/// maximum number of configurations to return".
pub(crate) fn push_config_page_size(requested: Option<i32>) -> i32 {
    match requested {
        Some(n) if n > 0 => n.min(MAX_PAGE_SIZE),
        _ => DEFAULT_PAGE_SIZE,
    }
}

/// Filtering and pagination parameters for task listing.
#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub tenant: Option<String>,
    pub owner: Option<String>,
    pub context_id: Option<String>,
    pub status: Option<TaskState>,
    /// 1..=100; `None` means the default of 50. Out-of-range values are
    /// rejected, see [`TaskFilter::resolved_page_size`].
    pub page_size: Option<i32>,
    pub page_token: Option<String>,
    /// 0 = omit history, None = no limit, Some(n) = last n messages.
    pub history_length: Option<i32>,
    /// Only tasks whose status timestamp is at or after this instant.
    pub status_timestamp_after: Option<DateTime<Utc>>,
    /// Default false — omit artifacts to reduce payload.
    pub include_artifacts: Option<bool>,
}

impl TaskFilter {
    /// The page size this listing uses: the requested value, or the
    /// default when unset. A value outside `1..=100` is an invalid
    /// request, not something to clamp silently.
    pub fn resolved_page_size(&self) -> Result<i32, A2aStorageError> {
        match self.page_size {
            None => Ok(DEFAULT_PAGE_SIZE),
            Some(n) if (1..=MAX_PAGE_SIZE).contains(&n) => Ok(n),
            Some(n) => Err(A2aStorageError::InvalidListRequest(format!(
                "pageSize must be between 1 and {MAX_PAGE_SIZE} inclusive, got {n}"
            ))),
        }
    }

    /// Whether `task` passes the `status_timestamp_after` bound
    /// (inclusive, per `ListTasksRequest.status_timestamp_after`).
    pub(crate) fn admits_status_time(&self, task: &Task) -> bool {
        self.status_timestamp_after.is_none_or(|after| {
            super::status_time::sort_key(task) >= super::status_time::key_of(&after)
        })
    }
}

/// Paginated result for task listing.
///
/// All fields are REQUIRED per proto ListTasksResponse.
#[derive(Debug, Clone)]
pub struct TaskListPage {
    pub tasks: Vec<Task>,
    /// Always present; empty string = no more pages.
    pub next_page_token: String,
    pub page_size: i32,
    /// Total matching tasks before pagination.
    pub total_size: i32,
}

/// Paginated result for push notification config listing.
#[derive(Debug, Clone)]
pub struct PushConfigListPage {
    pub configs: Vec<turul_a2a_proto::TaskPushNotificationConfig>,
    pub next_page_token: String,
}
