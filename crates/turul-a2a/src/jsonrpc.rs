//! JSON-RPC 2.0 dispatch layer.
//!
//! Routes JSON-RPC method calls to the same core handler functions used by HTTP routes.
//! Method names match `turul_a2a_types::wire::jsonrpc` constants (PascalCase).

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use turul_a2a_types::wire::jsonrpc as methods;

use crate::error::A2aError;
use crate::router::AppState;

/// JSON-RPC 2.0 dispatch handler.
///
/// POST /jsonrpc — accepts JSON-RPC 2.0 requests and dispatches to core handlers.
pub async fn jsonrpc_dispatch_handler(
    State(state): State<AppState>,
    body: String,
) -> axum::response::Response {
    // 1. Parse JSON
    let value: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            return Json(jsonrpc_error(Value::Null, -32700, "Parse error", None)).into_response();
        }
    };

    // 2. Validate JSON-RPC 2.0 envelope
    let jsonrpc = value.get("jsonrpc").and_then(|v| v.as_str());
    if jsonrpc != Some("2.0") {
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        return Json(jsonrpc_error(id, -32600, "Invalid Request: missing or wrong jsonrpc version", None)).into_response();
    }

    let method = match value.get("method").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => {
            let id = value.get("id").cloned().unwrap_or(Value::Null);
            return Json(jsonrpc_error(id, -32600, "Invalid Request: missing method", None)).into_response();
        }
    };

    // 3. Check if this is a notification (no "id" field = notification, must not reply)
    let is_notification = !value.as_object().map_or(false, |o| o.contains_key("id"));
    let id = value.get("id").cloned().unwrap_or(Value::Null);

    // 4. Validate params: must be object, null, or absent. Arrays/scalars are -32602.
    let params = match value.get("params") {
        None => json!({}),
        Some(Value::Null) => json!({}),
        Some(Value::Object(_)) => value.get("params").cloned().unwrap(),
        Some(_) => {
            if is_notification {
                return axum::http::StatusCode::NO_CONTENT.into_response();
            }
            return Json(jsonrpc_error(id, -32602, "Invalid params: params must be an object", None)).into_response();
        }
    };

    // 5. Extract tenant from params
    let tenant = params
        .get("tenant")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // 6. Dispatch by method name
    let result = dispatch(state, &method, &tenant, params).await;

    // 7. Notifications: suppress response entirely
    if is_notification {
        return axum::http::StatusCode::NO_CONTENT.into_response();
    }

    // 8. Build response
    match result {
        Ok(value) => Json(jsonrpc_success(id, value)).into_response(),
        Err(ref e) if matches!(e, A2aError::InvalidRequest { message } if message == "Method not found") => {
            Json(jsonrpc_error(id, -32601, "Method not found", None)).into_response()
        }
        Err(e) => Json(e.to_jsonrpc_error(Some(&id))).into_response(),
    }
}

async fn dispatch(
    state: AppState,
    method: &str,
    tenant: &str,
    params: Value,
) -> Result<Value, A2aError> {
    match method {
        methods::SEND_MESSAGE => dispatch_send_message(state, tenant, params).await,
        methods::GET_TASK => dispatch_get_task(state, tenant, params).await,
        methods::LIST_TASKS => dispatch_list_tasks(state, tenant, params).await,
        methods::CANCEL_TASK => dispatch_cancel_task(state, tenant, params).await,
        methods::GET_EXTENDED_AGENT_CARD => dispatch_get_extended_agent_card(state).await,
        methods::CREATE_TASK_PUSH_NOTIFICATION_CONFIG => {
            dispatch_create_push_config(state, tenant, params).await
        }
        methods::GET_TASK_PUSH_NOTIFICATION_CONFIG => {
            dispatch_get_push_config(state, tenant, params).await
        }
        methods::LIST_TASK_PUSH_NOTIFICATION_CONFIGS => {
            dispatch_list_push_configs(state, tenant, params).await
        }
        methods::DELETE_TASK_PUSH_NOTIFICATION_CONFIG => {
            dispatch_delete_push_config(state, tenant, params).await
        }
        methods::SEND_STREAMING_MESSAGE | methods::SUBSCRIBE_TO_TASK => {
            Err(A2aError::UnsupportedOperation {
                message: "Streaming not implemented in v0.1".into(),
            })
        }
        _ => Err(method_not_found(method)),
    }
}

fn method_not_found(_method: &str) -> A2aError {
    // Return a special error that maps to -32601
    A2aError::InvalidRequest {
        message: "Method not found".into(),
    }
}

// =========================================================
// Per-method dispatch — deserialize params, call core, serialize result
// =========================================================

async fn dispatch_send_message(
    state: AppState,
    tenant: &str,
    params: Value,
) -> Result<Value, A2aError> {
    let body = serde_json::to_string(&params).map_err(|e| A2aError::InvalidRequest {
        message: format!("Invalid params: {e}"),
    })?;
    // Reuse the core handler, which returns Json<Value> with SendMessageResponse shape
    let Json(response) = crate::router::core_send_message(state, tenant, "anonymous", body).await?;
    Ok(response)
}

