use chrono::{DateTime, Utc};
use turul_a2a_types::{Task, TaskState};

/// Filtering and pagination parameters for task listing.
#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub tenant: Option<String>,
    pub owner: Option<String>,
    pub context_id: Option<String>,
    pub status: Option<TaskState>,
    /// Max 100, default 50, min 1. Clamped by implementations.
    pub page_size: Option<i32>,
    pub page_token: Option<String>,
    /// 0 = omit history, None = no limit, Some(n) = last n messages.
    pub history_length: Option<i32>,
    pub status_timestamp_after: Option<DateTime<Utc>>,
    /// Default false — omit artifacts to reduce payload.
    pub include_artifacts: Option<bool>,
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
