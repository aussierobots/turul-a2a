//! DynamoDB storage backend for A2A tasks and push notification configs.
//!
//! ## Key Design
//!
//! Tasks PK: `{tenant}#{task_id}`. Owner is a non-key attribute used for
//! conditional writes (owner enforcement). Task IDs are server-generated
//! UUID v7, so collisions within a tenant are not possible in normal operation.
//!
//! Tables are shareable across tenants (tenant is in the PK). However, owner
//! isolation is enforced at the application layer via conditional expressions,
//! not via the key structure. If multiple independent agents share the same
//! tenant, they must use distinct task IDs (guaranteed by UUID v7 generation).

use async_trait::async_trait;
use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::AttributeValue;
use turul_a2a_types::{Artifact, Message, Task, TaskState, TaskStatus};

use super::atomic::A2aAtomicStore;
use super::error::A2aStorageError;
use super::event_store::A2aEventStore;
use super::filter::{PushConfigListPage, TaskFilter, TaskListPage};
use super::traits::{A2aPushNotificationStorage, A2aTaskStorage};
use crate::push::{
    A2aPushDeliveryStore, AbandonedReason, ClaimStatus, DeliveryClaim, DeliveryErrorClass,
    DeliveryOutcome, FailedDelivery, GaveUpReason,
};
use crate::streaming::StreamEvent;

/// DynamoDB storage configuration.
#[derive(Debug, Clone)]
pub struct DynamoDbConfig {
    pub tasks_table: String,
    pub push_configs_table: String,
    pub events_table: String,
    pub push_deliveries_table: String,
    /// Pending-dispatch marker table. See
    /// [`crate::push::A2aPushDeliveryStore::record_pending_dispatch`].
    pub push_pending_dispatches_table: String,
    /// TTL for task items in seconds. 0 = no expiry. Default: 7 days (604800).
    pub task_ttl_seconds: u64,
    /// TTL for event items in seconds. 0 = no expiry. Default: 24 hours (86400).
    pub event_ttl_seconds: u64,
    /// TTL for push-delivery claim items in seconds. 0 = no expiry.
    /// Default: 7 days (604800); GaveUp rows are retained for operator
    /// inspection within this window per ADR-011 §5b.
    pub push_delivery_ttl_seconds: u64,
}

/// 7 days in seconds.
const DEFAULT_TASK_TTL: u64 = 7 * 24 * 3600;
/// 24 hours in seconds.
const DEFAULT_EVENT_TTL: u64 = 24 * 3600;
/// 7 days in seconds.
const DEFAULT_PUSH_DELIVERY_TTL: u64 = 7 * 24 * 3600;

impl Default for DynamoDbConfig {
    fn default() -> Self {
        Self {
            tasks_table: "a2a_tasks".into(),
            push_configs_table: "a2a_push_configs".into(),
            events_table: "a2a_task_events".into(),
            push_deliveries_table: "a2a_push_deliveries".into(),
            push_pending_dispatches_table: "a2a_push_pending_dispatches".into(),
            task_ttl_seconds: DEFAULT_TASK_TTL,
            event_ttl_seconds: DEFAULT_EVENT_TTL,
            push_delivery_ttl_seconds: DEFAULT_PUSH_DELIVERY_TTL,
        }
    }
}

/// DynamoDB-backed A2A storage.
#[derive(Clone)]
pub struct DynamoDbA2aStorage {
    client: Client,
    config: DynamoDbConfig,
    /// ADR-013 §4.3 opt-in — see `InMemoryA2aStorage::push_dispatch_enabled`.
    push_dispatch_enabled: bool,
}

impl DynamoDbA2aStorage {
    pub async fn new(config: DynamoDbConfig) -> Result<Self, A2aStorageError> {
        let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = Client::new(&aws_config);
        Ok(Self {
            client,
            config,
            push_dispatch_enabled: false,
        })
    }

    pub fn from_client(client: Client, config: DynamoDbConfig) -> Self {
        Self {
            client,
            config,
            push_dispatch_enabled: false,
        }
    }

    /// Opt in to atomic pending-dispatch marker writes (ADR-013 §4.3).
    pub fn with_push_dispatch_enabled(mut self, enabled: bool) -> Self {
        self.push_dispatch_enabled = enabled;
        self
    }

    fn task_key(tenant: &str, task_id: &str) -> String {
        format!("{tenant}#{task_id}")
    }

    /// DynamoDB range-key form for an event row.
    ///
    /// The events table uses `pk` (HASH) + `sk` (RANGE). The sort key
    /// is the per-`pk` monotonic `eventSequence` rendered as a
    /// fixed-width zero-padded decimal string, so DynamoDB's
    /// lexicographic ordering on strings matches the numeric event
    /// ordering: `"00000000000000000001" < "00000000000000000002"
    /// < ... < "00000000000000000010"`. 20 digits comfortably fits a
    /// `u64` without the width collapsing. `eventSequence` is kept
    /// as a separate numeric attribute on each item so existing
    /// deserialisation and test expectations continue to work —
    /// `sk` is the key-schema carrier, `eventSequence` is the
    /// logical sequence.
    pub(crate) fn event_sort_key(seq: u64) -> String {
        format!("{seq:020}")
    }

    /// Single source of truth for the event put-item shape.
    ///
    /// Every event write path in this backend — `append_event`,
    /// `create_task_with_events`, `update_task_status_with_events`,
    /// `update_task_with_events` — constructs event rows through
    /// this helper so they cannot diverge. Required attributes:
    ///
    /// - `pk`: task partition key, from [`Self::task_key`].
    /// - `sk`: zero-padded range key, from [`Self::event_sort_key`].
    /// - `eventSequence`: numeric attribute carrying the logical
    ///   sequence (also encoded in `sk`, but kept as `N` so
    ///   deserialisation reads it without parsing the string key).
    /// - `eventData`: serialized `StreamEvent` JSON.
    /// - `ttl`: optional epoch-seconds attribute for DynamoDB TTL;
    ///   omitted when the backend is configured without event TTL.
    ///
    /// Exposed `pub(crate)` so unit tests in the same crate can
    /// exercise the shape directly without a live backend.
    pub(crate) fn build_event_item(
        pk: &str,
        seq: u64,
        event_data: &str,
        ttl: Option<AttributeValue>,
    ) -> std::collections::HashMap<String, AttributeValue> {
        let mut item = std::collections::HashMap::new();
        item.insert("pk".into(), AttributeValue::S(pk.to_string()));
        item.insert("sk".into(), AttributeValue::S(Self::event_sort_key(seq)));
        item.insert("eventSequence".into(), AttributeValue::N(seq.to_string()));
        item.insert(
            "eventData".into(),
            AttributeValue::S(event_data.to_string()),
        );
        if let Some(ttl_val) = ttl {
            item.insert("ttl".into(), ttl_val);
        }
        item
    }

    fn now_iso() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        format!("{:010}.{:03}", now.as_secs(), now.subsec_millis())
    }

    fn now_epoch() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Compute TTL epoch for a given TTL duration. Returns None if ttl_seconds is 0.
    fn ttl_epoch(ttl_seconds: u64) -> Option<AttributeValue> {
        if ttl_seconds == 0 {
            return None;
        }
        Some(AttributeValue::N(
            (Self::now_epoch() + ttl_seconds).to_string(),
        ))
    }

    fn task_to_json(task: &Task) -> Result<String, A2aStorageError> {
        serde_json::to_string(task).map_err(|e| A2aStorageError::SerializationError(e.to_string()))
    }

    fn task_from_json(json: &str) -> Result<Task, A2aStorageError> {
        let proto: turul_a2a_proto::Task = serde_json::from_str(json)
            .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;
        Task::try_from(proto).map_err(A2aStorageError::TypeError)
    }

    fn status_state_str(task: &Task) -> String {
        task.status()
            .and_then(|s| s.state().ok())
            .map(|s| format!("{s:?}"))
            .unwrap_or_default()
    }

    fn trim_task(task: Task, history_length: Option<i32>, include_artifacts: bool) -> Task {
        let mut proto = task.into_proto();
        if let Some(0) = history_length {
            proto.history.clear();
        } else if let Some(n) = history_length {
            if n > 0 {
                let n = n as usize;
                let start = proto.history.len().saturating_sub(n);
                proto.history = proto.history[start..].to_vec();
            }
        }
        if !include_artifacts {
            proto.artifacts.clear();
        }
        Task::try_from(proto)
            .unwrap_or_else(|_| Task::new("err", TaskStatus::new(TaskState::Failed)))
    }
}

#[async_trait]
impl A2aTaskStorage for DynamoDbA2aStorage {
    fn backend_name(&self) -> &'static str {
        "dynamodb"
    }

    async fn create_task(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
    ) -> Result<Task, A2aStorageError> {
        let pk = Self::task_key(tenant, task.id());
        let json = Self::task_to_json(&task)?;
        let state_str = Self::status_state_str(&task);

        let mut req = self
            .client
            .put_item()
            .table_name(&self.config.tasks_table)
            .item("pk", AttributeValue::S(pk))
            .item("tenant", AttributeValue::S(tenant.to_string()))
            .item("taskId", AttributeValue::S(task.id().to_string()))
            .item("owner", AttributeValue::S(owner.to_string()))
            .item(
                "contextId",
                AttributeValue::S(task.context_id().to_string()),
            )
            .item("statusState", AttributeValue::S(state_str))
            .item("taskJson", AttributeValue::S(json))
            .item("updatedAt", AttributeValue::S(Self::now_iso()));
        if let Some(ttl) = Self::ttl_epoch(self.config.task_ttl_seconds) {
            req = req.item("ttl", ttl);
        }
        req.send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

        Ok(task)
    }

    async fn get_task(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        history_length: Option<i32>,
    ) -> Result<Option<Task>, A2aStorageError> {
        let pk = Self::task_key(tenant, task_id);

        let result = self
            .client
            .get_item()
            .table_name(&self.config.tasks_table)
            .key("pk", AttributeValue::S(pk))
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

        let item = match result.item {
            Some(item) => item,
            None => return Ok(None),
        };

        // Check owner
        let stored_owner = item
            .get("owner")
            .and_then(|v| v.as_s().ok())
            .unwrap_or(&String::new())
            .clone();

        if stored_owner != owner {
            return Ok(None);
        }

        let json = item
            .get("taskJson")
            .and_then(|v| v.as_s().ok())
            .ok_or_else(|| A2aStorageError::DatabaseError("Missing task_json".into()))?;

        let task = Self::task_from_json(json)?;
        Ok(Some(Self::trim_task(task, history_length, true)))
    }

    async fn update_task(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
    ) -> Result<(), A2aStorageError> {
        let pk = Self::task_key(tenant, task.id());
        let json = Self::task_to_json(&task)?;
        let state_str = Self::status_state_str(&task);

        // Conditional write: only succeeds if the item exists AND owner matches.
        // This prevents ownership transfer and ensures isolation.
        let mut req = self
            .client
            .put_item()
            .table_name(&self.config.tasks_table)
            .item("pk", AttributeValue::S(pk))
            .item("tenant", AttributeValue::S(tenant.to_string()))
            .item("taskId", AttributeValue::S(task.id().to_string()))
            .item("owner", AttributeValue::S(owner.to_string()))
            .item(
                "contextId",
                AttributeValue::S(task.context_id().to_string()),
            )
            .item("statusState", AttributeValue::S(state_str))
            .item("taskJson", AttributeValue::S(json))
            .condition_expression("attribute_exists(pk) AND #o = :expected_owner")
            .expression_attribute_names("#o", "owner")
            .expression_attribute_values(":expected_owner", AttributeValue::S(owner.to_string()));
        if let Some(ttl) = Self::ttl_epoch(self.config.task_ttl_seconds) {
            req = req.item("ttl", ttl);
        }
        let result = req.send().await;

        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("ConditionalCheckFailed") {
                    Err(A2aStorageError::OwnerMismatch {
                        task_id: task.id().to_string(),
                    })
                } else {
                    Err(A2aStorageError::DatabaseError(err_str))
                }
            }
        }
    }

    async fn delete_task(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
    ) -> Result<bool, A2aStorageError> {
        // First check owner
        let existing = self.get_task(tenant, task_id, owner, Some(0)).await?;
        if existing.is_none() {
            return Ok(false);
        }

        let pk = Self::task_key(tenant, task_id);
        self.client
            .delete_item()
            .table_name(&self.config.tasks_table)
            .key("pk", AttributeValue::S(pk))
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

        Ok(true)
    }

    async fn list_tasks(&self, filter: TaskFilter) -> Result<TaskListPage, A2aStorageError> {
        let tenant = filter.tenant.as_deref().unwrap_or("");
        let owner = filter.owner.as_deref().unwrap_or("");
        let page_size = filter.page_size.map(|ps| ps.clamp(1, 100)).unwrap_or(50);

        // Scan with filter (for production, use GSI on tenant+owner)
        let mut scan = self
            .client
            .scan()
            .table_name(&self.config.tasks_table)
            .filter_expression("tenant = :t AND #o = :o")
            .expression_attribute_names("#o", "owner")
            .expression_attribute_values(":t", AttributeValue::S(tenant.to_string()))
            .expression_attribute_values(":o", AttributeValue::S(owner.to_string()));

        if let Some(ref ctx) = filter.context_id {
            scan = scan
                .filter_expression("tenant = :t AND #o = :o AND contextId = :ctx")
                .expression_attribute_values(":ctx", AttributeValue::S(ctx.clone()));
        }
        if let Some(ref status) = filter.status {
            let expr = if filter.context_id.is_some() {
                "tenant = :t AND #o = :o AND contextId = :ctx AND statusState = :st"
            } else {
                "tenant = :t AND #o = :o AND statusState = :st"
            };
            scan = scan
                .filter_expression(expr)
                .expression_attribute_values(":st", AttributeValue::S(format!("{status:?}")));
        }

        let result = scan
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

        let items = result.items.unwrap_or_default();
        let total_size = items.len() as i32;

        // Sort by updated_at DESC, task_id DESC (spec §3.1.4 MUST)
        let mut tasks_with_times: Vec<(Task, String)> = items
            .iter()
            .filter_map(|item| {
                let task = item
                    .get("taskJson")
                    .and_then(|v| v.as_s().ok())
                    .and_then(|json| Self::task_from_json(json).ok())?;
                let updated_at = item
                    .get("updatedAt")
                    .and_then(|v| v.as_s().ok())
                    .cloned()
                    .unwrap_or_default();
                Some((task, updated_at))
            })
            .collect();
        tasks_with_times.sort_by(|(a_task, a_time), (b_task, b_time)| {
            b_time
                .cmp(a_time)
                .then_with(|| b_task.id().cmp(a_task.id()))
        });

        // Apply cursor pagination: token is "updated_at|task_id"
        let start_idx = if let Some(ref token) = filter.page_token {
            if let Some((cursor_time, cursor_id)) = token.split_once('|') {
                tasks_with_times
                    .iter()
                    .position(|(t, time)| (time.as_str(), t.id()) < (cursor_time, cursor_id))
                    .unwrap_or(tasks_with_times.len())
            } else {
                tasks_with_times
                    .iter()
                    .position(|(t, _)| t.id() < token.as_str())
                    .unwrap_or(tasks_with_times.len())
            }
        } else {
            0
        };

        let include_artifacts = filter.include_artifacts.unwrap_or(false);
        let end_idx = (start_idx + page_size as usize).min(tasks_with_times.len());
        let page_tasks: Vec<Task> = tasks_with_times[start_idx..end_idx]
            .iter()
            .map(|(t, _)| Self::trim_task(t.clone(), filter.history_length, include_artifacts))
            .collect();

        let next_page_token = if end_idx < tasks_with_times.len() {
            tasks_with_times
                .get(end_idx - 1)
                .map(|(t, updated_at)| format!("{}|{}", updated_at, t.id()))
                .unwrap_or_default()
        } else {
            String::new()
        };

        Ok(TaskListPage {
            tasks: page_tasks,
            next_page_token,
            page_size,
            total_size,
        })
    }

    async fn update_task_status(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        new_status: TaskStatus,
    ) -> Result<Task, A2aStorageError> {
        let task = self
            .get_task(tenant, task_id, owner, None)
            .await?
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?;

        let current_state = task
            .status()
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?
            .state()
            .map_err(A2aStorageError::TypeError)?;

        let new_state = new_status.state().map_err(A2aStorageError::TypeError)?;

        turul_a2a_types::state_machine::validate_transition(current_state, new_state).map_err(
            |e| match e {
                turul_a2a_types::A2aTypeError::InvalidTransition { current, requested } => {
                    A2aStorageError::InvalidTransition { current, requested }
                }
                turul_a2a_types::A2aTypeError::TerminalState(s) => {
                    A2aStorageError::TerminalState(s)
                }
                other => A2aStorageError::TypeError(other),
            },
        )?;

        let mut proto = task.as_proto().clone();
        proto.status = Some(new_status.into_proto());
        let updated = Task::try_from(proto).map_err(A2aStorageError::TypeError)?;
        self.update_task(tenant, owner, updated.clone()).await?;
        Ok(updated)
    }

    async fn append_message(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        message: Message,
    ) -> Result<(), A2aStorageError> {
        let task = self
            .get_task(tenant, task_id, owner, None)
            .await?
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?;

        let mut proto = task.into_proto();
        proto.history.push(message.into_proto());
        let updated = Task::try_from(proto).map_err(A2aStorageError::TypeError)?;
        self.update_task(tenant, owner, updated).await
    }

    async fn append_artifact(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        artifact: Artifact,
        append: bool,
        _last_chunk: bool,
    ) -> Result<(), A2aStorageError> {
        let task = self
            .get_task(tenant, task_id, owner, None)
            .await?
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?;

        let mut proto = task.into_proto();
        if append {
            if let Some(existing) = proto
                .artifacts
                .iter_mut()
                .find(|a| a.artifact_id == artifact.as_proto().artifact_id)
            {
                existing.parts.extend(artifact.into_proto().parts);
                let updated = Task::try_from(proto).map_err(A2aStorageError::TypeError)?;
                return self.update_task(tenant, owner, updated).await;
            }
        }
        proto.artifacts.push(artifact.into_proto());
        let updated = Task::try_from(proto).map_err(A2aStorageError::TypeError)?;
        self.update_task(tenant, owner, updated).await
    }

    async fn task_count(&self) -> Result<usize, A2aStorageError> {
        let result = self
            .client
            .scan()
            .table_name(&self.config.tasks_table)
            .select(aws_sdk_dynamodb::types::Select::Count)
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
        Ok(result.count as usize)
    }

    async fn maintenance(&self) -> Result<(), A2aStorageError> {
        Ok(())
    }

    async fn set_cancel_requested(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
    ) -> Result<(), A2aStorageError> {
        let pk = Self::task_key(tenant, task_id);
        // Conditional update: item must exist with this owner AND be
        // non-terminal. DynamoDB's ConditionExpression is atomic at the
        // item level.
        let result = self
            .client
            .update_item()
            .table_name(&self.config.tasks_table)
            .key("pk", AttributeValue::S(pk.clone()))
            .update_expression("SET cancelRequested = :true")
            .condition_expression(
                "attribute_exists(pk) AND #owner = :owner \
                 AND (attribute_not_exists(statusState) \
                      OR NOT statusState IN (:completed, :failed, :canceled, :rejected))",
            )
            .expression_attribute_names("#owner", "owner")
            .expression_attribute_values(":true", AttributeValue::Bool(true))
            .expression_attribute_values(":owner", AttributeValue::S(owner.to_string()))
            .expression_attribute_values(":completed", AttributeValue::S("Completed".to_string()))
            .expression_attribute_values(":failed", AttributeValue::S("Failed".to_string()))
            .expression_attribute_values(":canceled", AttributeValue::S("Canceled".to_string()))
            .expression_attribute_values(":rejected", AttributeValue::S("Rejected".to_string()))
            .send()
            .await;

        match result {
            Ok(_) => Ok(()),
            Err(err) => {
                let err_dbg = format!("{err:?}");
                if err_dbg.contains("ConditionalCheckFailed") {
                    // Probe to classify: missing, wrong owner, or terminal.
                    match self.get_task(tenant, task_id, owner, Some(0)).await {
                        Ok(Some(t)) => {
                            let state = t.status().and_then(|s| s.state().ok());
                            if let Some(s) = state {
                                if turul_a2a_types::state_machine::is_terminal(s) {
                                    return Err(A2aStorageError::TerminalState(s));
                                }
                            }
                            // get_task succeeded but state is non-terminal —
                            // means owner check must have been the failing
                            // condition, but get_task (also owner-scoped)
                            // returned Some, which means... odd. Treat as
                            // not-found for safety.
                            Err(A2aStorageError::TaskNotFound(task_id.to_string()))
                        }
                        Ok(None) => Err(A2aStorageError::TaskNotFound(task_id.to_string())),
                        Err(_) => Err(A2aStorageError::DatabaseError(err_dbg)),
                    }
                } else {
                    Err(A2aStorageError::DatabaseError(err_dbg))
                }
            }
        }
    }
}

