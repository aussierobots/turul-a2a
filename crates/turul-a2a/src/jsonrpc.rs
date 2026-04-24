//! JSON-RPC 2.0 dispatch layer.
//!
//! Routes JSON-RPC method calls to the same core handler functions used by HTTP routes.
//! Method names match `turul_a2a_types::wire::jsonrpc` constants (PascalCase, A2A v1.0).
//!
//! A2A v0.3 compatibility: method name normalization is delegated to
//! `compat_v03::normalize_jsonrpc_method` — see that module for details.
//!
//! Streaming methods (`SendStreamingMessage`, `SubscribeToTask`) return SSE responses
//! with each event wrapped in a JSON-RPC response envelope.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;

use turul_a2a_types::wire::jsonrpc as methods;
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};

use crate::error::A2aError;
use crate::router::AppState;
use crate::storage::A2aEventStore;
use crate::streaming::{self, StreamEvent, replay};

/// JSON-RPC 2.0 dispatch handler.
///
/// POST /jsonrpc — accepts JSON-RPC 2.0 requests and dispatches to core handlers.
pub async fn jsonrpc_dispatch_handler(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    headers: axum::http::HeaderMap,
    body: String,
) -> axum::response::Response {
    let owner = ctx.identity.owner().to_string();
    let claims = ctx.identity.claims().cloned();
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
        return Json(jsonrpc_error(
            id,
            -32600,
            "Invalid Request: missing or wrong jsonrpc version",
            None,
        ))
        .into_response();
    }

    let raw_method = match value.get("method").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            let id = value.get("id").cloned().unwrap_or(Value::Null);
            return Json(jsonrpc_error(
                id,
                -32600,
                "Invalid Request: missing method",
                None,
            ))
            .into_response();
        }
    };

    // Method resolution: canonical v1.0 pass-through, or v0.3 compat normalization
    #[cfg(feature = "compat-v03")]
    let method = {
        let mode = crate::compat_v03::detect_compat_mode(raw_method, &headers);
        if mode == crate::compat_v03::CompatMode::V03 {
            let normalized = crate::compat_v03::maybe_normalize_method(raw_method, mode);
            tracing::info!(
                raw_method = %raw_method,
                normalized_method = %normalized,
                "A2A v0.3 compatibility mode activated"
            );
            normalized
        } else {
            raw_method.to_string()
        }
    };
    #[cfg(not(feature = "compat-v03"))]
    let method = raw_method.to_string();

    // 3. Check if this is a notification (no "id" field = notification, must not reply)
    let is_notification = !value.as_object().is_some_and(|o| o.contains_key("id"));
    let id = value.get("id").cloned().unwrap_or(Value::Null);

    // 4. Validate params: must be object, null, or absent. Arrays/scalars are -32602.
    #[cfg_attr(not(feature = "compat-v03"), allow(unused_mut))]
    let mut params = match value.get("params") {
        None => json!({}),
        Some(Value::Null) => json!({}),
        Some(Value::Object(_)) => value.get("params").cloned().unwrap(),
        Some(_) => {
            if is_notification {
                return axum::http::StatusCode::NO_CONTENT.into_response();
            }
            return Json(jsonrpc_error(
                id,
                -32602,
                "Invalid params: params must be an object",
                None,
            ))
            .into_response();
        }
    };

    // A2A v0.3 compat: normalize params (role enums, part formats) before dispatch
    #[cfg(feature = "compat-v03")]
    {
        let mode = crate::compat_v03::detect_compat_mode(raw_method, &headers);
        crate::compat_v03::maybe_normalize_params(&mut params, mode);
    }

    // 5. Extract tenant from params
    let tenant = params
        .get("tenant")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // 6. Streaming methods return SSE directly (bypass normal dispatch)
    if method == methods::SEND_STREAMING_MESSAGE || method == methods::SUBSCRIBE_TO_TASK {
        if is_notification {
            return axum::http::StatusCode::NO_CONTENT.into_response();
        }

        // Extract Last-Event-ID from HTTP header (Turul extension for JSON-RPC resume).
        // The proto SubscribeToTaskRequest has no cursor field, but since JSON-RPC
        // streaming runs over HTTP, clients MAY send this header for resumable replay.
        let last_event_id = headers
            .get("Last-Event-ID")
            .or_else(|| headers.get("last-event-id"))
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        return match dispatch_streaming(
            state,
            &method,
            &tenant,
            &owner,
            claims,
            params,
            id.clone(),
            last_event_id.as_deref(),
        )
        .await
        {
            Ok(response) => response,
            Err(e) => Json(e.to_jsonrpc_error(Some(&id))).into_response(),
        };
    }

    // 7. Non-streaming dispatch
    let result = dispatch(state, &method, &tenant, &owner, claims, params).await;

    // 8. Notifications: suppress response entirely
    if is_notification {
        return axum::http::StatusCode::NO_CONTENT.into_response();
    }

    // 9. Build response
    match result {
        Ok(value) => {
            #[cfg_attr(not(feature = "compat-v03"), allow(unused_mut))]
            let mut response = jsonrpc_success(id, value);
            // A2A v0.3 compat: normalize response envelope + enums
            #[cfg(feature = "compat-v03")]
            {
                let mode = crate::compat_v03::detect_compat_mode(raw_method, &headers);
                crate::compat_v03::maybe_normalize_response(&mut response, mode);
            }
            Json(response).into_response()
        }
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
    owner: &str,
    claims: Option<serde_json::Value>,
    params: Value,
) -> Result<Value, A2aError> {
    match method {
        methods::SEND_MESSAGE => dispatch_send_message(state, tenant, owner, claims, params).await,
        methods::GET_TASK => dispatch_get_task(state, tenant, owner, params).await,
        methods::LIST_TASKS => dispatch_list_tasks(state, tenant, owner, params).await,
        methods::CANCEL_TASK => dispatch_cancel_task(state, tenant, owner, params).await,
        methods::GET_EXTENDED_AGENT_CARD => dispatch_get_extended_agent_card(state).await,
        methods::CREATE_TASK_PUSH_NOTIFICATION_CONFIG => {
            dispatch_create_push_config(state, tenant, owner, params).await
        }
        methods::GET_TASK_PUSH_NOTIFICATION_CONFIG => {
            dispatch_get_push_config(state, tenant, owner, params).await
        }
        methods::LIST_TASK_PUSH_NOTIFICATION_CONFIGS => {
            dispatch_list_push_configs(state, tenant, owner, params).await
        }
        methods::DELETE_TASK_PUSH_NOTIFICATION_CONFIG => {
            dispatch_delete_push_config(state, tenant, owner, params).await
        }
        // Streaming methods handled before dispatch() is called
        methods::SEND_STREAMING_MESSAGE | methods::SUBSCRIBE_TO_TASK => Err(A2aError::Internal(
            "Streaming methods should not reach dispatch()".into(),
        )),
        _ => Err(method_not_found(method)),
    }
}

