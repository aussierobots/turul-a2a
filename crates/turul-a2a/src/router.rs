//! HTTP router matching proto google.api.http annotations.
//!
//! Routes use axum wildcard catch-all for task paths because the proto
//! uses `{id=*}:action` patterns that don't map directly to axum's `:param` syntax.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::error::A2aError;
use crate::executor::AgentExecutor;
use crate::storage::{A2aPushNotificationStorage, A2aTaskStorage};

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
) -> Result<Json<serde_json::Value>, A2aError> {
    match parse_task_path(&rest) {
        Some(TaskAction::GetTask(id)) => get_task_handler(state, &id).await,
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

async fn send_message_handler(
    State(_state): State<AppState>,
    _body: String,
) -> Result<Json<serde_json::Value>, A2aError> {
    // TODO: implement in handler wiring phase
    Err(A2aError::UnsupportedOperation {
        message: "SendMessage not yet implemented".into(),
    })
}

async fn send_streaming_message_handler(
    State(_state): State<AppState>,
) -> Result<Json<serde_json::Value>, A2aError> {
    Err(A2aError::UnsupportedOperation {
        message: "Streaming not implemented in v0.1".into(),
    })
}

async fn list_tasks_handler(
    State(_state): State<AppState>,
) -> Result<Json<serde_json::Value>, A2aError> {
    // TODO: implement
    Err(A2aError::UnsupportedOperation {
        message: "ListTasks not yet implemented".into(),
    })
}

async fn get_task_handler(
    state: AppState,
    _task_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    // TODO: implement with storage
    let _ = state;
    Err(A2aError::UnsupportedOperation {
        message: "GetTask not yet implemented".into(),
    })
}

async fn cancel_task_handler(
    state: AppState,
    _task_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    let _ = state;
    Err(A2aError::UnsupportedOperation {
        message: "CancelTask not yet implemented".into(),
    })
}

async fn create_push_config_handler(
    state: AppState,
    _task_id: &str,
    _body: String,
) -> Result<Json<serde_json::Value>, A2aError> {
    let _ = state;
    Err(A2aError::UnsupportedOperation {
        message: "CreatePushConfig not yet implemented".into(),
    })
}

async fn list_push_configs_handler(
    state: AppState,
    _task_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    let _ = state;
    Err(A2aError::UnsupportedOperation {
        message: "ListPushConfigs not yet implemented".into(),
    })
}

async fn get_push_config_handler(
    state: AppState,
    _task_id: &str,
    _config_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    let _ = state;
    Err(A2aError::UnsupportedOperation {
        message: "GetPushConfig not yet implemented".into(),
    })
}

async fn delete_push_config_handler(
    state: AppState,
    _task_id: &str,
    _config_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    let _ = state;
    Err(A2aError::UnsupportedOperation {
        message: "DeletePushConfig not yet implemented".into(),
    })
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