#[async_trait]
impl crate::storage::A2aCancellationSupervisor for DynamoDbA2aStorage {
    fn backend_name(&self) -> &'static str {
        "dynamodb"
    }

    async fn supervisor_get_cancel_requested(
        &self,
        tenant: &str,
        task_id: &str,
    ) -> Result<bool, A2aStorageError> {
        let pk = Self::task_key(tenant, task_id);
        let result = self
            .client
            .get_item()
            .table_name(&self.config.tasks_table)
            .key("pk", AttributeValue::S(pk))
            .projection_expression("cancelRequested, statusState")
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
        let Some(item) = result.item else {
            return Ok(false);
        };
        let state_str = item
            .get("statusState")
            .and_then(|v| v.as_s().ok())
            .map(String::as_str);
        let is_terminal = matches!(
            state_str,
            Some("Completed") | Some("Failed") | Some("Canceled") | Some("Rejected")
        );
        if is_terminal {
            return Ok(false);
        }
        let marker = item
            .get("cancelRequested")
            .and_then(|v| v.as_bool().ok().copied())
            .unwrap_or(false);
        Ok(marker)
    }

    async fn supervisor_list_cancel_requested(
        &self,
        tenant: &str,
        task_ids: &[String],
    ) -> Result<Vec<String>, A2aStorageError> {
        // DynamoDB BatchGetItem is the efficient path but has a 100-item
        // cap per call. Chunk the request and filter locally by marker +
        // non-terminal.
        let mut hits = Vec::new();
        for chunk in task_ids.chunks(100) {
            let keys: Vec<_> = chunk
                .iter()
                .map(|tid| {
                    let mut m = std::collections::HashMap::new();
                    m.insert(
                        "pk".to_string(),
                        AttributeValue::S(Self::task_key(tenant, tid)),
                    );
                    m
                })
                .collect();
            let keys_and_attrs = aws_sdk_dynamodb::types::KeysAndAttributes::builder()
                .set_keys(Some(keys))
                .projection_expression("pk, cancelRequested, statusState")
                .build()
                .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
            let resp = self
                .client
                .batch_get_item()
                .request_items(self.config.tasks_table.clone(), keys_and_attrs)
                .send()
                .await
                .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
            if let Some(responses) = resp.responses {
                if let Some(items) = responses.get(&self.config.tasks_table) {
                    for item in items {
                        let pk = item
                            .get("pk")
                            .and_then(|v| v.as_s().ok())
                            .cloned()
                            .unwrap_or_default();
                        let state_str = item
                            .get("statusState")
                            .and_then(|v| v.as_s().ok())
                            .map(String::as_str);
                        let is_terminal = matches!(
                            state_str,
                            Some("Completed")
                                | Some("Failed")
                                | Some("Canceled")
                                | Some("Rejected")
                        );
                        if is_terminal {
                            continue;
                        }
                        let marker = item
                            .get("cancelRequested")
                            .and_then(|v| v.as_bool().ok().copied())
                            .unwrap_or(false);
                        if !marker {
                            continue;
                        }
                        // pk = "{tenant}#{task_id}" per task_key; extract
                        // the task_id by splitting on the first '#'.
                        if let Some(task_id) = pk.split_once('#').map(|(_, t)| t.to_string()) {
                            hits.push(task_id);
                        }
                    }
                }
            }
        }
        Ok(hits)
    }
}

