//! HTTP router matching proto google.api.http annotations.
//!
//! Routes use axum wildcard catch-all for task paths because the proto
//! uses `{id=*}:action` patterns that don't map directly to axum's `:param` syntax.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::error::A2aError;
use crate::executor::AgentExecutor;
use crate::storage::{A2aPushNotificationStorage, A2aStorageError, A2aTaskStorage, TaskFilter, TaskListPage};
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};

/// Shared server state.
#[derive(Clone)]
pub struct AppState {
    pub executor: Arc<dyn AgentExecutor>,
    pub task_storage: Arc<dyn A2aTaskStorage>,
    pub push_storage: Arc<dyn A2aPushNotificationStorage>,
}

/// Build the axum router with all proto-defined routes.
pub fn build_router(state: AppState) -> Router {
    let router = Router::new()
        // Agent card discovery
        .route("/.well-known/agent-card.json", get(agent_card_handler))
        .route("/extendedAgentCard", get(extended_agent_card_handler))
        // Message operations (proto lines 23, 35)
        .route("/message:send", post(send_message_handler))
        .route("/message:stream", post(send_streaming_message_handler))
        // Task list (proto line 57)
        .route("/tasks", get(list_tasks_handler))
        // Task operations via wildcard (proto lines 47, 66, 78, 92-139)
        .route(
            "/tasks/{*rest}",
            get(task_get_dispatch)
                .post(task_post_dispatch)
                .delete(task_delete_dispatch),
        )
        // Tenant-prefixed routes (proto additional_bindings)
        .route("/{tenant}/message:send", post(send_message_handler))
        .route("/{tenant}/message:stream", post(send_streaming_message_handler))
        .route("/{tenant}/tasks", get(list_tasks_handler))
        .route("/{tenant}/extendedAgentCard", get(extended_agent_card_handler))
        .route(
            "/{tenant}/tasks/{*rest}",
            get(task_get_dispatch)
                .post(task_post_dispatch)
                .delete(task_delete_dispatch),
        );

    router.with_state(state)
}

// =========================================================
// Task path parsing
// =========================================================

/// Parsed task path action from the wildcard segment.
///
/// For push notification config paths, the same parse result is used for
/// multiple HTTP methods (GET=list/get, POST=create, DELETE=delete).
/// The dispatch functions disambiguate by HTTP method.
#[derive(Debug, PartialEq)]
enum TaskAction {
    /// /tasks/{id} — GET=GetTask
    GetTask(String),
    /// /tasks/{id}:cancel — POST=CancelTask
    CancelTask(String),
    /// /tasks/{id}:subscribe — GET=SubscribeToTask (SSE)
    SubscribeToTask(String),
    /// /tasks/{task_id}/pushNotificationConfigs — GET=list, POST=create
    PushConfigCollection(String),
    /// /tasks/{task_id}/pushNotificationConfigs/{config_id} — GET=get, DELETE=delete
    PushConfigItem(String, String),
}

fn parse_task_path(rest: &str) -> Option<TaskAction> {
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let parts: Vec<&str> = rest.split('/').collect();

    match parts.as_slice() {
        // /tasks/{id}:cancel or /tasks/{id}:subscribe or /tasks/{id}
        [segment] => {
            if let Some(id) = segment.strip_suffix(":cancel") {
                Some(TaskAction::CancelTask(id.to_string()))
            } else if let Some(id) = segment.strip_suffix(":subscribe") {
                Some(TaskAction::SubscribeToTask(id.to_string()))
            } else {
                Some(TaskAction::GetTask(segment.to_string()))
            }
        }
        // /tasks/{task_id}/pushNotificationConfigs — disambiguated by HTTP method in dispatch
        [task_id, "pushNotificationConfigs"] => {
            Some(TaskAction::PushConfigCollection(task_id.to_string()))
        }
        // /tasks/{task_id}/pushNotificationConfigs/{config_id} — disambiguated by HTTP method
        [task_id, "pushNotificationConfigs", config_id] => Some(TaskAction::PushConfigItem(
            task_id.to_string(),
            config_id.to_string(),
        )),
        _ => None,
    }
}

