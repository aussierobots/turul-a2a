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
}

impl Default for DynamoDbConfig {
    fn default() -> Self {
        Self {
            tasks_table: "a2a_tasks".into(),
            push_configs_table: "a2a_push_configs".into(),
            events_table: "a2a_task_events".into(),
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

    fn now_iso() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        format!("{:010}.{:03}", now.as_secs(), now.subsec_millis())
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

        self.client
            .put_item()
            .table_name(&self.config.tasks_table)
            .item("pk", AttributeValue::S(pk))
            .item("tenant", AttributeValue::S(tenant.to_string()))
            .item("taskId", AttributeValue::S(task.id().to_string()))
            .item("owner", AttributeValue::S(owner.to_string()))
            .item("contextId", AttributeValue::S(task.context_id().to_string()))
            .item("statusState", AttributeValue::S(state_str))
            .item("taskJson", AttributeValue::S(json))
            .item("updatedAt", AttributeValue::S(Self::now_iso()))
            .send()
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
        let result = self
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
            )
            .send()
            .await;

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

        self.client
            .put_item()
            .table_name(&self.config.push_configs_table)
            .item("pk", AttributeValue::S(pk))
            .item("tenant", AttributeValue::S(tenant.to_string()))
            .item("taskId", AttributeValue::S(config.task_id.clone()))
            .item("configId", AttributeValue::S(config.id.clone()))
            .item("configJson", AttributeValue::S(json))
            .send()
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

        self.client
            .put_item()
            .table_name(&self.config.events_table)
            .item("pk", AttributeValue::S(pk))
            .item("eventSequence", AttributeValue::N(new_seq.to_string()))
            .item("eventData", AttributeValue::S(event_data))
            .condition_expression("attribute_not_exists(pk)")
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

        let result = self.client
            .query()
            .table_name(&self.config.events_table)
            .key_condition_expression("pk = :pk AND eventSequence > :seq")
            .expression_attribute_values(":pk", AttributeValue::S(pk))
            .expression_attribute_values(":seq", AttributeValue::N(after_sequence.to_string()))
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
        let mut items = vec![
            aws_sdk_dynamodb::types::TransactWriteItem::builder()
                .put(
                    aws_sdk_dynamodb::types::Put::builder()
                        .table_name(&self.config.tasks_table)
                        .item("pk", AttributeValue::S(pk.clone()))
                        .item("owner", AttributeValue::S(owner.to_string()))
                        .item("taskJson", AttributeValue::S(task_json))
                        .item("contextId", AttributeValue::S(task.context_id().to_string()))
                        .item("statusState", AttributeValue::S(state_str))
                        .item("updatedAt", AttributeValue::S(Self::now_iso()))
                        .build()
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

            items.push(
                aws_sdk_dynamodb::types::TransactWriteItem::builder()
                    .put(
                        aws_sdk_dynamodb::types::Put::builder()
                            .table_name(&self.config.events_table)
                            .item("pk", AttributeValue::S(pk.clone()))
                            .item("eventSequence", AttributeValue::N(seq.to_string()))
                            .item("eventData", AttributeValue::S(event_data))
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
                turul_a2a_types::A2aTypeError::TerminalState(s) => A2aStorageError::TerminalState(s),
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

        // Build TransactWriteItems: task update + event puts
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
                        .condition_expression("#owner = :owner")
                        .expression_attribute_names("#owner", "owner")
                        .expression_attribute_values(":owner", AttributeValue::S(owner.to_string()))
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

            items.push(
                aws_sdk_dynamodb::types::TransactWriteItem::builder()
                    .put(
                        aws_sdk_dynamodb::types::Put::builder()
                            .table_name(&self.config.events_table)
                            .item("pk", AttributeValue::S(pk.clone()))
                            .item("eventSequence", AttributeValue::N(seq.to_string()))
                            .item("eventData", AttributeValue::S(event_data))
                            .condition_expression("attribute_not_exists(pk)")
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

        Ok((updated_task, sequences))
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
        let mut items = vec![
            aws_sdk_dynamodb::types::TransactWriteItem::builder()
                .put(
                    aws_sdk_dynamodb::types::Put::builder()
                        .table_name(&self.config.tasks_table)
                        .item("pk", AttributeValue::S(pk.clone()))
                        .item("owner", AttributeValue::S(owner.to_string()))
                        .item("taskJson", AttributeValue::S(task_json))
                        .item("contextId", AttributeValue::S(task.context_id().to_string()))
                        .item("statusState", AttributeValue::S(state_str))
                        .item("updatedAt", AttributeValue::S(Self::now_iso()))
                        .condition_expression("#owner = :owner")
                        .expression_attribute_names("#owner", "owner")
                        .expression_attribute_values(":owner", AttributeValue::S(owner.to_string()))
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

            items.push(
                aws_sdk_dynamodb::types::TransactWriteItem::builder()
                    .put(
                        aws_sdk_dynamodb::types::Put::builder()
                            .table_name(&self.config.events_table)
                            .item("pk", AttributeValue::S(pk.clone()))
                            .item("eventSequence", AttributeValue::N(seq.to_string()))
                            .item("eventData", AttributeValue::S(event_data))
                            .condition_expression("attribute_not_exists(pk)")
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

    /// Create the events table with pk (HASH) + eventSequence (RANGE).
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
                    .attribute_name("eventSequence")
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
                    .attribute_name("eventSequence")
                    .attribute_type(ScalarAttributeType::N)
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

    /// Each test gets a unique tenant prefix for data isolation.
    /// Tests can run in parallel without interfering — the PK includes
    /// the tenant, so each test's data is in a separate key space.
    async fn storage() -> TestStorage {
        static TABLES_CREATED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);

        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
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
            },
        );

        // Each test gets a unique tenant prefix for isolation
        TestStorage {
            inner: storage,
            tenant_prefix: uuid::Uuid::now_v7().to_string(),
        }
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
        parity_tests::test_create_and_retrieve(&storage().await).await;
    }

    #[tokio::test]
    async fn test_state_machine_enforcement() {
        parity_tests::test_state_machine_enforcement(&storage().await).await;
    }

    #[tokio::test]
    async fn test_terminal_state_rejection() {
        parity_tests::test_terminal_state_rejection(&storage().await).await;
    }

    #[tokio::test]
    async fn test_tenant_isolation() {
        parity_tests::test_tenant_isolation(&storage().await).await;
    }

    #[tokio::test]
    async fn test_owner_isolation() {
        parity_tests::test_owner_isolation(&storage().await).await;
    }

    #[tokio::test]
    async fn test_history_length() {
        parity_tests::test_history_length(&storage().await).await;
    }

    #[tokio::test]
    async fn test_list_pagination() {
        parity_tests::test_list_pagination(&storage().await).await;
    }

    #[tokio::test]
    async fn test_list_filter_by_status() {
        parity_tests::test_list_filter_by_status(&storage().await).await;
    }

    #[tokio::test]
    async fn test_list_filter_by_context_id() {
        parity_tests::test_list_filter_by_context_id(&storage().await).await;
    }

    #[tokio::test]
    async fn test_append_message() {
        parity_tests::test_append_message(&storage().await).await;
    }

    #[tokio::test]
    async fn test_append_artifact() {
        parity_tests::test_append_artifact(&storage().await).await;
    }

    #[tokio::test]
    async fn test_task_count() {
        parity_tests::test_task_count(&storage().await).await;
    }

    #[tokio::test]
    async fn test_owner_isolation_mutations() {
        parity_tests::test_owner_isolation_mutations(&storage().await).await;
    }

    #[tokio::test]
    async fn test_artifact_chunk_semantics() {
        parity_tests::test_artifact_chunk_semantics(&storage().await).await;
    }

    #[tokio::test]
    async fn test_push_config_crud() {
        parity_tests::test_push_config_crud(&storage().await).await;
    }

    #[tokio::test]
    async fn test_push_config_idempotent_delete() {
        parity_tests::test_push_config_idempotent_delete(&storage().await).await;
    }

    #[tokio::test]
    async fn test_push_config_tenant_isolation() {
        parity_tests::test_push_config_tenant_isolation(&storage().await).await;
    }

    #[tokio::test]
    async fn test_push_config_list_pagination() {
        parity_tests::test_push_config_list_pagination(&storage().await).await;
    }

    // Event store parity tests

    #[tokio::test]
    async fn test_event_append_and_retrieve() {
        parity_tests::test_event_append_and_retrieve(&storage().await).await;
    }

    #[tokio::test]
    async fn test_event_monotonic_ordering() {
        parity_tests::test_event_monotonic_ordering(&storage().await).await;
    }

    #[tokio::test]
    async fn test_event_per_task_isolation() {
        parity_tests::test_event_per_task_isolation(&storage().await).await;
    }

    #[tokio::test]
    async fn test_event_tenant_isolation() {
        parity_tests::test_event_tenant_isolation(&storage().await).await;
    }

    #[tokio::test]
    async fn test_event_latest_sequence() {
        parity_tests::test_event_latest_sequence(&storage().await).await;
    }

    #[tokio::test]
    async fn test_event_empty_task() {
        parity_tests::test_event_empty_task(&storage().await).await;
    }

    // Atomic store parity tests

    #[tokio::test]
    async fn test_atomic_create_task_with_events() {
        let s = storage().await;
        parity_tests::test_atomic_create_task_with_events(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_update_status_with_events() {
        let s = storage().await;
        parity_tests::test_atomic_update_status_with_events(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_status_rejects_invalid_transition() {
        let s = storage().await;
        parity_tests::test_atomic_status_rejects_invalid_transition(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_update_task_with_events() {
        let s = storage().await;
        parity_tests::test_atomic_update_task_with_events(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_owner_isolation() {
        let s = storage().await;
        parity_tests::test_atomic_owner_isolation(&s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_tenant_isolation() {
        let s = storage().await;
        parity_tests::test_atomic_tenant_isolation(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_create_with_empty_events() {
        let s = storage().await;
        parity_tests::test_atomic_create_with_empty_events(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_sequence_continuity() {
        let s = storage().await;
        parity_tests::test_atomic_sequence_continuity(&s, &s).await;
    }
}
