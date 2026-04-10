//! HTTP router matching proto google.api.http annotations.
//!
//! Routes use axum wildcard catch-all for task paths because the proto
//! uses `{id=*}:action` patterns that don't map directly to axum's `:param` syntax.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::error::A2aError;
use crate::executor::AgentExecutor;
use crate::storage::{A2aPushNotificationStorage, A2aStorageError, A2aTaskStorage, TaskFilter, TaskListPage};
use crate::streaming::StreamEvent;
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};

/// Shared server state.
#[derive(Clone)]
pub struct AppState {
    pub executor: Arc<dyn AgentExecutor>,
    pub task_storage: Arc<dyn A2aTaskStorage>,
    pub push_storage: Arc<dyn A2aPushNotificationStorage>,
    pub event_broker: crate::streaming::TaskEventBroker,
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
        .route("/{tenant}/message:send", post(tenant_send_message_handler))
        .route("/{tenant}/message:stream", post(tenant_send_streaming_message_handler))
        .route("/{tenant}/tasks", get(tenant_list_tasks_handler))
        .route("/{tenant}/extendedAgentCard", get(extended_agent_card_handler))
        .route(
            "/{tenant}/tasks/{*rest}",
            get(tenant_task_get_dispatch)
                .post(tenant_task_post_dispatch)
                .delete(tenant_task_delete_dispatch),
        );

    // JSON-RPC endpoint
    let router = router.route(
        "/jsonrpc",
        post(crate::jsonrpc::jsonrpc_dispatch_handler),
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
// Dispatch — default tenant (primary routes)
// =========================================================

async fn task_get_dispatch(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    Query(query): Query<TaskGetCombinedQuery>,
) -> Result<axum::response::Response, A2aError> {
    dispatch_task_get(state, DEFAULT_TENANT, &rest, &query).await
}

async fn task_post_dispatch(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    dispatch_task_post(state, DEFAULT_TENANT, &rest, body).await
}

async fn task_delete_dispatch(
    State(state): State<AppState>,
    Path(rest): Path<String>,
) -> Result<axum::response::Response, A2aError> {
    dispatch_task_delete(state, DEFAULT_TENANT, &rest).await
}

// =========================================================
// Dispatch — tenant-prefixed routes
// =========================================================

async fn tenant_send_message_handler(
    State(state): State<AppState>,
    Path(tenant): Path<String>,
    body: String,
) -> Result<Json<serde_json::Value>, A2aError> {
    core_send_message(state, &tenant, body).await
}

async fn tenant_list_tasks_handler(
    State(state): State<AppState>,
    Path(tenant): Path<String>,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<serde_json::Value>, A2aError> {
    core_list_tasks(state, &tenant, &query).await
}

async fn tenant_task_get_dispatch(
    State(state): State<AppState>,
    Path((tenant, rest)): Path<(String, String)>,
    Query(query): Query<TaskGetCombinedQuery>,
) -> Result<axum::response::Response, A2aError> {
    dispatch_task_get(state, &tenant, &rest, &query).await
}

async fn tenant_task_post_dispatch(
    State(state): State<AppState>,
    Path((tenant, rest)): Path<(String, String)>,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    dispatch_task_post(state, &tenant, &rest, body).await
}

async fn tenant_task_delete_dispatch(
    State(state): State<AppState>,
    Path((tenant, rest)): Path<(String, String)>,
) -> Result<axum::response::Response, A2aError> {
    dispatch_task_delete(state, &tenant, &rest).await
}

// =========================================================
// Shared dispatch logic
// =========================================================

/// Combined query params — works for both task and push config GET routes.
/// axum parses all query params into a single struct; unused fields default.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct TaskGetCombinedQuery {
    #[serde(rename = "historyLength")]
    history_length: Option<i32>,
    #[serde(rename = "pageSize")]
    page_size: Option<i32>,
    #[serde(rename = "pageToken")]
    page_token: Option<String>,
}

async fn dispatch_task_get(
    state: AppState,
    tenant: &str,
    rest: &str,
    query: &TaskGetCombinedQuery,
) -> Result<axum::response::Response, A2aError> {
    match parse_task_path(rest) {
        Some(TaskAction::GetTask(id)) => {
            let Json(v) = core_get_task(state, tenant, &id, query.history_length).await?;
            Ok(Json(v).into_response())
        }
        Some(TaskAction::SubscribeToTask(id)) => {
            core_subscribe_to_task(state, tenant, &id).await
        }
        Some(TaskAction::PushConfigCollection(task_id)) => {
            let pq = PushConfigQuery {
                page_size: query.page_size,
                page_token: query.page_token.clone(),
            };
            let Json(v) = core_list_push_configs(state, tenant, &task_id, &pq).await?;
            Ok(Json(v).into_response())
        }
        Some(TaskAction::PushConfigItem(task_id, config_id)) => {
            let Json(v) = core_get_push_config(state, tenant, &task_id, &config_id).await?;
            Ok(Json(v).into_response())
        }
        _ => Err(A2aError::InvalidRequest { message: "Invalid task path".into() }),
    }
}

async fn dispatch_task_post(
    state: AppState,
    tenant: &str,
    rest: &str,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    match parse_task_path(rest) {
        Some(TaskAction::CancelTask(id)) => {
            let Json(v) = core_cancel_task(state, tenant, &id).await?;
            Ok(Json(v).into_response())
        }
        Some(TaskAction::PushConfigCollection(task_id)) => {
            let Json(v) = core_create_push_config(state, tenant, &task_id, body).await?;
            Ok(Json(v).into_response())
        }
        _ => Err(A2aError::InvalidRequest { message: "Invalid task path".into() }),
    }
}

async fn dispatch_task_delete(
    state: AppState,
    tenant: &str,
    rest: &str,
) -> Result<axum::response::Response, A2aError> {
    match parse_task_path(rest) {
        Some(TaskAction::PushConfigItem(task_id, config_id)) => {
            let Json(v) = core_delete_push_config(state, tenant, &task_id, &config_id).await?;
            Ok(Json(v).into_response())
        }
        _ => Err(A2aError::InvalidRequest { message: "Invalid task path".into() }),
    }
}

// =========================================================
// Handlers — agent card (no tenant scoping)
// =========================================================

async fn agent_card_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::to_value(state.executor.agent_card()).unwrap_or_default())
}

