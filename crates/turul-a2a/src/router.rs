//! HTTP router matching proto google.api.http annotations.
//!
//! Routes use axum wildcard catch-all for task paths because the proto
//! uses `{id=*}:action` patterns that don't map directly to axum's `:param` syntax.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;

use crate::error::A2aError;
use crate::executor::AgentExecutor;
use crate::storage::{A2aAtomicStore, A2aEventStore, A2aPushNotificationStorage, A2aStorageError, A2aTaskStorage, TaskFilter, TaskListPage};
use crate::streaming::{StreamEvent, replay};
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};

/// Shared server state.
#[derive(Clone)]
pub struct AppState {
    pub executor: Arc<dyn AgentExecutor>,
    pub task_storage: Arc<dyn A2aTaskStorage>,
    pub push_storage: Arc<dyn A2aPushNotificationStorage>,
    pub event_store: Arc<dyn crate::storage::A2aEventStore>,
    pub atomic_store: Arc<dyn A2aAtomicStore>,
    pub event_broker: crate::streaming::TaskEventBroker,
    pub middleware_stack: Arc<crate::middleware::MiddlewareStack>,
    /// Runtime configuration carried from [`crate::server::A2aServerBuilder`].
    /// Consumers read fields via `state.runtime_config` — cancellation
    /// handler grace / poll interval, cross-instance cancel poll
    /// interval, blocking-send two-deadline timeouts, and push delivery
    /// tuning. Construct via
    /// [`crate::server::RuntimeConfig::default()`] when building
    /// `AppState` directly (tests, Lambda adapter) rather than through
    /// the server builder.
    pub runtime_config: crate::server::RuntimeConfig,

    /// In-flight task registry (ADR-010 §4.4). Holds one
    /// [`crate::server::in_flight::InFlightHandle`] per spawned executor
    /// — keyed by `(tenant, task_id)`. Populated by the executor spawn
    /// path; consumed by the `:cancel` handler to trip the local
    /// cancellation token if the executor runs on this instance. May be
    /// empty when no executor has been spawned through the registry yet.
    pub in_flight: Arc<crate::server::in_flight::InFlightRegistry>,

    /// Supervisor-only cancel-marker reads (ADR-012 §3 / §10). Separate
    /// from `task_storage` so handler code cannot reach the unscoped
    /// reads. Use `set_cancel_requested` on `task_storage` for marker
    /// writes (owner-scoped, handler-safe).
    pub cancellation_supervisor: Arc<dyn crate::storage::A2aCancellationSupervisor>,

    /// Push-delivery coordination store (ADR-011 §10). `None` on
    /// deployments that do not wire a `PushDeliveryWorker`; the
    /// push config CRUD paths continue to work without it —
    /// configs are stored, just not delivered. Set via
    /// [`crate::server::A2aServerBuilder::push_delivery_store`].
    pub push_delivery_store: Option<Arc<dyn crate::push::A2aPushDeliveryStore>>,

    /// Push-delivery dispatcher (ADR-011 §2, §13.13). Populated by the
    /// server builder iff `push_delivery_store` is wired. Handler and
    /// executor commit paths call [`PushDispatcher::dispatch`] after a
    /// successful terminal atomic-store write so every terminal —
    /// executor-driven, framework-forced CANCEL, or hard-timeout
    /// FAILED — fans out to registered push configs by the same
    /// contract.
    pub push_dispatcher: Option<Arc<crate::push::PushDispatcher>>,
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

    // JSON-RPC dispatch: /jsonrpc is the canonical endpoint
    let router = router.route("/jsonrpc", post(crate::jsonrpc::jsonrpc_dispatch_handler));

    // A2A v0.3 compat: root POST route for a2a-sdk 0.3.x clients that POST
    // to the agent card URL (base URL) rather than /message:send.
    // Removal condition: when a2a-sdk supports v1.0 routing.
    #[cfg(feature = "compat-v03")]
    let router = router.route("/", post(crate::jsonrpc::jsonrpc_dispatch_handler));

    // Wrap with auth Tower layer (runs second — after transport compliance)
    let auth_layer = crate::middleware::AuthLayer::new(state.middleware_stack.clone());
    // Wrap with transport compliance layer (runs first — outermost)
    let transport_layer = crate::middleware::transport::TransportComplianceLayer;
    router
        .with_state(state)
        .layer(auth_layer)
        .layer(transport_layer)
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
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    headers: axum::http::HeaderMap,
    Path(rest): Path<String>,
    Query(query): Query<TaskGetCombinedQuery>,
) -> Result<axum::response::Response, A2aError> {
    let last_event_id = headers
        .get("Last-Event-ID")
        .or_else(|| headers.get("last-event-id"))
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    dispatch_task_get(state, DEFAULT_TENANT, ctx.identity.owner(), &rest, &query, last_event_id.as_deref()).await
}

