use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use turul_a2a_types::{Artifact, Message, Task, TaskStatus};

use crate::streaming::StreamEvent;
use super::error::A2aStorageError;
use super::event_store::A2aEventStore;
use super::filter::{PushConfigListPage, TaskFilter, TaskListPage};
use super::traits::{A2aPushNotificationStorage, A2aTaskStorage};

/// Stored task with tenant/owner metadata not present in the proto Task.
#[derive(Debug, Clone)]
struct StoredTask {
    tenant: String,
    owner: String,
    task: Task,
}

type TaskKey = (String, String); // (tenant, task_id)
type PushConfigKey = (String, String, String); // (tenant, task_id, config_id)
type EventKey = (String, String); // (tenant, task_id)

/// In-memory A2A storage backend.
/// Implements A2aTaskStorage, A2aPushNotificationStorage, AND A2aEventStore
/// on the same struct — enforcing the same-backend requirement from ADR-009.
#[derive(Clone)]
pub struct InMemoryA2aStorage {
    tasks: Arc<RwLock<HashMap<TaskKey, StoredTask>>>,
    push_configs: Arc<RwLock<HashMap<PushConfigKey, turul_a2a_proto::TaskPushNotificationConfig>>>,
    events: Arc<RwLock<HashMap<EventKey, Vec<(u64, StreamEvent)>>>>,
    event_counters: Arc<RwLock<HashMap<EventKey, u64>>>,
}