#[async_trait]
impl A2aPushNotificationStorage for DynamoDbA2aStorage {
    fn backend_name(&self) -> &'static str {
        "dynamodb"
    }

    async fn create_config(
        &self,
        tenant: &str,
        mut config: turul_a2a_proto::TaskPushNotificationConfig,
    ) -> Result<turul_a2a_proto::TaskPushNotificationConfig, A2aStorageError> {
        if config.id.is_empty() {
            config.id = uuid::Uuid::now_v7().to_string();
        }
        let config_pk = format!("{tenant}#{}#{}", config.task_id, config.id);
        let task_pk = Self::task_key(tenant, &config.task_id);
        let json = serde_json::to_string(&config)
            .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

        // ADR-013 §6.4 causal-floor CAS: read
        // `a2a_tasks[task_pk].latestEventSequence`, then issue
        // `TransactWriteItems { ConditionCheck on latestEventSequence =
        // :seq_read, Put config }`. A concurrent atomic commit that
        // advanced `latestEventSequence` between our read and the
        // transaction surfaces as `TransactionCanceledException`
        // whose CancellationReason cites the ConditionCheck; the
        // outer loop retries with backoff.
        //
        // If the task row does not exist yet, the CAS targets
        // `attribute_not_exists(pk)` — no concurrent writer can race
        // us on a task that does not exist.
        const MAX_ATTEMPTS: u32 = 5;
        let backoffs_ms: [u64; 4] = [10, 50, 250, 1000];
        for attempt in 0..MAX_ATTEMPTS {
            // Read current latest_event_sequence (0 if row absent).
            let seq_read =
                query_latest_event_sequence(&self.client, &self.config.tasks_table, &task_pk)
                    .await?;

            // Build the ConditionCheck — branch on task existence.
            let task_exists = self
                .client
                .get_item()
                .table_name(&self.config.tasks_table)
                .key("pk", AttributeValue::S(task_pk.clone()))
                .projection_expression("pk")
                .send()
                .await
                .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?
                .item
                .is_some();

            let condition_check = if task_exists {
                aws_sdk_dynamodb::types::ConditionCheck::builder()
                    .table_name(&self.config.tasks_table)
                    .key("pk", AttributeValue::S(task_pk.clone()))
                    .condition_expression(
                        "latestEventSequence = :seq OR \
                         (attribute_not_exists(latestEventSequence) AND :seq = :zero)",
                    )
                    .expression_attribute_values(":seq", AttributeValue::N(seq_read.to_string()))
                    .expression_attribute_values(":zero", AttributeValue::N("0".into()))
                    .build()
                    .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?
            } else {
                aws_sdk_dynamodb::types::ConditionCheck::builder()
                    .table_name(&self.config.tasks_table)
                    .key("pk", AttributeValue::S(task_pk.clone()))
                    .condition_expression("attribute_not_exists(pk)")
                    .build()
                    .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?
            };

            let mut config_put = aws_sdk_dynamodb::types::Put::builder()
                .table_name(&self.config.push_configs_table)
                .item("pk", AttributeValue::S(config_pk.clone()))
                .item("tenant", AttributeValue::S(tenant.to_string()))
                .item("taskId", AttributeValue::S(config.task_id.clone()))
                .item("configId", AttributeValue::S(config.id.clone()))
                .item("configJson", AttributeValue::S(json.clone()))
                .item(
                    "registeredAfterEventSequence",
                    AttributeValue::N(seq_read.to_string()),
                );
            if let Some(ttl) = Self::ttl_epoch(self.config.task_ttl_seconds) {
                config_put = config_put.item("ttl", ttl);
            }
            let config_put = config_put
                .build()
                .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

            let items = vec![
                aws_sdk_dynamodb::types::TransactWriteItem::builder()
                    .condition_check(condition_check)
                    .build(),
                aws_sdk_dynamodb::types::TransactWriteItem::builder()
                    .put(config_put)
                    .build(),
            ];

            let send_result = self
                .client
                .transact_write_items()
                .set_transact_items(Some(items))
                .send()
                .await;

            match send_result {
                Ok(_) => return Ok(config),
                Err(e) => {
                    let err_dbg = format!("{e:?}");
                    let is_cas_loss = err_dbg.contains("TransactionCanceledException")
                        && err_dbg.contains("ConditionalCheckFailed");
                    if is_cas_loss && attempt + 1 < MAX_ATTEMPTS {
                        let ms = backoffs_ms[attempt as usize];
                        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                        continue;
                    }
                    if is_cas_loss {
                        return Err(A2aStorageError::CreateConfigCasTimeout {
                            tenant: tenant.into(),
                            task_id: config.task_id.clone(),
                        });
                    }
                    return Err(A2aStorageError::DatabaseError(err_dbg));
                }
            }
        }
        Err(A2aStorageError::CreateConfigCasTimeout {
            tenant: tenant.into(),
            task_id: config.task_id.clone(),
        })
    }

    async fn get_config(
        &self,
        tenant: &str,
        task_id: &str,
        config_id: &str,
    ) -> Result<Option<turul_a2a_proto::TaskPushNotificationConfig>, A2aStorageError> {
        let pk = format!("{tenant}#{task_id}#{config_id}");
        let result = self
            .client
            .get_item()
            .table_name(&self.config.push_configs_table)
            .key("pk", AttributeValue::S(pk))
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

        match result.item {
            Some(item) => {
                let json = item
                    .get("configJson")
                    .and_then(|v| v.as_s().ok())
                    .ok_or_else(|| A2aStorageError::DatabaseError("Missing config_json".into()))?;
                let config = serde_json::from_str(json)
                    .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    async fn list_configs(
        &self,
        tenant: &str,
        task_id: &str,
        page_token: Option<&str>,
        page_size: Option<i32>,
    ) -> Result<PushConfigListPage, A2aStorageError> {
        let page_size = page_size.map(|ps| ps.clamp(1, 100)).unwrap_or(50);

        let result = self
            .client
            .scan()
            .table_name(&self.config.push_configs_table)
            .filter_expression("tenant = :t AND taskId = :tid")
            .expression_attribute_values(":t", AttributeValue::S(tenant.to_string()))
            .expression_attribute_values(":tid", AttributeValue::S(task_id.to_string()))
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

        let items = result.items.unwrap_or_default();
        let mut configs: Vec<turul_a2a_proto::TaskPushNotificationConfig> = items
            .iter()
            .filter_map(|item| {
                item.get("configJson")
                    .and_then(|v| v.as_s().ok())
                    .and_then(|json| serde_json::from_str(json).ok())
            })
            .collect();
        configs.sort_by(|a, b| a.id.cmp(&b.id));

        let start_idx = if let Some(token) = page_token {
            configs
                .iter()
                .position(|c| c.id.as_str() > token)
                .unwrap_or(configs.len())
        } else {
            0
        };

        let end_idx = (start_idx + page_size as usize).min(configs.len());
        let page_configs = configs[start_idx..end_idx].to_vec();

        let next_page_token = if end_idx < configs.len() {
            page_configs
                .last()
                .map(|c| c.id.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };

        Ok(PushConfigListPage {
            configs: page_configs,
            next_page_token,
        })
    }

    async fn delete_config(
        &self,
        tenant: &str,
        task_id: &str,
        config_id: &str,
    ) -> Result<(), A2aStorageError> {
        let pk = format!("{tenant}#{task_id}#{config_id}");
        self.client
            .delete_item()
            .table_name(&self.config.push_configs_table)
            .key("pk", AttributeValue::S(pk))
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
        Ok(())
    }

    async fn list_configs_eligible_at_event(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        page_token: Option<&str>,
        page_size: Option<i32>,
    ) -> Result<PushConfigListPage, A2aStorageError> {
        let page_size = page_size.map(|ps| ps.clamp(1, 100)).unwrap_or(50);

        // Scan with filter on tenant + taskId + strict registeredAfter
        // upper bound. Pre-migration rows have the attribute absent;
        // default-0 semantics require `attribute_not_exists(...)` to
        // count as 0 < event_sequence when event_sequence > 0.
        let result = self
            .client
            .scan()
            .table_name(&self.config.push_configs_table)
            .filter_expression(
                "tenant = :t AND taskId = :tid AND \
                 (registeredAfterEventSequence < :seq OR \
                  attribute_not_exists(registeredAfterEventSequence))",
            )
            .expression_attribute_values(":t", AttributeValue::S(tenant.to_string()))
            .expression_attribute_values(":tid", AttributeValue::S(task_id.to_string()))
            .expression_attribute_values(":seq", AttributeValue::N(event_sequence.to_string()))
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

        let items = result.items.unwrap_or_default();
        let mut configs: Vec<turul_a2a_proto::TaskPushNotificationConfig> = items
            .iter()
            .filter_map(|item| {
                item.get("configJson")
                    .and_then(|v| v.as_s().ok())
                    .and_then(|json| serde_json::from_str(json).ok())
            })
            .collect();
        configs.sort_by(|a, b| a.id.cmp(&b.id));

        let start_idx = if let Some(token) = page_token {
            configs
                .iter()
                .position(|c| c.id.as_str() > token)
                .unwrap_or(configs.len())
        } else {
            0
        };

        let end_idx = (start_idx + page_size as usize).min(configs.len());
        let page_configs = configs[start_idx..end_idx].to_vec();

        let next_page_token = if end_idx < configs.len() {
            page_configs
                .last()
                .map(|c| c.id.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };

        Ok(PushConfigListPage {
            configs: page_configs,
            next_page_token,
        })
    }
}

/// Helper: query the current max event sequence for a task.
/// Read the stored `latestEventSequence` attribute for a task row.
/// Returns 0 for a missing task or missing attribute — the Put/Update
/// caller then overwrites with `max(0, new_seqs)` which preserves the
/// ADR-013 §6.3 monotonic invariant.
async fn query_latest_event_sequence(
    client: &Client,
    tasks_table: &str,
    pk: &str,
) -> Result<u64, A2aStorageError> {
    let result = client
        .get_item()
        .table_name(tasks_table)
        .key("pk", AttributeValue::S(pk.to_string()))
        .projection_expression("latestEventSequence")
        .send()
        .await
        .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
    if let Some(item) = result.item {
        if let Some(AttributeValue::N(n)) = item.get("latestEventSequence") {
            return n
                .parse::<u64>()
                .map_err(|e| A2aStorageError::DatabaseError(e.to_string()));
        }
    }
    Ok(0)
}

async fn query_max_sequence(
    client: &Client,
    table: &str,
    pk: &str,
) -> Result<u64, A2aStorageError> {
    // Query in reverse order, limit 1 to get the highest sequence
    let result = client
        .query()
        .table_name(table)
        .key_condition_expression("pk = :pk")
        .expression_attribute_values(":pk", AttributeValue::S(pk.to_string()))
        .scan_index_forward(false)
        .limit(1)
        .projection_expression("eventSequence")
        .send()
        .await
        .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

    if let Some(items) = result.items {
        if let Some(item) = items.first() {
            if let Some(AttributeValue::N(n)) = item.get("eventSequence") {
                return n
                    .parse::<u64>()
                    .map_err(|e| A2aStorageError::DatabaseError(e.to_string()));
            }
        }
    }
    Ok(0)
}

#[async_trait]
impl A2aEventStore for DynamoDbA2aStorage {
    fn backend_name(&self) -> &'static str {
        "dynamodb"
    }

    async fn append_event(
        &self,
        tenant: &str,
        task_id: &str,
        event: StreamEvent,
    ) -> Result<u64, A2aStorageError> {
        let pk = Self::task_key(tenant, task_id);
        let event_data = serde_json::to_string(&event)
            .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

        let max_seq = query_max_sequence(&self.client, &self.config.events_table, &pk).await?;
        let new_seq = max_seq + 1;

        // Uniqueness on the composite key `(pk, sk)` — both must be
        // absent. Before the table grew an `sk` range key, the
        // single-attribute check was a spurious success that
        // overwrote the first row for the partition on any second
        // put.
        let item = Self::build_event_item(
            &pk,
            new_seq,
            &event_data,
            Self::ttl_epoch(self.config.event_ttl_seconds),
        );
        self.client
            .put_item()
            .table_name(&self.config.events_table)
            .set_item(Some(item))
            .condition_expression("attribute_not_exists(pk) AND attribute_not_exists(sk)")
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

        Ok(new_seq)
    }

    async fn get_events_after(
        &self,
        tenant: &str,
        task_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, StreamEvent)>, A2aStorageError> {
        let pk = Self::task_key(tenant, task_id);

        // The events table's range key is `sk` (zero-padded decimal of
        // `eventSequence`). Filtering on `sk > :sk` gives a native
        // range scan; filtering on the numeric `eventSequence` would
        // force a FilterExpression (post-read) because it is not part
        // of the key schema. We still parse `eventSequence` from each
        // item for the logical sequence — `sk` is only the physical
        // key carrier.
        let after_sk = Self::event_sort_key(after_sequence);
        let result = self
            .client
            .query()
            .table_name(&self.config.events_table)
            .key_condition_expression("pk = :pk AND sk > :sk")
            .expression_attribute_values(":pk", AttributeValue::S(pk))
            .expression_attribute_values(":sk", AttributeValue::S(after_sk))
            .scan_index_forward(true)
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

        let mut events = Vec::new();
        if let Some(items) = result.items {
            for item in items {
                let seq = item
                    .get("eventSequence")
                    .and_then(|v| {
                        if let AttributeValue::N(n) = v {
                            Some(n)
                        } else {
                            None
                        }
                    })
                    .and_then(|n| n.parse::<u64>().ok())
                    .ok_or_else(|| {
                        A2aStorageError::DatabaseError("Missing eventSequence".into())
                    })?;

                let data = item
                    .get("eventData")
                    .and_then(|v| {
                        if let AttributeValue::S(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| A2aStorageError::DatabaseError("Missing eventData".into()))?;

                let event: StreamEvent = serde_json::from_str(data)
                    .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;
                events.push((seq, event));
            }
        }
        Ok(events)
    }

    async fn latest_sequence(&self, tenant: &str, task_id: &str) -> Result<u64, A2aStorageError> {
        let pk = Self::task_key(tenant, task_id);
        query_max_sequence(&self.client, &self.config.events_table, &pk).await
    }

    async fn cleanup_expired(&self) -> Result<u64, A2aStorageError> {
        Ok(0)
    }
}

#[async_trait]
impl A2aAtomicStore for DynamoDbA2aStorage {
    fn backend_name(&self) -> &'static str {
        "dynamodb"
    }

    fn push_dispatch_enabled(&self) -> bool {
        self.push_dispatch_enabled
    }

    async fn create_task_with_events(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
        events: Vec<StreamEvent>,
    ) -> Result<(Task, Vec<u64>), A2aStorageError> {
        let pk = Self::task_key(tenant, task.id());
        let task_json = Self::task_to_json(&task)?;
        let state_str = Self::status_state_str(&task);

        // Sequence allocation: create_task_with_events always starts
        // at seq=1 (no prior events for a fresh task), so the max
        // sequence is simply events.len() when events are non-empty.
        let sequences: Vec<u64> = (1..=events.len() as u64).collect();
        let latest_event_sequence = sequences.last().copied().unwrap_or(0);

        // Build TransactWriteItems: task put + event puts. The task
        // put carries `latestEventSequence` directly (ADR-013 §6.3) so
        // one Put covers the causal-floor maintenance.
        let mut task_put = aws_sdk_dynamodb::types::Put::builder()
            .table_name(&self.config.tasks_table)
            .item("pk", AttributeValue::S(pk.clone()))
            .item("tenant", AttributeValue::S(tenant.to_string()))
            .item("taskId", AttributeValue::S(task.id().to_string()))
            .item("owner", AttributeValue::S(owner.to_string()))
            .item("taskJson", AttributeValue::S(task_json))
            .item(
                "contextId",
                AttributeValue::S(task.context_id().to_string()),
            )
            .item("statusState", AttributeValue::S(state_str))
            .item("updatedAt", AttributeValue::S(Self::now_iso()))
            .item(
                "latestEventSequence",
                AttributeValue::N(latest_event_sequence.to_string()),
            );
        if let Some(ttl) = Self::ttl_epoch(self.config.task_ttl_seconds) {
            task_put = task_put.item("ttl", ttl);
        }
        let mut items = vec![
            aws_sdk_dynamodb::types::TransactWriteItem::builder()
                .put(
                    task_put
                        .build()
                        .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?,
                )
                .build(),
        ];

        for (i, event) in events.iter().enumerate() {
            let seq = sequences[i];
            let event_data = serde_json::to_string(event)
                .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

            let event_item = Self::build_event_item(
                &pk,
                seq,
                &event_data,
                Self::ttl_epoch(self.config.event_ttl_seconds),
            );
            let event_put = aws_sdk_dynamodb::types::Put::builder()
                .table_name(&self.config.events_table)
                .set_item(Some(event_item))
                .condition_expression("attribute_not_exists(pk) AND attribute_not_exists(sk)");
            items.push(
                aws_sdk_dynamodb::types::TransactWriteItem::builder()
                    .put(
                        event_put
                            .build()
                            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?,
                    )
                    .build(),
            );
        }

        self.client
            .transact_write_items()
            .set_transact_items(Some(items))
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

        Ok((task, sequences))
    }

    async fn update_task_status_with_events(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        new_status: TaskStatus,
        events: Vec<StreamEvent>,
    ) -> Result<(Task, Vec<u64>), A2aStorageError> {
        let pk = Self::task_key(tenant, task_id);

        // Read current task for validation
        let task = self
            .get_task(tenant, task_id, owner, None)
            .await?
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?;

        let current_state = task
            .status()
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?
            .state()
            .map_err(A2aStorageError::TypeError)?;
        let new_state = new_status.state().map_err(A2aStorageError::TypeError)?;

        turul_a2a_types::state_machine::validate_transition(current_state, new_state).map_err(
            |e| match e {
                turul_a2a_types::A2aTypeError::InvalidTransition { current, requested } => {
                    A2aStorageError::InvalidTransition { current, requested }
                }
                turul_a2a_types::A2aTypeError::TerminalState(s) => {
                    A2aStorageError::TerminalStateAlreadySet {
                        task_id: task_id.to_string(),
                        current_state: crate::storage::terminal_cas::task_state_wire_name(s)
                            .to_string(),
                    }
                }
                other => A2aStorageError::TypeError(other),
            },
        )?;

        // Build updated task
        let mut proto = task.as_proto().clone();
        proto.status = Some(new_status.into_proto());
        let updated_task = Task::try_from(proto).map_err(A2aStorageError::TypeError)?;

        let task_json = Self::task_to_json(&updated_task)?;
        let state_str = Self::status_state_str(&updated_task);

        // Get current max sequence for event numbering AND the
        // prior latest_event_sequence so the Put preserves it when
        // events is empty (ADR-013 §6.3 monotonic invariant).
        let max_seq = query_max_sequence(&self.client, &self.config.events_table, &pk).await?;
        let prior_latest =
            query_latest_event_sequence(&self.client, &self.config.tasks_table, &pk).await?;
        let new_latest_event_sequence = prior_latest.max(max_seq + events.len() as u64);

        // Build TransactWriteItems: task update + event puts.
        //
        // Terminal-write CAS (ADR-010 §7.1): the task Put's
        // ConditionExpression now ALSO asserts that the existing
        // statusState is NOT any of the four terminal values. Between the
        // get_task read above and this transact_write_items call, another
        // instance may have committed a terminal for this pk — the
        // condition-expression rejects our write atomically at the
        // backend, preserving the single-terminal-writer invariant.
        let mut items = vec![
            aws_sdk_dynamodb::types::TransactWriteItem::builder()
                .put(
                    aws_sdk_dynamodb::types::Put::builder()
                        .table_name(&self.config.tasks_table)
                        .item("pk", AttributeValue::S(pk.clone()))
                        .item("owner", AttributeValue::S(owner.to_string()))
                        .item("taskJson", AttributeValue::S(task_json))
                        .item(
                            "contextId",
                            AttributeValue::S(updated_task.context_id().to_string()),
                        )
                        .item("statusState", AttributeValue::S(state_str))
                        .item("updatedAt", AttributeValue::S(Self::now_iso()))
                        .item(
                            "latestEventSequence",
                            AttributeValue::N(new_latest_event_sequence.to_string()),
                        )
                        .condition_expression(
                            "#owner = :owner AND (attribute_not_exists(statusState) \
                             OR NOT statusState IN (:completed, :failed, :canceled, :rejected))",
                        )
                        .expression_attribute_names("#owner", "owner")
                        .expression_attribute_values(":owner", AttributeValue::S(owner.to_string()))
                        .expression_attribute_values(
                            ":completed",
                            AttributeValue::S("Completed".to_string()),
                        )
                        .expression_attribute_values(
                            ":failed",
                            AttributeValue::S("Failed".to_string()),
                        )
                        .expression_attribute_values(
                            ":canceled",
                            AttributeValue::S("Canceled".to_string()),
                        )
                        .expression_attribute_values(
                            ":rejected",
                            AttributeValue::S("Rejected".to_string()),
                        )
                        .build()
                        .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?,
                )
                .build(),
        ];

        // Append events and, when the push_dispatch opt-in is on,
        // add a Put for the pending-dispatch marker inside the same
        // TransactWriteItems (ADR-013 §4.3). Terminal `StatusUpdate`
        // events only — non-terminal / artifact events do not produce
        // markers.
        let now_micros = ddb_systime_to_micros(std::time::SystemTime::now());
        let marker_ttl = Self::ttl_epoch(self.config.push_delivery_ttl_seconds);
        let mut sequences = Vec::with_capacity(events.len());
        for (i, event) in events.iter().enumerate() {
            let seq = max_seq + (i as u64) + 1;
            sequences.push(seq);
            let event_data = serde_json::to_string(event)
                .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

            let event_item = Self::build_event_item(
                &pk,
                seq,
                &event_data,
                Self::ttl_epoch(self.config.event_ttl_seconds),
            );
            items.push(
                aws_sdk_dynamodb::types::TransactWriteItem::builder()
                    .put(
                        aws_sdk_dynamodb::types::Put::builder()
                            .table_name(&self.config.events_table)
                            .set_item(Some(event_item))
                            .condition_expression(
                                "attribute_not_exists(pk) AND attribute_not_exists(sk)",
                            )
                            .build()
                            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?,
                    )
                    .build(),
            );

            if self.push_dispatch_enabled
                && event.is_terminal()
                && matches!(event, StreamEvent::StatusUpdate { .. })
            {
                let marker_sk = seq.to_string();
                let mut marker_put = aws_sdk_dynamodb::types::Put::builder()
                    .table_name(&self.config.push_pending_dispatches_table)
                    .item("pk", AttributeValue::S(pk.clone()))
                    .item("sk", AttributeValue::S(marker_sk))
                    .item("tenant", AttributeValue::S(tenant.to_string()))
                    .item("taskId", AttributeValue::S(task_id.to_string()))
                    .item("eventSequence", AttributeValue::N(seq.to_string()))
                    .item("owner", AttributeValue::S(owner.to_string()))
                    .item(
                        "recordedAtMicros",
                        AttributeValue::N(now_micros.to_string()),
                    );
                if let Some(ttl) = marker_ttl.clone() {
                    marker_put = marker_put.item("ttl", ttl);
                }
                items.push(
                    aws_sdk_dynamodb::types::TransactWriteItem::builder()
                        .put(
                            marker_put
                                .build()
                                .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?,
                        )
                        .build(),
                );
            }
        }

        let send_result = self
            .client
            .transact_write_items()
            .set_transact_items(Some(items))
            .send()
            .await;

        match send_result {
            Ok(_) => Ok((updated_task, sequences)),
            Err(err) => {
                // Detect the task-item condition failure — either the owner
                // check or the non-terminal CAS failed. Re-fetch the task to
                // figure out which: if the current state is terminal, this is
                // a CAS loser; otherwise fall back to the generic DB error.
                //
                // Detection is via the service error string; the Rust SDK
                // surfaces `TransactionCanceledException` here.
                let err_dbg = format!("{err:?}");
                if err_dbg.contains("TransactionCanceledException")
                    || err_dbg.contains("ConditionalCheckFailed")
                {
                    // Probe current state to classify.
                    match self.get_task(tenant, task_id, owner, Some(0)).await {
                        Ok(Some(current_task)) => {
                            let probe_state = current_task.status().and_then(|s| s.state().ok());
                            if let Some(s) = probe_state {
                                if turul_a2a_types::state_machine::is_terminal(s) {
                                    return Err(A2aStorageError::TerminalStateAlreadySet {
                                        task_id: task_id.to_string(),
                                        current_state:
                                            crate::storage::terminal_cas::task_state_wire_name(s)
                                                .to_string(),
                                    });
                                }
                            }
                        }
                        Ok(None) => {
                            // Task disappeared between reads — unusual but surface
                            // as TaskNotFound.
                            return Err(A2aStorageError::TaskNotFound(task_id.to_string()));
                        }
                        Err(_) => {
                            // Fall through to generic error.
                        }
                    }
                }
                Err(A2aStorageError::DatabaseError(err_dbg))
            }
        }
    }

    async fn update_task_with_events(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
        events: Vec<StreamEvent>,
    ) -> Result<Vec<u64>, A2aStorageError> {
        let pk = Self::task_key(tenant, task.id());
        let task_json = Self::task_to_json(&task)?;
        let state_str = Self::status_state_str(&task);

        // Get current max sequence AND prior latest_event_sequence
        // (ADR-013 §6.3: full-replacement Put must not regress the
        // causal floor when events is empty).
        let max_seq = query_max_sequence(&self.client, &self.config.events_table, &pk).await?;
        let prior_latest =
            query_latest_event_sequence(&self.client, &self.config.tasks_table, &pk).await?;
        let new_latest_event_sequence = prior_latest.max(max_seq + events.len() as u64);

        // Build TransactWriteItems: task put (with owner check + terminal
        // CAS) + event puts. ADR-010 §7.1 extension: the ConditionExpression
        // rejects writes against a task whose persisted statusState is
        // already terminal, protecting against full-task replacement
        // clobbering a concurrent terminal commit.
        let mut task_put = aws_sdk_dynamodb::types::Put::builder()
            .table_name(&self.config.tasks_table)
            .item("pk", AttributeValue::S(pk.clone()))
            .item("tenant", AttributeValue::S(tenant.to_string()))
            .item("taskId", AttributeValue::S(task.id().to_string()))
            .item("owner", AttributeValue::S(owner.to_string()))
            .item("taskJson", AttributeValue::S(task_json))
            .item(
                "contextId",
                AttributeValue::S(task.context_id().to_string()),
            )
            .item("statusState", AttributeValue::S(state_str))
            .item("updatedAt", AttributeValue::S(Self::now_iso()))
            .item(
                "latestEventSequence",
                AttributeValue::N(new_latest_event_sequence.to_string()),
            )
            .condition_expression(
                "#owner = :owner AND (attribute_not_exists(statusState) \
                 OR NOT statusState IN (:completed, :failed, :canceled, :rejected))",
            )
            .expression_attribute_names("#owner", "owner")
            .expression_attribute_values(":owner", AttributeValue::S(owner.to_string()))
            .expression_attribute_values(":completed", AttributeValue::S("Completed".to_string()))
            .expression_attribute_values(":failed", AttributeValue::S("Failed".to_string()))
            .expression_attribute_values(":canceled", AttributeValue::S("Canceled".to_string()))
            .expression_attribute_values(":rejected", AttributeValue::S("Rejected".to_string()));
        if let Some(ttl) = Self::ttl_epoch(self.config.task_ttl_seconds) {
            task_put = task_put.item("ttl", ttl);
        }
        let mut items = vec![
            aws_sdk_dynamodb::types::TransactWriteItem::builder()
                .put(
                    task_put
                        .build()
                        .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?,
                )
                .build(),
        ];

        let mut sequences = Vec::with_capacity(events.len());
        for (i, event) in events.iter().enumerate() {
            let seq = max_seq + (i as u64) + 1;
            sequences.push(seq);
            let event_data = serde_json::to_string(event)
                .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

            let event_item = Self::build_event_item(
                &pk,
                seq,
                &event_data,
                Self::ttl_epoch(self.config.event_ttl_seconds),
            );
            let event_put = aws_sdk_dynamodb::types::Put::builder()
                .table_name(&self.config.events_table)
                .set_item(Some(event_item))
                .condition_expression("attribute_not_exists(pk) AND attribute_not_exists(sk)");
            items.push(
                aws_sdk_dynamodb::types::TransactWriteItem::builder()
                    .put(
                        event_put
                            .build()
                            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?,
                    )
                    .build(),
            );
        }

        let task_id = task.id().to_string();
        let tx_result = self
            .client
            .transact_write_items()
            .set_transact_items(Some(items))
            .send()
            .await;

        match tx_result {
            Ok(_) => Ok(sequences),
            Err(err) => {
                // Classify via the service error string: if the tx was
                // cancelled because the task-item's condition failed AND
                // the persisted state is terminal, return
                // TerminalStateAlreadySet. Owner-mismatch falls through to
                // a generic DB error since the existing contract path
                // never mapped update_task_with_events to OwnerMismatch.
                let err_dbg = format!("{err:?}");
                if err_dbg.contains("TransactionCanceledException")
                    || err_dbg.contains("ConditionalCheckFailed")
                {
                    match self.get_task(tenant, &task_id, owner, Some(0)).await {
                        Ok(Some(current_task)) => {
                            let probe_state = current_task.status().and_then(|s| s.state().ok());
                            if let Some(s) = probe_state {
                                if turul_a2a_types::state_machine::is_terminal(s) {
                                    return Err(A2aStorageError::TerminalStateAlreadySet {
                                        task_id,
                                        current_state:
                                            crate::storage::terminal_cas::task_state_wire_name(s)
                                                .to_string(),
                                    });
                                }
                            }
                        }
                        Ok(None) => {
                            return Err(A2aStorageError::TaskNotFound(task_id));
                        }
                        Err(_) => {}
                    }
                }
                Err(A2aStorageError::DatabaseError(err_dbg))
            }
        }
    }
}

// ===========================================================================
// A2aPushDeliveryStore (ADR-011 §10)
//
// Table shape: `pk` (HASH) + `sk` (RANGE). For this table,
// `pk = "{tenant}#{task_id}#{event_sequence}"` and `sk = config_id`.
// The composite key keeps all configs for one event co-located so a
// Query can enumerate them, while still giving each (event, config)
// pair a unique primary-key slot.
//
// Fencing: `record_attempt_started` and `record_delivery_outcome`
// use ConditionExpression on `(claimant = :c AND generation = :g)`.
// A condition failure returns `StaleDeliveryClaim`.
//
// Timestamps: stored as numeric `claimedAtMicros` / `expiresAtMicros`
// / etc. so DynamoDB comparisons on expiry are numeric and correct
// across clock boundaries.
// ===========================================================================

impl DynamoDbA2aStorage {
    fn push_delivery_pk(tenant: &str, task_id: &str, event_sequence: u64) -> String {
        format!("{tenant}#{task_id}#{event_sequence}")
    }
}

fn ddb_systime_to_micros(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}
fn ddb_micros_to_systime(m: i64) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_micros(m.max(0) as u64)
}
fn ddb_claim_status_to_str(s: ClaimStatus) -> &'static str {
    match s {
        ClaimStatus::Pending => "Pending",
        ClaimStatus::Attempting => "Attempting",
        ClaimStatus::Succeeded => "Succeeded",
        ClaimStatus::GaveUp => "GaveUp",
        ClaimStatus::Abandoned => "Abandoned",
    }
}
fn ddb_claim_status_from_str(s: &str) -> Result<ClaimStatus, A2aStorageError> {
    match s {
        "Pending" => Ok(ClaimStatus::Pending),
        "Attempting" => Ok(ClaimStatus::Attempting),
        "Succeeded" => Ok(ClaimStatus::Succeeded),
        "GaveUp" => Ok(ClaimStatus::GaveUp),
        "Abandoned" => Ok(ClaimStatus::Abandoned),
        other => Err(A2aStorageError::DatabaseError(format!(
            "unknown claim status: {other}"
        ))),
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", content = "s")]
enum DdbErrorClassWire {
    NetworkError,
    Timeout,
    HttpError4xx(u16),
    HttpError5xx(u16),
    HttpError429,
    SSRFBlocked,
    PayloadTooLarge,
    ConfigDeleted,
    TaskDeleted,
    TlsRejected,
}
impl From<DeliveryErrorClass> for DdbErrorClassWire {
    fn from(c: DeliveryErrorClass) -> Self {
        match c {
            DeliveryErrorClass::NetworkError => Self::NetworkError,
            DeliveryErrorClass::Timeout => Self::Timeout,
            DeliveryErrorClass::HttpError4xx { status } => Self::HttpError4xx(status),
            DeliveryErrorClass::HttpError5xx { status } => Self::HttpError5xx(status),
            DeliveryErrorClass::HttpError429 => Self::HttpError429,
            DeliveryErrorClass::SSRFBlocked => Self::SSRFBlocked,
            DeliveryErrorClass::PayloadTooLarge => Self::PayloadTooLarge,
            DeliveryErrorClass::ConfigDeleted => Self::ConfigDeleted,
            DeliveryErrorClass::TaskDeleted => Self::TaskDeleted,
            DeliveryErrorClass::TlsRejected => Self::TlsRejected,
        }
    }
}
impl From<DdbErrorClassWire> for DeliveryErrorClass {
    fn from(w: DdbErrorClassWire) -> Self {
        match w {
            DdbErrorClassWire::NetworkError => DeliveryErrorClass::NetworkError,
            DdbErrorClassWire::Timeout => DeliveryErrorClass::Timeout,
            DdbErrorClassWire::HttpError4xx(s) => DeliveryErrorClass::HttpError4xx { status: s },
            DdbErrorClassWire::HttpError5xx(s) => DeliveryErrorClass::HttpError5xx { status: s },
            DdbErrorClassWire::HttpError429 => DeliveryErrorClass::HttpError429,
            DdbErrorClassWire::SSRFBlocked => DeliveryErrorClass::SSRFBlocked,
            DdbErrorClassWire::PayloadTooLarge => DeliveryErrorClass::PayloadTooLarge,
            DdbErrorClassWire::ConfigDeleted => DeliveryErrorClass::ConfigDeleted,
            DdbErrorClassWire::TaskDeleted => DeliveryErrorClass::TaskDeleted,
            DdbErrorClassWire::TlsRejected => DeliveryErrorClass::TlsRejected,
        }
    }
}
fn ddb_error_class_to_json(c: DeliveryErrorClass) -> String {
    serde_json::to_string(&DdbErrorClassWire::from(c)).unwrap_or_else(|_| "{}".into())
}
fn ddb_error_class_from_json(s: &str) -> Option<DeliveryErrorClass> {
    serde_json::from_str::<DdbErrorClassWire>(s)
        .ok()
        .map(Into::into)
}
fn ddb_gave_up_reason_to_str(r: GaveUpReason) -> &'static str {
    match r {
        GaveUpReason::MaxAttemptsExhausted => "MaxAttemptsExhausted",
        GaveUpReason::NonRetryableHttpStatus => "NonRetryableHttpStatus",
        GaveUpReason::SsrfBlocked => "SsrfBlocked",
        GaveUpReason::PayloadTooLarge => "PayloadTooLarge",
        GaveUpReason::TlsRejected => "TlsRejected",
    }
}
fn ddb_abandoned_reason_to_str(r: AbandonedReason) -> &'static str {
    match r {
        AbandonedReason::ConfigDeleted => "ConfigDeleted",
        AbandonedReason::TaskDeleted => "TaskDeleted",
        AbandonedReason::NonHttpsUrlInProduction => "NonHttpsUrlInProduction",
    }
}

#[async_trait]
impl A2aPushDeliveryStore for DynamoDbA2aStorage {
    fn backend_name(&self) -> &'static str {
        "dynamodb"
    }

    async fn claim_delivery(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        config_id: &str,
        claimant: &str,
        owner: &str,
        claim_expiry: std::time::Duration,
    ) -> Result<DeliveryClaim, A2aStorageError> {
        let now = std::time::SystemTime::now();
        let now_micros = ddb_systime_to_micros(now);
        let expires_micros = ddb_systime_to_micros(now + claim_expiry);
        let pk = Self::push_delivery_pk(tenant, task_id, event_sequence);
        let sk = config_id.to_string();

        let existing = self
            .client
            .get_item()
            .table_name(&self.config.push_deliveries_table)
            .key("pk", AttributeValue::S(pk.clone()))
            .key("sk", AttributeValue::S(sk.clone()))
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

        if let Some(item) = existing.item {
            let prev_expires = item
                .get("expiresAtMicros")
                .and_then(|v| {
                    if let AttributeValue::N(n) = v {
                        n.parse::<i64>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let prev_status_s = item
                .get("status")
                .and_then(|v| {
                    if let AttributeValue::S(s) = v {
                        Some(s.as_str())
                    } else {
                        None
                    }
                })
                .unwrap_or("Pending");
            let prev_status = ddb_claim_status_from_str(prev_status_s)?;
            let is_terminal = matches!(
                prev_status,
                ClaimStatus::Succeeded | ClaimStatus::GaveUp | ClaimStatus::Abandoned
            );
            let still_live = prev_expires > now_micros;
            if is_terminal || still_live {
                return Err(A2aStorageError::ClaimAlreadyHeld {
                    tenant: tenant.to_string(),
                    task_id: task_id.to_string(),
                    event_sequence,
                    config_id: config_id.to_string(),
                });
            }
            let prev_gen = item
                .get("generation")
                .and_then(|v| {
                    if let AttributeValue::N(n) = v {
                        n.parse::<i64>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let prev_count = item
                .get("deliveryAttemptCount")
                .and_then(|v| {
                    if let AttributeValue::N(n) = v {
                        n.parse::<i64>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let prev_owner = item
                .get("owner")
                .and_then(|v| {
                    if let AttributeValue::S(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let new_gen = prev_gen + 1;

            // Re-claim UpdateItem fenced on generation match AND
            // still-non-terminal status AND still-expired. Without
            // all three guards, a stale read + concurrent terminal
            // commit could let a re-claim reset the row to Pending —
            // reopening a succeeded/gave-up delivery, which breaks
            // at-least-once and freezes-terminal both.
            let update = self
                .client
                .update_item()
                .table_name(&self.config.push_deliveries_table)
                .key("pk", AttributeValue::S(pk.clone()))
                .key("sk", AttributeValue::S(sk.clone()))
                .update_expression(
                    "SET claimant = :claimant, generation = :newgen, \
                     claimedAtMicros = :claimed, expiresAtMicros = :expires, \
                     #status = :pending \
                     REMOVE gaveUpAtMicros, gaveUpReason, abandonedReason",
                )
                .condition_expression(
                    "generation = :prevgen \
                     AND (#status = :pending OR #status = :attempting) \
                     AND expiresAtMicros < :nowguard",
                )
                .expression_attribute_names("#status", "status")
                .expression_attribute_values(":claimant", AttributeValue::S(claimant.to_string()))
                .expression_attribute_values(":newgen", AttributeValue::N(new_gen.to_string()))
                .expression_attribute_values(":prevgen", AttributeValue::N(prev_gen.to_string()))
                .expression_attribute_values(":claimed", AttributeValue::N(now_micros.to_string()))
                .expression_attribute_values(
                    ":expires",
                    AttributeValue::N(expires_micros.to_string()),
                )
                .expression_attribute_values(":pending", AttributeValue::S("Pending".into()))
                .expression_attribute_values(":attempting", AttributeValue::S("Attempting".into()))
                .expression_attribute_values(
                    ":nowguard",
                    AttributeValue::N(now_micros.to_string()),
                );
            let update = if let Some(ttl) = Self::ttl_epoch(self.config.push_delivery_ttl_seconds) {
                update
                    .update_expression(
                        "SET claimant = :claimant, generation = :newgen, \
                         claimedAtMicros = :claimed, expiresAtMicros = :expires, \
                         #status = :pending, #ttl = :ttl \
                         REMOVE gaveUpAtMicros, gaveUpReason, abandonedReason",
                    )
                    .expression_attribute_names("#ttl", "ttl")
                    .expression_attribute_values(":ttl", ttl)
            } else {
                update
            };
            let res = update.send().await;
            match res {
                Ok(_) => {
                    return Ok(DeliveryClaim {
                        claimant: claimant.to_string(),
                        owner: prev_owner,
                        generation: new_gen as u64,
                        claimed_at: now,
                        delivery_attempt_count: prev_count as u32,
                        status: ClaimStatus::Pending,
                    });
                }
                Err(e) => {
                    let err_dbg = format!("{e:?}");
                    if err_dbg.contains("ConditionalCheckFailed") {
                        return Err(A2aStorageError::ClaimAlreadyHeld {
                            tenant: tenant.to_string(),
                            task_id: task_id.to_string(),
                            event_sequence,
                            config_id: config_id.to_string(),
                        });
                    }
                    return Err(A2aStorageError::DatabaseError(err_dbg));
                }
            }
        }

        // Fresh INSERT with condition that (pk, sk) does not exist.
        let mut put = self
            .client
            .put_item()
            .table_name(&self.config.push_deliveries_table)
            .item("pk", AttributeValue::S(pk))
            .item("sk", AttributeValue::S(sk))
            .item("tenant", AttributeValue::S(tenant.to_string()))
            .item("taskId", AttributeValue::S(task_id.to_string()))
            .item(
                "eventSequence",
                AttributeValue::N(event_sequence.to_string()),
            )
            .item("configId", AttributeValue::S(config_id.to_string()))
            .item("claimant", AttributeValue::S(claimant.to_string()))
            .item("owner", AttributeValue::S(owner.to_string()))
            .item("generation", AttributeValue::N("1".into()))
            .item("claimedAtMicros", AttributeValue::N(now_micros.to_string()))
            .item(
                "expiresAtMicros",
                AttributeValue::N(expires_micros.to_string()),
            )
            .item("deliveryAttemptCount", AttributeValue::N("0".into()))
            .item("status", AttributeValue::S("Pending".into()))
            .condition_expression("attribute_not_exists(pk) AND attribute_not_exists(sk)");
        if let Some(ttl) = Self::ttl_epoch(self.config.push_delivery_ttl_seconds) {
            put = put.item("ttl", ttl);
        }
        match put.send().await {
            Ok(_) => Ok(DeliveryClaim {
                claimant: claimant.to_string(),
                owner: owner.to_string(),
                generation: 1,
                claimed_at: now,
                delivery_attempt_count: 0,
                status: ClaimStatus::Pending,
            }),
            Err(e) => {
                let err_dbg = format!("{e:?}");
                if err_dbg.contains("ConditionalCheckFailed") {
                    Err(A2aStorageError::ClaimAlreadyHeld {
                        tenant: tenant.to_string(),
                        task_id: task_id.to_string(),
                        event_sequence,
                        config_id: config_id.to_string(),
                    })
                } else {
                    Err(A2aStorageError::DatabaseError(err_dbg))
                }
            }
        }
    }

    async fn record_attempt_started(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        config_id: &str,
        claimant: &str,
        claim_generation: u64,
    ) -> Result<u32, A2aStorageError> {
        let now_micros = ddb_systime_to_micros(std::time::SystemTime::now());
        let pk = Self::push_delivery_pk(tenant, task_id, event_sequence);
        let sk = config_id.to_string();
        let res = self
            .client
            .update_item()
            .table_name(&self.config.push_deliveries_table)
            .key("pk", AttributeValue::S(pk))
            .key("sk", AttributeValue::S(sk))
            // Condition fences on identity AND non-terminal status:
            // terminal rows are frozen. If the row is Succeeded /
            // GaveUp / Abandoned, the UpdateItem fails
            // ConditionalCheckFailed and we map to
            // StaleDeliveryClaim, which is what the worker's retry
            // loop expects.
            .update_expression(
                "SET deliveryAttemptCount = deliveryAttemptCount + :one, \
                     #status = :attempting, \
                     firstAttemptedAtMicros = if_not_exists(firstAttemptedAtMicros, :now), \
                     lastAttemptedAtMicros = :now",
            )
            .condition_expression(
                "claimant = :claimant AND generation = :gen \
                 AND (#status = :pending OR #status = :attempting)",
            )
            .expression_attribute_names("#status", "status")
            .expression_attribute_values(":one", AttributeValue::N("1".into()))
            .expression_attribute_values(":pending", AttributeValue::S("Pending".into()))
            .expression_attribute_values(":attempting", AttributeValue::S("Attempting".into()))
            .expression_attribute_values(":now", AttributeValue::N(now_micros.to_string()))
            .expression_attribute_values(":claimant", AttributeValue::S(claimant.to_string()))
            .expression_attribute_values(":gen", AttributeValue::N(claim_generation.to_string()))
            .return_values(aws_sdk_dynamodb::types::ReturnValue::AllNew)
            .send()
            .await;
        match res {
            Ok(out) => {
                let count = out
                    .attributes
                    .as_ref()
                    .and_then(|m| m.get("deliveryAttemptCount"))
                    .and_then(|v| {
                        if let AttributeValue::N(n) = v {
                            n.parse::<i64>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                Ok(count.max(0) as u32)
            }
            Err(e) => {
                let err_dbg = format!("{e:?}");
                if err_dbg.contains("ConditionalCheckFailed") {
                    Err(A2aStorageError::StaleDeliveryClaim {
                        tenant: tenant.to_string(),
                        task_id: task_id.to_string(),
                        event_sequence,
                        config_id: config_id.to_string(),
                    })
                } else {
                    Err(A2aStorageError::DatabaseError(err_dbg))
                }
            }
        }
    }

    async fn record_delivery_outcome(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        config_id: &str,
        claimant: &str,
        claim_generation: u64,
        outcome: DeliveryOutcome,
    ) -> Result<(), A2aStorageError> {
        let now_micros = ddb_systime_to_micros(std::time::SystemTime::now());
        let pk = Self::push_delivery_pk(tenant, task_id, event_sequence);
        let sk = config_id.to_string();

        // Idempotency requires reading the current status; do that up
        // front so we can short-circuit on an already-matching terminal.
        let existing = self
            .client
            .get_item()
            .table_name(&self.config.push_deliveries_table)
            .key("pk", AttributeValue::S(pk.clone()))
            .key("sk", AttributeValue::S(sk.clone()))
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
        let cur_status = existing
            .item
            .as_ref()
            .and_then(|m| m.get("status"))
            .and_then(|v| {
                if let AttributeValue::S(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .and_then(|s| ddb_claim_status_from_str(s).ok());

        // Terminal-frozen gate: once Succeeded/GaveUp/Abandoned,
        // further outcomes are silent no-ops regardless of which
        // outcome the worker is trying to record.
        if matches!(
            cur_status,
            Some(ClaimStatus::Succeeded) | Some(ClaimStatus::GaveUp) | Some(ClaimStatus::Abandoned)
        ) {
            return Ok(());
        }

        // Non-terminal-path UpdateItem: fenced on identity AND
        // still-non-terminal status. The status guard protects
        // against a race where the read above saw non-terminal but
        // a concurrent commit landed a terminal before our
        // UpdateItem; the condition fails and we map to
        // StaleDeliveryClaim, leaving the terminal intact.
        let builder = self
            .client
            .update_item()
            .table_name(&self.config.push_deliveries_table)
            .key("pk", AttributeValue::S(pk))
            .key("sk", AttributeValue::S(sk))
            .condition_expression(
                "claimant = :claimant AND generation = :gen \
                 AND (#status = :pending OR #status = :attempting)",
            )
            .expression_attribute_names("#status", "status")
            .expression_attribute_values(":claimant", AttributeValue::S(claimant.to_string()))
            .expression_attribute_values(":gen", AttributeValue::N(claim_generation.to_string()))
            .expression_attribute_values(":pending", AttributeValue::S("Pending".into()))
            .expression_attribute_values(":attempting", AttributeValue::S("Attempting".into()));

        let res = match outcome {
            DeliveryOutcome::Succeeded { http_status } => {
                builder
                    .update_expression(
                        "SET #status = :st, lastHttpStatus = :hs, \
                         firstAttemptedAtMicros = if_not_exists(firstAttemptedAtMicros, :now), \
                         lastAttemptedAtMicros = :now",
                    )
                    .expression_attribute_names("#status", "status")
                    .expression_attribute_values(":st", AttributeValue::S("Succeeded".into()))
                    .expression_attribute_values(":hs", AttributeValue::N(http_status.to_string()))
                    .expression_attribute_values(":now", AttributeValue::N(now_micros.to_string()))
                    .send()
                    .await
            }
            DeliveryOutcome::Retry {
                http_status,
                error_class,
                next_attempt_at: _,
            } => {
                let mut b = builder
                    .update_expression(
                        "SET #status = :st, lastErrorClass = :ec, lastAttemptedAtMicros = :now",
                    )
                    .expression_attribute_names("#status", "status")
                    .expression_attribute_values(":st", AttributeValue::S("Attempting".into()))
                    .expression_attribute_values(
                        ":ec",
                        AttributeValue::S(ddb_error_class_to_json(error_class)),
                    )
                    .expression_attribute_values(":now", AttributeValue::N(now_micros.to_string()));
                if let Some(hs) = http_status {
                    b = b
                        .update_expression(
                            "SET #status = :st, lastErrorClass = :ec, lastAttemptedAtMicros = :now, lastHttpStatus = :hs",
                        )
                        .expression_attribute_values(":hs", AttributeValue::N(hs.to_string()));
                }
                b.send().await
            }
            DeliveryOutcome::GaveUp {
                reason,
                last_error_class,
                last_http_status,
            } => {
                if matches!(cur_status, Some(ClaimStatus::GaveUp)) {
                    return Ok(());
                }
                let mut b = builder
                    .update_expression(
                        "SET #status = :st, gaveUpAtMicros = :now, \
                         gaveUpReason = :gr, lastErrorClass = :ec, \
                         firstAttemptedAtMicros = if_not_exists(firstAttemptedAtMicros, :claimedGuard), \
                         lastAttemptedAtMicros = if_not_exists(lastAttemptedAtMicros, :now)",
                    )
                    .expression_attribute_names("#status", "status")
                    .expression_attribute_values(":st", AttributeValue::S("GaveUp".into()))
                    .expression_attribute_values(":now", AttributeValue::N(now_micros.to_string()))
                    .expression_attribute_values(
                        ":gr",
                        AttributeValue::S(ddb_gave_up_reason_to_str(reason).into()),
                    )
                    .expression_attribute_values(
                        ":ec",
                        AttributeValue::S(ddb_error_class_to_json(last_error_class)),
                    )
                    .expression_attribute_values(
                        ":claimedGuard",
                        AttributeValue::N(now_micros.to_string()),
                    );
                if let Some(hs) = last_http_status {
                    b = b
                        .update_expression(
                            "SET #status = :st, gaveUpAtMicros = :now, \
                             gaveUpReason = :gr, lastErrorClass = :ec, lastHttpStatus = :hs, \
                             firstAttemptedAtMicros = if_not_exists(firstAttemptedAtMicros, :claimedGuard), \
                             lastAttemptedAtMicros = if_not_exists(lastAttemptedAtMicros, :now)",
                        )
                        .expression_attribute_values(":hs", AttributeValue::N(hs.to_string()));
                }
                b.send().await
            }
            DeliveryOutcome::Abandoned { reason } => {
                if matches!(cur_status, Some(ClaimStatus::Abandoned)) {
                    return Ok(());
                }
                builder
                    .update_expression("SET #status = :st, abandonedReason = :ar")
                    .expression_attribute_names("#status", "status")
                    .expression_attribute_values(":st", AttributeValue::S("Abandoned".into()))
                    .expression_attribute_values(
                        ":ar",
                        AttributeValue::S(ddb_abandoned_reason_to_str(reason).into()),
                    )
                    .send()
                    .await
            }
        };

        match res {
            Ok(_) => {
                // Suppress unused-helper warnings for conversion utilities.
                let _ = ddb_claim_status_to_str(ClaimStatus::Pending);
                Ok(())
            }
            Err(e) => {
                let err_dbg = format!("{e:?}");
                if err_dbg.contains("ConditionalCheckFailed") {
                    Err(A2aStorageError::StaleDeliveryClaim {
                        tenant: tenant.to_string(),
                        task_id: task_id.to_string(),
                        event_sequence,
                        config_id: config_id.to_string(),
                    })
                } else {
                    Err(A2aStorageError::DatabaseError(err_dbg))
                }
            }
        }
    }

    async fn sweep_expired_claims(&self) -> Result<u64, A2aStorageError> {
        // Table-level Scan with FilterExpression. This is coarse; a
        // production deployment that needs more efficient sweep
        // should add a GSI on (expiresAtMicros, status). For ADR-011
        // correctness, the contract is the return count — not
        // throughput.
        let now_micros = ddb_systime_to_micros(std::time::SystemTime::now());
        let mut count: u64 = 0;
        let mut last_key = None;
        loop {
            let mut scan = self
                .client
                .scan()
                .table_name(&self.config.push_deliveries_table)
                .filter_expression(
                    "expiresAtMicros < :now AND (#status = :pending OR #status = :attempting)",
                )
                .expression_attribute_names("#status", "status")
                .expression_attribute_values(":now", AttributeValue::N(now_micros.to_string()))
                .expression_attribute_values(":pending", AttributeValue::S("Pending".into()))
                .expression_attribute_values(":attempting", AttributeValue::S("Attempting".into()));
            if let Some(k) = last_key.take() {
                scan = scan.set_exclusive_start_key(Some(k));
            }
            let out = scan
                .send()
                .await
                .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
            count += out.count as u64;
            last_key = out.last_evaluated_key;
            if last_key.is_none() {
                break;
            }
        }
        Ok(count)
    }

    async fn list_reclaimable_claims(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::push::claim::ReclaimableClaim>, A2aStorageError> {
        // Same coarse scan as `sweep_expired_claims`, but carrying
        // enough identity on each row to drive a redispatch. The
        // sweep loop bounds load by passing a `limit` and iterating
        // tick by tick.
        let now_micros = ddb_systime_to_micros(std::time::SystemTime::now());
        let mut out: Vec<crate::push::claim::ReclaimableClaim> = Vec::new();
        let mut last_key = None;
        while out.len() < limit {
            let mut scan = self
                .client
                .scan()
                .table_name(&self.config.push_deliveries_table)
                .filter_expression(
                    "expiresAtMicros < :now AND (#status = :pending OR #status = :attempting)",
                )
                .expression_attribute_names("#status", "status")
                .expression_attribute_values(":now", AttributeValue::N(now_micros.to_string()))
                .expression_attribute_values(":pending", AttributeValue::S("Pending".into()))
                .expression_attribute_values(":attempting", AttributeValue::S("Attempting".into()));
            if let Some(k) = last_key.take() {
                scan = scan.set_exclusive_start_key(Some(k));
            }
            let page = scan
                .send()
                .await
                .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
            if let Some(items) = page.items {
                for item in items {
                    if out.len() >= limit {
                        break;
                    }
                    let tenant = item
                        .get("tenant")
                        .and_then(|v| {
                            if let AttributeValue::S(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    let owner = item
                        .get("owner")
                        .and_then(|v| {
                            if let AttributeValue::S(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    let task_id = item
                        .get("taskId")
                        .and_then(|v| {
                            if let AttributeValue::S(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    let event_sequence = item
                        .get("eventSequence")
                        .and_then(|v| {
                            if let AttributeValue::N(n) = v {
                                n.parse::<u64>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    let config_id = item
                        .get("configId")
                        .and_then(|v| {
                            if let AttributeValue::S(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    out.push(crate::push::claim::ReclaimableClaim {
                        tenant,
                        owner,
                        task_id,
                        event_sequence,
                        config_id,
                    });
                }
            }
            last_key = page.last_evaluated_key;
            if last_key.is_none() {
                break;
            }
        }
        Ok(out)
    }

    async fn record_pending_dispatch(
        &self,
        tenant: &str,
        owner: &str,
        task_id: &str,
        event_sequence: u64,
    ) -> Result<(), A2aStorageError> {
        let now_micros = ddb_systime_to_micros(std::time::SystemTime::now());
        let pk = format!("{tenant}#{task_id}");
        let sk = event_sequence.to_string();
        // Unconditional PutItem — idempotent by key; overwrite
        // refreshes recorded_at so an in-progress redispatch isn't
        // treated as stale.
        let mut put = self
            .client
            .put_item()
            .table_name(&self.config.push_pending_dispatches_table)
            .item("pk", AttributeValue::S(pk))
            .item("sk", AttributeValue::S(sk))
            .item("tenant", AttributeValue::S(tenant.to_string()))
            .item("taskId", AttributeValue::S(task_id.to_string()))
            .item(
                "eventSequence",
                AttributeValue::N(event_sequence.to_string()),
            )
            .item("owner", AttributeValue::S(owner.to_string()))
            .item(
                "recordedAtMicros",
                AttributeValue::N(now_micros.to_string()),
            );
        if let Some(ttl) = Self::ttl_epoch(self.config.push_delivery_ttl_seconds) {
            put = put.item("ttl", ttl);
        }
        put.send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
        Ok(())
    }

    async fn delete_pending_dispatch(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
    ) -> Result<(), A2aStorageError> {
        let pk = format!("{tenant}#{task_id}");
        let sk = event_sequence.to_string();
        self.client
            .delete_item()
            .table_name(&self.config.push_pending_dispatches_table)
            .key("pk", AttributeValue::S(pk))
            .key("sk", AttributeValue::S(sk))
            .send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
        Ok(())
    }

    async fn list_stale_pending_dispatches(
        &self,
        older_than_recorded_at: std::time::SystemTime,
        limit: usize,
    ) -> Result<Vec<crate::push::claim::PendingDispatch>, A2aStorageError> {
        let cutoff_micros = ddb_systime_to_micros(older_than_recorded_at);
        let mut out: Vec<crate::push::claim::PendingDispatch> = Vec::new();
        let mut last_key = None;
        while out.len() < limit {
            let mut scan = self
                .client
                .scan()
                .table_name(&self.config.push_pending_dispatches_table)
                .filter_expression("recordedAtMicros <= :cutoff")
                .expression_attribute_values(
                    ":cutoff",
                    AttributeValue::N(cutoff_micros.to_string()),
                );
            if let Some(k) = last_key.take() {
                scan = scan.set_exclusive_start_key(Some(k));
            }
            let page = scan
                .send()
                .await
                .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
            if let Some(items) = page.items {
                for item in items {
                    if out.len() >= limit {
                        break;
                    }
                    let tenant = item
                        .get("tenant")
                        .and_then(|v| {
                            if let AttributeValue::S(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    let owner = item
                        .get("owner")
                        .and_then(|v| {
                            if let AttributeValue::S(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    let task_id = item
                        .get("taskId")
                        .and_then(|v| {
                            if let AttributeValue::S(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    let event_sequence = item
                        .get("eventSequence")
                        .and_then(|v| {
                            if let AttributeValue::N(n) = v {
                                n.parse::<u64>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    let recorded_at_micros = item
                        .get("recordedAtMicros")
                        .and_then(|v| {
                            if let AttributeValue::N(n) = v {
                                n.parse::<i64>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    out.push(crate::push::claim::PendingDispatch {
                        tenant,
                        owner,
                        task_id,
                        event_sequence,
                        recorded_at: ddb_micros_to_systime(recorded_at_micros),
                    });
                }
            }
            last_key = page.last_evaluated_key;
            if last_key.is_none() {
                break;
            }
        }
        Ok(out)
    }

    async fn list_failed_deliveries(
        &self,
        tenant: &str,
        since: std::time::SystemTime,
        limit: usize,
    ) -> Result<Vec<FailedDelivery>, A2aStorageError> {
        // Table-level Scan filtered by tenant + GaveUp + since. Same
        // scalability note as sweep.
        let since_micros = ddb_systime_to_micros(since);
        let mut rows: Vec<FailedDelivery> = Vec::new();
        let mut last_key = None;
        loop {
            let mut scan = self
                .client
                .scan()
                .table_name(&self.config.push_deliveries_table)
                .filter_expression("tenant = :t AND #status = :st AND gaveUpAtMicros >= :since")
                .expression_attribute_names("#status", "status")
                .expression_attribute_values(":t", AttributeValue::S(tenant.to_string()))
                .expression_attribute_values(":st", AttributeValue::S("GaveUp".into()))
                .expression_attribute_values(":since", AttributeValue::N(since_micros.to_string()));
            if let Some(k) = last_key.take() {
                scan = scan.set_exclusive_start_key(Some(k));
            }
            let out = scan
                .send()
                .await
                .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
            if let Some(items) = out.items {
                for item in items {
                    let task_id = item
                        .get("taskId")
                        .and_then(|v| {
                            if let AttributeValue::S(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    let config_id = item
                        .get("configId")
                        .and_then(|v| {
                            if let AttributeValue::S(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    let event_sequence = item
                        .get("eventSequence")
                        .and_then(|v| {
                            if let AttributeValue::N(n) = v {
                                n.parse::<u64>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    let claimed = item
                        .get("claimedAtMicros")
                        .and_then(|v| {
                            if let AttributeValue::N(n) = v {
                                n.parse::<i64>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    let first = item.get("firstAttemptedAtMicros").and_then(|v| {
                        if let AttributeValue::N(n) = v {
                            n.parse::<i64>().ok()
                        } else {
                            None
                        }
                    });
                    let last = item.get("lastAttemptedAtMicros").and_then(|v| {
                        if let AttributeValue::N(n) = v {
                            n.parse::<i64>().ok()
                        } else {
                            None
                        }
                    });
                    let gave_up = item
                        .get("gaveUpAtMicros")
                        .and_then(|v| {
                            if let AttributeValue::N(n) = v {
                                n.parse::<i64>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(claimed);
                    let attempt_count = item
                        .get("deliveryAttemptCount")
                        .and_then(|v| {
                            if let AttributeValue::N(n) = v {
                                n.parse::<i64>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    let http_status = item.get("lastHttpStatus").and_then(|v| {
                        if let AttributeValue::N(n) = v {
                            n.parse::<u16>().ok()
                        } else {
                            None
                        }
                    });
                    let err_class_s = item.get("lastErrorClass").and_then(|v| {
                        if let AttributeValue::S(s) = v {
                            Some(s.as_str())
                        } else {
                            None
                        }
                    });
                    let last_error_class = err_class_s
                        .and_then(ddb_error_class_from_json)
                        .unwrap_or(DeliveryErrorClass::NetworkError);
                    rows.push(FailedDelivery {
                        task_id,
                        config_id,
                        event_sequence,
                        first_attempted_at: ddb_micros_to_systime(first.unwrap_or(claimed)),
                        last_attempted_at: ddb_micros_to_systime(last.unwrap_or(gave_up)),
                        gave_up_at: ddb_micros_to_systime(gave_up),
                        delivery_attempt_count: attempt_count.max(0) as u32,
                        last_http_status: http_status,
                        last_error_class,
                    });
                }
            }
            last_key = out.last_evaluated_key;
            if last_key.is_none() {
                break;
            }
        }
        rows.sort_by(|a, b| b.gave_up_at.cmp(&a.gave_up_at));
        rows.truncate(limit);
        Ok(rows)
    }
}

/// DynamoDB parity tests require AWS credentials and DynamoDB access.
///
/// Run sequentially (DynamoDB table creation is throttled):
///   cargo test --features dynamodb -p turul-a2a --lib -- storage::dynamodb --test-threads=1
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::parity_tests;
    use aws_sdk_dynamodb::types::{
        AttributeDefinition, KeySchemaElement, KeyType, ScalarAttributeType,
    };

    async fn create_test_table(client: &Client, table_name: &str) {
        // Create the table (ignore if already exists)
        let result = client
            .create_table()
            .table_name(table_name)
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("pk")
                    .key_type(KeyType::Hash)
                    .build()
                    .unwrap(),
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("pk")
                    .attribute_type(ScalarAttributeType::S)
                    .build()
                    .unwrap(),
            )
            .billing_mode(aws_sdk_dynamodb::types::BillingMode::PayPerRequest)
            .send()
            .await;

        if let Err(e) = &result {
            let err_debug = format!("{e:?}");
            // Table already exists is fine
            if !err_debug.contains("ResourceInUse") && !err_debug.contains("Table already exists") {
                panic!("Failed to create table {table_name}: {err_debug}");
            }
        }

        // Wait for table to become ACTIVE
        for _ in 0..30 {
            let desc = client.describe_table().table_name(table_name).send().await;
            if let Ok(resp) = desc {
                if let Some(table) = resp.table {
                    if table.table_status == Some(aws_sdk_dynamodb::types::TableStatus::Active) {
                        return;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        panic!("Table {table_name} did not become ACTIVE in time");
    }

    const TEST_TASKS_TABLE: &str = "test_a2a_tasks";
    const TEST_PUSH_TABLE: &str = "test_a2a_push_configs";
    const TEST_EVENTS_TABLE: &str = "test_a2a_task_events";
    const TEST_PUSH_DELIVERIES_TABLE: &str = "test_a2a_push_deliveries";

    /// Create the push-deliveries table: `pk` HASH (String) + `sk`
    /// RANGE (String). Matches the shape used by the production
    /// adapter (`{tenant}#{task_id}#{event_sequence}` as `pk`,
    /// `config_id` as `sk`).
    async fn create_push_deliveries_table(client: &Client, table_name: &str) {
        let result = client
            .create_table()
            .table_name(table_name)
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("pk")
                    .key_type(KeyType::Hash)
                    .build()
                    .unwrap(),
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("sk")
                    .key_type(KeyType::Range)
                    .build()
                    .unwrap(),
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("pk")
                    .attribute_type(ScalarAttributeType::S)
                    .build()
                    .unwrap(),
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("sk")
                    .attribute_type(ScalarAttributeType::S)
                    .build()
                    .unwrap(),
            )
            .billing_mode(aws_sdk_dynamodb::types::BillingMode::PayPerRequest)
            .send()
            .await;
        if let Err(e) = &result {
            let err_debug = format!("{e:?}");
            if !err_debug.contains("ResourceInUse") && !err_debug.contains("Table already exists") {
                panic!("Failed to create push-deliveries table {table_name}: {err_debug}");
            }
        }
        for _ in 0..30 {
            let desc = client.describe_table().table_name(table_name).send().await;
            if let Ok(resp) = desc {
                if let Some(table) = resp.table {
                    if table.table_status == Some(aws_sdk_dynamodb::types::TableStatus::Active) {
                        return;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        panic!("Push-deliveries table {table_name} did not become ACTIVE in time");
    }

    /// Create the events table with `pk` (HASH) + `sk` (RANGE),
    /// matching the production schema. `eventSequence` is carried as
    /// a numeric attribute on each item for deserialisation; `sk` is
    /// the zero-padded decimal form used as the DynamoDB range key
    /// so lexicographic order matches numeric event order.
    async fn create_events_table(client: &Client, table_name: &str) {
        let result = client
            .create_table()
            .table_name(table_name)
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("pk")
                    .key_type(KeyType::Hash)
                    .build()
                    .unwrap(),
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("sk")
                    .key_type(KeyType::Range)
                    .build()
                    .unwrap(),
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("pk")
                    .attribute_type(ScalarAttributeType::S)
                    .build()
                    .unwrap(),
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("sk")
                    .attribute_type(ScalarAttributeType::S)
                    .build()
                    .unwrap(),
            )
            .billing_mode(aws_sdk_dynamodb::types::BillingMode::PayPerRequest)
            .send()
            .await;

        if let Err(e) = &result {
            let err_debug = format!("{e:?}");
            if !err_debug.contains("ResourceInUse") && !err_debug.contains("Table already exists") {
                panic!("Failed to create events table {table_name}: {err_debug}");
            }
        }

        // Wait for table to become ACTIVE
        for _ in 0..30 {
            let desc = client.describe_table().table_name(table_name).send().await;
            if let Ok(resp) = desc {
                if let Some(table) = resp.table {
                    if table.table_status == Some(aws_sdk_dynamodb::types::TableStatus::Active) {
                        return;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        panic!("Events table {table_name} did not become ACTIVE in time");
    }

    /// Live DynamoDB tests are opt-in — they require either Local
    /// DynamoDB or real DynamoDB access. The following environment
    /// variables gate and configure them:
    ///
    /// - `A2A_DYNAMODB_TESTS=1` — required; anything else skips the
    ///   live tests gracefully (they print a skip note and return
    ///   `Ok`).
    /// - `A2A_DYNAMODB_ENDPOINT=http://localhost:8000` — optional
    ///   endpoint override for Local DynamoDB (dynamodb-local). When
    ///   unset, the standard AWS SDK endpoint resolution applies.
    /// - Test table names are the constants `TEST_TASKS_TABLE`,
    ///   `TEST_PUSH_TABLE`, `TEST_EVENTS_TABLE`. Each test gets a
    ///   unique `tenant_prefix` so parallel test runs do not
    ///   interfere in the same table.
    ///
    /// Returns `None` when the gate env var is unset, which the
    /// per-test macro `skip_unless_dynamodb_env!()` turns into an
    /// early return.
    async fn storage() -> Option<TestStorage> {
        if std::env::var("A2A_DYNAMODB_TESTS").ok().as_deref() != Some("1") {
            return None;
        }

        static TABLES_CREATED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);

        let loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        let loader = match std::env::var("A2A_DYNAMODB_ENDPOINT") {
            Ok(ep) if !ep.is_empty() => loader.endpoint_url(ep),
            _ => loader,
        };
        let config = loader.load().await;
        let client = Client::new(&config);

        // Create tables once (idempotent)
        if !TABLES_CREATED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            create_test_table(&client, TEST_TASKS_TABLE).await;
            create_test_table(&client, TEST_PUSH_TABLE).await;
            create_events_table(&client, TEST_EVENTS_TABLE).await;
            create_push_deliveries_table(&client, TEST_PUSH_DELIVERIES_TABLE).await;
        }

        let storage = DynamoDbA2aStorage::from_client(
            client,
            DynamoDbConfig {
                tasks_table: TEST_TASKS_TABLE.into(),
                push_configs_table: TEST_PUSH_TABLE.into(),
                events_table: TEST_EVENTS_TABLE.into(),
                push_deliveries_table: TEST_PUSH_DELIVERIES_TABLE.into(),
                ..DynamoDbConfig::default()
            },
        );

        Some(TestStorage {
            inner: storage,
            tenant_prefix: uuid::Uuid::now_v7().to_string(),
        })
    }

    /// Early-return a test when the live DynamoDB env is not
    /// configured. Prints a clear skip note so the reason is visible
    /// in CI logs.
    macro_rules! skip_unless_dynamodb_env {
        ($storage_binding:ident) => {
            let $storage_binding = match storage().await {
                Some(s) => s,
                None => {
                    eprintln!(
                        "SKIP dynamodb live test: set A2A_DYNAMODB_TESTS=1 \
                         (and optionally A2A_DYNAMODB_ENDPOINT=http://localhost:8000 \
                         for Local DynamoDB) to enable."
                    );
                    return;
                }
            };
        };
    }

    /// Wrapper that prefixes tenant on all calls for test isolation.
    /// The parity tests pass tenant strings like "default", "tenant-a", etc.
    /// This wrapper makes them unique per test instance.
    struct TestStorage {
        inner: DynamoDbA2aStorage,
        tenant_prefix: String,
    }

    impl TestStorage {
        fn scoped_tenant(&self, tenant: &str) -> String {
            format!("{}_{}", self.tenant_prefix, tenant)
        }

        fn with_push_dispatch_enabled(mut self, enabled: bool) -> Self {
            self.inner = self.inner.with_push_dispatch_enabled(enabled);
            self
        }
    }

    #[async_trait]
    impl A2aTaskStorage for TestStorage {
        fn backend_name(&self) -> &'static str {
            "dynamodb-test"
        }

        async fn create_task(
            &self,
            tenant: &str,
            owner: &str,
            task: Task,
        ) -> Result<Task, A2aStorageError> {
            self.inner
                .create_task(&self.scoped_tenant(tenant), owner, task)
                .await
        }

        async fn get_task(
            &self,
            tenant: &str,
            task_id: &str,
            owner: &str,
            history_length: Option<i32>,
        ) -> Result<Option<Task>, A2aStorageError> {
            self.inner
                .get_task(&self.scoped_tenant(tenant), task_id, owner, history_length)
                .await
        }

        async fn update_task(
            &self,
            tenant: &str,
            owner: &str,
            task: Task,
        ) -> Result<(), A2aStorageError> {
            self.inner
                .update_task(&self.scoped_tenant(tenant), owner, task)
                .await
        }

        async fn delete_task(
            &self,
            tenant: &str,
            task_id: &str,
            owner: &str,
        ) -> Result<bool, A2aStorageError> {
            self.inner
                .delete_task(&self.scoped_tenant(tenant), task_id, owner)
                .await
        }

        async fn list_tasks(
            &self,
            mut filter: TaskFilter,
        ) -> Result<TaskListPage, A2aStorageError> {
            filter.tenant = filter.tenant.map(|t| self.scoped_tenant(&t));
            self.inner.list_tasks(filter).await
        }

        async fn update_task_status(
            &self,
            tenant: &str,
            task_id: &str,
            owner: &str,
            new_status: TaskStatus,
        ) -> Result<Task, A2aStorageError> {
            self.inner
                .update_task_status(&self.scoped_tenant(tenant), task_id, owner, new_status)
                .await
        }

        async fn append_message(
            &self,
            tenant: &str,
            task_id: &str,
            owner: &str,
            message: Message,
        ) -> Result<(), A2aStorageError> {
            self.inner
                .append_message(&self.scoped_tenant(tenant), task_id, owner, message)
                .await
        }

        async fn append_artifact(
            &self,
            tenant: &str,
            task_id: &str,
            owner: &str,
            artifact: Artifact,
            append: bool,
            last_chunk: bool,
        ) -> Result<(), A2aStorageError> {
            self.inner
                .append_artifact(
                    &self.scoped_tenant(tenant),
                    task_id,
                    owner,
                    artifact,
                    append,
                    last_chunk,
                )
                .await
        }

        async fn task_count(&self) -> Result<usize, A2aStorageError> {
            // Count only this test's tasks by filtering on tenant prefix
            let result = self
                .inner
                .client
                .scan()
                .table_name(&self.inner.config.tasks_table)
                .filter_expression("begins_with(pk, :prefix)")
                .expression_attribute_values(
                    ":prefix",
                    AttributeValue::S(format!("{}_", self.tenant_prefix)),
                )
                .select(aws_sdk_dynamodb::types::Select::Count)
                .send()
                .await
                .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;
            Ok(result.count as usize)
        }

        async fn maintenance(&self) -> Result<(), A2aStorageError> {
            self.inner.maintenance().await
        }

        async fn set_cancel_requested(
            &self,
            tenant: &str,
            task_id: &str,
            owner: &str,
        ) -> Result<(), A2aStorageError> {
            self.inner
                .set_cancel_requested(&self.scoped_tenant(tenant), task_id, owner)
                .await
        }
    }

    #[async_trait]
    impl crate::storage::A2aCancellationSupervisor for TestStorage {
        fn backend_name(&self) -> &'static str {
            "dynamodb-test"
        }

        async fn supervisor_get_cancel_requested(
            &self,
            tenant: &str,
            task_id: &str,
        ) -> Result<bool, A2aStorageError> {
            <DynamoDbA2aStorage as crate::storage::A2aCancellationSupervisor>::supervisor_get_cancel_requested(
                &self.inner, &self.scoped_tenant(tenant), task_id,
            ).await
        }

        async fn supervisor_list_cancel_requested(
            &self,
            tenant: &str,
            task_ids: &[String],
        ) -> Result<Vec<String>, A2aStorageError> {
            <DynamoDbA2aStorage as crate::storage::A2aCancellationSupervisor>::supervisor_list_cancel_requested(
                &self.inner, &self.scoped_tenant(tenant), task_ids,
            ).await
        }
    }

    #[async_trait]
    impl A2aPushNotificationStorage for TestStorage {
        fn backend_name(&self) -> &'static str {
            "dynamodb-test"
        }

        async fn create_config(
            &self,
            tenant: &str,
            config: turul_a2a_proto::TaskPushNotificationConfig,
        ) -> Result<turul_a2a_proto::TaskPushNotificationConfig, A2aStorageError> {
            self.inner
                .create_config(&self.scoped_tenant(tenant), config)
                .await
        }

        async fn get_config(
            &self,
            tenant: &str,
            task_id: &str,
            config_id: &str,
        ) -> Result<Option<turul_a2a_proto::TaskPushNotificationConfig>, A2aStorageError> {
            self.inner
                .get_config(&self.scoped_tenant(tenant), task_id, config_id)
                .await
        }

        async fn list_configs(
            &self,
            tenant: &str,
            task_id: &str,
            page_token: Option<&str>,
            page_size: Option<i32>,
        ) -> Result<PushConfigListPage, A2aStorageError> {
            self.inner
                .list_configs(&self.scoped_tenant(tenant), task_id, page_token, page_size)
                .await
        }

        async fn delete_config(
            &self,
            tenant: &str,
            task_id: &str,
            config_id: &str,
        ) -> Result<(), A2aStorageError> {
            self.inner
                .delete_config(&self.scoped_tenant(tenant), task_id, config_id)
                .await
        }

        async fn list_configs_eligible_at_event(
            &self,
            tenant: &str,
            task_id: &str,
            event_sequence: u64,
            page_token: Option<&str>,
            page_size: Option<i32>,
        ) -> Result<PushConfigListPage, A2aStorageError> {
            self.inner
                .list_configs_eligible_at_event(
                    &self.scoped_tenant(tenant),
                    task_id,
                    event_sequence,
                    page_token,
                    page_size,
                )
                .await
        }
    }

    #[async_trait]
    impl A2aEventStore for TestStorage {
        fn backend_name(&self) -> &'static str {
            "dynamodb-test"
        }

        async fn append_event(
            &self,
            tenant: &str,
            task_id: &str,
            event: StreamEvent,
        ) -> Result<u64, A2aStorageError> {
            self.inner
                .append_event(&self.scoped_tenant(tenant), task_id, event)
                .await
        }

        async fn get_events_after(
            &self,
            tenant: &str,
            task_id: &str,
            after_sequence: u64,
        ) -> Result<Vec<(u64, StreamEvent)>, A2aStorageError> {
            self.inner
                .get_events_after(&self.scoped_tenant(tenant), task_id, after_sequence)
                .await
        }

        async fn latest_sequence(
            &self,
            tenant: &str,
            task_id: &str,
        ) -> Result<u64, A2aStorageError> {
            self.inner
                .latest_sequence(&self.scoped_tenant(tenant), task_id)
                .await
        }

        async fn cleanup_expired(&self) -> Result<u64, A2aStorageError> {
            self.inner.cleanup_expired().await
        }
    }

    #[async_trait]
    impl A2aAtomicStore for TestStorage {
        fn backend_name(&self) -> &'static str {
            "dynamodb-test"
        }

        fn push_dispatch_enabled(&self) -> bool {
            self.inner.push_dispatch_enabled()
        }

        async fn create_task_with_events(
            &self,
            tenant: &str,
            owner: &str,
            task: Task,
            events: Vec<StreamEvent>,
        ) -> Result<(Task, Vec<u64>), A2aStorageError> {
            self.inner
                .create_task_with_events(&self.scoped_tenant(tenant), owner, task, events)
                .await
        }

        async fn update_task_status_with_events(
            &self,
            tenant: &str,
            task_id: &str,
            owner: &str,
            new_status: TaskStatus,
            events: Vec<StreamEvent>,
        ) -> Result<(Task, Vec<u64>), A2aStorageError> {
            self.inner
                .update_task_status_with_events(
                    &self.scoped_tenant(tenant),
                    task_id,
                    owner,
                    new_status,
                    events,
                )
                .await
        }

        async fn update_task_with_events(
            &self,
            tenant: &str,
            owner: &str,
            task: Task,
            events: Vec<StreamEvent>,
        ) -> Result<Vec<u64>, A2aStorageError> {
            self.inner
                .update_task_with_events(&self.scoped_tenant(tenant), owner, task, events)
                .await
        }
    }

    #[async_trait]
    impl A2aPushDeliveryStore for TestStorage {
        fn backend_name(&self) -> &'static str {
            "dynamodb-test"
        }

        async fn claim_delivery(
            &self,
            tenant: &str,
            task_id: &str,
            event_sequence: u64,
            config_id: &str,
            claimant: &str,
            owner: &str,
            claim_expiry: std::time::Duration,
        ) -> Result<DeliveryClaim, A2aStorageError> {
            self.inner
                .claim_delivery(
                    &self.scoped_tenant(tenant),
                    task_id,
                    event_sequence,
                    config_id,
                    claimant,
                    owner,
                    claim_expiry,
                )
                .await
        }

        async fn record_attempt_started(
            &self,
            tenant: &str,
            task_id: &str,
            event_sequence: u64,
            config_id: &str,
            claimant: &str,
            claim_generation: u64,
        ) -> Result<u32, A2aStorageError> {
            self.inner
                .record_attempt_started(
                    &self.scoped_tenant(tenant),
                    task_id,
                    event_sequence,
                    config_id,
                    claimant,
                    claim_generation,
                )
                .await
        }

        async fn record_delivery_outcome(
            &self,
            tenant: &str,
            task_id: &str,
            event_sequence: u64,
            config_id: &str,
            claimant: &str,
            claim_generation: u64,
            outcome: DeliveryOutcome,
        ) -> Result<(), A2aStorageError> {
            self.inner
                .record_delivery_outcome(
                    &self.scoped_tenant(tenant),
                    task_id,
                    event_sequence,
                    config_id,
                    claimant,
                    claim_generation,
                    outcome,
                )
                .await
        }

        async fn sweep_expired_claims(&self) -> Result<u64, A2aStorageError> {
            self.inner.sweep_expired_claims().await
        }

        async fn list_reclaimable_claims(
            &self,
            limit: usize,
        ) -> Result<Vec<crate::push::claim::ReclaimableClaim>, A2aStorageError> {
            self.inner.list_reclaimable_claims(limit).await
        }

        async fn record_pending_dispatch(
            &self,
            tenant: &str,
            owner: &str,
            task_id: &str,
            event_sequence: u64,
        ) -> Result<(), A2aStorageError> {
            self.inner
                .record_pending_dispatch(
                    &self.scoped_tenant(tenant),
                    owner,
                    task_id,
                    event_sequence,
                )
                .await
        }

        async fn delete_pending_dispatch(
            &self,
            tenant: &str,
            task_id: &str,
            event_sequence: u64,
        ) -> Result<(), A2aStorageError> {
            self.inner
                .delete_pending_dispatch(&self.scoped_tenant(tenant), task_id, event_sequence)
                .await
        }

        async fn list_stale_pending_dispatches(
            &self,
            older_than_recorded_at: std::time::SystemTime,
            limit: usize,
        ) -> Result<Vec<crate::push::claim::PendingDispatch>, A2aStorageError> {
            self.inner
                .list_stale_pending_dispatches(older_than_recorded_at, limit)
                .await
        }

        async fn list_failed_deliveries(
            &self,
            tenant: &str,
            since: std::time::SystemTime,
            limit: usize,
        ) -> Result<Vec<FailedDelivery>, A2aStorageError> {
            self.inner
                .list_failed_deliveries(&self.scoped_tenant(tenant), since, limit)
                .await
        }
    }

    #[tokio::test]
    async fn test_create_and_retrieve() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_create_and_retrieve(&s).await;
    }

    #[tokio::test]
    async fn test_state_machine_enforcement() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_state_machine_enforcement(&s).await;
    }

    #[tokio::test]
    async fn test_terminal_state_rejection() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_terminal_state_rejection(&s).await;
    }

    #[tokio::test]
    async fn test_tenant_isolation() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_tenant_isolation(&s).await;
    }

    #[tokio::test]
    async fn test_owner_isolation() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_owner_isolation(&s).await;
    }

    #[tokio::test]
    async fn test_history_length() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_history_length(&s).await;
    }

    #[tokio::test]
    async fn test_list_pagination() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_list_pagination(&s).await;
    }

    #[tokio::test]
    async fn test_list_filter_by_status() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_list_filter_by_status(&s).await;
    }

    #[tokio::test]
    async fn test_list_filter_by_context_id() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_list_filter_by_context_id(&s).await;
    }

    #[tokio::test]
    async fn test_append_message() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_append_message(&s).await;
    }

    #[tokio::test]
    async fn test_append_artifact() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_append_artifact(&s).await;
    }

    #[tokio::test]
    async fn test_task_count() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_task_count(&s).await;
    }

    #[tokio::test]
    async fn test_owner_isolation_mutations() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_owner_isolation_mutations(&s).await;
    }

    #[tokio::test]
    async fn test_artifact_chunk_semantics() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_artifact_chunk_semantics(&s).await;
    }

    #[tokio::test]
    async fn test_push_config_crud() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_config_crud(&s).await;
    }

    #[tokio::test]
    async fn test_push_config_idempotent_delete() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_config_idempotent_delete(&s).await;
    }

    #[tokio::test]
    async fn test_push_config_tenant_isolation() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_config_tenant_isolation(&s).await;
    }

    #[tokio::test]
    async fn test_push_config_list_pagination() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_config_list_pagination(&s).await;
    }

    // Event store parity tests

    #[tokio::test]
    async fn test_event_append_and_retrieve() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_event_append_and_retrieve(&s).await;
    }

    #[tokio::test]
    async fn test_event_monotonic_ordering() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_event_monotonic_ordering(&s).await;
    }

    #[tokio::test]
    async fn test_event_per_task_isolation() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_event_per_task_isolation(&s).await;
    }

    #[tokio::test]
    async fn test_event_tenant_isolation() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_event_tenant_isolation(&s).await;
    }

    #[tokio::test]
    async fn test_event_latest_sequence() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_event_latest_sequence(&s).await;
    }

    #[tokio::test]
    async fn test_event_empty_task() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_event_empty_task(&s).await;
    }

    // Atomic store parity tests

    #[tokio::test]
    async fn test_atomic_create_task_with_events() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_atomic_create_task_with_events(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_update_status_with_events() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_atomic_update_status_with_events(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_status_rejects_invalid_transition() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_atomic_status_rejects_invalid_transition(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_update_task_with_events() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_atomic_update_task_with_events(&s, &s, &s).await;
    }

    // Terminal-write CAS (ADR-010 §7.1) parity tests.

    #[tokio::test]
    async fn test_terminal_cas_single_winner_on_concurrent_terminals() {
        skip_unless_dynamodb_env!(s);
        let s = std::sync::Arc::new(s);
        parity_tests::test_terminal_cas_single_winner_on_concurrent_terminals(
            s.clone(),
            s.clone(),
            s,
        )
        .await;
    }

    #[tokio::test]
    async fn test_terminal_cas_single_winner_from_submitted_includes_rejected() {
        skip_unless_dynamodb_env!(s);
        let s = std::sync::Arc::new(s);
        parity_tests::test_terminal_cas_single_winner_from_submitted_includes_rejected(
            s.clone(),
            s.clone(),
            s,
        )
        .await;
    }

    #[tokio::test]
    async fn test_terminal_cas_rejects_sequential_second_terminal() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_terminal_cas_rejects_sequential_second_terminal(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_update_task_with_events_rejects_terminal_already_set() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_update_task_with_events_rejects_terminal_already_set(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_invalid_transition_distinct_from_terminal_already_set() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_invalid_transition_distinct_from_terminal_already_set(&s, &s).await;
    }

    // Cancel-marker parity (ADR-012).

    #[tokio::test]
    async fn test_cancel_marker_roundtrip() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_cancel_marker_roundtrip(&s, &s).await;
    }

    #[tokio::test]
    async fn test_supervisor_list_cancel_requested_parity() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_supervisor_list_cancel_requested_parity(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_owner_isolation() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_atomic_owner_isolation(&s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_tenant_isolation() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_atomic_tenant_isolation(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_create_with_empty_events() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_atomic_create_with_empty_events(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_sequence_continuity() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_atomic_sequence_continuity(&s, &s).await;
    }

    // =========================================================
    // Pure unit tests — no runtime / no LocalDynamo required.
    //
    // These pin the DynamoDB-specific schema shape that the
    // `A2aEventStore` trait cannot express: every event row must
    // carry `pk`, `sk`, `eventSequence`, `eventData`. The four event
    // write paths (`append_event`, `create_task_with_events`,
    // `update_task_status_with_events`, `update_task_with_events`)
    // all route through [`DynamoDbA2aStorage::build_event_item`], so
    // testing the helper plus compiling the call sites is sufficient
    // proof of shape — the sites would fail to compile if they
    // stopped going through the helper.
    // =========================================================

    #[test]
    fn event_sort_key_is_twenty_digit_zero_padded_decimal() {
        assert_eq!(
            DynamoDbA2aStorage::event_sort_key(0),
            "00000000000000000000"
        );
        assert_eq!(
            DynamoDbA2aStorage::event_sort_key(1),
            "00000000000000000001"
        );
        assert_eq!(
            DynamoDbA2aStorage::event_sort_key(42),
            "00000000000000000042"
        );
        // 20 digits comfortably holds u64::MAX without width collapse.
        let max = DynamoDbA2aStorage::event_sort_key(u64::MAX);
        assert_eq!(max.len(), 20);
        assert_eq!(max, "18446744073709551615");
    }

    #[test]
    fn event_sort_key_lex_order_matches_numeric_order() {
        assert!(DynamoDbA2aStorage::event_sort_key(1) < DynamoDbA2aStorage::event_sort_key(2));
        // Digit-boundary checks: plain itoa mis-orders these
        // lexicographically ("9" > "10"). Zero-padding prevents that.
        assert!(DynamoDbA2aStorage::event_sort_key(9) < DynamoDbA2aStorage::event_sort_key(10));
        assert!(DynamoDbA2aStorage::event_sort_key(99) < DynamoDbA2aStorage::event_sort_key(100));
        assert!(
            DynamoDbA2aStorage::event_sort_key(999_999)
                < DynamoDbA2aStorage::event_sort_key(1_000_000)
        );
        let mut numeric: Vec<u64> = (0u64..200).collect();
        let mut lex: Vec<String> = numeric
            .iter()
            .copied()
            .map(DynamoDbA2aStorage::event_sort_key)
            .collect();
        lex.sort();
        numeric.sort();
        let numeric_as_keys: Vec<String> = numeric
            .into_iter()
            .map(DynamoDbA2aStorage::event_sort_key)
            .collect();
        assert_eq!(lex, numeric_as_keys);
    }

    /// Core shape assertion for the event put-item: every row has
    /// exactly the four required attributes (plus TTL when
    /// configured). Used across all write paths.
    fn assert_required_event_attributes(
        item: &std::collections::HashMap<String, AttributeValue>,
        expected_pk: &str,
        expected_seq: u64,
        expected_data: &str,
    ) {
        // pk — partition key
        match item.get("pk") {
            Some(AttributeValue::S(v)) => assert_eq!(v, expected_pk),
            other => panic!("missing/wrong pk attribute: {other:?}"),
        }
        // sk — range key, must be zero-padded 20-digit decimal of seq
        match item.get("sk") {
            Some(AttributeValue::S(v)) => {
                assert_eq!(v, &format!("{expected_seq:020}"));
                assert_eq!(v.len(), 20, "sk must be fixed-width 20 digits");
            }
            other => panic!("missing/wrong sk attribute: {other:?}"),
        }
        // eventSequence — numeric attribute carrying the logical seq
        match item.get("eventSequence") {
            Some(AttributeValue::N(v)) => {
                assert_eq!(v, &expected_seq.to_string());
            }
            other => panic!("missing/wrong eventSequence attribute: {other:?}"),
        }
        // eventData — serialized StreamEvent JSON
        match item.get("eventData") {
            Some(AttributeValue::S(v)) => assert_eq!(v, expected_data),
            other => panic!("missing/wrong eventData attribute: {other:?}"),
        }
    }

    #[test]
    fn build_event_item_has_required_attributes_for_append_event_path() {
        // `append_event` writes one event row with seq = max + 1,
        // no TTL when unconfigured.
        let item = DynamoDbA2aStorage::build_event_item(
            "tenant-a#task-1",
            7,
            "{\"statusUpdate\":{}}",
            None,
        );
        assert_required_event_attributes(&item, "tenant-a#task-1", 7, "{\"statusUpdate\":{}}");
        assert!(
            !item.contains_key("ttl"),
            "TTL must be absent when not configured"
        );
    }

    #[test]
    fn build_event_item_has_required_attributes_for_create_task_path() {
        // `create_task_with_events` writes events starting at seq = 1.
        let item = DynamoDbA2aStorage::build_event_item(
            "tenant-b#task-2",
            1,
            "{\"statusUpdate\":{\"state\":\"TASK_STATE_SUBMITTED\"}}",
            None,
        );
        assert_required_event_attributes(
            &item,
            "tenant-b#task-2",
            1,
            "{\"statusUpdate\":{\"state\":\"TASK_STATE_SUBMITTED\"}}",
        );
    }

    #[test]
    fn build_event_item_has_required_attributes_for_update_status_path() {
        // `update_task_status_with_events` writes events at seq =
        // max_seq + 1; the terminal-preservation CAS still includes
        // these rows. Numeric values near u32::MAX confirm the
        // encoding stays stable at high sequences.
        let big_seq = 4_000_000_000u64;
        let item = DynamoDbA2aStorage::build_event_item(
            "tenant-c#task-3",
            big_seq,
            "{\"statusUpdate\":{\"state\":\"TASK_STATE_COMPLETED\"}}",
            None,
        );
        assert_required_event_attributes(
            &item,
            "tenant-c#task-3",
            big_seq,
            "{\"statusUpdate\":{\"state\":\"TASK_STATE_COMPLETED\"}}",
        );
    }

    #[test]
    fn build_event_item_has_required_attributes_for_update_task_path() {
        // `update_task_with_events` writes artifact-update events;
        // sequence advancement follows the same contract.
        let item = DynamoDbA2aStorage::build_event_item(
            "tenant-d#task-4",
            42,
            "{\"artifactUpdate\":{\"append\":true}}",
            None,
        );
        assert_required_event_attributes(
            &item,
            "tenant-d#task-4",
            42,
            "{\"artifactUpdate\":{\"append\":true}}",
        );
    }

    #[test]
    fn build_event_item_includes_ttl_when_provided() {
        let ttl_attr = AttributeValue::N("1234567890".into());
        let item = DynamoDbA2aStorage::build_event_item("t#x", 1, "{}", Some(ttl_attr.clone()));
        assert_required_event_attributes(&item, "t#x", 1, "{}");
        match item.get("ttl") {
            Some(got) => assert_eq!(got, &ttl_attr),
            None => panic!("ttl attribute should be present when provided"),
        }
    }

    // =========================================================
    // A2aPushDeliveryStore parity (ADR-011 §10). Live-gated via
    // A2A_DYNAMODB_TESTS; see storage()'s docstring for env vars.
    // =========================================================

    #[tokio::test]
    async fn test_push_claim_is_exclusive() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_claim_is_exclusive(&s).await;
    }

    #[tokio::test]
    async fn test_push_claim_expired_is_reclaimable() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_claim_expired_is_reclaimable(&s).await;
    }

    #[tokio::test]
    async fn test_push_outcome_fenced_to_current_claim() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_outcome_fenced_to_current_claim(&s).await;
    }

    #[tokio::test]
    async fn test_push_claim_terminal_succeeded_blocks_reclaim() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_claim_terminal_succeeded_blocks_reclaim(&s).await;
    }

    #[tokio::test]
    async fn test_push_claim_terminal_gaveup_blocks_reclaim() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_claim_terminal_gaveup_blocks_reclaim(&s).await;
    }

    #[tokio::test]
    async fn test_push_claim_terminal_abandoned_blocks_reclaim_and_not_listed() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_claim_terminal_abandoned_blocks_reclaim_and_not_listed(&s).await;
    }

    #[tokio::test]
    async fn test_push_attempt_started_advances_count_and_status() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_attempt_started_advances_count_and_status(&s).await;
    }

    #[tokio::test]
    async fn test_push_attempt_started_is_fenced() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_attempt_started_is_fenced(&s).await;
    }

    #[tokio::test]
    async fn test_push_retry_outcome_keeps_claim_open() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_retry_outcome_keeps_claim_open(&s).await;
    }

    #[tokio::test]
    async fn test_push_outcome_idempotent_on_terminal() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_outcome_idempotent_on_terminal(&s).await;
    }

    #[tokio::test]
    async fn test_push_sweep_counts_expired_nonterminal_and_preserves_status() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_sweep_counts_expired_nonterminal_and_preserves_status(&s).await;
    }

    #[tokio::test]
    async fn test_push_list_reclaimable_filters_and_returns_identity() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_list_reclaimable_filters_and_returns_identity(&s).await;
    }

    #[tokio::test]
    async fn test_push_list_failed_filters_and_orders() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_list_failed_filters_and_orders(&s).await;
    }

    #[tokio::test]
    async fn test_push_list_failed_is_tenant_scoped() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_list_failed_is_tenant_scoped(&s).await;
    }

    #[tokio::test]
    async fn test_push_failed_delivery_diagnostics_roundtrip() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_failed_delivery_diagnostics_roundtrip(&s).await;
    }

    #[tokio::test]
    async fn test_push_attempt_started_rejected_after_terminal() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_attempt_started_rejected_after_terminal(&s).await;
    }

    #[tokio::test]
    async fn test_push_outcome_does_not_overwrite_terminal() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_outcome_does_not_overwrite_terminal(&s).await;
    }

    #[tokio::test]
    async fn test_push_concurrent_claim_race() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_push_concurrent_claim_race(std::sync::Arc::new(s)).await;
    }

    // Atomic pending-dispatch marker parity (ADR-013 §4.3 / §10.1).

    #[tokio::test]
    async fn test_atomic_marker_written_for_terminal_status() {
        skip_unless_dynamodb_env!(s);
        let s = s.with_push_dispatch_enabled(true);
        parity_tests::test_atomic_marker_written_for_terminal_status(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_marker_skipped_for_non_terminal_status() {
        skip_unless_dynamodb_env!(s);
        let s = s.with_push_dispatch_enabled(true);
        parity_tests::test_atomic_marker_skipped_for_non_terminal_status(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_marker_skipped_for_artifact_event() {
        skip_unless_dynamodb_env!(s);
        let s = s.with_push_dispatch_enabled(true);
        parity_tests::test_atomic_marker_skipped_for_artifact_event(&s, &s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_marker_absent_when_opt_in_off() {
        skip_unless_dynamodb_env!(s);
        // default: push_dispatch_enabled=false
        parity_tests::test_atomic_marker_absent_when_opt_in_off(&s, &s, &s).await;
    }

    // Causal-floor eligibility parity (ADR-013 §4.5 / §10.3 / §10.4).

    #[tokio::test]
    async fn test_config_registered_at_or_after_event_not_eligible() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_config_registered_at_or_after_event_not_eligible(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_late_create_config_stamps_advanced_sequence() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_late_create_config_stamps_advanced_sequence(&s, &s, &s).await;
    }
}
