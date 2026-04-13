//! A2A Protocol v1.0 client library.
//!
//! Independent of the server crate — depends only on turul-a2a-types and turul-a2a-proto.

pub mod builders;
mod error;
pub mod prelude;
pub mod response;
pub mod sse;

use turul_a2a_proto as pb;
use turul_a2a_types::wire;

pub use builders::MessageBuilder;
pub use error::A2aClientError;
pub use sse::{SseEvent, SseStream, StreamEvent, TypedSseEvent, TypedSseStream};

/// Auth configuration for the client.
#[derive(Debug, Clone)]
pub enum ClientAuth {
    None,
    Bearer(String),
    ApiKey { header: String, key: String },
}

impl Default for ClientAuth {
    fn default() -> Self {
        Self::None
    }
}

/// A2A client for communicating with A2A agents.
#[derive(Debug, Clone)]
pub struct A2aClient {
    base_url: String,
    tenant: Option<String>,
    auth: ClientAuth,
    http: reqwest::Client,
    agent_card: Option<pb::AgentCard>,
}

impl A2aClient {
    /// Create a client pointing at a base URL.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            tenant: None,
            auth: ClientAuth::None,
            http: reqwest::Client::new(),
            agent_card: None,
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

    /// Discover the agent by fetching `/.well-known/agent-card.json`.
    pub async fn discover(base_url: impl Into<String>) -> Result<Self, A2aClientError> {
        let mut client = Self::new(base_url);
        let card = client.fetch_agent_card().await?;
        client.agent_card = Some(card);
        Ok(client)
    }

