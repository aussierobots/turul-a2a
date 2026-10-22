//! HTTP router matching proto google.api.http annotations.
//!
//! Routes use axum wildcard catch-all for task paths because the proto
//! uses `{id=*}:action` patterns that don't map directly to axum's `:param` syntax.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::error::A2aError;
use crate::executor::AgentExecutor;
use crate::storage::{
    A2aAtomicStore, A2aPushNotificationStorage, A2aStorageError, A2aTaskStorage, TaskFilter,
    TaskListPage,
};
use crate::streaming::StreamEvent;
use crate::streaming::task_stream::{StreamItem, TaskEventStream, task_event_stream};
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

    /// In-flight task registry. Holds one
    /// [`crate::server::in_flight::InFlightHandle`] per spawned executor
    /// — keyed by `(tenant, task_id)`. Populated by the executor spawn
    /// path; consumed by the `:cancel` handler to trip the local
    /// cancellation token if the executor runs on this instance. May be
    /// empty when no executor has been spawned through the registry yet.
    pub in_flight: Arc<crate::server::in_flight::InFlightRegistry>,

    /// Supervisor-only cancel-marker reads. Separate from `task_storage`
    /// so handler code cannot reach the unscoped reads. Use
    /// `set_cancel_requested` on `task_storage` for marker writes
    /// (owner-scoped, handler-safe).
    pub cancellation_supervisor: Arc<dyn crate::storage::A2aCancellationSupervisor>,

    /// Push-delivery coordination store. `None` on
    /// deployments that do not wire a `PushDeliveryWorker`; the
    /// push config CRUD paths continue to work without it —
    /// configs are stored, just not delivered. Set via
    /// [`crate::server::A2aServerBuilder::push_delivery_store`].
    pub push_delivery_store: Option<Arc<dyn crate::push::A2aPushDeliveryStore>>,

    /// Push-delivery dispatcher. Populated by the server builder iff
    /// `push_delivery_store` is wired. Handler and executor commit paths
    /// call [`crate::push::PushDispatcher::dispatch`] after a successful
    /// terminal atomic-store write so every terminal — executor-driven,
    /// framework-forced CANCEL, or hard-timeout FAILED — fans out to
    /// registered push configs by the same contract.
    pub push_dispatcher: Option<Arc<crate::push::PushDispatcher>>,

    /// Durable executor queue. When `Some`, the
    /// `return_immediately = true` path in
    /// [`core_send_message`] enqueues a
    /// [`crate::durable_executor::QueuedExecutorJob`] instead of
    /// spawning the executor locally. A separate invocation consumes
    /// the queue and runs the executor to terminal. Wired exclusively
    /// through `LambdaA2aServerBuilder::with_durable_executor` /
    /// `with_sqs_return_immediately` in `turul-a2a-aws-lambda`; the
    /// long-lived [`crate::server::A2aServerBuilder`] keeps this
    /// `None` because `tokio::spawn` is already durable in a
    /// long-lived process.
    pub durable_executor_queue: Option<Arc<dyn crate::durable_executor::DurableExecutorQueue>>,
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
        .route(
            "/{tenant}/message:stream",
            post(tenant_send_streaming_message_handler),
        )
        .route("/{tenant}/tasks", get(tenant_list_tasks_handler))
        .route(
            "/{tenant}/extendedAgentCard",
            get(extended_agent_card_handler),
        )
        .route(
            "/{tenant}/tasks/{*rest}",
            get(tenant_task_get_dispatch)
                .post(tenant_task_post_dispatch)
                .delete(tenant_task_delete_dispatch),
        );

    // JSON-RPC dispatch: /jsonrpc is the canonical endpoint
    let router = router.route(
        crate::jsonrpc::JSONRPC_PATH,
        post(crate::jsonrpc::jsonrpc_dispatch_handler),
    );

    // A2A v0.3 compat: root POST route for v0.3-wire clients (Strands
    // Agents, a2a-sdk 0.3.x) that POST JSON-RPC to the agent card URL.
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
    /// /tasks/{id}:subscribe — GET (proto annotation) or POST (spec
    /// Section 11.3.2 text) = SubscribeToTask (SSE)
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
    Path(rest): Path<String>,
    Query(query): Query<TaskGetCombinedQuery>,
) -> Result<axum::response::Response, A2aError> {
    dispatch_task_get(state, DEFAULT_TENANT, ctx.identity.owner(), &rest, &query).await
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
    headers: axum::http::HeaderMap,
    Path(tenant): Path<String>,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    let claims = ctx.identity.claims().cloned();
    let echoed = negotiate_profile_extensions(&state, &headers)?;
    crate::profile_dispatch::check_skill_invocation_body(&echoed, &body)?;
    let Json(value) = core_send_message(state, &tenant, ctx.identity.owner(), claims, body).await?;
    Ok(attach_extension_echo(Json(value).into_response(), &echoed))
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
    Path((tenant, rest)): Path<(String, String)>,
    Query(query): Query<TaskGetCombinedQuery>,
) -> Result<axum::response::Response, A2aError> {
    dispatch_task_get(state, &tenant, ctx.identity.owner(), &rest, &query).await
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
) -> Result<axum::response::Response, A2aError> {
    match parse_task_path(rest) {
        Some(TaskAction::GetTask(id)) => {
            let Json(v) = core_get_task(state, tenant, owner, &id, query.history_length).await?;
            Ok(Json(v).into_response())
        }
        Some(TaskAction::SubscribeToTask(id)) => {
            core_subscribe_to_task(state, tenant, owner, &id).await
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
        _ => Err(A2aError::InvalidRequest {
            message: "Invalid task path".into(),
        }),
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
        Some(TaskAction::SubscribeToTask(id)) => {
            core_subscribe_to_task(state, tenant, owner, &id).await
        }
        Some(TaskAction::PushConfigCollection(task_id)) => {
            let Json(v) = core_create_push_config(state, tenant, owner, &task_id, body).await?;
            Ok(Json(v).into_response())
        }
        _ => Err(A2aError::InvalidRequest {
            message: "Invalid task path".into(),
        }),
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
            let Json(v) =
                core_delete_push_config(state, tenant, owner, &task_id, &config_id).await?;
            Ok(Json(v).into_response())
        }
        _ => Err(A2aError::InvalidRequest {
            message: "Invalid task path".into(),
        }),
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
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
) -> Result<impl IntoResponse, A2aError> {
    match state.executor.extended_agent_card(ctx.identity.claims()) {
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
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    let claims = ctx.identity.claims().cloned();
    let echoed = negotiate_profile_extensions(&state, &headers)?;
    crate::profile_dispatch::check_skill_invocation_body(&echoed, &body)?;
    let response =
        core_send_streaming_message(state, DEFAULT_TENANT, ctx.identity.owner(), claims, body)
            .await?;
    Ok(attach_extension_echo(response, &echoed))
}

async fn tenant_send_streaming_message_handler(
    State(state): State<AppState>,
    axum::Extension(ctx): axum::Extension<crate::middleware::RequestContext>,
    headers: axum::http::HeaderMap,
    Path(tenant): Path<String>,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    let claims = ctx.identity.claims().cloned();
    let echoed = negotiate_profile_extensions(&state, &headers)?;
    crate::profile_dispatch::check_skill_invocation_body(&echoed, &body)?;
    let response =
        core_send_streaming_message(state, &tenant, ctx.identity.owner(), claims, body).await?;
    Ok(attach_extension_echo(response, &echoed))
}

/// Open the stream for `SendStreamingMessage`: resolve the target task,
/// commit the start of the turn, snapshot the task, then spawn the
/// executor. The snapshot and its event sequence are read before the
/// executor runs, so nothing it emits can fall between them.
///
/// Shared by every transport.
pub(crate) async fn open_send_stream(
    state: AppState,
    tenant: &str,
    owner: &str,
    claims: Option<serde_json::Value>,
    body: String,
) -> Result<TaskEventStream, A2aError> {
    let request: turul_a2a_proto::SendMessageRequest =
        serde_json::from_str(&body).map_err(|e| A2aError::InvalidRequest {
            message: format!("Invalid request body: {e}"),
        })?;
    let (accepted_output_modes, request_metadata) = executor_request_inputs(&request);
    let history_length = request
        .configuration
        .as_ref()
        .and_then(|c| c.history_length);
    let proto_message = request.message.ok_or(A2aError::InvalidRequest {
        message: "message field is required".into(),
    })?;
    let message = Message::try_from(proto_message).map_err(|e| A2aError::InvalidRequest {
        message: format!("Invalid message: {e}"),
    })?;
    let inline_push_config = inline_push_config(request.configuration.as_ref())?;

    let target = resolve_send_target(&state, tenant, owner, &message).await?;
    start_task_turn(&state, tenant, owner, &target, &message).await?;
    if let Some(cfg) = inline_push_config {
        register_inline_push_config(&state, tenant, owner, &target, cfg).await?;
    }

    let sequence = state
        .event_store
        .latest_sequence(tenant, &target.task_id)
        .await
        .map_err(A2aError::from)?;
    let task = state
        .task_storage
        .get_task(tenant, &target.task_id, owner, history_length)
        .await
        .map_err(A2aError::from)?
        .ok_or_else(|| A2aError::TaskNotFound {
            task_id: target.task_id.clone(),
        })?;
    let wake_rx = state.event_broker.subscribe(&target.task_id).await;

    // Streaming does not block on the executor's yielded receiver;
    // subscribers observe its events through the durable store.
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
        task_id: target.task_id.clone(),
        context_id: target.context_id.clone(),
        message,
        claims,
        accepted_output_modes,
        request_metadata,
    };
    let _spawn = crate::server::spawn::spawn_tracked_executor(spawn_deps, scope)?;

    Ok(task_event_stream(
        state.event_store,
        tenant.to_string(),
        target.task_id,
        task,
        sequence,
        wake_rx,
    ))
}

/// Open the stream for `SubscribeToTask`. Unknown or foreign tasks are
/// `TaskNotFound`; terminal tasks are `UnsupportedOperation` (A2A
/// §3.1.6). Every subscription — including a client reconnecting after
/// a dropped stream — opens with the task snapshot, which carries the
/// full current state (§3.1.6 first-event rule; §3.5.2 reconnection).
///
/// Shared by every transport.
pub(crate) async fn open_subscribe_stream(
    state: AppState,
    tenant: &str,
    owner: &str,
    task_id: &str,
) -> Result<TaskEventStream, A2aError> {
    // Read the sequence before the snapshot: an event committed in
    // between may appear in both, but none can be missed.
    let sequence = state
        .event_store
        .latest_sequence(tenant, task_id)
        .await
        .map_err(A2aError::from)?;
    let task = state
        .task_storage
        .get_task(tenant, task_id, owner, None)
        .await
        .map_err(A2aError::from)?
        .ok_or_else(|| A2aError::TaskNotFound {
            task_id: task_id.to_string(),
        })?;

    if let Some(status) = task.status()
        && let Ok(s) = status.state()
        && s.is_terminal()
    {
        return Err(A2aError::UnsupportedOperation {
            message: format!("Task {task_id} is already in terminal state {s:?}"),
        });
    }
    let wake_rx = state.event_broker.subscribe(task_id).await;

    Ok(task_event_stream(
        state.event_store,
        tenant.to_string(),
        task_id.to_string(),
        task,
        sequence,
        wake_rx,
    ))
}

pub(crate) async fn core_send_streaming_message(
    state: AppState,
    tenant: &str,
    owner: &str,
    claims: Option<serde_json::Value>,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    let stream = open_send_stream(state, tenant, owner, claims, body).await?;
    Ok(sse_response(stream, |item| item))
}

pub(crate) async fn core_subscribe_to_task(
    state: AppState,
    tenant: &str,
    owner: &str,
    task_id: &str,
) -> Result<axum::response::Response, A2aError> {
    let stream = open_subscribe_stream(state, tenant, owner, task_id).await?;
    Ok(sse_response(stream, |item| item))
}

/// Encode a task stream as SSE. Each item becomes one event whose
/// `data:` is `wrap` applied to the `StreamResponse` JSON
/// (`{"task": …}` / `{"statusUpdate": …}` / `{"artifactUpdate": …}`).
/// A storage error ends the stream.
pub(crate) fn sse_response(
    stream: TaskEventStream,
    wrap: impl Fn(serde_json::Value) -> serde_json::Value + Send + 'static,
) -> axum::response::Response {
    use tokio_stream::StreamExt;

    let events = stream.items.map_while(move |item| {
        let body = match item.ok()? {
            StreamItem::Task(task) => {
                serde_json::json!({"task": serde_json::to_value(&task).unwrap_or_default()})
            }
            StreamItem::Event(event) => serde_json::to_value(&event).unwrap_or_default(),
        };
        let data = serde_json::to_string(&wrap(body)).unwrap_or_default();
        Some(Ok::<_, std::convert::Infallible>(
            Event::default().data(data),
        ))
    });
    Sse::new(events)
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
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<axum::response::Response, A2aError> {
    let claims = ctx.identity.claims().cloned();
    let echoed = negotiate_profile_extensions(&state, &headers)?;
    crate::profile_dispatch::check_skill_invocation_body(&echoed, &body)?;
    let Json(value) =
        core_send_message(state, DEFAULT_TENANT, ctx.identity.owner(), claims, body).await?;
    Ok(attach_extension_echo(Json(value).into_response(), &echoed))
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
    /// RFC 3339 / ISO 8601 instant (proto `google.protobuf.Timestamp`
    /// JSON form).
    #[serde(rename = "statusTimestampAfter")]
    pub(crate) status_timestamp_after: Option<String>,
}

impl ListTasksQuery {
    /// The query a proto `ListTasksRequest` carries: the gRPC message, or
    /// JSON-RPC params decoded with the proto3 JSON mapping. proto3 leaves an
    /// unset string empty and an unset enum `TASK_STATE_UNSPECIFIED`; both
    /// mean "no filter". A value no `TaskState` has, or a `Timestamp` whose
    /// nanos fall outside `0..=999_999_999`, is an invalid request.
    pub(crate) fn from_proto(req: &turul_a2a_proto::ListTasksRequest) -> Result<Self, A2aError> {
        let status = match turul_a2a_proto::TaskState::try_from(req.status) {
            Ok(turul_a2a_proto::TaskState::Unspecified) => None,
            Ok(s) => Some(s.as_str_name().to_string()),
            Err(_) => {
                return Err(A2aError::InvalidRequest {
                    message: format!("Invalid status value: {}", req.status),
                });
            }
        };
        let status_timestamp_after = req
            .status_timestamp_after
            .as_ref()
            .map(|ts| {
                u32::try_from(ts.nanos)
                    .ok()
                    .filter(|nanos| *nanos < 1_000_000_000)
                    .and_then(|nanos| chrono::DateTime::from_timestamp(ts.seconds, nanos))
                    .map(|at| at.to_rfc3339())
                    .ok_or_else(|| A2aError::InvalidRequest {
                        message: "statusTimestampAfter is out of range".into(),
                    })
            })
            .transpose()?;
        Ok(Self {
            context_id: Some(req.context_id.clone()).filter(|s| !s.is_empty()),
            status,
            page_size: req.page_size,
            page_token: Some(req.page_token.clone()).filter(|s| !s.is_empty()),
            history_length: req.history_length,
            include_artifacts: req.include_artifacts,
            status_timestamp_after,
        })
    }
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

#[doc(hidden)]
/// The task a `SendMessage` / `SendStreamingMessage` targets.
pub(crate) struct SendTarget {
    pub(crate) task_id: String,
    pub(crate) context_id: String,
    pub(crate) is_continuation: bool,
}

/// Resolve which task an incoming message targets. Shared by blocking
/// and streaming send so both apply the same rules (A2A §3.1.2, §3.4.3):
///
/// - no `taskId` → a new task in the message's `contextId`, or a new one;
/// - unknown or foreign `taskId` → `TaskNotFound`;
/// - terminal task → `UnsupportedOperation`;
/// - `contextId` differing from the task's → rejected; absent → inferred;
/// - only INPUT_REQUIRED / AUTH_REQUIRED tasks accept follow-ups, and
///   only once the previous turn's executor has returned on this
///   instance. Checked before any write, so a refused follow-up leaves
///   the task untouched.
pub(crate) async fn resolve_send_target(
    state: &AppState,
    tenant: &str,
    owner: &str,
    message: &Message,
) -> Result<SendTarget, A2aError> {
    let msg_task_id = message.as_proto().task_id.clone();
    if msg_task_id.is_empty() {
        let context_id = if message.as_proto().context_id.is_empty() {
            uuid::Uuid::now_v7().to_string()
        } else {
            message.as_proto().context_id.clone()
        };
        return Ok(SendTarget {
            task_id: uuid::Uuid::now_v7().to_string(),
            context_id,
            is_continuation: false,
        });
    }

    let existing = state
        .task_storage
        .get_task(tenant, &msg_task_id, owner, None)
        .await
        .map_err(A2aError::from)?
        .ok_or_else(|| A2aError::TaskNotFound {
            task_id: msg_task_id.clone(),
        })?;

    let msg_context_id = &message.as_proto().context_id;
    if !msg_context_id.is_empty() && msg_context_id != existing.context_id() {
        return Err(A2aError::InvalidRequest {
            message: format!(
                "contextId mismatch: message has '{}' but task {} has '{}'",
                msg_context_id,
                msg_task_id,
                existing.context_id()
            ),
        });
    }

    if let Some(status) = existing.status()
        && let Ok(s) = status.state()
    {
        match s {
            TaskState::InputRequired | TaskState::AuthRequired => {}
            s if s.is_terminal() => {
                return Err(A2aError::UnsupportedOperation {
                    message: format!(
                        "Task {msg_task_id} is in terminal state {s:?} and cannot accept further messages"
                    ),
                });
            }
            _ => {
                return Err(A2aError::InvalidRequest {
                    message: format!(
                        "Task {msg_task_id} is in state {s:?}, only INPUT_REQUIRED or AUTH_REQUIRED tasks accept follow-up messages"
                    ),
                });
            }
        }
    }

    if state
        .in_flight
        .get(&(tenant.to_string(), msg_task_id.clone()))
        .is_some()
    {
        return Err(A2aError::UnsupportedOperation {
            message: format!(
                "Task {msg_task_id} is still being processed by its executor; \
                 send the follow-up once the current turn has finished"
            ),
        });
    }

    Ok(SendTarget {
        task_id: msg_task_id,
        context_id: existing.context_id().to_string(),
        is_continuation: true,
    })
}

/// Commit the start of a turn so subscribers and push delivery observe
/// the lifecycle: a new task is created in SUBMITTED and advanced to
/// WORKING; a continuation appends the message and moves
/// INPUT_REQUIRED / AUTH_REQUIRED → WORKING. Every status change goes
/// through the CAS-guarded atomic store.
pub(crate) async fn start_task_turn(
    state: &AppState,
    tenant: &str,
    owner: &str,
    target: &SendTarget,
    message: &Message,
) -> Result<(), A2aError> {
    let status_event = |state: TaskState| StreamEvent::StatusUpdate {
        status_update: crate::streaming::StatusUpdatePayload {
            task_id: target.task_id.clone(),
            context_id: target.context_id.clone(),
            status: serde_json::to_value(TaskStatus::new(state)).unwrap_or_default(),
        },
    };

    if target.is_continuation {
        state
            .task_storage
            .append_message(tenant, &target.task_id, owner, message.clone())
            .await
            .map_err(A2aError::from)?;
    } else {
        let mut task = Task::new(&target.task_id, TaskStatus::new(TaskState::Submitted))
            .with_context_id(&target.context_id);
        task.append_message(message.clone());
        state
            .atomic_store
            .create_task_with_events(
                tenant,
                owner,
                task,
                vec![status_event(TaskState::Submitted)],
            )
            .await
            .map_err(A2aError::from)?;
        state.event_broker.notify(&target.task_id).await;
    }

    state
        .atomic_store
        .update_task_status_with_events(
            tenant,
            &target.task_id,
            owner,
            TaskStatus::new(TaskState::Working),
            vec![status_event(TaskState::Working)],
        )
        .await
        .map_err(A2aError::from)?;
    state.event_broker.notify(&target.task_id).await;
    Ok(())
}

/// `SendMessageConfiguration.taskPushNotificationConfig`, checked
/// before anything is written: a missing or malformed URL is an
/// `InvalidRequest` and no task is persisted. Push configs operate
/// independently of execution mode (A2A v1.0.1 §3.2.2), so blocking,
/// non-blocking and streaming sends all register it.
fn inline_push_config(
    configuration: Option<&turul_a2a_proto::SendMessageConfiguration>,
) -> Result<Option<turul_a2a_proto::TaskPushNotificationConfig>, A2aError> {
    let Some(cfg) = configuration.and_then(|c| c.task_push_notification_config.clone()) else {
        return Ok(None);
    };
    if cfg.url.is_empty() {
        return Err(A2aError::InvalidRequest {
            message: "inline push config url is required".into(),
        });
    }
    if let Err(e) = url::Url::parse(&cfg.url) {
        return Err(A2aError::InvalidRequest {
            message: format!("inline push config url is not a valid URL: {e}"),
        });
    }
    Ok(Some(cfg))
}

/// Register an inline push config against the task whose turn has
/// just started, before its executor is spawned, so no executor runs
/// without its callback. If registration fails the task is moved to
/// FAILED and the storage error is returned; the executor never runs.
async fn register_inline_push_config(
    state: &AppState,
    tenant: &str,
    owner: &str,
    target: &SendTarget,
    mut cfg: turul_a2a_proto::TaskPushNotificationConfig,
) -> Result<(), A2aError> {
    cfg.task_id = target.task_id.clone();
    if let Err(e) = state.push_storage.create_config(tenant, cfg).await {
        tracing::warn!(
            tenant = tenant,
            owner = owner,
            task_id = %target.task_id,
            "inline push config registration failed: transitioning task to FAILED"
        );
        let reason = format!("inline push notification config registration failed: {e}");
        fail_unstarted_task(state, tenant, owner, target, &reason).await;
        return Err(A2aError::from(e));
    }
    Ok(())
}

/// Move a task whose turn started but whose executor never ran to
/// FAILED, with `reason` as the agent status message. Best effort: if
/// the transition itself fails the caller still returns its original
/// error, and the task stays WORKING until something else resolves it.
async fn fail_unstarted_task(
    state: &AppState,
    tenant: &str,
    owner: &str,
    target: &SendTarget,
    reason: &str,
) {
    let reason_msg = Message::new(
        uuid::Uuid::now_v7().to_string(),
        turul_a2a_types::Role::Agent,
        vec![turul_a2a_types::Part::text(reason)],
    );
    let failed_status = TaskStatus::new(TaskState::Failed).with_message(reason_msg);
    let failed_event = StreamEvent::StatusUpdate {
        status_update: crate::streaming::StatusUpdatePayload {
            task_id: target.task_id.clone(),
            context_id: target.context_id.clone(),
            status: serde_json::to_value(&failed_status).unwrap_or_default(),
        },
    };
    if let Err(compensate_err) = state
        .atomic_store
        .update_task_status_with_events(
            tenant,
            &target.task_id,
            owner,
            failed_status,
            vec![failed_event],
        )
        .await
    {
        tracing::error!(
            tenant = tenant,
            owner = owner,
            task_id = %target.task_id,
            error = %compensate_err,
            "FAILED compensation itself failed: task may remain WORKING"
        );
    }
    state.event_broker.notify(&target.task_id).await;
}

/// Request-level executor inputs carried by a `SendMessageRequest`:
/// `configuration.accepted_output_modes` and `metadata` (as a JSON
/// object). The single extraction point for the blocking, streaming and
/// durable-queue send paths.
fn executor_request_inputs(
    request: &turul_a2a_proto::SendMessageRequest,
) -> (Vec<String>, Option<serde_json::Value>) {
    let accepted_output_modes = request
        .configuration
        .as_ref()
        .map(|c| c.accepted_output_modes.clone())
        .unwrap_or_default();
    let request_metadata = request
        .metadata
        .as_ref()
        .and_then(|m| serde_json::to_value(m).ok());
    (accepted_output_modes, request_metadata)
}

pub async fn core_send_message(
    state: AppState,
    tenant: &str,
    owner: &str,
    claims: Option<serde_json::Value>,
    body: String,
) -> Result<Json<serde_json::Value>, A2aError> {
    let request: turul_a2a_proto::SendMessageRequest =
        serde_json::from_str(&body).map_err(|e| A2aError::InvalidRequest {
            message: format!("Invalid request body: {e}"),
        })?;
    let (accepted_output_modes, request_metadata) = executor_request_inputs(&request);

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

    // Runtimes that cannot guarantee post-return
    // execution (e.g., AWS Lambda) refuse `return_immediately = true`
    // here — before any storage write — to avoid silently orphaning
    // the spawned executor. Mapped to HTTP 400 / JSON-RPC -32004 /
    // gRPC INVALID_ARGUMENT.
    if return_immediately && !state.runtime_config.supports_return_immediately {
        tracing::warn!(
            tenant = tenant,
            owner = owner,
            "rejecting SendMessageConfiguration.return_immediately=true: \
             this runtime does not support post-return executor continuation \
             (a persistent durable-executor worker is required)"
        );
        return Err(A2aError::UnsupportedOperation {
            message: "return_immediately=true is not supported on this runtime \
                      (post-return executor continuation is not guaranteed); \
                      resubmit with return_immediately=false for a blocking \
                      send"
                .into(),
        });
    }

    // Extract SendMessageConfiguration.history_length
    // preserving the proto tri-state — None=unbounded, Some(0)=empty,
    // Some(n>0)=last n (a2a.proto:150-154). Do NOT collapse Some(0) to
    // None; the storage layer's `trim_task` already differentiates
    // (e.g., storage/postgres.rs:212-222).
    let history_length: Option<i32> = request
        .configuration
        .as_ref()
        .and_then(|c| c.history_length);

    let inline_push_config = inline_push_config(request.configuration.as_ref())?;

    let target = resolve_send_target(&state, tenant, owner, &message).await?;
    let task_id = target.task_id.clone();
    let context_id = target.context_id.clone();

    // If a durable executor queue is wired AND
    // `return_immediately` is requested, build the
    // envelope and run `check_payload_size` BEFORE any storage write.
    // Oversize payloads return `A2aError::InvalidRequest` (HTTP 400 /
    // JSON-RPC -32602 / gRPC INVALID_ARGUMENT) with zero task
    // persistence. The built envelope is stashed for the enqueue call
    // after task creation to avoid re-cloning.
    let durable_queue = if return_immediately {
        state.durable_executor_queue.clone()
    } else {
        None
    };
    let durable_job: Option<crate::durable_executor::QueuedExecutorJob> =
        if let Some(queue) = durable_queue.as_ref() {
            let now_micros = chrono::Utc::now().timestamp_micros();
            let job = crate::durable_executor::QueuedExecutorJob {
                version: crate::durable_executor::QueuedExecutorJob::VERSION,
                tenant: tenant.to_string(),
                owner: owner.to_string(),
                task_id: task_id.clone(),
                context_id: context_id.clone(),
                message: message.as_proto().clone(),
                claims: claims.clone(),
                accepted_output_modes: accepted_output_modes.clone(),
                request_metadata: request_metadata.clone(),
                enqueued_at_micros: now_micros,
            };
            if let Err(e) = queue.check_payload_size(&job) {
                return Err(A2aError::InvalidRequest {
                    message: format!("durable executor queue: {e}"),
                });
            }
            Some(job)
        } else {
            None
        };

    start_task_turn(&state, tenant, owner, &target, &message).await?;

    if let Some(cfg) = inline_push_config {
        register_inline_push_config(&state, tenant, owner, &target, cfg).await?;
    }

    // If the durable executor path is active, enqueue the
    // (already-built + size-checked) job and return the Working task.
    // The executor is NOT spawned locally —
    // a separate invocation consumes the queue (SQS Lambda handler)
    // and runs the executor to terminal via
    // `run_executor_for_existing_task`.
    //
    // Enqueue failure uses the same FAILED compensation as inline
    // push-config registration failure (`fail_unstarted_task`).
    if let (Some(queue), Some(job)) = (durable_queue.as_ref(), durable_job) {
        match queue.enqueue(job).await {
            Ok(()) => {
                let current = state
                    .task_storage
                    .get_task(tenant, &task_id, owner, history_length)
                    .await
                    .map_err(A2aError::from)?
                    .ok_or_else(|| A2aError::TaskNotFound {
                        task_id: task_id.clone(),
                    })?;
                return Ok(Json(serde_json::json!({
                    "task": serde_json::to_value(&current).unwrap_or_default()
                })));
            }
            Err(e) => {
                let reason = format!("durable executor enqueue failed: {e}");
                tracing::warn!(
                    tenant = tenant,
                    owner = owner,
                    task_id = %task_id,
                    queue_kind = queue.kind(),
                    "durable executor enqueue failed: transitioning task to FAILED"
                );
                fail_unstarted_task(&state, tenant, owner, &target, &reason).await;
                return Err(A2aError::Internal(reason));
            }
        }
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
        claims: claims.clone(),
        accepted_output_modes,
        request_metadata,
    };
    let spawn = crate::server::spawn::spawn_tracked_executor(spawn_deps, scope)?;

    if return_immediately {
        // Non-blocking send (A2A spec §3.2.2, proto
        // SendMessageConfiguration.return_immediately=true): return the
        // task in its current state immediately. Drop yielded_rx — the
        // background executor keeps running; the caller polls / streams
        // to observe completion.
        //
        // `history_length` is threaded through so the
        // storage-layer `trim_task` honours the proto tri-state
        // (a2a.proto:150-154).
        drop(spawn.yielded_rx);
        let current = state
            .task_storage
            .get_task(tenant, &task_id, owner, history_length)
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

    // If the caller supplied `history_length`, re-read
    // from storage so the same `trim_task` path that `GetTask` uses
    // applies to the send response. Skipping this when the field is
    // unset avoids an unnecessary storage round-trip on the default
    // path. The task yielded from the executor is otherwise authoritative.
    let task = if history_length.is_some() {
        state
            .task_storage
            .get_task(tenant, &task_id, owner, history_length)
            .await
            .map_err(A2aError::from)?
            .unwrap_or(task)
    } else {
        task
    };

    Ok(Json(serde_json::json!({
        "task": serde_json::to_value(&task).unwrap_or_default()
    })))
}

/// Two-deadline blocking-send timeout.
///
/// 1. **Soft deadline** (`blocking_task_timeout`): wait for the
///    executor to emit a terminal or interrupted event. If the deadline
///    fires first, trip `cancellation` and enter the cooperative window.
/// 2. **Cooperative window** (through `timeout_abort_grace`): the
///    cancellation token is already set; a well-behaved executor
///    observes it and emits `sink.cancelled(...)` (or another terminal).
/// 3. **Hard deadline** (`soft + grace`): abort the executor, then
///    force-commit `FAILED` via `update_task_status_with_events` —
///    CAS-guarded. On `TerminalStateAlreadySet` another writer committed
///    a terminal first; re-read and return the persisted terminal.
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

    // Hard timeout. The executor has overrun its cooperative window, so
    // it is aborted whichever way the terminal CAS below resolves. The
    // cloneable AbortHandle is independent of the supervisor's
    // JoinHandle ownership; the supervisor's `.await` returns
    // `JoinError::Cancelled` once the abort takes effect at the
    // executor's next yield point, and its sentinel cleans the registry.
    handle.abort();

    // Force-commit FAILED via CAS.
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
        .update_task_status_with_events(tenant, task_id, owner, failed_status, vec![failed_event])
        .await;

    match result {
        Ok((task, seqs)) => {
            // framework-committed terminals fan out
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
            // Another writer (the executor's own sink, or a CancelTask
            // on any instance) committed a terminal first. Return the
            // actually-persisted terminal.
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
        Some(s) => Some(
            TaskState::from_wire_name(s).ok_or_else(|| A2aError::InvalidRequest {
                message: format!("Invalid status value: {s}"),
            })?,
        ),
        None => None,
    };
    let status_timestamp_after = match &query.status_timestamp_after {
        Some(s) => Some(
            chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|_| A2aError::InvalidRequest {
                    message: format!("Invalid statusTimestampAfter value: {s}"),
                })?
                .with_timezone(&chrono::Utc),
        ),
        None => None,
    };

    let filter = TaskFilter {
        tenant: Some(tenant.to_string()),
        owner: Some(owner.to_string()),
        context_id: query.context_id.clone(),
        status,
        page_size: query.page_size,
        // proto3 leaves an unset string empty: "" asks for the first page.
        page_token: query.page_token.clone().filter(|t| !t.is_empty()),
        history_length: query.history_length,
        status_timestamp_after,
        include_artifacts: query.include_artifacts,
    };

    let page = state
        .task_storage
        .list_tasks(filter)
        .await
        .map_err(A2aError::from)?;
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

/// `CancelTask` handler.
///
/// Sequence:
/// 1. Validate existence + ownership (via owner-scoped `get_task`).
/// 2. Reject with `TaskNotCancelable` if task is already terminal.
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
/// Framework-committed terminals carry `message = None` —
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
        .ok_or_else(|| A2aError::TaskNotFound {
            task_id: task_id.to_string(),
        })?;

    // Step 2: reject terminal tasks up-front with TaskNotCancelable.
    if let Some(status) = initial_task.status()
        && let Ok(s) = status.state()
        && turul_a2a_types::state_machine::is_terminal(s)
    {
        return Err(A2aError::TaskNotCancelable {
            task_id: task_id.to_string(),
        });
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
            return Err(A2aError::TaskNotFound {
                task_id: task_id.to_string(),
            });
        }
        Err(A2aStorageError::TerminalState { .. })
        | Err(A2aStorageError::InvalidTransition { .. })
        | Err(A2aStorageError::TerminalStateAlreadySet { .. }) => {
            return Err(A2aError::TaskNotCancelable {
                task_id: task_id.to_string(),
            });
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
    let deadline = tokio::time::Instant::now() + state.runtime_config.cancel_handler_grace;
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
                if let Some(status) = current.status()
                    && let Ok(s) = status.state()
                    && turul_a2a_types::state_machine::is_terminal(s)
                {
                    // Cooperative terminal (or another path) resolved
                    // the cancel during grace. Return persisted state.
                    state.event_broker.notify(task_id).await;
                    return Ok(Json(serde_json::to_value(&current).unwrap_or_default()));
                }
            }
            None => {
                return Err(A2aError::TaskNotFound {
                    task_id: task_id.to_string(),
                });
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
    // Framework-committed terminals use `message = None` so history /
    // SSE consumers can distinguish them from executor-authored cancels
    // without conflating framework telemetry with agent output.
    let cancel_event = StreamEvent::StatusUpdate {
        status_update: crate::streaming::StatusUpdatePayload {
            task_id: task_id.to_string(),
            context_id,
            status: serde_json::to_value(TaskStatus::new(TaskState::Canceled)).unwrap_or_default(),
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
            // framework-committed CANCELED must
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
                .ok_or_else(|| A2aError::TaskNotFound {
                    task_id: task_id.to_string(),
                })?;
            state.event_broker.notify(task_id).await;
            Ok(Json(serde_json::to_value(&persisted).unwrap_or_default()))
        }
        Err(A2aStorageError::TaskNotFound(id)) => Err(A2aError::TaskNotFound { task_id: id }),
        Err(A2aStorageError::TerminalState { .. })
        | Err(A2aStorageError::InvalidTransition { .. }) => Err(A2aError::TaskNotCancelable {
            task_id: task_id.to_string(),
        }),
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

    // The URL must parse at CRUD time: an unparseable URL can never be
    // delivered, and rejecting it here gives the operator immediate
    // feedback instead of a silent skip at delivery time.
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

    let created = state
        .push_storage
        .create_config(tenant, config)
        .await
        .map_err(A2aError::from)?;
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
        .list_configs(
            tenant,
            task_id,
            query.page_token.as_deref(),
            query.page_size,
        )
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

// =========================================================
// Profile-extension negotiation (skill-invocation dispatcher etc.)
// =========================================================

/// Parse the inbound `A2A-Extensions` header, validate against the
/// agent card's advertised extensions, and return the negotiated set
/// for echo on the response. Returns `UnsupportedOperationError` when
/// the server advertises a `required = true` extension the client
/// did not activate.
pub(crate) fn negotiate_profile_extensions(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> Result<std::collections::HashSet<String>, A2aError> {
    let activated = crate::profile_dispatch::activated_from_headers(headers);
    let card = state.executor.agent_card();
    let advertised: &[turul_a2a_proto::AgentExtension] = card
        .capabilities
        .as_ref()
        .map(|c| c.extensions.as_slice())
        .unwrap_or(&[]);
    crate::profile_dispatch::validate_activation(&activated, advertised)
}

/// Attach the `A2A-Extensions` echo header to a response when the
/// negotiated set is non-empty. The header is set unconditionally
/// when non-empty so streaming and non-streaming responses both carry
/// the activation echo.
pub(crate) fn attach_extension_echo(
    mut response: axum::response::Response,
    echoed: &std::collections::HashSet<String>,
) -> axum::response::Response {
    if let Some(value) = crate::profile_dispatch::response_header_value(echoed)
        && let Ok(header_value) = axum::http::HeaderValue::from_str(&value)
    {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(crate::profile_dispatch::A2A_EXTENSIONS_HEADER),
            header_value,
        );
    }
    response
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