async fn extended_agent_card_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, A2aError> {
    match state.executor.extended_agent_card(None) {
        Some(card) => Ok(Json(serde_json::to_value(card).unwrap_or_default())),
        None => Err(A2aError::ExtendedAgentCardNotConfigured),
    }
}

async fn send_streaming_message_handler(
    State(state): State<AppState>,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    core_send_streaming_message(state, DEFAULT_TENANT, body).await
}

async fn tenant_send_streaming_message_handler(
    State(state): State<AppState>,
    Path(tenant): Path<String>,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    core_send_streaming_message(state, &tenant, body).await
}

pub(crate) async fn core_send_streaming_message(
    state: AppState,
    tenant: &str,
    body: String,
) -> Result<axum::response::Response, A2aError> {
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

    let task_id = uuid::Uuid::now_v7().to_string();
    let context_id = if message.as_proto().context_id.is_empty() {
        uuid::Uuid::now_v7().to_string()
    } else {
        message.as_proto().context_id.clone()
    };

    // Subscribe BEFORE creating the task so we don't miss events
    let rx = state.event_broker.subscribe(&task_id).await;

    // Create task
    let mut task = Task::new(&task_id, TaskStatus::new(TaskState::Submitted))
        .with_context_id(&context_id);
    task.append_message(message.clone());

    state
        .task_storage
        .create_task(tenant, DEFAULT_OWNER, task)
        .await
        .map_err(A2aError::from)?;

    // Publish initial status
    state
        .event_broker
        .publish_status_update(&task_id, &context_id, &TaskStatus::new(TaskState::Submitted))
        .await;

    // Spawn task execution in background
    let exec_state = state.clone();
    let exec_tenant = tenant.to_string();
    let exec_task_id = task_id.clone();
    let exec_context_id = context_id.clone();
    let exec_message = message.clone();
    tokio::spawn(async move {
        // Move to Working
        if let Ok(mut task) = exec_state
            .task_storage
            .update_task_status(&exec_tenant, &exec_task_id, DEFAULT_OWNER, TaskStatus::new(TaskState::Working))
            .await
        {
            exec_state
                .event_broker
                .publish_status_update(&exec_task_id, &exec_context_id, &TaskStatus::new(TaskState::Working))
                .await;

            // Execute agent logic
            let _ = exec_state.executor.execute(&mut task, &exec_message).await;

            // Persist final state
            let _ = exec_state
                .task_storage
                .update_task(&exec_tenant, DEFAULT_OWNER, task.clone())
                .await;

            // Publish final status
            if let Some(status) = task.status() {
                exec_state
                    .event_broker
                    .publish_status_update(&exec_task_id, &exec_context_id, &status)
                    .await;
            }
        }
    });

    // Return SSE stream
    Ok(make_sse_response(rx))
}