impl InMemoryA2aStorage {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            push_configs: Arc::new(RwLock::new(HashMap::new())),
            events: Arc::new(RwLock::new(HashMap::new())),
            event_counters: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryA2aStorage {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply history_length trimming to a task clone.
fn trim_history(task: &Task, history_length: Option<i32>) -> Task {
    match history_length {
        Some(0) => {
            // Clone task, clear history
            let mut proto = task.as_proto().clone();
            proto.history.clear();
            // Safe: original task had valid state, clearing history doesn't invalidate
            Task::try_from(proto).unwrap_or_else(|_| task.clone())
        }
        Some(n) if n > 0 => {
            let n = n as usize;
            let history = task.history();
            if history.len() <= n {
                return task.clone();
            }
            let mut proto = task.as_proto().clone();
            let start = proto.history.len().saturating_sub(n);
            proto.history = proto.history[start..].to_vec();
            Task::try_from(proto).unwrap_or_else(|_| task.clone())
        }
        _ => task.clone(), // None = no limit
    }
}

/// Apply include_artifacts filtering.
fn strip_artifacts(task: &Task, include: bool) -> Task {
    if include {
        return task.clone();
    }
    let mut proto = task.as_proto().clone();
    proto.artifacts.clear();
    Task::try_from(proto).unwrap_or_else(|_| task.clone())
}

#[async_trait]
impl A2aTaskStorage for InMemoryA2aStorage {
    fn backend_name(&self) -> &'static str {
        "in-memory"
    }

    async fn create_task(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
    ) -> Result<Task, A2aStorageError> {
        let key = (tenant.to_string(), task.id().to_string());
        let stored = StoredTask {
            tenant: tenant.to_string(),
            owner: owner.to_string(),
            task: task.clone(),
        };
        let mut tasks = self.tasks.write().await;
        tasks.insert(key, stored);
        Ok(task)
    }

    async fn get_task(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        history_length: Option<i32>,
    ) -> Result<Option<Task>, A2aStorageError> {
        let tasks = self.tasks.read().await;
        let key = (tenant.to_string(), task_id.to_string());
        match tasks.get(&key) {
            Some(stored) if stored.owner == owner => {
                Ok(Some(trim_history(&stored.task, history_length)))
            }
            _ => Ok(None), // Not found or wrong owner — same result per spec
        }
    }

    async fn update_task(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
    ) -> Result<(), A2aStorageError> {
        let key = (tenant.to_string(), task.id().to_string());
        let mut tasks = self.tasks.write().await;
        match tasks.get(&key) {
            Some(stored) if stored.owner == owner => {
                tasks.insert(
                    key,
                    StoredTask {
                        tenant: tenant.to_string(),
                        owner: owner.to_string(),
                        task,
                    },
                );
                Ok(())
            }
            Some(_) => Err(A2aStorageError::OwnerMismatch {
                task_id: task.id().to_string(),
            }),
            None => Err(A2aStorageError::TaskNotFound(task.id().to_string())),
        }
    }

    async fn delete_task(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
    ) -> Result<bool, A2aStorageError> {
        let key = (tenant.to_string(), task_id.to_string());
        let mut tasks = self.tasks.write().await;
        match tasks.get(&key) {
            Some(stored) if stored.owner == owner => {
                tasks.remove(&key);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn list_tasks(&self, filter: TaskFilter) -> Result<TaskListPage, A2aStorageError> {
        let tasks = self.tasks.read().await;

        // Filter
        let mut filtered: Vec<&StoredTask> = tasks
            .values()
            .filter(|s| {
                if let Some(ref t) = filter.tenant {
                    if s.tenant != *t {
                        return false;
                    }
                }
                if let Some(ref o) = filter.owner {
                    if s.owner != *o {
                        return false;
                    }
                }
                if let Some(ref ctx) = filter.context_id {
                    if s.task.context_id() != ctx.as_str() {
                        return false;
                    }
                }
                if let Some(ref status) = filter.status {
                    if let Some(ts) = s.task.status() {
                        if let Ok(state) = ts.state() {
                            if state != *status {
                                return false;
                            }
                        }
                    }
                }
                true
            })
            .collect();

        // Sort deterministically by task ID
        filtered.sort_by(|a, b| a.task.id().cmp(b.task.id()));

        let total_size = filtered.len() as i32;
        let page_size = filter
            .page_size
            .map(|ps| ps.clamp(1, 100))
            .unwrap_or(50);

        // Cursor-based pagination: page_token is the last task_id from previous page
        let start_idx = if let Some(ref token) = filter.page_token {
            filtered
                .iter()
                .position(|s| s.task.id() > token.as_str())
                .unwrap_or(filtered.len())
        } else {
            0
        };

        let end_idx = (start_idx + page_size as usize).min(filtered.len());
        let page_tasks: Vec<Task> = filtered[start_idx..end_idx]
            .iter()
            .map(|s| {
                let t = trim_history(&s.task, filter.history_length);
                strip_artifacts(&t, filter.include_artifacts.unwrap_or(false))
            })
            .collect();

        let next_page_token = if end_idx < filtered.len() {
            page_tasks.last().map(|t| t.id().to_string()).unwrap_or_default()
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
        let key = (tenant.to_string(), task_id.to_string());
        let mut tasks = self.tasks.write().await;
        let stored = tasks
            .get(&key)
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?;

        if stored.owner != owner {
            return Err(A2aStorageError::OwnerMismatch {
                task_id: task_id.to_string(),
            });
        }

        // Get current state
        let current_state = stored
            .task
            .status()
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?
            .state()
            .map_err(|e| A2aStorageError::TypeError(e))?;

        let new_state = new_status.state().map_err(|e| A2aStorageError::TypeError(e))?;

        // Validate state machine transition
        turul_a2a_types::state_machine::validate_transition(current_state, new_state).map_err(
            |e| match e {
                turul_a2a_types::A2aTypeError::InvalidTransition { current, requested } => {
                    A2aStorageError::InvalidTransition {
                        current,
                        requested,
                    }
                }
                turul_a2a_types::A2aTypeError::TerminalState(s) => {
                    A2aStorageError::TerminalState(s)
                }
                other => A2aStorageError::TypeError(other),
            },
        )?;

        // Update status on proto
        let mut proto = stored.task.as_proto().clone();
        proto.status = Some(new_status.into_proto());
        let updated_task =
            Task::try_from(proto).map_err(|e| A2aStorageError::TypeError(e))?;

        tasks.insert(
            key,
            StoredTask {
                tenant: tenant.to_string(),
                owner: owner.to_string(),
                task: updated_task.clone(),
            },
        );

        Ok(updated_task)
    }

    async fn append_message(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        message: Message,
    ) -> Result<(), A2aStorageError> {
        let key = (tenant.to_string(), task_id.to_string());
        let mut tasks = self.tasks.write().await;
        let stored = tasks
            .get_mut(&key)
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?;

        if stored.owner != owner {
            return Err(A2aStorageError::OwnerMismatch {
                task_id: task_id.to_string(),
            });
        }

        stored.task.append_message(message);
        Ok(())
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
        let key = (tenant.to_string(), task_id.to_string());
        let mut tasks = self.tasks.write().await;
        let stored = tasks
            .get_mut(&key)
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?;

        if stored.owner != owner {
            return Err(A2aStorageError::OwnerMismatch {
                task_id: task_id.to_string(),
            });
        }

        if append {
            // Find existing artifact with same ID and append parts
            let proto_task = stored.task.as_proto_mut();
            if let Some(existing) = proto_task
                .artifacts
                .iter_mut()
                .find(|a| a.artifact_id == artifact.as_proto().artifact_id)
            {
                existing.parts.extend(artifact.into_proto().parts);
                return Ok(());
            }
        }

        // No existing or append=false: add/replace
        stored.task.append_artifact(artifact);
        Ok(())
    }

    async fn task_count(&self) -> Result<usize, A2aStorageError> {
        Ok(self.tasks.read().await.len())
    }

    async fn maintenance(&self) -> Result<(), A2aStorageError> {
        Ok(())
    }
}

#[async_trait]
impl A2aPushNotificationStorage for InMemoryA2aStorage {
    fn backend_name(&self) -> &'static str {
        "in-memory"
    }

    async fn create_config(
        &self,
        tenant: &str,
        mut config: turul_a2a_proto::TaskPushNotificationConfig,
    ) -> Result<turul_a2a_proto::TaskPushNotificationConfig, A2aStorageError> {
        if config.id.is_empty() {
            config.id = uuid::Uuid::now_v7().to_string();
        }
        let key = (
            tenant.to_string(),
            config.task_id.clone(),
            config.id.clone(),
        );
        self.push_configs.write().await.insert(key, config.clone());
        Ok(config)
    }

    async fn get_config(
        &self,
        tenant: &str,
        task_id: &str,
        config_id: &str,
    ) -> Result<Option<turul_a2a_proto::TaskPushNotificationConfig>, A2aStorageError> {
        let key = (
            tenant.to_string(),
            task_id.to_string(),
            config_id.to_string(),
        );
        Ok(self.push_configs.read().await.get(&key).cloned())
    }

    async fn list_configs(
        &self,
        tenant: &str,
        task_id: &str,
        page_token: Option<&str>,
        page_size: Option<i32>,
    ) -> Result<PushConfigListPage, A2aStorageError> {
        let configs = self.push_configs.read().await;
        let mut matching: Vec<_> = configs
            .iter()
            .filter(|((t, tid, _), _)| t == tenant && tid == task_id)
            .map(|(_, v)| v.clone())
            .collect();

        // Deterministic order by config id
        matching.sort_by(|a, b| a.id.cmp(&b.id));

        let page_size = page_size.map(|ps| ps.clamp(1, 100)).unwrap_or(50) as usize;

        let start_idx = if let Some(token) = page_token {
            matching
                .iter()
                .position(|c| c.id.as_str() > token)
                .unwrap_or(matching.len())
        } else {
            0
        };

        let end_idx = (start_idx + page_size).min(matching.len());
        let page_configs = matching[start_idx..end_idx].to_vec();

        let next_page_token = if end_idx < matching.len() {
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
        let key = (
            tenant.to_string(),
            task_id.to_string(),
            config_id.to_string(),
        );
        self.push_configs.write().await.remove(&key);
        Ok(()) // Idempotent
    }
}

#[async_trait]
impl A2aEventStore for InMemoryA2aStorage {
    fn backend_name(&self) -> &'static str {
        "in-memory"
    }

    async fn append_event(
        &self,
        tenant: &str,
        task_id: &str,
        event: StreamEvent,
    ) -> Result<u64, A2aStorageError> {
        let key = (tenant.to_string(), task_id.to_string());

        // Atomic sequence assignment
        let mut counters = self.event_counters.write().await;
        let counter = counters.entry(key.clone()).or_insert(0);
        *counter += 1;
        let seq = *counter;
        drop(counters);

        let mut events = self.events.write().await;
        events
            .entry(key)
            .or_insert_with(Vec::new)
            .push((seq, event));

        Ok(seq)
    }

    async fn get_events_after(
        &self,
        tenant: &str,
        task_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, StreamEvent)>, A2aStorageError> {
        let key = (tenant.to_string(), task_id.to_string());
        let events = self.events.read().await;
        let task_events = events.get(&key);

        match task_events {
            Some(evts) => Ok(evts
                .iter()
                .filter(|(seq, _)| *seq > after_sequence)
                .cloned()
                .collect()),
            None => Ok(vec![]),
        }
    }

    async fn latest_sequence(
        &self,
        tenant: &str,
        task_id: &str,
    ) -> Result<u64, A2aStorageError> {
        let key = (tenant.to_string(), task_id.to_string());
        let counters = self.event_counters.read().await;
        Ok(*counters.get(&key).unwrap_or(&0))
    }

    async fn cleanup_expired(&self) -> Result<u64, A2aStorageError> {
        // In-memory: no TTL tracking. No-op.
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::parity_tests;

    fn storage() -> InMemoryA2aStorage {
        InMemoryA2aStorage::new()
    }

    #[tokio::test]
    async fn test_create_and_retrieve() {
        parity_tests::test_create_and_retrieve(&storage()).await;
    }

    #[tokio::test]
    async fn test_state_machine_enforcement() {
        parity_tests::test_state_machine_enforcement(&storage()).await;
    }

    #[tokio::test]
    async fn test_terminal_state_rejection() {
        parity_tests::test_terminal_state_rejection(&storage()).await;
    }

    #[tokio::test]
    async fn test_tenant_isolation() {
        parity_tests::test_tenant_isolation(&storage()).await;
    }

    #[tokio::test]
    async fn test_owner_isolation() {
        parity_tests::test_owner_isolation(&storage()).await;
    }

    #[tokio::test]
    async fn test_history_length() {
        parity_tests::test_history_length(&storage()).await;
    }

    #[tokio::test]
    async fn test_list_pagination() {
        parity_tests::test_list_pagination(&storage()).await;
    }

    #[tokio::test]
    async fn test_list_filter_by_status() {
        parity_tests::test_list_filter_by_status(&storage()).await;
    }

    #[tokio::test]
    async fn test_list_filter_by_context_id() {
        parity_tests::test_list_filter_by_context_id(&storage()).await;
    }

    #[tokio::test]
    async fn test_append_message() {
        parity_tests::test_append_message(&storage()).await;
    }

    #[tokio::test]
    async fn test_append_artifact() {
        parity_tests::test_append_artifact(&storage()).await;
    }

    #[tokio::test]
    async fn test_task_count() {
        parity_tests::test_task_count(&storage()).await;
    }

    #[tokio::test]
    async fn test_push_config_crud() {
        parity_tests::test_push_config_crud(&storage()).await;
    }

    #[tokio::test]
    async fn test_push_config_list_pagination() {
        parity_tests::test_push_config_list_pagination(&storage()).await;
    }

    #[tokio::test]
    async fn test_push_config_idempotent_delete() {
        parity_tests::test_push_config_idempotent_delete(&storage()).await;
    }

    #[tokio::test]
    async fn test_push_config_tenant_isolation() {
        parity_tests::test_push_config_tenant_isolation(&storage()).await;
    }

    #[tokio::test]
    async fn test_owner_isolation_mutations() {
        parity_tests::test_owner_isolation_mutations(&storage()).await;
    }

    #[tokio::test]
    async fn test_artifact_chunk_semantics() {
        parity_tests::test_artifact_chunk_semantics(&storage()).await;
    }

    // Event store parity tests

    #[tokio::test]
    async fn test_event_append_and_retrieve() {
        parity_tests::test_event_append_and_retrieve(&storage()).await;
    }

    #[tokio::test]
    async fn test_event_monotonic_ordering() {
        parity_tests::test_event_monotonic_ordering(&storage()).await;
    }

    #[tokio::test]
    async fn test_event_per_task_isolation() {
        parity_tests::test_event_per_task_isolation(&storage()).await;
    }

    #[tokio::test]
    async fn test_event_tenant_isolation() {
        parity_tests::test_event_tenant_isolation(&storage()).await;
    }

    #[tokio::test]
    async fn test_event_latest_sequence() {
        parity_tests::test_event_latest_sequence(&storage()).await;
    }

    #[tokio::test]
    async fn test_event_empty_task() {
        parity_tests::test_event_empty_task(&storage()).await;
    }
}
