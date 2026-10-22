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

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use turul_a2a_types::wire::jsonrpc as methods;

use crate::error::A2aError;
use crate::router::AppState;

/// Path of the JSON-RPC endpoint.
pub(crate) const JSONRPC_PATH: &str = "/jsonrpc";

/// Whether `path` is served by the JSON-RPC dispatcher: the canonical
/// endpoint, plus the root path the `compat-v03` feature routes for v0.3
/// clients that POST to the agent card URL.
pub(crate) fn is_jsonrpc_path(path: &str) -> bool {
    path == JSONRPC_PATH || (cfg!(feature = "compat-v03") && path == "/")
}

/// JSON-RPC 2.0 dispatch handler.
///
/// POST /jsonrpc — accepts JSON-RPC 2.0 requests and dispatches to core handlers.
///
/// Wraps the inner dispatcher with profile-extension activation:
/// the `A2A-Extensions` request header is parsed up front, a
/// `required = true` advertised extension that the client did not
/// activate trips an `UnsupportedOperationError` response without
/// dispatching, and on success the activated URIs are echoed back in
/// the same response header.
pub async fn jsonrpc_dispatch_handler(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    headers: axum::http::HeaderMap,
    body: String,
) -> axum::response::Response {
    let echoed = match crate::router::negotiate_profile_extensions(&state, &headers) {
        Ok(set) => set,
        Err(err) => {
            // The request id is not yet known here; passing `None`
            // produces a JSON-RPC error envelope with `id: null` per
            // §5.1, which is the right shape when the rejection is
            // pre-dispatch.
            return crate::router::attach_extension_echo(
                Json(err.to_jsonrpc_error(None)).into_response(),
                &std::collections::HashSet::new(),
            );
        }
    };
    let response = jsonrpc_dispatch_inner(State(state), axum::Extension(ctx), headers, body).await;
    crate::router::attach_extension_echo(response, &echoed)
}

