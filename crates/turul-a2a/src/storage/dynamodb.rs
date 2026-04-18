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
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use turul_a2a_types::{Artifact, Message, Task, TaskState, TaskStatus};

use crate::streaming::StreamEvent;
use super::atomic::A2aAtomicStore;
use super::error::A2aStorageError;
use super::event_store::A2aEventStore;
use super::filter::{PushConfigListPage, TaskFilter, TaskListPage};
use super::traits::{A2aPushNotificationStorage, A2aTaskStorage};

/// DynamoDB storage configuration.
#[derive(Debug, Clone)]
pub struct DynamoDbConfig {
    pub tasks_table: String,
    pub push_configs_table: String,
    pub events_table: String,
    /// TTL for task items in seconds. 0 = no expiry. Default: 7 days (604800).
    pub task_ttl_seconds: u64,
    /// TTL for event items in seconds. 0 = no expiry. Default: 24 hours (86400).
    pub event_ttl_seconds: u64,
}

/// 7 days in seconds.
const DEFAULT_TASK_TTL: u64 = 7 * 24 * 3600;
/// 24 hours in seconds.
const DEFAULT_EVENT_TTL: u64 = 24 * 3600;

impl Default for DynamoDbConfig {
    fn default() -> Self {
        Self {
            tasks_table: "a2a_tasks".into(),
            push_configs_table: "a2a_push_configs".into(),
            events_table: "a2a_task_events".into(),
            task_ttl_seconds: DEFAULT_TASK_TTL,
            event_ttl_seconds: DEFAULT_EVENT_TTL,
        }
    }
}

/// DynamoDB-backed A2A storage.
#[derive(Clone)]
pub struct DynamoDbA2aStorage {
    client: Client,
    config: DynamoDbConfig,
}

impl DynamoDbA2aStorage {
    pub async fn new(config: DynamoDbConfig) -> Result<Self, A2aStorageError> {
        let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = Client::new(&aws_config);
        Ok(Self { client, config })
    }

    pub fn from_client(client: Client, config: DynamoDbConfig) -> Self {
        Self { client, config }
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
        item.insert(
            "eventSequence".into(),
            AttributeValue::N(seq.to_string()),
        );
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
        Some(AttributeValue::N((Self::now_epoch() + ttl_seconds).to_string()))
    }

    fn task_to_json(task: &Task) -> Result<String, A2aStorageError> {
        serde_json::to_string(task).map_err(|e| A2aStorageError::SerializationError(e.to_string()))
    }