async fn task_post_dispatch(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    Path(rest): Path<String>,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    dispatch_task_post(state, DEFAULT_TENANT, ctx.identity.owner(), &rest, body).await
}

async fn task_delete_dispatch(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    Path(rest): Path<String>,
) -> Result<axum::response::Response, A2aError> {
    dispatch_task_delete(state, DEFAULT_TENANT, ctx.identity.owner(), &rest).await
}

// =========================================================
// Dispatch — tenant-prefixed routes
// =========================================================

async fn tenant_send_message_handler(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    Path(tenant): Path<String>,
    body: String,
) -> Result<Json<serde_json::Value>, A2aError> {
    core_send_message(state, &tenant, ctx.identity.owner(), body).await
}

async fn tenant_list_tasks_handler(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    Path(tenant): Path<String>,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<serde_json::Value>, A2aError> {
    core_list_tasks(state, &tenant, ctx.identity.owner(), &query).await
}

async fn tenant_task_get_dispatch(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    headers: axum::http::HeaderMap,
    Path((tenant, rest)): Path<(String, String)>,
    Query(query): Query<TaskGetCombinedQuery>,
) -> Result<axum::response::Response, A2aError> {
    let last_event_id = headers
        .get("Last-Event-ID")
        .or_else(|| headers.get("last-event-id"))
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    dispatch_task_get(state, &tenant, ctx.identity.owner(), &rest, &query, last_event_id.as_deref()).await
}

async fn tenant_task_post_dispatch(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    Path((tenant, rest)): Path<(String, String)>,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    dispatch_task_post(state, &tenant, ctx.identity.owner(), &rest, body).await
}

async fn tenant_task_delete_dispatch(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    Path((tenant, rest)): Path<(String, String)>,
) -> Result<axum::response::Response, A2aError> {
    dispatch_task_delete(state, &tenant, ctx.identity.owner(), &rest).await
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
    owner: &str,
    rest: &str,
    query: &TaskGetCombinedQuery,
    last_event_id: Option<&str>,
) -> Result<axum::response::Response, A2aError> {
    match parse_task_path(rest) {
        Some(TaskAction::GetTask(id)) => {
            let Json(v) = core_get_task(state, tenant, owner, &id, query.history_length).await?;
            Ok(Json(v).into_response())
        }
        Some(TaskAction::SubscribeToTask(id)) => {
            core_subscribe_to_task(state, tenant, owner, &id, last_event_id).await
        }
        Some(TaskAction::PushConfigCollection(task_id)) => {
            let pq = PushConfigQuery {
                page_size: query.page_size,
                page_token: query.page_token.clone(),
            };
            let Json(v) = core_list_push_configs(state, tenant, owner, &task_id, &pq).await?;
            Ok(Json(v).into_response())
        }
        Some(TaskAction::PushConfigItem(task_id, config_id)) => {
            let Json(v) = core_get_push_config(state, tenant, owner, &task_id, &config_id).await?;
            Ok(Json(v).into_response())
        }
        _ => Err(A2aError::InvalidRequest { message: "Invalid task path".into() }),
    }
}

async fn dispatch_task_post(
    state: AppState,
    tenant: &str,
    owner: &str,
    rest: &str,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    match parse_task_path(rest) {
        Some(TaskAction::CancelTask(id)) => {
            let Json(v) = core_cancel_task(state, tenant, owner, &id).await?;
            Ok(Json(v).into_response())
        }
        Some(TaskAction::PushConfigCollection(task_id)) => {
            let Json(v) = core_create_push_config(state, tenant, owner, &task_id, body).await?;
            Ok(Json(v).into_response())
        }
        _ => Err(A2aError::InvalidRequest { message: "Invalid task path".into() }),
    }
}

async fn dispatch_task_delete(
    state: AppState,
    tenant: &str,
    owner: &str,
    rest: &str,
) -> Result<axum::response::Response, A2aError> {
    match parse_task_path(rest) {
        Some(TaskAction::PushConfigItem(task_id, config_id)) => {
            let Json(v) = core_delete_push_config(state, tenant, owner, &task_id, &config_id).await?;
            Ok(Json(v).into_response())
        }
        _ => Err(A2aError::InvalidRequest { message: "Invalid task path".into() }),
    }
}

// =========================================================
// Handlers — agent card (no tenant scoping)
// =========================================================

async fn agent_card_handler(State(state): State<AppState>) -> impl IntoResponse {
    let card = serde_json::to_value(state.executor.agent_card()).unwrap_or_default();
    #[cfg(feature = "compat-v03")]
    let card = crate::compat_v03::inject_agent_card_compat(card);
    Json(card)
}

async fn extended_agent_card_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, A2aError> {
    match state.executor.extended_agent_card(None) {
        Some(extended) => {
            let card = serde_json::to_value(extended).unwrap_or_default();
            #[cfg(feature = "compat-v03")]
            let card = crate::compat_v03::inject_agent_card_compat(card);
            Ok(Json(card))
        }
        None => Err(A2aError::ExtendedAgentCardNotConfigured),
    }
}

async fn send_streaming_message_handler(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    core_send_streaming_message(state, DEFAULT_TENANT, ctx.identity.owner(), body).await
}

async fn tenant_send_streaming_message_handler(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    Path(tenant): Path<String>,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    core_send_streaming_message(state, &tenant, ctx.identity.owner(), body).await
}

pub(crate) async fn core_send_streaming_message(
    state: AppState,
    tenant: &str,
    owner: &str,
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

    // Subscribe to wake-ups BEFORE creating the task so we don't miss notifications.
    let wake_rx = state.event_broker.subscribe(&task_id).await;

    // Atomic: create task (SUBMITTED) + SUBMITTED event.
    let mut task = Task::new(&task_id, TaskStatus::new(TaskState::Submitted))
        .with_context_id(&context_id);
    task.append_message(message.clone());

    let submitted_event = StreamEvent::StatusUpdate {
        status_update: crate::streaming::StatusUpdatePayload {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            status: serde_json::to_value(TaskStatus::new(TaskState::Submitted))
                .unwrap_or_default(),
        },
    };

    state
        .atomic_store
        .create_task_with_events(tenant, owner, task, vec![submitted_event])
        .await
        .map_err(A2aError::from)?;

    state.event_broker.notify(&task_id).await;

    // Advance SUBMITTED → WORKING via the CAS-guarded atomic store so
    // streaming subscribers see the WORKING event before the executor
    // emits its own events.
    let working_event = StreamEvent::StatusUpdate {
        status_update: crate::streaming::StatusUpdatePayload {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            status: serde_json::to_value(TaskStatus::new(TaskState::Working))
                .unwrap_or_default(),
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

    // Spawn the executor on a tracked handle. We drop the yielded
    // receiver — streaming transport does not block on it; subscribers
    // observe terminal events through the durable store instead.
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
    };
    let _spawn = crate::server::spawn::spawn_tracked_executor(spawn_deps, scope)?;

    // SSE stream reads from the durable store. No initial Task
    // snapshot: SUBMITTED + WORKING are already in the event store and
    // will replay on subscription.
    Ok(make_store_sse_response(
        state.event_store,
        tenant.to_string(),
        task_id,
        0,
        wake_rx,
        None,
    ))
}

pub(crate) async fn core_subscribe_to_task(
    state: AppState,
    tenant: &str,
    owner: &str,
    task_id: &str,
    last_event_id_header: Option<&str>,
) -> Result<axum::response::Response, A2aError> {
    // Verify task exists and caller owns it
    let task = state
        .task_storage
        .get_task(tenant, task_id, owner, None)
        .await
        .map_err(A2aError::from)?
        .ok_or_else(|| A2aError::TaskNotFound {
            task_id: task_id.to_string(),
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

    // Parse Last-Event-ID for replay
    let after_sequence = last_event_id_header
        .and_then(replay::parse_last_event_id)
        .filter(|parsed| parsed.task_id == task_id)
        .map(|parsed| parsed.sequence)
        .unwrap_or(0);

    // Spec §3.1.6: MUST return a Task object as the first event.
    // On reconnection (Last-Event-ID present), skip the snapshot — client has context.
    let initial_task = if after_sequence == 0 { Some(task) } else { None };

    // Subscribe to wake-up notifications
    let wake_rx = state.event_broker.subscribe(task_id).await;

    Ok(make_store_sse_response(
        state.event_store,
        tenant.to_string(),
        task_id.to_string(),
        after_sequence,
        wake_rx,
        initial_task,
    ))
}

/// Polling interval for cross-instance subscribers that don't receive
/// same-instance broker notifications.
const STORE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Build an SSE response that reads events from the durable store.
///
/// 1. Initial Task snapshot (spec §3.1.6): emit Task object as first event (if provided)
/// 2. Replay: emit all events after `after_sequence` from the store
/// 3. Live loop: wait for broker wake-up or poll timeout, re-query store
/// 4. Close: when a terminal event is emitted
///
/// Each SSE event has `id: {task_id}:{sequence}` for reconnection support.
/// The initial Task snapshot uses `id: {task_id}:0` (before any event sequence).
fn make_store_sse_response(
    event_store: Arc<dyn A2aEventStore>,
    tenant: String,
    task_id: String,
    after_sequence: u64,
    wake_rx: broadcast::Receiver<()>,
    initial_task: Option<Task>,
) -> axum::response::Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);

    tokio::spawn(async move {
        // Spec §3.1.6: emit Task object as first event in the stream
        if let Some(task) = initial_task {
            let task_json = serde_json::json!({"task": serde_json::to_value(&task).unwrap_or_default()});
            let json = serde_json::to_string(&task_json).unwrap_or_default();
            let sse_event = Event::default()
                .id(replay::format_event_id(&task_id, 0))
                .data(json);
            if tx.send(Ok(sse_event)).await.is_err() {
                return;
            }
        }

        let mut last_seq = after_sequence;
        let mut wake_rx = wake_rx;

        loop {
            // Query store for events after last_seq
            let events = match event_store.get_events_after(&tenant, &task_id, last_seq).await {
                Ok(e) => e,
                Err(_) => break,
            };

            let mut saw_terminal = false;
            for (seq, event) in events {
                last_seq = seq;
                let event_id = replay::format_event_id(&task_id, seq);
                let json = serde_json::to_string(&event).unwrap_or_default();
                let sse_event = Event::default().id(event_id).data(json);
                if tx.send(Ok(sse_event)).await.is_err() {
                    return; // subscriber disconnected
                }
                if event.is_terminal() {
                    saw_terminal = true;
                }
            }

            if saw_terminal {
                break; // close stream after terminal event
            }

            // Wait for broker wake-up or periodic poll (cross-instance fallback)
            tokio::select! {
                result = wake_rx.recv() => {
                    match result {
                        Ok(()) => {} // re-query store
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(_)) => {} // re-query
                    }
                }
                _ = tokio::time::sleep(STORE_POLL_INTERVAL) => {
                    // Periodic poll for cross-instance correctness
                }
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// =========================================================
// Default-tenant axum handlers (delegate to core with DEFAULT_TENANT)
// =========================================================

const DEFAULT_TENANT: &str = "";

async fn send_message_handler(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    body: String,
) -> Result<Json<serde_json::Value>, A2aError> {
    core_send_message(state, DEFAULT_TENANT, ctx.identity.owner(), body).await
}

async fn list_tasks_handler(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<serde_json::Value>, A2aError> {
    core_list_tasks(state, DEFAULT_TENANT, ctx.identity.owner(), &query).await
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

#[doc(hidden)]
pub async fn core_send_message(
    state: AppState,
    tenant: &str,
    owner: &str,
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

    let return_immediately = request
        .configuration
        .as_ref()
        .map(|c| c.return_immediately)
        .unwrap_or(false);

    let msg_task_id = message.as_proto().task_id.clone();

    // If message has a task_id, continue the existing task; otherwise create new.
    let (task_id, context_id, is_continuation) = if !msg_task_id.is_empty() {
        let existing = state
            .task_storage
            .get_task(tenant, &msg_task_id, owner, None)
            .await
            .map_err(A2aError::from)?
            .ok_or_else(|| A2aError::TaskNotFound {
                task_id: msg_task_id.clone(),
            })?;

        // Reject contextId/taskId mismatch (spec §3.4.3 MUST).
        let msg_context_id = &message.as_proto().context_id;
        if !msg_context_id.is_empty() && msg_context_id != existing.context_id() {
            return Err(A2aError::InvalidRequest {
                message: format!(
                    "contextId mismatch: message has '{}' but task {} has '{}'",
                    msg_context_id, msg_task_id, existing.context_id()
                ),
            });
        }

        // Continuation is only valid from interrupted states.
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

        (
            msg_task_id,
            existing.context_id().to_string(),
            true,
        )
    } else {
        let context_id = if message.as_proto().context_id.is_empty() {
            uuid::Uuid::now_v7().to_string()
        } else {
            message.as_proto().context_id.clone()
        };
        (uuid::Uuid::now_v7().to_string(), context_id, false)
    };

    // Set up task storage in WORKING atomically, emitting SUBMITTED
    // + WORKING events through the CAS-guarded atomic store so that
    // subscribers / push delivery observe the lifecycle even on the
    // blocking-send path. For continuations, append the incoming
    // message and transition INPUT_REQUIRED/AUTH_REQUIRED → WORKING.
    if is_continuation {
        state
            .task_storage
            .append_message(tenant, &task_id, owner, message.clone())
            .await
            .map_err(A2aError::from)?;

        let working_event = StreamEvent::StatusUpdate {
            status_update: crate::streaming::StatusUpdatePayload {
                task_id: task_id.clone(),
                context_id: context_id.clone(),
                status: serde_json::to_value(TaskStatus::new(TaskState::Working))
                    .unwrap_or_default(),
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
    } else {
        let mut task = Task::new(&task_id, TaskStatus::new(TaskState::Submitted))
            .with_context_id(&context_id);
        task.append_message(message.clone());

        let submitted_event = StreamEvent::StatusUpdate {
            status_update: crate::streaming::StatusUpdatePayload {
                task_id: task_id.clone(),
                context_id: context_id.clone(),
                status: serde_json::to_value(TaskStatus::new(TaskState::Submitted))
                    .unwrap_or_default(),
            },
        };
        state
            .atomic_store
            .create_task_with_events(tenant, owner, task, vec![submitted_event])
            .await
            .map_err(A2aError::from)?;
        state.event_broker.notify(&task_id).await;

        let working_event = StreamEvent::StatusUpdate {
            status_update: crate::streaming::StatusUpdatePayload {
                task_id: task_id.clone(),
                context_id: context_id.clone(),
                status: serde_json::to_value(TaskStatus::new(TaskState::Working))
                    .unwrap_or_default(),
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
    }

    // Spawn the executor on a tracked handle with a live EventSink.
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
    };
    let spawn = crate::server::spawn::spawn_tracked_executor(spawn_deps, scope)?;

    if return_immediately {
        // Non-blocking send (A2A spec §3.2.2, proto
        // SendMessageConfiguration.return_immediately=true): return the
        // task in its current state immediately. Drop yielded_rx — the
        // background executor keeps running; the caller polls / streams
        // to observe completion.
        drop(spawn.yielded_rx);
        let current = state
            .task_storage
            .get_task(tenant, &task_id, owner, None)
            .await
            .map_err(A2aError::from)?
            .ok_or_else(|| A2aError::TaskNotFound {
                task_id: task_id.clone(),
            })?;
        return Ok(Json(serde_json::json!({
            "task": serde_json::to_value(&current).unwrap_or_default()
        })));
    }

    // Blocking send: await yielded_rx with the two-deadline timeout.
    let task = await_yielded_with_two_deadlines(
        spawn.yielded_rx,
        spawn.cancellation,
        spawn.handle,
        &state,
        tenant,
        owner,
        &task_id,
        &context_id,
    )
    .await?;

    Ok(Json(serde_json::json!({
        "task": serde_json::to_value(&task).unwrap_or_default()
    })))
}

/// Two-deadline blocking-send timeout (ADR-010 §4.1).
///
/// 1. **Soft deadline** (`blocking_task_timeout`): wait for the
///    executor to emit a terminal or interrupted event. If the deadline
///    fires first, trip `cancellation` and enter the cooperative window.
/// 2. **Cooperative window** (through `timeout_abort_grace`): the
///    cancellation token is already set; a well-behaved executor
///    observes it and emits `sink.cancelled(...)` (or another terminal).
/// 3. **Hard deadline** (`soft + grace`): force-commit `FAILED` via
///    `update_task_status_with_events` — CAS-guarded. On
///    `TerminalStateAlreadySet` the executor won the race at the last
///    moment; re-read and return the persisted terminal. On success,
///    abort the spawned JoinHandle as best-effort cleanup.
///
/// Returns the Task the caller should hand back to the client.
#[allow(clippy::too_many_arguments)] // router glue: all args are request-derived
async fn await_yielded_with_two_deadlines(
    mut yielded_rx: tokio::sync::oneshot::Receiver<Task>,
    cancellation: tokio_util::sync::CancellationToken,
    handle: Arc<crate::server::in_flight::InFlightHandle>,
    state: &AppState,
    tenant: &str,
    owner: &str,
    task_id: &str,
    context_id: &str,
) -> Result<Task, A2aError> {
    let soft = tokio::time::Instant::now() + state.runtime_config.blocking_task_timeout;
    let hard = soft + state.runtime_config.timeout_abort_grace;

    // Soft wait: wait for yielded OR the soft deadline.
    let soft_outcome = tokio::select! {
        result = &mut yielded_rx => YieldedOutcome::Yielded(result.ok()),
        _ = tokio::time::sleep_until(soft) => YieldedOutcome::SoftTimeout,
    };

    if let YieldedOutcome::Yielded(Some(task)) = soft_outcome {
        return Ok(task);
    }
    if let YieldedOutcome::Yielded(None) = soft_outcome {
        // The yielded sender was dropped without firing — the
        // supervisor sentinel ran its cleanup before any terminal
        // landed. Surface this as Internal so the client sees a clear
        // failure rather than hanging.
        return Err(A2aError::Internal(
            "executor exited without emitting a terminal or interrupted event".into(),
        ));
    }

    // Cooperative wait: soft deadline expired. Trip the cancellation
    // token so the executor observes cancellation, and wait for the
    // cooperative window to expire (or for yielded to fire from the
    // executor's own terminal emit).
    cancellation.cancel();

    let cooperative_outcome = tokio::select! {
        result = &mut yielded_rx => YieldedOutcome::Yielded(result.ok()),
        _ = tokio::time::sleep_until(hard) => YieldedOutcome::HardTimeout,
    };

    if let YieldedOutcome::Yielded(Some(task)) = cooperative_outcome {
        return Ok(task);
    }
    // Yielded-None during the cooperative wait also falls through to the
    // hard timeout.

    // Hard timeout: force-commit FAILED via CAS.
    let reason_msg = Message::new(
        uuid::Uuid::now_v7().to_string(),
        turul_a2a_types::Role::Agent,
        vec![turul_a2a_types::Part::text(
            "task timed out: hard deadline exceeded without terminal emission",
        )],
    );
    let failed_status = TaskStatus::new(TaskState::Failed).with_message(reason_msg);
    let failed_event = StreamEvent::StatusUpdate {
        status_update: crate::streaming::StatusUpdatePayload {
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            status: serde_json::to_value(&failed_status).unwrap_or_default(),
        },
    };

    let failed_event_for_dispatch = failed_event.clone();
    let result = state
        .atomic_store
        .update_task_status_with_events(
            tenant,
            task_id,
            owner,
            failed_status,
            vec![failed_event],
        )
        .await;

    match result {
        Ok((task, seqs)) => {
            // Abort the spawned executor. The cloneable AbortHandle
            // is independent of the supervisor's JoinHandle
            // ownership, so this works whether or not the supervisor
            // has already taken the JoinHandle to `.await` it. The
            // supervisor continues to track the task for registry
            // cleanup — its `.await` returns `JoinError::Cancelled`
            // once the abort propagates at the next executor yield
            // point.
            handle.abort();
            // ADR-011 §13.13: framework-committed terminals fan out
            // to push configs identically to executor-emitted ones.
            // Hard-timeout FAILED lands here.
            if let Some(dispatcher) = &state.push_dispatcher {
                let seq = seqs.first().copied().unwrap_or(0);
                dispatcher.dispatch(
                    tenant.to_string(),
                    owner.to_string(),
                    task.clone(),
                    vec![(seq, failed_event_for_dispatch)],
                );
            }
            state.event_broker.notify(task_id).await;
            Ok(task)
        }
        Err(crate::storage::A2aStorageError::TerminalStateAlreadySet { .. }) => {
            // Executor emitted its own terminal while we were
            // racing the hard deadline. Return the actually-persisted
            // terminal.
            state.event_broker.notify(task_id).await;
            let persisted = state
                .task_storage
                .get_task(tenant, task_id, owner, None)
                .await
                .map_err(A2aError::from)?
                .ok_or_else(|| A2aError::TaskNotFound {
                    task_id: task_id.to_string(),
                })?;
            Ok(persisted)
        }
        Err(e) => Err(A2aError::from(e)),
    }
}

#[allow(clippy::large_enum_variant)] // Option<Task> is ~hundreds of bytes; the enum is short-lived on the stack of one function
enum YieldedOutcome {
    Yielded(Option<Task>),
    SoftTimeout,
    HardTimeout,
}

pub(crate) async fn core_list_tasks(
    state: AppState,
    tenant: &str,
    owner: &str,
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
        owner: Some(owner.to_string()),
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
    owner: &str,
    task_id: &str,
    history_length: Option<i32>,
) -> Result<Json<serde_json::Value>, A2aError> {
    let task = state
        .task_storage
        .get_task(tenant, task_id, owner, history_length)
        .await
        .map_err(A2aError::from)?
        .ok_or_else(|| A2aError::TaskNotFound {
            task_id: task_id.to_string(),
        })?;

    Ok(Json(serde_json::to_value(&task).unwrap_or_default()))
}

/// ADR-012 `CancelTask` handler.
///
/// Sequence:
/// 1. Validate existence + ownership (via owner-scoped `get_task`).
/// 2. Reject with 409 if task is already terminal.
/// 3. Write the cancel marker (owner-scoped, idempotent).
/// 4. Trip the local in-flight cancellation token if present.
/// 5. Wait up to `cancel_handler_grace`, polling task state every
///    `cancel_handler_poll_interval`. On observed terminal, return that
///    persisted task snapshot (cooperative cancel won the race).
/// 6. On grace expiry, force-commit `CANCELED` via the atomic store
///    (CAS-guarded via the atomic store's terminal-preservation
///    contract). On success, return CANCELED. On
///    `TerminalStateAlreadySet`, re-read and return the actual persisted
///    terminal (another path won at the CAS layer).
///
/// Framework-committed terminals carry `message = None` (ADR-012 §8) —
/// the plain `TaskStatus::new(TaskState::Canceled)` constructor produces
/// exactly that.
#[doc(hidden)]
pub async fn core_cancel_task(
    state: AppState,
    tenant: &str,
    owner: &str,
    task_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    // Step 1: validate existence + ownership. `get_task` is owner-scoped —
    // wrong owner returns None (TaskNotFound) as anti-enumeration.
    let initial_task = state
        .task_storage
        .get_task(tenant, task_id, owner, Some(0))
        .await
        .map_err(A2aError::from)?
        .ok_or_else(|| A2aError::TaskNotFound { task_id: task_id.to_string() })?;

    // Step 2: reject terminal tasks up-front with 409.
    if let Some(status) = initial_task.status() {
        if let Ok(s) = status.state() {
            if turul_a2a_types::state_machine::is_terminal(s) {
                return Err(A2aError::TaskNotCancelable { task_id: task_id.to_string() });
            }
        }
    }

    let context_id = initial_task.context_id().to_string();

    // Step 3: write the cancel-requested marker. Idempotent; errors map
    // to the usual wire responses.
    match state
        .task_storage
        .set_cancel_requested(tenant, task_id, owner)
        .await
    {
        Ok(()) => {}
        Err(A2aStorageError::TaskNotFound(_)) => {
            return Err(A2aError::TaskNotFound { task_id: task_id.to_string() });
        }
        Err(A2aStorageError::TerminalState(_))
        | Err(A2aStorageError::InvalidTransition { .. })
        | Err(A2aStorageError::TerminalStateAlreadySet { .. }) => {
            return Err(A2aError::TaskNotCancelable { task_id: task_id.to_string() });
        }
        Err(other) => return Err(A2aError::from(other)),
    }

    // Step 4: fast-path token trip if this instance owns the in-flight
    // executor for this task. Cross-instance cases rely on the supervisor
    // poll loop (see `server::in_flight::run_cross_instance_cancel_poller`).
    let in_flight_key = (tenant.to_string(), task_id.to_string());
    if let Some(handle) = state.in_flight.get(&in_flight_key) {
        handle.cancellation.cancel();
    }

    // Step 5: grace-wait with poll. Return early if the task reaches
    // a terminal state via the executor's cooperative response.
    let deadline =
        tokio::time::Instant::now() + state.runtime_config.cancel_handler_grace;
    let poll_interval = state.runtime_config.cancel_handler_poll_interval;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }

        match state
            .task_storage
            .get_task(tenant, task_id, owner, Some(0))
            .await
            .map_err(A2aError::from)?
        {
            Some(current) => {
                if let Some(status) = current.status() {
                    if let Ok(s) = status.state() {
                        if turul_a2a_types::state_machine::is_terminal(s) {
                            // Cooperative terminal (or another path) resolved
                            // the cancel during grace. Return persisted state.
                            state.event_broker.notify(task_id).await;
                            return Ok(Json(serde_json::to_value(&current).unwrap_or_default()));
                        }
                    }
                }
            }
            None => {
                return Err(A2aError::TaskNotFound { task_id: task_id.to_string() });
            }
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let sleep_for = std::cmp::min(poll_interval, remaining);
        tokio::time::sleep(sleep_for).await;
    }

    // Step 6: grace expired. Force-commit CANCELED via atomic store.
    // Per ADR-012 §8, framework-committed terminals use `message = None`
    // so history / SSE consumers can distinguish from executor-authored
    // cancels without conflating framework telemetry with agent output.
    let cancel_event = StreamEvent::StatusUpdate {
        status_update: crate::streaming::StatusUpdatePayload {
            task_id: task_id.to_string(),
            context_id,
            status: serde_json::to_value(TaskStatus::new(TaskState::Canceled))
                .unwrap_or_default(),
        },
    };
    let cancel_event_for_dispatch = cancel_event.clone();

    let result = state
        .atomic_store
        .update_task_status_with_events(
            tenant,
            task_id,
            owner,
            TaskStatus::new(TaskState::Canceled),
            vec![cancel_event],
        )
        .await;

    match result {
        Ok((task, seqs)) => {
            // ADR-011 §13.13: framework-committed CANCELED must
            // trigger push delivery exactly like an executor-emitted
            // cancel. The :cancel grace-expiry path reaches this
            // arm after the executor failed to react in time.
            if let Some(dispatcher) = &state.push_dispatcher {
                let seq = seqs.first().copied().unwrap_or(0);
                dispatcher.dispatch(
                    tenant.to_string(),
                    owner.to_string(),
                    task.clone(),
                    vec![(seq, cancel_event_for_dispatch)],
                );
            }
            state.event_broker.notify(task_id).await;
            Ok(Json(serde_json::to_value(&task).unwrap_or_default()))
        }
        Err(A2aStorageError::TerminalStateAlreadySet { .. }) => {
            // Race resolved at the atomic-store CAS: another writer
            // (executor emitting its own terminal, or a concurrent
            // CancelTask from another instance) committed first. Re-read
            // and return the actual persisted terminal.
            let persisted = state
                .task_storage
                .get_task(tenant, task_id, owner, None)
                .await
                .map_err(A2aError::from)?
                .ok_or_else(|| A2aError::TaskNotFound { task_id: task_id.to_string() })?;
            state.event_broker.notify(task_id).await;
            Ok(Json(serde_json::to_value(&persisted).unwrap_or_default()))
        }
        Err(A2aStorageError::TaskNotFound(id)) => Err(A2aError::TaskNotFound { task_id: id }),
        Err(A2aStorageError::TerminalState(_))
        | Err(A2aStorageError::InvalidTransition { .. }) => {
            Err(A2aError::TaskNotCancelable { task_id: task_id.to_string() })
        }
        Err(other) => Err(A2aError::from(other)),
    }
}

/// Verify the caller owns the task before push config operations.
async fn verify_task_ownership(
    state: &AppState,
    tenant: &str,
    owner: &str,
    task_id: &str,
) -> Result<(), A2aError> {
    state
        .task_storage
        .get_task(tenant, task_id, owner, Some(0))
        .await
        .map_err(A2aError::from)?
        .ok_or_else(|| A2aError::TaskNotFound {
            task_id: task_id.to_string(),
        })?;
    Ok(())
}

pub(crate) async fn core_create_push_config(
    state: AppState,
    tenant: &str,
    owner: &str,
    task_id: &str,
    body: String,
) -> Result<Json<serde_json::Value>, A2aError> {
    verify_task_ownership(&state, tenant, owner, task_id).await?;

    let mut config: turul_a2a_proto::TaskPushNotificationConfig = serde_json::from_str(&body)
        .map_err(|e| A2aError::InvalidRequest {
            message: format!("Invalid push config: {e}"),
        })?;
    config.task_id = task_id.to_string();

    // ADR-011 §R1: URL must parse at CRUD time. The dispatcher used
    // to silently skip unparseable URLs with no failed-delivery row,
    // so an operator who mistyped the webhook had no feedback loop.
    // Scheme (http/https) and SSRF checks still happen at delivery
    // time — those depend on runtime flags (`allow_insecure_push_urls`)
    // the CRUD call does not see.
    if config.url.is_empty() {
        return Err(A2aError::InvalidRequest {
            message: "push config url is required".into(),
        });
    }
    if let Err(e) = url::Url::parse(&config.url) {
        return Err(A2aError::InvalidRequest {
            message: format!("push config url is not a valid URL: {e}"),
        });
    }

    let created = state.push_storage.create_config(tenant, config).await.map_err(A2aError::from)?;
    Ok(Json(serde_json::to_value(&created).unwrap_or_default()))
}

pub(crate) async fn core_list_push_configs(
    state: AppState,
    tenant: &str,
    owner: &str,
    task_id: &str,
    query: &PushConfigQuery,
) -> Result<Json<serde_json::Value>, A2aError> {
    verify_task_ownership(&state, tenant, owner, task_id).await?;

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
    owner: &str,
    task_id: &str,
    config_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    verify_task_ownership(&state, tenant, owner, task_id).await?;

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
    owner: &str,
    task_id: &str,
    config_id: &str,
) -> Result<Json<serde_json::Value>, A2aError> {
    verify_task_ownership(&state, tenant, owner, task_id).await?;

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

    // v0.3 compat tests live in compat_v03.rs
}
