//! `A2aService` implementation — gRPC transport adapter (ADR-014 §2.1).
//!
//! Every RPC dispatches into the same `core_*` function that HTTP and
//! JSON-RPC call (ADR-005 extended). The adapter does only:
//!   1. tenant + owner extraction (metadata → fallback to proto → fallback "")
//!   2. serialize incoming proto to JSON for the core body argument
//!   3. deserialize core JSON response back to the proto response type
//!   4. map `A2aError` via [`crate::grpc::error::a2a_to_status`]
//!
//! Streaming RPCs are stubbed out here (return `UNIMPLEMENTED`); the
//! streaming module (next commit) replaces them with the replay-then-live
//! loop backed by the durable event store.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use tonic::{Request, Response, Status};
use turul_a2a_proto as pb;

use crate::grpc::error::a2a_to_status;
use crate::middleware::context::RequestContext;
use crate::router::{self, AppState, ListTasksQuery, PushConfigQuery};

/// Metadata key the adapter reads to scope a request to a tenant.
/// ADR-014 §2.4 — ASCII, lowercase per HTTP/2 header canonicalisation.
pub const TENANT_METADATA: &str = "x-tenant-id";

/// gRPC service implementing `lf.a2a.v1.A2AService` by forwarding each
/// RPC into the shared core handler layer.
pub struct GrpcService {
    pub(crate) state: AppState,
}