    fn task_from_json(json: &str) -> Result<Task, A2aStorageError> {
        let proto: turul_a2a_proto::Task =
            serde_json::from_str(json).map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;
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
        Task::try_from(proto).unwrap_or_else(|_| Task::new("err", TaskStatus::new(TaskState::Failed)))
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

        let mut req = self.client
            .put_item()
            .table_name(&self.config.tasks_table)
            .item("pk", AttributeValue::S(pk))
            .item("tenant", AttributeValue::S(tenant.to_string()))
            .item("taskId", AttributeValue::S(task.id().to_string()))
            .item("owner", AttributeValue::S(owner.to_string()))
            .item("contextId", AttributeValue::S(task.context_id().to_string()))
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
            .item("contextId", AttributeValue::S(task.context_id().to_string()))
            .item("statusState", AttributeValue::S(state_str))
            .item("taskJson", AttributeValue::S(json))
            .condition_expression("attribute_exists(pk) AND #o = :expected_owner")
            .expression_attribute_names("#o", "owner")
            .expression_attribute_values(
                ":expected_owner",
                AttributeValue::S(owner.to_string()),
            );
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
                let task = item.get("taskJson")
                    .and_then(|v| v.as_s().ok())
                    .and_then(|json| Self::task_from_json(json).ok())?;
                let updated_at = item.get("updatedAt")
                    .and_then(|v| v.as_s().ok())
                    .cloned()
                    .unwrap_or_default();
                Some((task, updated_at))
            })
            .collect();
        tasks_with_times.sort_by(|(a_task, a_time), (b_task, b_time)| {
            b_time.cmp(a_time)
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
            tasks_with_times.get(end_idx - 1).map(|(t, updated_at)| {
                format!("{}|{}", updated_at, t.id())
            }).unwrap_or_default()
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
                turul_a2a_types::A2aTypeError::TerminalState(s) => A2aStorageError::TerminalState(s),
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
                            let state = t
                                .status()
                                .and_then(|s| s.state().ok());
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
                            Some("Completed") | Some("Failed") | Some("Canceled") | Some("Rejected")
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
        let pk = format!("{tenant}#{}#{}", config.task_id, config.id);
        let json = serde_json::to_string(&config)
            .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

        let mut req = self.client
            .put_item()
            .table_name(&self.config.push_configs_table)
            .item("pk", AttributeValue::S(pk))
            .item("tenant", AttributeValue::S(tenant.to_string()))
            .item("taskId", AttributeValue::S(config.task_id.clone()))
            .item("configId", AttributeValue::S(config.id.clone()))
            .item("configJson", AttributeValue::S(json));
        if let Some(ttl) = Self::ttl_epoch(self.config.task_ttl_seconds) {
            req = req.item("ttl", ttl);
        }
        req.send()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?;

        Ok(config)
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
            page_configs.last().map(|c| c.id.clone()).unwrap_or_default()
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
}

/// Helper: query the current max event sequence for a task.
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
                return n.parse::<u64>().map_err(|e| A2aStorageError::DatabaseError(e.to_string()));
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
        let result = self.client
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
                    .and_then(|v| if let AttributeValue::N(n) = v { Some(n) } else { None })
                    .and_then(|n| n.parse::<u64>().ok())
                    .ok_or_else(|| A2aStorageError::DatabaseError("Missing eventSequence".into()))?;

                let data = item
                    .get("eventData")
                    .and_then(|v| if let AttributeValue::S(s) = v { Some(s) } else { None })
                    .ok_or_else(|| A2aStorageError::DatabaseError("Missing eventData".into()))?;

                let event: StreamEvent = serde_json::from_str(data)
                    .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;
                events.push((seq, event));
            }
        }
        Ok(events)
    }

    async fn latest_sequence(
        &self,
        tenant: &str,
        task_id: &str,
    ) -> Result<u64, A2aStorageError> {
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

        // Build TransactWriteItems: task put + event puts
        let mut task_put = aws_sdk_dynamodb::types::Put::builder()
            .table_name(&self.config.tasks_table)
            .item("pk", AttributeValue::S(pk.clone()))
            .item("tenant", AttributeValue::S(tenant.to_string()))
            .item("taskId", AttributeValue::S(task.id().to_string()))
            .item("owner", AttributeValue::S(owner.to_string()))
            .item("taskJson", AttributeValue::S(task_json))
            .item("contextId", AttributeValue::S(task.context_id().to_string()))
            .item("statusState", AttributeValue::S(state_str))
            .item("updatedAt", AttributeValue::S(Self::now_iso()));
        if let Some(ttl) = Self::ttl_epoch(self.config.task_ttl_seconds) {
            task_put = task_put.item("ttl", ttl);
        }
        let mut items = vec![
            aws_sdk_dynamodb::types::TransactWriteItem::builder()
                .put(
                    task_put.build()
                        .map_err(|e| A2aStorageError::DatabaseError(format!("{e:?}")))?,
                )
                .build(),
        ];

        let mut sequences = Vec::with_capacity(events.len());
        for (i, event) in events.iter().enumerate() {
            let seq = (i + 1) as u64;
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
                .condition_expression(
                    "attribute_not_exists(pk) AND attribute_not_exists(sk)",
                );
            items.push(
                aws_sdk_dynamodb::types::TransactWriteItem::builder()
                    .put(
                        event_put.build()
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
        let task = self.get_task(tenant, task_id, owner, None).await?
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

        // Get current max sequence for event numbering
        let max_seq = query_max_sequence(&self.client, &self.config.events_table, &pk).await?;

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
                        .item("contextId", AttributeValue::S(updated_task.context_id().to_string()))
                        .item("statusState", AttributeValue::S(state_str))
                        .item("updatedAt", AttributeValue::S(Self::now_iso()))
                        .condition_expression(
                            "#owner = :owner AND (attribute_not_exists(statusState) \
                             OR NOT statusState IN (:completed, :failed, :canceled, :rejected))",
                        )
                        .expression_attribute_names("#owner", "owner")
                        .expression_attribute_values(":owner", AttributeValue::S(owner.to_string()))
                        .expression_attribute_values(":completed", AttributeValue::S("Completed".to_string()))
                        .expression_attribute_values(":failed", AttributeValue::S("Failed".to_string()))
                        .expression_attribute_values(":canceled", AttributeValue::S("Canceled".to_string()))
                        .expression_attribute_values(":rejected", AttributeValue::S("Rejected".to_string()))
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
                            let probe_state = current_task
                                .status()
                                .and_then(|s| s.state().ok());
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

        // Get current max sequence
        let max_seq = query_max_sequence(&self.client, &self.config.events_table, &pk).await?;

        // Build TransactWriteItems: task put (with owner check) + event puts
        let mut task_put = aws_sdk_dynamodb::types::Put::builder()
            .table_name(&self.config.tasks_table)
            .item("pk", AttributeValue::S(pk.clone()))
            .item("tenant", AttributeValue::S(tenant.to_string()))
            .item("taskId", AttributeValue::S(task.id().to_string()))
            .item("owner", AttributeValue::S(owner.to_string()))
            .item("taskJson", AttributeValue::S(task_json))
            .item("contextId", AttributeValue::S(task.context_id().to_string()))
            .item("statusState", AttributeValue::S(state_str))
            .item("updatedAt", AttributeValue::S(Self::now_iso()))
            .condition_expression("#owner = :owner")
            .expression_attribute_names("#owner", "owner")
            .expression_attribute_values(":owner", AttributeValue::S(owner.to_string()));
        if let Some(ttl) = Self::ttl_epoch(self.config.task_ttl_seconds) {
            task_put = task_put.item("ttl", ttl);
        }
        let mut items = vec![
            aws_sdk_dynamodb::types::TransactWriteItem::builder()
                .put(
                    task_put.build()
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
                .condition_expression(
                    "attribute_not_exists(pk) AND attribute_not_exists(sk)",
                );
            items.push(
                aws_sdk_dynamodb::types::TransactWriteItem::builder()
                    .put(
                        event_put.build()
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

        Ok(sequences)
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
            let desc = client
                .describe_table()
                .table_name(table_name)
                .send()
                .await;
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
            let desc = client
                .describe_table()
                .table_name(table_name)
                .send()
                .await;
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
        }

        let storage = DynamoDbA2aStorage::from_client(
            client,
            DynamoDbConfig {
                tasks_table: TEST_TASKS_TABLE.into(),
                push_configs_table: TEST_PUSH_TABLE.into(),
                events_table: TEST_EVENTS_TABLE.into(),
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
    }

    #[async_trait]
    impl A2aTaskStorage for TestStorage {
        fn backend_name(&self) -> &'static str { "dynamodb-test" }

        async fn create_task(&self, tenant: &str, owner: &str, task: Task)
            -> Result<Task, A2aStorageError> {
            self.inner.create_task(&self.scoped_tenant(tenant), owner, task).await
        }

        async fn get_task(&self, tenant: &str, task_id: &str, owner: &str, history_length: Option<i32>)
            -> Result<Option<Task>, A2aStorageError> {
            self.inner.get_task(&self.scoped_tenant(tenant), task_id, owner, history_length).await
        }

        async fn update_task(&self, tenant: &str, owner: &str, task: Task)
            -> Result<(), A2aStorageError> {
            self.inner.update_task(&self.scoped_tenant(tenant), owner, task).await
        }

        async fn delete_task(&self, tenant: &str, task_id: &str, owner: &str)
            -> Result<bool, A2aStorageError> {
            self.inner.delete_task(&self.scoped_tenant(tenant), task_id, owner).await
        }

        async fn list_tasks(&self, mut filter: TaskFilter) -> Result<TaskListPage, A2aStorageError> {
            filter.tenant = filter.tenant.map(|t| self.scoped_tenant(&t));
            self.inner.list_tasks(filter).await
        }

        async fn update_task_status(&self, tenant: &str, task_id: &str, owner: &str, new_status: TaskStatus)
            -> Result<Task, A2aStorageError> {
            self.inner.update_task_status(&self.scoped_tenant(tenant), task_id, owner, new_status).await
        }

        async fn append_message(&self, tenant: &str, task_id: &str, owner: &str, message: Message)
            -> Result<(), A2aStorageError> {
            self.inner.append_message(&self.scoped_tenant(tenant), task_id, owner, message).await
        }

        async fn append_artifact(&self, tenant: &str, task_id: &str, owner: &str, artifact: Artifact, append: bool, last_chunk: bool)
            -> Result<(), A2aStorageError> {
            self.inner.append_artifact(&self.scoped_tenant(tenant), task_id, owner, artifact, append, last_chunk).await
        }

        async fn task_count(&self) -> Result<usize, A2aStorageError> {
            // Count only this test's tasks by filtering on tenant prefix
            let result = self.inner.client
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

        async fn set_cancel_requested(&self, tenant: &str, task_id: &str, owner: &str)
            -> Result<(), A2aStorageError> {
            self.inner.set_cancel_requested(&self.scoped_tenant(tenant), task_id, owner).await
        }
    }

    #[async_trait]
    impl crate::storage::A2aCancellationSupervisor for TestStorage {
        fn backend_name(&self) -> &'static str { "dynamodb-test" }

        async fn supervisor_get_cancel_requested(&self, tenant: &str, task_id: &str)
            -> Result<bool, A2aStorageError> {
            <DynamoDbA2aStorage as crate::storage::A2aCancellationSupervisor>::supervisor_get_cancel_requested(
                &self.inner, &self.scoped_tenant(tenant), task_id,
            ).await
        }

        async fn supervisor_list_cancel_requested(&self, tenant: &str, task_ids: &[String])
            -> Result<Vec<String>, A2aStorageError> {
            <DynamoDbA2aStorage as crate::storage::A2aCancellationSupervisor>::supervisor_list_cancel_requested(
                &self.inner, &self.scoped_tenant(tenant), task_ids,
            ).await
        }
    }

    #[async_trait]
    impl A2aPushNotificationStorage for TestStorage {
        fn backend_name(&self) -> &'static str { "dynamodb-test" }

        async fn create_config(&self, tenant: &str, config: turul_a2a_proto::TaskPushNotificationConfig)
            -> Result<turul_a2a_proto::TaskPushNotificationConfig, A2aStorageError> {
            self.inner.create_config(&self.scoped_tenant(tenant), config).await
        }

        async fn get_config(&self, tenant: &str, task_id: &str, config_id: &str)
            -> Result<Option<turul_a2a_proto::TaskPushNotificationConfig>, A2aStorageError> {
            self.inner.get_config(&self.scoped_tenant(tenant), task_id, config_id).await
        }

        async fn list_configs(&self, tenant: &str, task_id: &str, page_token: Option<&str>, page_size: Option<i32>)
            -> Result<PushConfigListPage, A2aStorageError> {
            self.inner.list_configs(&self.scoped_tenant(tenant), task_id, page_token, page_size).await
        }

        async fn delete_config(&self, tenant: &str, task_id: &str, config_id: &str)
            -> Result<(), A2aStorageError> {
            self.inner.delete_config(&self.scoped_tenant(tenant), task_id, config_id).await
        }
    }

    #[async_trait]
    impl A2aEventStore for TestStorage {
        fn backend_name(&self) -> &'static str { "dynamodb-test" }

        async fn append_event(&self, tenant: &str, task_id: &str, event: StreamEvent)
            -> Result<u64, A2aStorageError> {
            self.inner.append_event(&self.scoped_tenant(tenant), task_id, event).await
        }

        async fn get_events_after(&self, tenant: &str, task_id: &str, after_sequence: u64)
            -> Result<Vec<(u64, StreamEvent)>, A2aStorageError> {
            self.inner.get_events_after(&self.scoped_tenant(tenant), task_id, after_sequence).await
        }

        async fn latest_sequence(&self, tenant: &str, task_id: &str)
            -> Result<u64, A2aStorageError> {
            self.inner.latest_sequence(&self.scoped_tenant(tenant), task_id).await
        }

        async fn cleanup_expired(&self) -> Result<u64, A2aStorageError> {
            self.inner.cleanup_expired().await
        }
    }

    #[async_trait]
    impl A2aAtomicStore for TestStorage {
        fn backend_name(&self) -> &'static str { "dynamodb-test" }

        async fn create_task_with_events(&self, tenant: &str, owner: &str, task: Task, events: Vec<StreamEvent>)
            -> Result<(Task, Vec<u64>), A2aStorageError> {
            self.inner.create_task_with_events(&self.scoped_tenant(tenant), owner, task, events).await
        }

        async fn update_task_status_with_events(&self, tenant: &str, task_id: &str, owner: &str, new_status: TaskStatus, events: Vec<StreamEvent>)
            -> Result<(Task, Vec<u64>), A2aStorageError> {
            self.inner.update_task_status_with_events(&self.scoped_tenant(tenant), task_id, owner, new_status, events).await
        }

        async fn update_task_with_events(&self, tenant: &str, owner: &str, task: Task, events: Vec<StreamEvent>)
            -> Result<Vec<u64>, A2aStorageError> {
            self.inner.update_task_with_events(&self.scoped_tenant(tenant), owner, task, events).await
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

    // Terminal-write CAS (ADR-010 §7.1) — phase B parity.

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
    async fn test_invalid_transition_distinct_from_terminal_already_set() {
        skip_unless_dynamodb_env!(s);
        parity_tests::test_invalid_transition_distinct_from_terminal_already_set(&s, &s).await;
    }

    // Cancel-marker parity (ADR-012 / phase C).

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
        assert!(
            DynamoDbA2aStorage::event_sort_key(1) < DynamoDbA2aStorage::event_sort_key(2)
        );
        // Digit-boundary checks: plain itoa mis-orders these
        // lexicographically ("9" > "10"). Zero-padding prevents that.
        assert!(
            DynamoDbA2aStorage::event_sort_key(9) < DynamoDbA2aStorage::event_sort_key(10)
        );
        assert!(
            DynamoDbA2aStorage::event_sort_key(99) < DynamoDbA2aStorage::event_sort_key(100)
        );
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
        match item.get("pk") {
            Some(AttributeValue::S(v)) => assert_eq!(v, expected_pk),
            other => panic!("missing/wrong pk attribute: {other:?}"),
        }
        match item.get("sk") {
            Some(AttributeValue::S(v)) => {
                assert_eq!(v, &format!("{expected_seq:020}"));
                assert_eq!(v.len(), 20, "sk must be fixed-width 20 digits");
            }
            other => panic!("missing/wrong sk attribute: {other:?}"),
        }
        match item.get("eventSequence") {
            Some(AttributeValue::N(v)) => {
                assert_eq!(v, &expected_seq.to_string());
            }
            other => panic!("missing/wrong eventSequence attribute: {other:?}"),
        }
        match item.get("eventData") {
            Some(AttributeValue::S(v)) => assert_eq!(v, expected_data),
            other => panic!("missing/wrong eventData attribute: {other:?}"),
        }
    }

    #[test]
    fn build_event_item_has_required_attributes_for_append_event_path() {
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
        // Numeric values near u32::MAX confirm the encoding stays
        // stable at high sequences.
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
        let item = DynamoDbA2aStorage::build_event_item(
            "t#x",
            1,
            "{}",
            Some(ttl_attr.clone()),
        );
        assert_required_event_attributes(&item, "t#x", 1, "{}");
        match item.get("ttl") {
            Some(got) => assert_eq!(got, &ttl_attr),
            None => panic!("ttl attribute should be present when provided"),
        }
    }
}