// =========================================================
// Dispatch handlers for wildcard task routes
// =========================================================

async fn task_get_dispatch(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    Query(query): Query<GetTaskQuery>,
) -> Result<Json<serde_json::Value>, A2aError> {
    match parse_task_path(&rest) {
        Some(TaskAction::GetTask(id)) => get_task_handler(state, &id, query.history_length).await,
        Some(TaskAction::SubscribeToTask(_id)) => Err(A2aError::UnsupportedOperation {
            message: "Streaming not implemented in v0.1".into(),
        }),
        // GET on pushNotificationConfigs collection = list
        Some(TaskAction::PushConfigCollection(task_id)) => {
            list_push_configs_handler(state, &task_id).await
        }
        // GET on pushNotificationConfigs/{id} = get single
        Some(TaskAction::PushConfigItem(task_id, config_id)) => {
            get_push_config_handler(state, &task_id, &config_id).await
        }
        _ => Err(A2aError::InvalidRequest {
            message: "Invalid task path".into(),
        }),
    }
}

async fn task_post_dispatch(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    body: String,
) -> Result<Json<serde_json::Value>, A2aError> {
    match parse_task_path(&rest) {
        Some(TaskAction::CancelTask(id)) => cancel_task_handler(state, &id).await,
        // POST on pushNotificationConfigs = create
        Some(TaskAction::PushConfigCollection(task_id)) => {
            create_push_config_handler(state, &task_id, body).await
        }
        _ => Err(A2aError::InvalidRequest {
            message: "Invalid task path".into(),
        }),
    }
}

async fn task_delete_dispatch(
    State(state): State<AppState>,
    Path(rest): Path<String>,
) -> Result<Json<serde_json::Value>, A2aError> {
    match parse_task_path(&rest) {
        // DELETE on pushNotificationConfigs/{id} = delete
        Some(TaskAction::PushConfigItem(task_id, config_id)) => {
            delete_push_config_handler(state, &task_id, &config_id).await
        }
        _ => Err(A2aError::InvalidRequest {
            message: "Invalid task path".into(),
        }),
    }
}

// =========================================================
// Handler stubs — return spec-correct envelopes/errors
// =========================================================

async fn agent_card_handler(
    State(state): State<AppState>,
) -> impl IntoResponse {
    Json(serde_json::to_value(state.executor.agent_card()).unwrap_or_default())
}

async fn extended_agent_card_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, A2aError> {
    // TODO: check auth claims from middleware
    match state.executor.extended_agent_card(None) {
        Some(card) => Ok(Json(serde_json::to_value(card).unwrap_or_default())),
        None => Err(A2aError::ExtendedAgentCardNotConfigured),
    }
}

// Default tenant/owner for unauthenticated requests.
// Auth middleware would override these from JWT claims.
const DEFAULT_TENANT: &str = "";
const DEFAULT_OWNER: &str = "anonymous";

async fn send_message_handler(
    State(state): State<AppState>,
    body: String,
) -> Result<Json<serde_json::Value>, A2aError> {
    // Parse SendMessageRequest
    let request: turul_a2a_proto::SendMessageRequest = serde_json::from_str(&body)
        .map_err(|e| A2aError::InvalidRequest {
            message: format!("Invalid request body: {e}"),
        })?;

    let proto_message = request.message.ok_or(A2aError::InvalidRequest {
        message: "message field is required".into(),
    })?;

    let message = Message::try_from(proto_message).map_err(|e| A2aError::InvalidRequest {
        message: format!("Invalid message: {e}"),
    })?;

    let tenant = if request.tenant.is_empty() {
        DEFAULT_TENANT
    } else {
        &request.tenant
    };

    // Create a new task with Submitted status
    let task_id = uuid::Uuid::now_v7().to_string();
    let context_id = if message.as_proto().context_id.is_empty() {
        uuid::Uuid::now_v7().to_string()
    } else {
        message.as_proto().context_id.clone()
    };

    let mut task = Task::new(&task_id, TaskStatus::new(TaskState::Submitted))
        .with_context_id(&context_id);
    task.append_message(message.clone());

    // Persist initial task
    state
        .task_storage
        .create_task(tenant, DEFAULT_OWNER, task)
        .await
        .map_err(A2aError::from)?;

    // Move to Working
    let task = state
        .task_storage
        .update_task_status(tenant, &task_id, DEFAULT_OWNER, TaskStatus::new(TaskState::Working))
        .await
        .map_err(A2aError::from)?;

    // Execute agent logic
    let mut task = task;
    state.executor.execute(&mut task, &message).await?;

    // Persist final state
    state
        .task_storage
        .update_task(tenant, DEFAULT_OWNER, task.clone())
        .await
        .map_err(A2aError::from)?;

    // Return SendMessageResponse with task variant
    let response = serde_json::json!({
        "task": serde_json::to_value(&task).unwrap_or_default()
    });
    Ok(Json(response))
}

