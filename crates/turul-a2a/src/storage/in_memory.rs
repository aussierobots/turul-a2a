use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use turul_a2a_types::{Artifact, Message, Task, TaskStatus};

use crate::streaming::StreamEvent;
use super::atomic::A2aAtomicStore;
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
    /// Last update timestamp (spec §3.1.4: ListTasks sorted by last update time DESC).
    updated_at: String,
}

/// Current UTC timestamp as ISO 8601 string.
fn now_iso() -> String {
    // Use a monotonic-friendly format: YYYY-MM-DDTHH:MM:SS.fffZ
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    // Simple formatting without chrono dependency
    format!("{secs:010}.{millis:03}")
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
            updated_at: now_iso(),
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
                        updated_at: now_iso(),
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

        // Sort by last update time descending (spec §3.1.4 MUST).
        // Tiebreak by task_id descending for determinism.
        filtered.sort_by(|a, b| {
            b.updated_at.cmp(&a.updated_at)
                .then_with(|| b.task.id().cmp(a.task.id()))
        });

        let total_size = filtered.len() as i32;
        let page_size = filter
            .page_size
            .map(|ps| ps.clamp(1, 100))
            .unwrap_or(50);

        // Cursor-based pagination: token is "updated_at|task_id" of last item.
        // Next page starts after items that sort before this cursor.
        let start_idx = if let Some(ref token) = filter.page_token {
            if let Some((cursor_time, cursor_id)) = token.split_once('|') {
                filtered
                    .iter()
                    .position(|s| {
                        (s.updated_at.as_str(), s.task.id()) < (cursor_time, cursor_id)
                    })
                    .unwrap_or(filtered.len())
            } else {
                // Legacy token format (task_id only) — skip past it
                filtered
                    .iter()
                    .position(|s| s.task.id() < token.as_str())
                    .unwrap_or(filtered.len())
            }
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

        // Encode cursor as "updated_at|task_id"
        let next_page_token = if end_idx < filtered.len() {
            filtered.get(end_idx - 1).map(|s| {
                format!("{}|{}", s.updated_at, s.task.id())
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
                updated_at: now_iso(),
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

        // Hold both locks for the entire operation to prevent interleaving
        // with atomic methods. Lock order: event_counters → events
        // (consistent with atomic methods which acquire tasks first, then these).
        let mut counters = self.event_counters.write().await;
        let mut events = self.events.write().await;

        let counter = counters.entry(key.clone()).or_insert(0);
        *counter += 1;
        let seq = *counter;

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

#[async_trait]
impl A2aAtomicStore for InMemoryA2aStorage {
    fn backend_name(&self) -> &'static str {
        "in-memory"
    }

    async fn create_task_with_events(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
        events: Vec<StreamEvent>,
    ) -> Result<(Task, Vec<u64>), A2aStorageError> {
        // Acquire all locks in consistent order: tasks → event_counters → events
        let mut tasks = self.tasks.write().await;
        let mut counters = self.event_counters.write().await;
        let mut event_map = self.events.write().await;

        let task_key = (tenant.to_string(), task.id().to_string());
        let event_key = (tenant.to_string(), task.id().to_string());

        // Insert task
        tasks.insert(
            task_key,
            StoredTask {
                tenant: tenant.to_string(),
                owner: owner.to_string(),
                task: task.clone(),
                updated_at: now_iso(),
            },
        );

        // Append events with atomic sequence assignment
        let mut sequences = Vec::with_capacity(events.len());
        let task_events = event_map.entry(event_key.clone()).or_default();
        let counter = counters.entry(event_key).or_insert(0);

        for event in events {
            *counter += 1;
            let seq = *counter;
            sequences.push(seq);
            task_events.push((seq, event));
        }

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
        // Acquire all locks in consistent order: tasks → event_counters → events
        let mut tasks = self.tasks.write().await;
        let mut counters = self.event_counters.write().await;
        let mut event_map = self.events.write().await;

        let task_key = (tenant.to_string(), task_id.to_string());
        let event_key = (tenant.to_string(), task_id.to_string());

        // Validate task exists and owner matches
        let stored = tasks
            .get(&task_key)
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?;

        if stored.owner != owner {
            return Err(A2aStorageError::OwnerMismatch {
                task_id: task_id.to_string(),
            });
        }

        // Validate state machine transition
        let current_state = stored
            .task
            .status()
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?
            .state()
            .map_err(A2aStorageError::TypeError)?;

        let new_state = new_status.state().map_err(A2aStorageError::TypeError)?;

        // Terminal-write CAS contract (ADR-010 §7.1): the state machine
        // emits `TerminalState` when `current_state` is terminal, which we
        // translate to `TerminalStateAlreadySet` — the CAS "you lost the
        // race" signal. Non-terminal illegal transitions remain
        // `InvalidTransition`. Because this whole method holds the tasks
        // write lock for the entire read-check-write, the CAS is
        // guaranteed atomic in-process.
        turul_a2a_types::state_machine::validate_transition(current_state, new_state).map_err(
            |e| match e {
                turul_a2a_types::A2aTypeError::InvalidTransition { current, requested } => {
                    A2aStorageError::InvalidTransition { current, requested }
                }
                turul_a2a_types::A2aTypeError::TerminalState(s) => {
                    A2aStorageError::TerminalStateAlreadySet {
                        task_id: task_id.to_string(),
                        current_state: crate::storage::terminal_cas::task_state_wire_name(s).to_string(),
                    }
                }
                other => A2aStorageError::TypeError(other),
            },
        )?;

        // Update task status
        let mut proto = stored.task.as_proto().clone();
        proto.status = Some(new_status.into_proto());
        let updated_task = Task::try_from(proto).map_err(A2aStorageError::TypeError)?;

        tasks.insert(
            task_key,
            StoredTask {
                tenant: tenant.to_string(),
                owner: owner.to_string(),
                task: updated_task.clone(),
                updated_at: now_iso(),
            },
        );

        // Append events with atomic sequence assignment
        let mut sequences = Vec::with_capacity(events.len());
        let task_events = event_map.entry(event_key.clone()).or_default();
        let counter = counters.entry(event_key).or_insert(0);

        for event in events {
            *counter += 1;
            let seq = *counter;
            sequences.push(seq);
            task_events.push((seq, event));
        }

        Ok((updated_task, sequences))
    }

    async fn update_task_with_events(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
        events: Vec<StreamEvent>,
    ) -> Result<Vec<u64>, A2aStorageError> {
        // Acquire all locks in consistent order: tasks → event_counters → events
        let mut tasks = self.tasks.write().await;
        let mut counters = self.event_counters.write().await;
        let mut event_map = self.events.write().await;

        let task_key = (tenant.to_string(), task.id().to_string());
        let event_key = (tenant.to_string(), task.id().to_string());

        // Validate task exists and owner matches
        match tasks.get(&task_key) {
            Some(stored) if stored.owner == owner => {}
            Some(_) => {
                return Err(A2aStorageError::OwnerMismatch {
                    task_id: task.id().to_string(),
                });
            }
            None => {
                return Err(A2aStorageError::TaskNotFound(task.id().to_string()));
            }
        }

        // Replace task
        tasks.insert(
            task_key,
            StoredTask {
                tenant: tenant.to_string(),
                owner: owner.to_string(),
                task,
                updated_at: now_iso(),
            },
        );

        // Append events with atomic sequence assignment
        let mut sequences = Vec::with_capacity(events.len());
        let task_events = event_map.entry(event_key.clone()).or_default();
        let counter = counters.entry(event_key).or_insert(0);

        for event in events {
            *counter += 1;
            let seq = *counter;
            sequences.push(seq);
            task_events.push((seq, event));
        }

        Ok(sequences)
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

    // Atomic store parity tests

    #[tokio::test]
    async fn test_atomic_create_task_with_events() {
        let s = storage();
        parity_tests::test_atomic_create_task_with_events(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_update_status_with_events() {
        let s = storage();
        parity_tests::test_atomic_update_status_with_events(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_status_rejects_invalid_transition() {
        let s = storage();
        parity_tests::test_atomic_status_rejects_invalid_transition(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_update_task_with_events() {
        let s = storage();
        parity_tests::test_atomic_update_task_with_events(&s, &s, &s).await;
    }

    // Terminal-write CAS (ADR-010 §7.1) — phase B parity.

    #[tokio::test]
    async fn test_terminal_cas_single_winner_on_concurrent_terminals() {
        let s = std::sync::Arc::new(storage());
        parity_tests::test_terminal_cas_single_winner_on_concurrent_terminals(
            s.clone(),
            s.clone(),
            s,
        )
        .await;
    }

    #[tokio::test]
    async fn test_terminal_cas_single_winner_from_submitted_includes_rejected() {
        let s = std::sync::Arc::new(storage());
        parity_tests::test_terminal_cas_single_winner_from_submitted_includes_rejected(
            s.clone(),
            s.clone(),
            s,
        )
        .await;
    }

    #[tokio::test]
    async fn test_terminal_cas_rejects_sequential_second_terminal() {
        let s = storage();
        parity_tests::test_terminal_cas_rejects_sequential_second_terminal(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_invalid_transition_distinct_from_terminal_already_set() {
        let s = storage();
        parity_tests::test_invalid_transition_distinct_from_terminal_already_set(&s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_owner_isolation() {
        let s = storage();
        parity_tests::test_atomic_owner_isolation(&s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_tenant_isolation() {
        let s = storage();
        parity_tests::test_atomic_tenant_isolation(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_create_with_empty_events() {
        let s = storage();
        parity_tests::test_atomic_create_with_empty_events(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_sequence_continuity() {
        let s = storage();
        parity_tests::test_atomic_sequence_continuity(&s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_concurrent_sequence_integrity() {
        let s = std::sync::Arc::new(storage());
        parity_tests::test_atomic_concurrent_sequence_integrity(s).await;
    }
}
