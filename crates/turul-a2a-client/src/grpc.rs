//! gRPC client wrapper.
//!
//! Thin ergonomic layer over the tonic-generated
//! `turul_a2a_proto::grpc::A2aServiceClient`. Purpose: offer the same
//! verb surface as `A2aClient` (send, get, list, cancel, stream,
//! subscribe, and push-config CRUD) while injecting auth + tenant
//! metadata uniformly per call.
//!
//! Tenant handling matches ADR-014 §2.4 normative precedence: when
//! the caller passes a `with_tenant(...)` value, the client writes it
//! to the proto request's `tenant` field (the normative wire
//! location). `x-tenant-id` metadata is NOT emitted by this client —
//! the server prefers proto field anyway, and emitting metadata
//! alongside the proto field would be redundant.
//!
//! Auth metadata: `authorization: Bearer ...` /
//! `x-api-key: ...`. All ASCII; no `-bin`.

use std::pin::Pin;

use futures::Stream;
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use turul_a2a_proto as pb;

use crate::ClientAuth;
use crate::error::A2aClientError;

/// gRPC client for an A2A agent.
#[derive(Clone)]
pub struct A2aGrpcClient {
    inner: pb::grpc::A2aServiceClient<Channel>,
    tenant: Option<String>,
    auth: ClientAuth,
}

impl A2aGrpcClient {
    /// Connect to the agent at the given endpoint.
    ///
    /// `endpoint` accepts anything tonic's `Endpoint::from_shared`
    /// accepts — typically `"http://host:port"` or
    /// `"https://host:port"`. Use `Channel` directly for advanced
    /// composition (TLS config, load balancing).
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self, A2aClientError> {
        let ep = tonic::transport::Endpoint::from_shared(endpoint.into())
            .map_err(|e| A2aClientError::GrpcTransport(e.to_string()))?;
        let channel = ep.connect().await?;
        Ok(Self {
            inner: pb::grpc::A2aServiceClient::new(channel),
            tenant: None,
            auth: ClientAuth::None,
        })
    }

    /// Build from a pre-constructed channel (TLS, proxy, etc.).
    pub fn from_channel(channel: Channel) -> Self {
        Self {
            inner: pb::grpc::A2aServiceClient::new(channel),
            tenant: None,
            auth: ClientAuth::None,
        }
    }