pub(crate) async fn core_subscribe_to_task(
    state: AppState,
    tenant: &str,
    task_id: &str,
) -> Result<axum::response::Response, A2aError> {
    // Verify task exists
    let task = state
        .task_storage
        .get_task(tenant, task_id, DEFAULT_OWNER, Some(0))
        .await
        .map_err(A2aError::from)?
        .ok_or_else(|| A2aError::TaskNotFound {
            task_id: task_id.to_string(),
        })?;

    // Check if task is already terminal
    if let Some(status) = task.status() {
        if let Ok(s) = status.state() {
            if s.is_terminal() {
                return Err(A2aError::UnsupportedOperation {
                    message: format!("Task {task_id} is already in terminal state {s:?}"),
                });
            }
        }
    }

    let rx = state.event_broker.subscribe(task_id).await;

    // Send current status as first event
    if let Some(status) = task.status() {
        state
            .event_broker
            .publish_status_update(task_id, task.context_id(), &status)
            .await;
    }

    Ok(make_sse_response(rx))
}

fn make_sse_response(
    rx: tokio::sync::broadcast::Receiver<StreamEvent>,
) -> axum::response::Response {
    let stream = BroadcastStream::new(rx).filter_map(|result| async move {
        match result {
            Ok(event) => {
                let json = serde_json::to_string(&event).ok()?;
                Some(Ok::<_, std::convert::Infallible>(Event::default().data(json)))
            }
            Err(_) => None, // Lagged — skip
        }
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// =========================================================
// Default-tenant axum handlers (delegate to core with DEFAULT_TENANT)
// =========================================================

const DEFAULT_TENANT: &str = "";
const DEFAULT_OWNER: &str = "anonymous";

async fn send_message_handler(
    State(state): State<AppState>,
    body: String,
) -> Result<Json<serde_json::Value>, A2aError> {
    core_send_message(state, DEFAULT_TENANT, body).await
}

async fn list_tasks_handler(
    State(state): State<AppState>,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<serde_json::Value>, A2aError> {
    core_list_tasks(state, DEFAULT_TENANT, &query).await
}

// =========================================================
// Query param structs
// =========================================================

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub(crate) struct ListTasksQuery {
    #[serde(rename = "contextId")]
    pub(crate) context_id: Option<String>,
    pub(crate) status: Option<String>,
    #[serde(rename = "pageSize")]
    pub(crate) page_size: Option<i32>,
    #[serde(rename = "pageToken")]
    pub(crate) page_token: Option<String>,
    #[serde(rename = "historyLength")]
    pub(crate) history_length: Option<i32>,
    #[serde(rename = "includeArtifacts")]
    pub(crate) include_artifacts: Option<bool>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub(crate) struct PushConfigQuery {
    #[serde(rename = "pageSize")]
    pub(crate) page_size: Option<i32>,
    #[serde(rename = "pageToken")]
    pub(crate) page_token: Option<String>,
}

// =========================================================
// Core handler functions — all take tenant explicitly
// =========================================================

fn parse_task_state(s: &str) -> Option<TaskState> {
    match s {
        "TASK_STATE_SUBMITTED" => Some(TaskState::Submitted),
        "TASK_STATE_WORKING" => Some(TaskState::Working),
        "TASK_STATE_COMPLETED" => Some(TaskState::Completed),
        "TASK_STATE_FAILED" => Some(TaskState::Failed),
        "TASK_STATE_CANCELED" => Some(TaskState::Canceled),
        "TASK_STATE_INPUT_REQUIRED" => Some(TaskState::InputRequired),
        "TASK_STATE_REJECTED" => Some(TaskState::Rejected),
        "TASK_STATE_AUTH_REQUIRED" => Some(TaskState::AuthRequired),
        _ => None,
    }
}

pub(crate) async fn core_send_message(
    state: AppState,
    tenant: &str,
    body: String,
) -> Result<Json<serde_json::Value>, A2aError> {
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

    let msg_task_id = message.as_proto().task_id.clone();

    // If message has a task_id, continue the existing task; otherwise create new
    let (task_id, is_continuation) = if !msg_task_id.is_empty() {
        // Verify the task exists and is accessible
        let existing = state
            .task_storage
            .get_task(tenant, &msg_task_id, DEFAULT_OWNER, None)
            .await
            .map_err(A2aError::from)?
            .ok_or_else(|| A2aError::TaskNotFound {
                task_id: msg_task_id.clone(),
            })?;

        // Only allow continuation for interrupted states (INPUT_REQUIRED, AUTH_REQUIRED)
        if let Some(status) = existing.status() {
            if let Ok(s) = status.state() {
                match s {
                    TaskState::InputRequired | TaskState::AuthRequired => {}
                    _ => {
                        return Err(A2aError::InvalidRequest {
                            message: format!(
                                "Task {msg_task_id} is in state {s:?}, only INPUT_REQUIRED or AUTH_REQUIRED tasks accept follow-up messages"
                            ),
                        });
                    }
                }
            }
        }

        (msg_task_id, true)
    } else {
        (uuid::Uuid::now_v7().to_string(), false)
    };

    if is_continuation {
        // Append message to existing task
        state
            .task_storage
            .append_message(tenant, &task_id, DEFAULT_OWNER, message.clone())
            .await
            .map_err(A2aError::from)?;

        // Move to Working if in interrupted state
        let task = state
            .task_storage
            .update_task_status(tenant, &task_id, DEFAULT_OWNER, TaskStatus::new(TaskState::Working))
            .await
            .map_err(A2aError::from)?;

        let mut task = task;
        state.executor.execute(&mut task, &message).await?;

        state
            .task_storage
            .update_task(tenant, DEFAULT_OWNER, task.clone())
            .await
            .map_err(A2aError::from)?;

        Ok(Json(serde_json::json!({
            "task": serde_json::to_value(&task).unwrap_or_default()
        })))
    } else {
        // Create new task
        let context_id = if message.as_proto().context_id.is_empty() {
            uuid::Uuid::now_v7().to_string()
        } else {
            message.as_proto().context_id.clone()
        };

        let mut task = Task::new(&task_id, TaskStatus::new(TaskState::Submitted))
            .with_context_id(&context_id);
        task.append_message(message.clone());

        state
            .task_storage
            .create_task(tenant, DEFAULT_OWNER, task)
            .await
            .map_err(A2aError::from)?;

        let task = state
            .task_storage
            .update_task_status(tenant, &task_id, DEFAULT_OWNER, TaskStatus::new(TaskState::Working))
            .await
            .map_err(A2aError::from)?;

        let mut task = task;
        state.executor.execute(&mut task, &message).await?;

        state
            .task_storage
            .update_task(tenant, DEFAULT_OWNER, task.clone())
            .await
            .map_err(A2aError::from)?;

        Ok(Json(serde_json::json!({
            "task": serde_json::to_value(&task).unwrap_or_default()
        })))
    }
}

pub(crate) async fn core_list_tasks(
    state: AppState,
    tenant: &str,
    query: &ListTasksQuery,
) -> Result<Json<serde_json::Value>, A2aError> {
    let status = match &query.status {
        Some(s) => Some(parse_task_state(s).ok_or_else(|| A2aError::InvalidRequest {
            message: format!("Invalid status value: {s}"),
        })?),
        None => None,
    };

    let filter = TaskFilter {
        tenant: Some(tenant.to_string()),
        owner: Some(DEFAULT_OWNER.to_string()),
        context_id: query.context_id.clone(),
        status,
        page_size: query.page_size,
        page_token: query.page_token.clone(),
        history_length: query.history_length,
        include_artifacts: query.include_artifacts,
        ..Default::default()
    };

    let page = state.task_storage.list_tasks(filter).await.map_err(A2aError::from)?;
    Ok(Json(list_page_to_json(&page)))
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

pub(crate) async fn core_get_task(
    state: AppState,
    tenant: &str,
    task_id: &str,
    history_length: Option<i32>,
) -> Result<Json<serde_json::Value>, A2aError> {
    let task = state
        .task_storage
        .get_task(tenant, task_id, DEFAULT_OWNER, history_length)
        .await
        .map_err(A2aError::from)?
        .ok_or_else(|| A2aError::TaskNotFound {
            task_id: task_id.to_string(),
        })?;

    Ok(Json(serde_json::to_value(&task).unwrap_or_default()))
}

pub(crate) async fn core_cancel_task(
    state: AppState,
    tenant: &str,
    task_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    let result = state
        .task_storage
        .update_task_status(tenant, task_id, DEFAULT_OWNER, TaskStatus::new(TaskState::Canceled))
        .await;

    match result {
        Ok(task) => Ok(Json(serde_json::to_value(&task).unwrap_or_default())),
        Err(A2aStorageError::TaskNotFound(id)) => Err(A2aError::TaskNotFound { task_id: id }),
        Err(A2aStorageError::TerminalState(_)) | Err(A2aStorageError::InvalidTransition { .. }) => {
            Err(A2aError::TaskNotCancelable { task_id: task_id.to_string() })
        }
        Err(other) => Err(A2aError::from(other)),
    }
}

pub(crate) async fn core_create_push_config(
    state: AppState,
    tenant: &str,
    task_id: &str,
    body: String,
) -> Result<Json<serde_json::Value>, A2aError> {
    let mut config: turul_a2a_proto::TaskPushNotificationConfig = serde_json::from_str(&body)
        .map_err(|e| A2aError::InvalidRequest {
            message: format!("Invalid push config: {e}"),
        })?;
    config.task_id = task_id.to_string();

    let created = state.push_storage.create_config(tenant, config).await.map_err(A2aError::from)?;
    Ok(Json(serde_json::to_value(&created).unwrap_or_default()))
}

pub(crate) async fn core_list_push_configs(
    state: AppState,
    tenant: &str,
    task_id: &str,
    query: &PushConfigQuery,
) -> Result<Json<serde_json::Value>, A2aError> {
    let page = state
        .push_storage
        .list_configs(tenant, task_id, query.page_token.as_deref(), query.page_size)
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

pub(crate) async fn core_get_push_config(
    state: AppState,
    tenant: &str,
    task_id: &str,
    config_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    let config = state
        .push_storage
        .get_config(tenant, task_id, config_id)
        .await
        .map_err(A2aError::from)?
        .ok_or_else(|| A2aError::TaskNotFound {
            task_id: format!("push config {config_id} for task {task_id}"),
        })?;

    Ok(Json(serde_json::to_value(&config).unwrap_or_default()))
}

pub(crate) async fn core_delete_push_config(
    state: AppState,
    tenant: &str,
    task_id: &str,
    config_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    state
        .push_storage
        .delete_config(tenant, task_id, config_id)
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