// =========================================================
// JSON-RPC streaming dispatch
// =========================================================

#[allow(clippy::too_many_arguments)] // dispatch glue: all args are request-derived
async fn dispatch_streaming(
    state: AppState,
    method: &str,
    tenant: &str,
    owner: &str,
    claims: Option<serde_json::Value>,
    params: Value,
    request_id: Value,
    last_event_id: Option<&str>,
) -> Result<axum::response::Response, A2aError> {
    match method {
        methods::SEND_STREAMING_MESSAGE => {
            dispatch_send_streaming_message(state, tenant, owner, claims, params, request_id).await
        }
        methods::SUBSCRIBE_TO_TASK => {
            dispatch_subscribe_to_task(state, tenant, owner, params, request_id, last_event_id)
                .await
        }
        _ => Err(A2aError::Internal("Unknown streaming method".into())),
    }
}

async fn dispatch_send_streaming_message(
    state: AppState,
    tenant: &str,
    owner: &str,
    claims: Option<serde_json::Value>,
    params: Value,
    request_id: Value,
) -> Result<axum::response::Response, A2aError> {
    // Parse the SendMessageRequest from params
    let request: turul_a2a_proto::SendMessageRequest =
        serde_json::from_value(params).map_err(|e| A2aError::InvalidRequest {
            message: format!("Invalid params: {e}"),
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

    // Subscribe to wake-ups BEFORE creating the task
    let wake_rx = state.event_broker.subscribe(&task_id).await;

    // Atomic: create task + SUBMITTED event
    let mut task =
        Task::new(&task_id, TaskStatus::new(TaskState::Submitted)).with_context_id(&context_id);
    task.append_message(message.clone());

    let submitted_event = StreamEvent::StatusUpdate {
        status_update: streaming::StatusUpdatePayload {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            status: serde_json::to_value(TaskStatus::new(TaskState::Submitted)).unwrap_or_default(),
        },
    };

    state
        .atomic_store
        .create_task_with_events(tenant, owner, task, vec![submitted_event])
        .await
        .map_err(A2aError::from)?;
    state.event_broker.notify(&task_id).await;

    // SUBMITTED → WORKING via CAS so subscribers see WORKING before
    // the executor begins emitting.
    let working_event = StreamEvent::StatusUpdate {
        status_update: streaming::StatusUpdatePayload {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            status: serde_json::to_value(TaskStatus::new(TaskState::Working)).unwrap_or_default(),
        },
    };
    state
        .atomic_store
        .update_task_status_with_events(
            tenant,
            &task_id,
            owner,
            TaskStatus::new(TaskState::Working),
            vec![working_event],
        )
        .await
        .map_err(A2aError::from)?;
    state.event_broker.notify(&task_id).await;

    // Spawn the executor on a tracked handle. Streaming transport does
    // not block on yielded_rx.
    let spawn_deps = crate::server::spawn::SpawnDeps {
        executor: state.executor.clone(),
        task_storage: state.task_storage.clone(),
        atomic_store: state.atomic_store.clone(),
        event_broker: state.event_broker.clone(),
        in_flight: state.in_flight.clone(),
        push_dispatcher: state.push_dispatcher.clone(),
    };
    let scope = crate::server::spawn::SpawnScope {
        tenant: tenant.to_string(),
        owner: owner.to_string(),
        task_id: task_id.clone(),
        context_id: context_id.clone(),
        message: message.clone(),
        claims,
    };
    let _spawn = crate::server::spawn::spawn_tracked_executor(spawn_deps, scope)?;

    Ok(make_jsonrpc_sse_response(
        request_id,
        state.event_store,
        tenant.to_string(),
        task_id,
        0,
        wake_rx,
        None,
    ))
}

async fn dispatch_subscribe_to_task(
    state: AppState,
    tenant: &str,
    owner: &str,
    params: Value,
    request_id: Value,
    last_event_id: Option<&str>,
) -> Result<axum::response::Response, A2aError> {
    let task_id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(A2aError::InvalidRequest {
            message: "Missing required field: id".into(),
        })?
        .to_string();

    // Verify task exists and caller owns it
    let task = state
        .task_storage
        .get_task(tenant, &task_id, owner, None)
        .await
        .map_err(A2aError::from)?
        .ok_or_else(|| A2aError::TaskNotFound {
            task_id: task_id.clone(),
        })?;

    // Spec §3.1.6: terminal tasks return UnsupportedOperationError
    if let Some(status) = task.status() {
        if let Ok(s) = status.state() {
            if s.is_terminal() {
                return Err(A2aError::UnsupportedOperation {
                    message: format!("Task {task_id} is already in terminal state {s:?}"),
                });
            }
        }
    }

    // Parse Last-Event-ID for replay (Turul extension — proto has no cursor field)
    let after_sequence = last_event_id
        .and_then(replay::parse_last_event_id)
        .filter(|parsed| parsed.task_id == task_id)
        .map(|parsed| parsed.sequence)
        .unwrap_or(0);

    // Spec §3.1.6: first event is Task snapshot (when not reconnecting)
    let initial_task = if after_sequence == 0 {
        Some(task)
    } else {
        None
    };

    let wake_rx = state.event_broker.subscribe(&task_id).await;

    Ok(make_jsonrpc_sse_response(
        request_id,
        state.event_store,
        tenant.to_string(),
        task_id,
        after_sequence,
        wake_rx,
        initial_task,
    ))
}

// =========================================================
// JSON-RPC SSE response builder
// =========================================================

/// Polling interval for cross-instance subscribers.
const JSONRPC_STORE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Build an SSE response with JSON-RPC envelopes around each event.
///
/// Each SSE `data:` line is: `{"jsonrpc":"2.0","result":{event},"id":request_id}`
fn make_jsonrpc_sse_response(
    request_id: Value,
    event_store: Arc<dyn A2aEventStore>,
    tenant: String,
    task_id: String,
    after_sequence: u64,
    wake_rx: broadcast::Receiver<()>,
    initial_task: Option<Task>,
) -> axum::response::Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);

    tokio::spawn(async move {
        // Spec §3.1.6: emit Task object as first event (when not reconnecting)
        if let Some(task) = initial_task {
            let task_json = json!({"task": serde_json::to_value(&task).unwrap_or_default()});
            let envelope = json!({
                "jsonrpc": "2.0",
                "result": task_json,
                "id": request_id,
            });
            let sse_event = Event::default()
                .id(replay::format_event_id(&task_id, 0))
                .data(serde_json::to_string(&envelope).unwrap_or_default());
            if tx.send(Ok(sse_event)).await.is_err() {
                return;
            }
        }

        let mut last_seq = after_sequence;
        let mut wake_rx = wake_rx;

        loop {
            let events = match event_store
                .get_events_after(&tenant, &task_id, last_seq)
                .await
            {
                Ok(e) => e,
                Err(_) => break,
            };

            let mut saw_terminal = false;
            for (seq, event) in events {
                last_seq = seq;
                let event_id = replay::format_event_id(&task_id, seq);
                let event_json = serde_json::to_value(&event).unwrap_or_default();
                let envelope = json!({
                    "jsonrpc": "2.0",
                    "result": event_json,
                    "id": request_id,
                });
                let sse_event = Event::default()
                    .id(event_id)
                    .data(serde_json::to_string(&envelope).unwrap_or_default());
                if tx.send(Ok(sse_event)).await.is_err() {
                    return;
                }
                if event.is_terminal() {
                    saw_terminal = true;
                }
            }

            if saw_terminal {
                break;
            }

            tokio::select! {
                result = wake_rx.recv() => {
                    match result {
                        Ok(()) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                    }
                }
                _ = tokio::time::sleep(JSONRPC_STORE_POLL_INTERVAL) => {}
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
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
    owner: &str,
    claims: Option<serde_json::Value>,
    params: Value,
) -> Result<Value, A2aError> {
    let body = serde_json::to_string(&params).map_err(|e| A2aError::InvalidRequest {
        message: format!("Invalid params: {e}"),
    })?;
    // Reuse the core handler, which returns Json<Value> with SendMessageResponse shape
    let Json(response) =
        crate::router::core_send_message(state, tenant, owner, claims, body).await?;
    Ok(response)
}

async fn dispatch_get_task(
    state: AppState,
    tenant: &str,
    owner: &str,
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

    let Json(response) =
        crate::router::core_get_task(state, tenant, owner, &id, history_length).await?;
    Ok(response)
}

async fn dispatch_list_tasks(
    state: AppState,
    tenant: &str,
    owner: &str,
    params: Value,
) -> Result<Value, A2aError> {
    let query = crate::router::ListTasksQuery {
        context_id: params
            .get("contextId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        status: params
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        page_size: params
            .get("pageSize")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32),
        page_token: params
            .get("pageToken")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        history_length: params
            .get("historyLength")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32),
        include_artifacts: params.get("includeArtifacts").and_then(|v| v.as_bool()),
    };

    let Json(response) = crate::router::core_list_tasks(state, tenant, owner, &query).await?;
    Ok(response)
}

async fn dispatch_cancel_task(
    state: AppState,
    tenant: &str,
    owner: &str,
    params: Value,
) -> Result<Value, A2aError> {
    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(A2aError::InvalidRequest {
            message: "Missing required field: id".into(),
        })?
        .to_string();

    let Json(response) = crate::router::core_cancel_task(state, tenant, owner, &id).await?;
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
    owner: &str,
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

    let Json(response) =
        crate::router::core_create_push_config(state, tenant, owner, &task_id, body).await?;
    Ok(response)
}

async fn dispatch_get_push_config(
    state: AppState,
    tenant: &str,
    owner: &str,
    params: Value,
) -> Result<Value, A2aError> {
    let task_id = params
        .get("taskId")
        .and_then(|v| v.as_str())
        .ok_or(A2aError::InvalidRequest {
            message: "Missing required field: taskId".into(),
        })?
        .to_string();
    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(A2aError::InvalidRequest {
            message: "Missing required field: id".into(),
        })?
        .to_string();

    let Json(response) =
        crate::router::core_get_push_config(state, tenant, owner, &task_id, &id).await?;
    Ok(response)
}

async fn dispatch_list_push_configs(
    state: AppState,
    tenant: &str,
    owner: &str,
    params: Value,
) -> Result<Value, A2aError> {
    let task_id = params
        .get("taskId")
        .and_then(|v| v.as_str())
        .ok_or(A2aError::InvalidRequest {
            message: "Missing required field: taskId".into(),
        })?
        .to_string();

    let query = crate::router::PushConfigQuery {
        page_size: params
            .get("pageSize")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32),
        page_token: params
            .get("pageToken")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    };

    let Json(response) =
        crate::router::core_list_push_configs(state, tenant, owner, &task_id, &query).await?;
    Ok(response)
}

async fn dispatch_delete_push_config(
    state: AppState,
    tenant: &str,
    owner: &str,
    params: Value,
) -> Result<Value, A2aError> {
    let task_id = params
        .get("taskId")
        .and_then(|v| v.as_str())
        .ok_or(A2aError::InvalidRequest {
            message: "Missing required field: taskId".into(),
        })?
        .to_string();
    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(A2aError::InvalidRequest {
            message: "Missing required field: id".into(),
        })?
        .to_string();

    let Json(response) =
        crate::router::core_delete_push_config(state, tenant, owner, &task_id, &id).await?;
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