    /// Fetch the agent card from the well-known endpoint.
    pub async fn fetch_agent_card(&self) -> Result<pb::AgentCard, A2aClientError> {
        let url = format!("{}{}", self.base_url, wire::http::WELL_KNOWN_AGENT_CARD);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(A2aClientError::Http {
                status: resp.status().as_u16(),
                message: resp.text().await.unwrap_or_default(),
            });
        }
        let card: pb::AgentCard = resp.json().await?;
        Ok(card)
    }

    /// Get the cached agent card (from `discover()` or `fetch_agent_card()`).
    pub fn agent_card(&self) -> Option<&pb::AgentCard> {
        self.agent_card.as_ref()
    }

    /// Build the URL with optional tenant prefix.
    /// Tenant is percent-encoded to handle special characters safely.
    fn url(&self, path: &str) -> String {
        match &self.tenant {
            Some(tenant) => {
                let encoded_tenant =
                    reqwest::Url::parse("http://x")
                        .unwrap()
                        .join(&format!("{}/", tenant))
                        .map(|u| u.path().trim_end_matches('/').trim_start_matches('/').to_string())
                        .unwrap_or_else(|_| tenant.clone());
                format!("{}/{}{}", self.base_url, encoded_tenant, path)
            }
            None => format!("{}{}", self.base_url, path),
        }
    }

    /// Build a request with auth headers and A2A-Version.
    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, url).header("a2a-version", "1.0");
        match &self.auth {
            ClientAuth::None => {}
            ClientAuth::Bearer(token) => {
                req = req.bearer_auth(token);
            }
            ClientAuth::ApiKey { header, key } => {
                req = req.header(header.as_str(), key.as_str());
            }
        }
        req
    }

    // =========================================================
    // Primary API — wrapper-first
    // =========================================================

    /// Send a message to the agent. Returns a wrapper `SendResponse`.
    ///
    /// Accepts `MessageBuilder` directly (no `.build()` needed) or a raw proto request.
    pub async fn send_message(
        &self,
        request: impl Into<pb::SendMessageRequest>,
    ) -> Result<crate::response::SendResponse, A2aClientError> {
        let proto_resp = self.send_message_proto(request.into()).await?;
        crate::response::SendResponse::try_from(proto_resp)
    }

    /// Get a task by ID. Returns a wrapper `Task`.
    pub async fn get_task(
        &self,
        task_id: &str,
        history_length: Option<i32>,
    ) -> Result<turul_a2a_types::Task, A2aClientError> {
        let proto_task = self.get_task_proto(task_id, history_length).await?;
        turul_a2a_types::Task::try_from(proto_task)
            .map_err(|e| A2aClientError::Conversion(e.to_string()))
    }

    /// Cancel a task. Returns a wrapper `Task`.
    pub async fn cancel_task(
        &self,
        task_id: &str,
    ) -> Result<turul_a2a_types::Task, A2aClientError> {
        let proto_task = self.cancel_task_proto(task_id).await?;
        turul_a2a_types::Task::try_from(proto_task)
            .map_err(|e| A2aClientError::Conversion(e.to_string()))
    }

    /// List tasks. Returns a wrapper `ListResponse` with wrapper `Task`s.
    pub async fn list_tasks(
        &self,
        params: &ListTasksParams,
    ) -> Result<crate::response::ListResponse, A2aClientError> {
        let proto_resp = self.list_tasks_proto(params).await?;
        crate::response::ListResponse::try_from(proto_resp)
    }

    // =========================================================
    // Proto-level escape hatches
    // =========================================================

    /// Send a message, returning the raw proto response.
    pub async fn send_message_proto(
        &self,
        request: pb::SendMessageRequest,
    ) -> Result<pb::SendMessageResponse, A2aClientError> {
        let url = self.url("/message:send");
        let resp = self
            .request(reqwest::Method::POST, &url)
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(self.parse_error(resp).await);
        }
        let response: pb::SendMessageResponse = resp.json().await?;
        Ok(response)
    }

    /// Get a task by ID, returning the raw proto Task.
    pub async fn get_task_proto(
        &self,
        task_id: &str,
        history_length: Option<i32>,
    ) -> Result<pb::Task, A2aClientError> {
        let url = self.url(&format!("/tasks/{task_id}"));
        let mut req = self.request(reqwest::Method::GET, &url);
        if let Some(hl) = history_length {
            req = req.query(&[("historyLength", hl.to_string())]);
        }
        let resp = req.send().await?;

        if !resp.status().is_success() {
            return Err(self.parse_error(resp).await);
        }
        let task: pb::Task = resp.json().await?;
        Ok(task)
    }

    /// Cancel a task, returning the raw proto Task.
    pub async fn cancel_task_proto(
        &self,
        task_id: &str,
    ) -> Result<pb::Task, A2aClientError> {
        let url = self.url(&format!("/tasks/{task_id}:cancel"));
        let resp = self
            .request(reqwest::Method::POST, &url)
            .header("a2a-version", "1.0")
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(self.parse_error(resp).await);
        }
        let task: pb::Task = resp.json().await?;
        Ok(task)
    }

    /// List tasks, returning the raw proto response.
    pub async fn list_tasks_proto(
        &self,
        params: &ListTasksParams,
    ) -> Result<pb::ListTasksResponse, A2aClientError> {
        let url = self.url("/tasks");
        let mut req = self.request(reqwest::Method::GET, &url);
        if let Some(ref ctx) = params.context_id {
            req = req.query(&[("contextId", ctx.as_str())]);
        }
        if let Some(ref status) = params.status {
            req = req.query(&[("status", status.as_str())]);
        }
        if let Some(ps) = params.page_size {
            req = req.query(&[("pageSize", &ps.to_string())]);
        }
        if let Some(ref pt) = params.page_token {
            req = req.query(&[("pageToken", pt.as_str())]);
        }

        let resp = req.send().await?;

        if !resp.status().is_success() {
            return Err(self.parse_error(resp).await);
        }
        let response: pb::ListTasksResponse = resp.json().await?;
        Ok(response)
    }

    // =========================================================
    // Streaming methods
    // =========================================================

    /// Send a streaming message. Returns a typed event stream.
    ///
    /// Events are `StreamEvent` variants: `Task`, `Message`, `StatusUpdate`, `ArtifactUpdate`.
    /// The stream closes when the task reaches a terminal state.
    pub async fn send_streaming_message(
        &self,
        request: impl Into<pb::SendMessageRequest>,
    ) -> Result<TypedSseStream, A2aClientError> {
        let raw = self.send_streaming_message_raw(request).await?;
        Ok(TypedSseStream::from_raw(raw))
    }

    /// Subscribe to task events. Returns a typed event stream.
    ///
    /// The first event is a `StreamEvent::Task` snapshot (spec §3.1.6).
    /// Subsequent events are `StatusUpdate` / `ArtifactUpdate` from the durable store.
    pub async fn subscribe_to_task(
        &self,
        task_id: &str,
        last_event_id: Option<&str>,
    ) -> Result<TypedSseStream, A2aClientError> {
        let raw = self.subscribe_to_task_raw(task_id, last_event_id).await?;
        Ok(TypedSseStream::from_raw(raw))
    }

    /// Send a streaming message, returning raw SSE events (untyped JSON).
    pub async fn send_streaming_message_raw(
        &self,
        request: impl Into<pb::SendMessageRequest>,
    ) -> Result<SseStream, A2aClientError> {
        let request = request.into();
        let url = self.url("/message:stream");
        let resp = self
            .request(reqwest::Method::POST, &url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&request)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(self.parse_error_from_status(resp).await);
        }
        Ok(SseStream::from_response(resp))
    }

    /// Subscribe to task events, returning raw SSE events (untyped JSON).
    pub async fn subscribe_to_task_raw(
        &self,
        task_id: &str,
        last_event_id: Option<&str>,
    ) -> Result<SseStream, A2aClientError> {
        let url = self.url(&format!("/tasks/{task_id}:subscribe"));
        let mut req = self
            .request(reqwest::Method::GET, &url)
            .header("accept", "text/event-stream");

        if let Some(lei) = last_event_id {
            req = req.header("Last-Event-ID", lei);
        }

        let resp = req.send().await?;

        if !resp.status().is_success() {
            return Err(self.parse_error_from_status(resp).await);
        }
        Ok(SseStream::from_response(resp))
    }

    // =========================================================
    // Push notification config CRUD
    // =========================================================

    /// Create a push notification config for a task.
    pub async fn create_push_config(
        &self,
        task_id: &str,
        config: pb::TaskPushNotificationConfig,
    ) -> Result<pb::TaskPushNotificationConfig, A2aClientError> {
        let url = self.url(&format!("/tasks/{task_id}/pushNotificationConfigs"));
        let resp = self
            .request(reqwest::Method::POST, &url)
            .header("content-type", "application/json")
            .json(&config)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(self.parse_error_from_status(resp).await);
        }
        Ok(resp.json().await?)
    }

    /// Get a push notification config by ID.
    pub async fn get_push_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<pb::TaskPushNotificationConfig, A2aClientError> {
        let url = self.url(&format!(
            "/tasks/{task_id}/pushNotificationConfigs/{config_id}"
        ));
        let resp = self.request(reqwest::Method::GET, &url).send().await?;

        if !resp.status().is_success() {
            return Err(self.parse_error_from_status(resp).await);
        }
        Ok(resp.json().await?)
    }

    /// List push notification configs for a task.
    pub async fn list_push_configs(
        &self,
        task_id: &str,
        page_size: Option<i32>,
        page_token: Option<&str>,
    ) -> Result<pb::ListTaskPushNotificationConfigsResponse, A2aClientError> {
        let url = self.url(&format!("/tasks/{task_id}/pushNotificationConfigs"));
        let mut req = self.request(reqwest::Method::GET, &url);
        if let Some(ps) = page_size {
            req = req.query(&[("pageSize", ps.to_string())]);
        }
        if let Some(pt) = page_token {
            req = req.query(&[("pageToken", pt)]);
        }
        let resp = req.send().await?;

        if !resp.status().is_success() {
            return Err(self.parse_error_from_status(resp).await);
        }
        Ok(resp.json().await?)
    }

    /// Delete a push notification config.
    pub async fn delete_push_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<(), A2aClientError> {
        let url = self.url(&format!(
            "/tasks/{task_id}/pushNotificationConfigs/{config_id}"
        ));
        let resp = self
            .request(reqwest::Method::DELETE, &url)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(self.parse_error_from_status(resp).await);
        }
        Ok(())
    }

    // =========================================================
    // Extended agent card
    // =========================================================

    /// Fetch the extended agent card (requires authentication).
    pub async fn fetch_extended_agent_card(
        &self,
    ) -> Result<pb::AgentCard, A2aClientError> {
        let url = self.url("/extendedAgentCard");
        let resp = self.request(reqwest::Method::GET, &url).send().await?;

        if !resp.status().is_success() {
            return Err(self.parse_error_from_status(resp).await);
        }
        Ok(resp.json().await?)
    }

    // =========================================================
    // Internal helpers
    // =========================================================

    /// Parse error from a non-success response (takes ownership).
    async fn parse_error_from_status(&self, resp: reqwest::Response) -> A2aClientError {
        self.parse_error(resp).await
    }

    /// Parse an error response from the server.
    async fn parse_error(&self, resp: reqwest::Response) -> A2aClientError {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();

        // Try to parse as AIP-193 error
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(error) = json.get("error") {
                let message = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error")
                    .to_string();

                // Extract ErrorInfo reason if present
                let reason = error
                    .get("details")
                    .and_then(|d| d.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|info| info.get("reason"))
                    .and_then(|r| r.as_str())
                    .map(|s| s.to_string());

                return A2aClientError::A2aError {
                    status,
                    message,
                    reason,
                };
            }
        }

        A2aClientError::Http {
            status,
            message: body,
        }
    }
}

/// Parameters for listing tasks.
#[derive(Debug, Clone, Default)]
pub struct ListTasksParams {
    pub context_id: Option<String>,
    pub status: Option<String>,
    pub page_size: Option<i32>,
    pub page_token: Option<String>,
}