async fn jsonrpc_dispatch_inner(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    #[cfg_attr(not(feature = "compat-v03"), allow(unused_variables))]
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

    // Reject explicit `id: null`. Notifications are signalled by the absence
    // of the `id` key, not by `id: null`. Aligning with that reading lets the
    // server use a String|Number id type internally with no Null variant.
    // The rejection response carries `id: null` per JSON-RPC 2.0 §5.1
    // (id MUST be Null when the request id cannot be detected/used).
    if !is_notification && id.is_null() {
        return Json(jsonrpc_error(
            Value::Null,
            -32600,
            "Invalid Request: id must be a string or number",
            None,
        ))
        .into_response();
    }

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

        return match dispatch_streaming(state, &method, &tenant, &owner, claims, params, id.clone())
            .await
        {
            Ok(response) => response,
            Err(e) => Json(e.to_jsonrpc_error(Some(&id))).into_response(),
        };
    }

    // 7. Non-streaming dispatch.
    //
    // Notifications run dispatch for side effects, then drop the response.
    if is_notification {
        let _ = dispatch(state, &method, &tenant, &owner, claims, params).await;
        return axum::http::StatusCode::NO_CONTENT.into_response();
    }

    // Route known non-streaming methods through turul-rpc when the id and
    // params are representable as turul-rpc types. Unknown methods, exotic
    // numeric ids (non-i64), and any other edge cases fall back to the
    // legacy direct path so wire behavior stays unchanged.
    let representable_id = match &id {
        Value::String(s) => Some(turul_rpc::RequestId::String(s.clone())),
        Value::Number(n) => n.as_i64().map(turul_rpc::RequestId::Number),
        _ => None,
    };
    let method_is_known_non_streaming = matches!(
        method.as_str(),
        methods::SEND_MESSAGE
            | methods::GET_TASK
            | methods::LIST_TASKS
            | methods::CANCEL_TASK
            | methods::GET_EXTENDED_AGENT_CARD
            | methods::CREATE_TASK_PUSH_NOTIFICATION_CONFIG
            | methods::GET_TASK_PUSH_NOTIFICATION_CONFIG
            | methods::LIST_TASK_PUSH_NOTIFICATION_CONFIGS
            | methods::DELETE_TASK_PUSH_NOTIFICATION_CONFIG
    );

    if let (Some(rid), true) = (representable_id, method_is_known_non_streaming) {
        let params_obj: std::collections::HashMap<String, Value> = match &params {
            Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            _ => std::collections::HashMap::new(),
        };
        let request = turul_rpc::JsonRpcRequest::new(
            rid,
            method.clone(),
            Some(turul_rpc::RequestParams::Object(params_obj)),
        );
        let mut session_ctx = turul_rpc::SessionContext {
            session_id: String::new(),
            metadata: std::collections::HashMap::new(),
            broadcaster: None,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            extensions: std::collections::HashMap::new(),
        };
        session_ctx
            .extensions
            .insert("tenant".to_string(), Value::String(tenant.clone()));
        session_ctx
            .extensions
            .insert("owner".to_string(), Value::String(owner.clone()));
        if let Some(c) = claims.clone() {
            session_ctx.extensions.insert("claims".to_string(), c);
        }

        let mut dispatcher = turul_rpc::JsonRpcDispatcher::<A2aError>::new();
        dispatcher.set_default_handler(A2aJsonRpcHandler {
            state: state.clone(),
        });
        let msg = dispatcher
            .handle_request_with_context(request, session_ctx)
            .await;

        #[cfg_attr(not(feature = "compat-v03"), allow(unused_mut))]
        let mut response = serde_json::to_value(&msg)
            .expect("JsonRpcMessage serialization is infallible for in-memory values");
        #[cfg(feature = "compat-v03")]
        {
            let mode = crate::compat_v03::detect_compat_mode(raw_method, &headers);
            crate::compat_v03::maybe_normalize_response(&mut response, mode);
        }
        return Json(response).into_response();
    }

    // Fallback: direct dispatch for unknown methods and non-i64 numeric ids.
    let result = dispatch(state, &method, &tenant, &owner, claims, params).await;

    match result {
        Ok(value) => {
            #[cfg_attr(not(feature = "compat-v03"), allow(unused_mut))]
            let mut response = jsonrpc_success(id, value);
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

/// Adapter from turul-rpc's typed handler trait into A2A's existing dispatch
/// table. The outer handler (`jsonrpc_dispatch_handler`) guarantees by
/// construction that `method` is a known non-streaming A2A method, that
/// `params` is `RequestParams::Object`, and that `session_context` carries
/// tenant/owner/claims under those keys.
struct A2aJsonRpcHandler {
    state: AppState,
}

#[async_trait::async_trait]
impl turul_rpc::JsonRpcHandler for A2aJsonRpcHandler {
    type Error = A2aError;

    async fn handle(
        &self,
        method: &str,
        params: Option<turul_rpc::RequestParams>,
        session_context: Option<turul_rpc::SessionContext>,
    ) -> Result<Value, Self::Error> {
        let ctx = session_context.expect("outer A2A handler always populates SessionContext");
        let tenant = ctx
            .extensions
            .get("tenant")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let owner = ctx
            .extensions
            .get("owner")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let claims = ctx.extensions.get("claims").cloned();

        let params_value = match params {
            Some(turul_rpc::RequestParams::Object(map)) => Value::Object(map.into_iter().collect()),
            _ => json!({}),
        };

        dispatch(
            self.state.clone(),
            method,
            &tenant,
            &owner,
            claims,
            params_value,
        )
        .await
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
) -> Result<axum::response::Response, A2aError> {
    match method {
        methods::SEND_STREAMING_MESSAGE => {
            dispatch_send_streaming_message(state, tenant, owner, claims, params, request_id).await
        }
        methods::SUBSCRIBE_TO_TASK => {
            dispatch_subscribe_to_task(state, tenant, owner, params, request_id).await
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
    let stream =
        crate::router::open_send_stream(state, tenant, owner, claims, params.to_string()).await?;
    Ok(jsonrpc_sse_response(stream, request_id))
}

async fn dispatch_subscribe_to_task(
    state: AppState,
    tenant: &str,
    owner: &str,
    params: Value,
    request_id: Value,
) -> Result<axum::response::Response, A2aError> {
    let task_id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(A2aError::InvalidRequest {
            message: "Missing required field: id".into(),
        })?;
    let stream = crate::router::open_subscribe_stream(state, tenant, owner, task_id).await?;
    Ok(jsonrpc_sse_response(stream, request_id))
}

/// SSE whose every `data:` line is
/// `{"jsonrpc":"2.0","result":{StreamResponse},"id":request_id}`.
fn jsonrpc_sse_response(
    stream: crate::streaming::task_stream::TaskEventStream,
    request_id: Value,
) -> axum::response::Response {
    crate::router::sse_response(stream, move |result| {
        json!({
            "jsonrpc": "2.0",
            "result": result,
            "id": request_id,
        })
    })
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
    let request: turul_a2a_proto::ListTasksRequest =
        serde_json::from_value(params).map_err(|e| A2aError::InvalidRequest {
            message: format!("Invalid ListTasks params: {e}"),
        })?;
    let query = crate::router::ListTasksQuery::from_proto(&request)?;

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