impl GrpcService {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

/// Pull the tenant ID for this request. Precedence (ADR-014 §2.4):
///   1. `x-tenant-id` ASCII metadata
///   2. `tenant` field on the proto request
///   3. empty string (= DEFAULT_TENANT, matches the HTTP default route)
fn tenant_from<T>(req: &Request<T>, proto_tenant: &str) -> String {
    if let Some(val) = req.metadata().get(TENANT_METADATA) {
        if let Ok(s) = val.to_str() {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    proto_tenant.to_string()
}

/// Pull the caller's owner identity from the request extensions.
///
/// When the `A2aAuthLayer` is wired (next commit), it injects a
/// [`RequestContext`] into `http::Request::extensions_mut()`; tonic
/// surfaces those same extensions to the service impl. When no layer is
/// configured (bare test harness, compile-only checks) the owner falls
/// back to `"anonymous"` — identical to the HTTP path's behaviour.
fn owner_from<T>(req: &Request<T>) -> String {
    req.extensions()
        .get::<RequestContext>()
        .map(|ctx| ctx.identity.owner().to_string())
        .unwrap_or_else(|| "anonymous".to_string())
}

/// Convert a serde_json::Error at the adapter boundary into a
/// `tonic::Status`. Adapter-layer JSON failures are internal: the core
/// handler produced a Value it was meant to produce, and we failed to
/// decode it into the proto response type — that is a framework bug,
/// not an A2A error.
fn internal_from_json(err: serde_json::Error) -> Status {
    Status::internal(format!("grpc adapter: proto/json mismatch: {err}"))
}

/// Boxed stream type used by both streaming RPCs. Keeps the `A2aService`
/// associated types stable between the placeholder implementations here
/// and the real streaming module that lands in the next commit.
pub type BoxedStreamResponseStream =
    Pin<Box<dyn Stream<Item = Result<pb::StreamResponse, Status>> + Send + 'static>>;

#[async_trait]
impl pb::grpc::A2aService for GrpcService {
    // --- SendMessage ----------------------------------------------------

    async fn send_message(
        &self,
        request: Request<pb::SendMessageRequest>,
    ) -> Result<Response<pb::SendMessageResponse>, Status> {
        let owner = owner_from(&request);
        let tenant = tenant_from(&request, &request.get_ref().tenant);

        // core_send_message wants the JSON body the HTTP handler would have
        // received — serialize the proto request back to JSON to reuse the
        // existing input path (including `configuration.returnImmediately`).
        let body = serde_json::to_string(request.get_ref()).map_err(internal_from_json)?;

        let value = router::core_send_message(self.state.clone(), &tenant, &owner, body)
            .await
            .map_err(a2a_to_status)?
            .0;

        let response: pb::SendMessageResponse =
            serde_json::from_value(value).map_err(internal_from_json)?;
        Ok(Response::new(response))
    }

    // --- GetTask --------------------------------------------------------

    async fn get_task(
        &self,
        request: Request<pb::GetTaskRequest>,
    ) -> Result<Response<pb::Task>, Status> {
        let owner = owner_from(&request);
        let tenant = tenant_from(&request, &request.get_ref().tenant);
        let task_id = request.get_ref().id.clone();
        let history_length = request.get_ref().history_length;

        let value =
            router::core_get_task(self.state.clone(), &tenant, &owner, &task_id, history_length)
                .await
                .map_err(a2a_to_status)?
                .0;

        let task: pb::Task = serde_json::from_value(value).map_err(internal_from_json)?;
        Ok(Response::new(task))
    }

    // --- ListTasks ------------------------------------------------------

    async fn list_tasks(
        &self,
        request: Request<pb::ListTasksRequest>,
    ) -> Result<Response<pb::ListTasksResponse>, Status> {
        let owner = owner_from(&request);
        let req = request.get_ref();
        let tenant = tenant_from(&request, &req.tenant);

        // Translate the proto status enum into the string the shared core
        // query struct parses. None when unspecified (0 is the proto3
        // default-absent sentinel).
        let status = pb::TaskState::try_from(req.status)
            .ok()
            .filter(|s| *s != pb::TaskState::Unspecified)
            .map(|s| s.as_str_name().to_string());

        let query = ListTasksQuery {
            context_id: Some(req.context_id.clone()).filter(|s| !s.is_empty()),
            status,
            page_size: req.page_size,
            page_token: Some(req.page_token.clone()).filter(|s| !s.is_empty()),
            history_length: req.history_length,
            include_artifacts: None,
        };

        let value = router::core_list_tasks(self.state.clone(), &tenant, &owner, &query)
            .await
            .map_err(a2a_to_status)?
            .0;

        let response: pb::ListTasksResponse =
            serde_json::from_value(value).map_err(internal_from_json)?;
        Ok(Response::new(response))
    }

    // --- CancelTask -----------------------------------------------------

    async fn cancel_task(
        &self,
        request: Request<pb::CancelTaskRequest>,
    ) -> Result<Response<pb::Task>, Status> {
        let owner = owner_from(&request);
        let tenant = tenant_from(&request, &request.get_ref().tenant);
        let task_id = request.get_ref().id.clone();

        let value = router::core_cancel_task(self.state.clone(), &tenant, &owner, &task_id)
            .await
            .map_err(a2a_to_status)?
            .0;

        let task: pb::Task = serde_json::from_value(value).map_err(internal_from_json)?;
        Ok(Response::new(task))
    }

    // --- Push config CRUD ----------------------------------------------

    async fn create_task_push_notification_config(
        &self,
        request: Request<pb::TaskPushNotificationConfig>,
    ) -> Result<Response<pb::TaskPushNotificationConfig>, Status> {
        let owner = owner_from(&request);
        let tenant = tenant_from(&request, &request.get_ref().tenant);
        let task_id = request.get_ref().task_id.clone();
        if task_id.is_empty() {
            return Err(Status::invalid_argument("push config task_id is required"));
        }
        let body = serde_json::to_string(request.get_ref()).map_err(internal_from_json)?;

        let value =
            router::core_create_push_config(self.state.clone(), &tenant, &owner, &task_id, body)
                .await
                .map_err(a2a_to_status)?
                .0;

        let config: pb::TaskPushNotificationConfig =
            serde_json::from_value(value).map_err(internal_from_json)?;
        Ok(Response::new(config))
    }

    async fn get_task_push_notification_config(
        &self,
        request: Request<pb::GetTaskPushNotificationConfigRequest>,
    ) -> Result<Response<pb::TaskPushNotificationConfig>, Status> {
        let owner = owner_from(&request);
        let tenant = tenant_from(&request, &request.get_ref().tenant);
        let task_id = request.get_ref().task_id.clone();
        let config_id = request.get_ref().id.clone();

        let value = router::core_get_push_config(
            self.state.clone(),
            &tenant,
            &owner,
            &task_id,
            &config_id,
        )
        .await
        .map_err(a2a_to_status)?
        .0;

        let config: pb::TaskPushNotificationConfig =
            serde_json::from_value(value).map_err(internal_from_json)?;
        Ok(Response::new(config))
    }

    async fn list_task_push_notification_configs(
        &self,
        request: Request<pb::ListTaskPushNotificationConfigsRequest>,
    ) -> Result<Response<pb::ListTaskPushNotificationConfigsResponse>, Status> {
        let owner = owner_from(&request);
        let req = request.get_ref();
        let tenant = tenant_from(&request, &req.tenant);
        let task_id = req.task_id.clone();

        let query = PushConfigQuery {
            page_size: Some(req.page_size).filter(|n| *n > 0),
            page_token: Some(req.page_token.clone()).filter(|s| !s.is_empty()),
        };

        let value =
            router::core_list_push_configs(self.state.clone(), &tenant, &owner, &task_id, &query)
                .await
                .map_err(a2a_to_status)?
                .0;

        // core returns {"configs": [...], "nextPageToken": "..."} — reshape
        // into the proto response (which uses `configs` + `next_page_token`
        // / camelCase via pbjson).
        let response: pb::ListTaskPushNotificationConfigsResponse =
            serde_json::from_value(value).map_err(internal_from_json)?;
        Ok(Response::new(response))
    }

    async fn delete_task_push_notification_config(
        &self,
        request: Request<pb::DeleteTaskPushNotificationConfigRequest>,
    ) -> Result<Response<pb::pbjson_types::Empty>, Status> {
        let owner = owner_from(&request);
        let tenant = tenant_from(&request, &request.get_ref().tenant);
        let task_id = request.get_ref().task_id.clone();
        let config_id = request.get_ref().id.clone();

        let _ = router::core_delete_push_config(
            self.state.clone(),
            &tenant,
            &owner,
            &task_id,
            &config_id,
        )
        .await
        .map_err(a2a_to_status)?;

        Ok(Response::new(pb::pbjson_types::Empty {}))
    }

    // --- GetExtendedAgentCard ------------------------------------------

    async fn get_extended_agent_card(
        &self,
        _request: Request<pb::GetExtendedAgentCardRequest>,
    ) -> Result<Response<pb::AgentCard>, Status> {
        // No core_* helper — the HTTP handler calls the executor directly
        // (`router.rs:391-402`). Mirror that path verbatim: claims are
        // `None` for now (the HTTP GET endpoint passes `None` too; ADR-007
        // §7 extension to pass JWT claims through is future work).
        match self.state.executor.extended_agent_card(None) {
            Some(card) => Ok(Response::new(card)),
            None => Err(a2a_to_status(
                crate::error::A2aError::ExtendedAgentCardNotConfigured,
            )),
        }
    }

    // --- Streaming RPCs (placeholder — real impl lands in next commit) --

    type SendStreamingMessageStream = BoxedStreamResponseStream;

    async fn send_streaming_message(
        &self,
        _request: Request<pb::SendMessageRequest>,
    ) -> Result<Response<Self::SendStreamingMessageStream>, Status> {
        Err(Status::unimplemented(
            "SendStreamingMessage not yet wired to event broker",
        ))
    }

    type SubscribeToTaskStream = BoxedStreamResponseStream;

    async fn subscribe_to_task(
        &self,
        _request: Request<pb::SubscribeToTaskRequest>,
    ) -> Result<Response<Self::SubscribeToTaskStream>, Status> {
        Err(Status::unimplemented(
            "SubscribeToTask not yet wired to event broker",
        ))
    }
}