    pub fn with_auth(mut self, auth: ClientAuth) -> Self {
        self.auth = auth;
        self
    }

    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }

    /// Wrap a proto value in a `tonic::Request` and inject auth
    /// metadata. Tenant is NOT written as metadata — callers set
    /// it on the proto request itself (see per-verb helpers below).
    fn prepare<T>(&self, value: T) -> Request<T> {
        let mut req = Request::new(value);
        match &self.auth {
            ClientAuth::None => {}
            ClientAuth::Bearer(token) => {
                if let Ok(v) = MetadataValue::try_from(format!("Bearer {token}")) {
                    req.metadata_mut().insert("authorization", v);
                }
            }
            ClientAuth::ApiKey { header, key } => {
                if let Ok(v) = MetadataValue::try_from(key.as_str()) {
                    // Metadata keys must be lowercase ASCII.
                    let lower = header.to_ascii_lowercase();
                    if let Ok(name) = tonic::metadata::MetadataKey::from_bytes(lower.as_bytes()) {
                        req.metadata_mut().insert(name, v);
                    }
                }
            }
        }
        req
    }

    fn scoped_tenant(&self) -> String {
        self.tenant.clone().unwrap_or_default()
    }

    // --- Unary verbs ---------------------------------------------------

    pub async fn send_message(
        &mut self,
        message: pb::Message,
    ) -> Result<pb::SendMessageResponse, A2aClientError> {
        let req = pb::SendMessageRequest {
            tenant: self.scoped_tenant(),
            message: Some(message),
            configuration: None,
            metadata: None,
        };
        Ok(self
            .inner
            .send_message(self.prepare(req))
            .await?
            .into_inner())
    }

    pub async fn get_task(
        &mut self,
        task_id: impl Into<String>,
        history_length: Option<i32>,
    ) -> Result<pb::Task, A2aClientError> {
        let req = pb::GetTaskRequest {
            tenant: self.scoped_tenant(),
            id: task_id.into(),
            history_length,
        };
        Ok(self.inner.get_task(self.prepare(req)).await?.into_inner())
    }

    pub async fn list_tasks(
        &mut self,
        page_size: Option<i32>,
        page_token: Option<String>,
    ) -> Result<pb::ListTasksResponse, A2aClientError> {
        let req = pb::ListTasksRequest {
            tenant: self.scoped_tenant(),
            context_id: String::new(),
            status: 0,
            page_size,
            page_token: page_token.unwrap_or_default(),
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
        };
        Ok(self.inner.list_tasks(self.prepare(req)).await?.into_inner())
    }

    pub async fn cancel_task(
        &mut self,
        task_id: impl Into<String>,
    ) -> Result<pb::Task, A2aClientError> {
        let req = pb::CancelTaskRequest {
            tenant: self.scoped_tenant(),
            id: task_id.into(),
            metadata: None,
        };
        Ok(self
            .inner
            .cancel_task(self.prepare(req))
            .await?
            .into_inner())
    }

    pub async fn get_extended_agent_card(&mut self) -> Result<pb::AgentCard, A2aClientError> {
        let req = pb::GetExtendedAgentCardRequest {
            tenant: self.scoped_tenant(),
        };
        Ok(self
            .inner
            .get_extended_agent_card(self.prepare(req))
            .await?
            .into_inner())
    }

    // --- Push config CRUD ---------------------------------------------

    /// Create a push notification config from `url` + `token` (the 80%
    /// case). For `authentication` or other advanced fields, build a
    /// [`turul_a2a_types::PushConfig`] via
    /// [`turul_a2a_types::PushConfigBuilder`] and call
    /// [`Self::create_push_config_with`].
    pub async fn create_push_config(
        &mut self,
        task_id: impl Into<String>,
        url: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<turul_a2a_types::PushConfig, A2aClientError> {
        let task_id = task_id.into();
        let cfg = turul_a2a_types::PushConfigBuilder::new(url, token)
            .task_id(task_id.clone())
            .build();
        self.create_push_config_with(cfg).await
    }

    /// Create a push notification config from a fully-constructed
    /// [`turul_a2a_types::PushConfig`].
    pub async fn create_push_config_with(
        &mut self,
        config: turul_a2a_types::PushConfig,
    ) -> Result<turul_a2a_types::PushConfig, A2aClientError> {
        let mut proto = config.into_proto();
        if proto.tenant.is_empty() {
            proto.tenant = self.scoped_tenant();
        }
        let resp = self
            .inner
            .create_task_push_notification_config(self.prepare(proto))
            .await?
            .into_inner();
        Ok(resp.into())
    }

    pub async fn get_push_config(
        &mut self,
        task_id: impl Into<String>,
        config_id: impl Into<String>,
    ) -> Result<turul_a2a_types::PushConfig, A2aClientError> {
        let req = pb::GetTaskPushNotificationConfigRequest {
            tenant: self.scoped_tenant(),
            task_id: task_id.into(),
            id: config_id.into(),
        };
        let resp = self
            .inner
            .get_task_push_notification_config(self.prepare(req))
            .await?
            .into_inner();
        Ok(resp.into())
    }

    pub async fn list_push_configs(
        &mut self,
        task_id: impl Into<String>,
    ) -> Result<turul_a2a_types::PushConfigPage, A2aClientError> {
        let req = pb::ListTaskPushNotificationConfigsRequest {
            tenant: self.scoped_tenant(),
            task_id: task_id.into(),
            page_size: 0,
            page_token: String::new(),
        };
        let resp = self
            .inner
            .list_task_push_notification_configs(self.prepare(req))
            .await?
            .into_inner();
        Ok(turul_a2a_types::PushConfigPage::new(
            resp.configs.into_iter().map(Into::into).collect(),
            (!resp.next_page_token.is_empty()).then_some(resp.next_page_token),
        ))
    }

    pub async fn delete_push_config(
        &mut self,
        task_id: impl Into<String>,
        config_id: impl Into<String>,
    ) -> Result<(), A2aClientError> {
        let req = pb::DeleteTaskPushNotificationConfigRequest {
            tenant: self.scoped_tenant(),
            task_id: task_id.into(),
            id: config_id.into(),
        };
        self.inner
            .delete_task_push_notification_config(self.prepare(req))
            .await?;
        Ok(())
    }

    // --- Streaming verbs -----------------------------------------------

    pub async fn send_streaming_message(
        &mut self,
        message: pb::Message,
    ) -> Result<GrpcStreamResponses, A2aClientError> {
        let req = pb::SendMessageRequest {
            tenant: self.scoped_tenant(),
            message: Some(message),
            configuration: None,
            metadata: None,
        };
        let stream = self
            .inner
            .send_streaming_message(self.prepare(req))
            .await?
            .into_inner();
        Ok(Box::pin(map_stream(stream)))
    }

    /// Subscribe to a task. `last_event_id` is optional — when set,
    /// the server replays events after that `(task_id, sequence)`
    /// marker
    pub async fn subscribe_to_task(
        &mut self,
        task_id: impl Into<String>,
        last_event_id: Option<String>,
    ) -> Result<GrpcStreamResponses, A2aClientError> {
        let req = pb::SubscribeToTaskRequest {
            tenant: self.scoped_tenant(),
            id: task_id.into(),
        };
        let mut tonic_req = self.prepare(req);
        if let Some(id) = last_event_id {
            if let Ok(v) = MetadataValue::try_from(id) {
                tonic_req.metadata_mut().insert("a2a-last-event-id", v);
            }
        }
        let stream = self.inner.subscribe_to_task(tonic_req).await?.into_inner();
        Ok(Box::pin(map_stream(stream)))
    }
}

/// Stream item type emitted by the gRPC client's streaming verbs.
pub type GrpcStreamResponses =
    Pin<Box<dyn Stream<Item = Result<pb::StreamResponse, A2aClientError>> + Send>>;

fn map_stream(
    s: tonic::Streaming<pb::StreamResponse>,
) -> impl Stream<Item = Result<pb::StreamResponse, A2aClientError>> {
    use futures::StreamExt;
    s.map(|r| r.map_err(A2aClientError::from))
}