async fn send_streaming_message_handler(
    State(_state): State<AppState>,
) -> Result<Json<serde_json::Value>, A2aError> {
    Err(A2aError::UnsupportedOperation {
        message: "Streaming not implemented in v0.1".into(),
    })
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct ListTasksQuery {
    #[serde(rename = "contextId")]
    context_id: Option<String>,
    status: Option<String>,
    #[serde(rename = "pageSize")]
    page_size: Option<i32>,
    #[serde(rename = "pageToken")]
    page_token: Option<String>,
    #[serde(rename = "historyLength")]
    history_length: Option<i32>,
    #[serde(rename = "includeArtifacts")]
    include_artifacts: Option<bool>,
}

async fn list_tasks_handler(
    State(state): State<AppState>,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<serde_json::Value>, A2aError> {
    let filter = TaskFilter {
        tenant: Some(DEFAULT_TENANT.to_string()),
        owner: Some(DEFAULT_OWNER.to_string()),
        context_id: query.context_id,
        status: None, // TODO: parse TaskState from query.status string
        page_size: query.page_size,
        page_token: query.page_token,
        history_length: query.history_length,
        include_artifacts: query.include_artifacts,
        ..Default::default()
    };

    let page = state
        .task_storage
        .list_tasks(filter)
        .await
        .map_err(A2aError::from)?;

    let response = list_page_to_json(&page);
    Ok(Json(response))
}

fn list_page_to_json(page: &TaskListPage) -> serde_json::Value {
    let tasks: Vec<serde_json::Value> = page
        .tasks
        .iter()
        .map(|t| serde_json::to_value(t).unwrap_or_default())
        .collect();
    serde_json::json!({
        "tasks": tasks,
        "nextPageToken": page.next_page_token,
        "pageSize": page.page_size,
        "totalSize": page.total_size,
    })
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct GetTaskQuery {
    #[serde(rename = "historyLength")]
    history_length: Option<i32>,
}

async fn get_task_handler(
    state: AppState,
    task_id: &str,
    history_length: Option<i32>,
) -> Result<Json<serde_json::Value>, A2aError> {
    let task = state
        .task_storage
        .get_task(DEFAULT_TENANT, task_id, DEFAULT_OWNER, history_length)
        .await
        .map_err(A2aError::from)?
        .ok_or_else(|| A2aError::TaskNotFound {
            task_id: task_id.to_string(),
        })?;

    Ok(Json(serde_json::to_value(&task).unwrap_or_default()))
}

async fn cancel_task_handler(
    state: AppState,
    task_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    // Try to transition to Canceled
    let result = state
        .task_storage
        .update_task_status(
            DEFAULT_TENANT,
            task_id,
            DEFAULT_OWNER,
            TaskStatus::new(TaskState::Canceled),
        )
        .await;

    match result {
        Ok(task) => Ok(Json(serde_json::to_value(&task).unwrap_or_default())),
        Err(A2aStorageError::TaskNotFound(id)) => Err(A2aError::TaskNotFound { task_id: id }),
        Err(A2aStorageError::TerminalState(_)) | Err(A2aStorageError::InvalidTransition { .. }) => {
            Err(A2aError::TaskNotCancelable {
                task_id: task_id.to_string(),
            })
        }
        Err(other) => Err(A2aError::from(other)),
    }
}

async fn create_push_config_handler(
    state: AppState,
    task_id: &str,
    body: String,
) -> Result<Json<serde_json::Value>, A2aError> {
    let mut config: turul_a2a_proto::TaskPushNotificationConfig = serde_json::from_str(&body)
        .map_err(|e| A2aError::InvalidRequest {
            message: format!("Invalid push config: {e}"),
        })?;
    config.task_id = task_id.to_string();

    let created = state
        .push_storage
        .create_config(DEFAULT_TENANT, config)
        .await
        .map_err(A2aError::from)?;

    Ok(Json(serde_json::to_value(&created).unwrap_or_default()))
}

async fn list_push_configs_handler(
    state: AppState,
    task_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    let page = state
        .push_storage
        .list_configs(DEFAULT_TENANT, task_id, None, None)
        .await
        .map_err(A2aError::from)?;

    let configs: Vec<serde_json::Value> = page
        .configs
        .iter()
        .map(|c| serde_json::to_value(c).unwrap_or_default())
        .collect();

    Ok(Json(serde_json::json!({
        "configs": configs,
        "nextPageToken": page.next_page_token,
    })))
}

async fn get_push_config_handler(
    state: AppState,
    task_id: &str,
    config_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    let config = state
        .push_storage
        .get_config(DEFAULT_TENANT, task_id, config_id)
        .await
        .map_err(A2aError::from)?
        .ok_or_else(|| A2aError::TaskNotFound {
            task_id: format!("push config {config_id} for task {task_id}"),
        })?;

    Ok(Json(serde_json::to_value(&config).unwrap_or_default()))
}

async fn delete_push_config_handler(
    state: AppState,
    task_id: &str,
    config_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    state
        .push_storage
        .delete_config(DEFAULT_TENANT, task_id, config_id)
        .await
        .map_err(A2aError::from)?;

    Ok(Json(serde_json::json!({})))
}

// IntoResponse for A2aError — returns AIP-193 HTTP error body
impl IntoResponse for A2aError {
    fn into_response(self) -> axum::response::Response {
        let status = axum::http::StatusCode::from_u16(self.http_status())
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let body = self.to_http_error_body();
        (status, Json(body)).into_response()
    }
}

// =========================================================
// Path parsing tests
// =========================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_get_task() {
        assert_eq!(
            parse_task_path("abc-123"),
            Some(TaskAction::GetTask("abc-123".into()))
        );
        assert_eq!(
            parse_task_path("/abc-123"),
            Some(TaskAction::GetTask("abc-123".into()))
        );
    }

    #[test]
    fn parse_cancel_task() {
        assert_eq!(
            parse_task_path("abc-123:cancel"),
            Some(TaskAction::CancelTask("abc-123".into()))
        );
    }

    #[test]
    fn parse_subscribe_to_task() {
        assert_eq!(
            parse_task_path("abc-123:subscribe"),
            Some(TaskAction::SubscribeToTask("abc-123".into()))
        );
    }

    #[test]
    fn parse_push_config_collection() {
        // The collection path parses as CreatePushConfig; HTTP method disambiguates GET=list vs POST=create
        assert_eq!(
            parse_task_path("task-1/pushNotificationConfigs"),
            Some(TaskAction::PushConfigCollection("task-1".into()))
        );
    }

    #[test]
    fn parse_push_config_item() {
        // The item path parses as DeletePushConfig; HTTP method disambiguates GET=get vs DELETE=delete
        assert_eq!(
            parse_task_path("task-1/pushNotificationConfigs/cfg-1"),
            Some(TaskAction::PushConfigItem("task-1".into(), "cfg-1".into()))
        );
    }

    #[test]
    fn parse_invalid_path() {
        assert_eq!(parse_task_path("a/b/c/d"), None);
    }
}