async fn dispatch_get_task(
    state: AppState,
    tenant: &str,
    params: Value,
) -> Result<Value, A2aError> {
    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(A2aError::InvalidRequest {
            message: "Missing required field: id".into(),
        })?
        .to_string();

    let history_length = params
        .get("historyLength")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);

    let Json(response) = crate::router::core_get_task(state, tenant, "anonymous", &id, history_length).await?;
    Ok(response)
}

async fn dispatch_list_tasks(
    state: AppState,
    tenant: &str,
    params: Value,
) -> Result<Value, A2aError> {
    let query = crate::router::ListTasksQuery {
        context_id: params.get("contextId").and_then(|v| v.as_str()).map(|s| s.to_string()),
        status: params.get("status").and_then(|v| v.as_str()).map(|s| s.to_string()),
        page_size: params.get("pageSize").and_then(|v| v.as_i64()).map(|v| v as i32),
        page_token: params.get("pageToken").and_then(|v| v.as_str()).map(|s| s.to_string()),
        history_length: params.get("historyLength").and_then(|v| v.as_i64()).map(|v| v as i32),
        include_artifacts: params.get("includeArtifacts").and_then(|v| v.as_bool()),
    };

    let Json(response) = crate::router::core_list_tasks(state, tenant, "anonymous", &query).await?;
    Ok(response)
}

async fn dispatch_cancel_task(
    state: AppState,
    tenant: &str,
    params: Value,
) -> Result<Value, A2aError> {
    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(A2aError::InvalidRequest {
            message: "Missing required field: id".into(),
        })?
        .to_string();

    let Json(response) = crate::router::core_cancel_task(state, tenant, "anonymous", &id).await?;
    Ok(response)
}

async fn dispatch_get_extended_agent_card(state: AppState) -> Result<Value, A2aError> {
    match state.executor.extended_agent_card(None) {
        Some(card) => Ok(serde_json::to_value(card).unwrap_or_default()),
        None => Err(A2aError::ExtendedAgentCardNotConfigured),
    }
}

async fn dispatch_create_push_config(
    state: AppState,
    tenant: &str,
    params: Value,
) -> Result<Value, A2aError> {
    let task_id = params
        .get("taskId")
        .and_then(|v| v.as_str())
        .ok_or(A2aError::InvalidRequest {
            message: "Missing required field: taskId".into(),
        })?
        .to_string();

    let body = serde_json::to_string(&params).map_err(|e| A2aError::InvalidRequest {
        message: format!("Invalid params: {e}"),
    })?;

    let Json(response) = crate::router::core_create_push_config(state, tenant, "anonymous", &task_id, body).await?;
    Ok(response)
}

async fn dispatch_get_push_config(
    state: AppState,
    tenant: &str,
    params: Value,
) -> Result<Value, A2aError> {
    let task_id = params.get("taskId").and_then(|v| v.as_str()).ok_or(A2aError::InvalidRequest {
        message: "Missing required field: taskId".into(),
    })?.to_string();
    let id = params.get("id").and_then(|v| v.as_str()).ok_or(A2aError::InvalidRequest {
        message: "Missing required field: id".into(),
    })?.to_string();

    let Json(response) = crate::router::core_get_push_config(state, tenant, "anonymous", &task_id, &id).await?;
    Ok(response)
}

async fn dispatch_list_push_configs(
    state: AppState,
    tenant: &str,
    params: Value,
) -> Result<Value, A2aError> {
    let task_id = params.get("taskId").and_then(|v| v.as_str()).ok_or(A2aError::InvalidRequest {
        message: "Missing required field: taskId".into(),
    })?.to_string();

    let query = crate::router::PushConfigQuery {
        page_size: params.get("pageSize").and_then(|v| v.as_i64()).map(|v| v as i32),
        page_token: params.get("pageToken").and_then(|v| v.as_str()).map(|s| s.to_string()),
    };

    let Json(response) = crate::router::core_list_push_configs(state, tenant, "anonymous", &task_id, &query).await?;
    Ok(response)
}

async fn dispatch_delete_push_config(
    state: AppState,
    tenant: &str,
    params: Value,
) -> Result<Value, A2aError> {
    let task_id = params.get("taskId").and_then(|v| v.as_str()).ok_or(A2aError::InvalidRequest {
        message: "Missing required field: taskId".into(),
    })?.to_string();
    let id = params.get("id").and_then(|v| v.as_str()).ok_or(A2aError::InvalidRequest {
        message: "Missing required field: id".into(),
    })?.to_string();

    let Json(response) = crate::router::core_delete_push_config(state, tenant, "anonymous", &task_id, &id).await?;
    Ok(response)
}

// =========================================================
// JSON-RPC response builders
// =========================================================

fn jsonrpc_success(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn jsonrpc_error(id: Value, code: i32, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({
        "code": code,
        "message": message,
    });
    if let Some(d) = data {
        error["data"] = d;
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error,
    })
}
