use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use turul_a2a_types::{Artifact, Message, Task, TaskStatus};

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

/// Stored task with tenant/owner metadata not present in the proto Task.
#[derive(Debug, Clone)]
struct StoredTask {
    tenant: String,
    owner: String,
    task: Task,
    /// Last update timestamp (spec §3.1.4: ListTasks sorted by last update time DESC).
    updated_at: String,
    /// ADR-012 cancel-requested marker. Storage-internal; never on wire.
    cancel_requested: bool,
    /// ADR-013 §6.3 causal floor: monotonic per-task event sequence
    /// maintained UNCONDITIONALLY on every atomic commit (regardless
    /// of `push_dispatch_enabled`). `create_config` reads this value
    /// to stamp the config's `registered_after_event_sequence`; the
    /// eligibility filter uses strict `<` against this floor so a
    /// config recorded at or after a terminal event is excluded from
    /// that event's fan-out. Zero until the first event commits.
    latest_event_sequence: u64,
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
type PushDeliveryKey = (String, String, u64, String); // (tenant, task_id, event_sequence, config_id)

/// A single push-delivery claim row (ADR-011 §10). Stored values are
/// exactly those required to satisfy the trait contract — no
/// credentials, no request bodies, no response bodies.
#[derive(Debug, Clone)]
struct StoredDeliveryClaim {
    claimant: String,
    /// Owner recorded on the first claim so the reclaim sweeper can
    /// call owner-scoped `get_task`. Not changed on re-claim.
    owner: String,
    generation: u64,
    claimed_at: std::time::SystemTime,
    expires_at: std::time::SystemTime,
    delivery_attempt_count: u32,
    status: ClaimStatus,
    first_attempted_at: Option<std::time::SystemTime>,
    last_attempted_at: Option<std::time::SystemTime>,
    last_http_status: Option<u16>,
    last_error_class: Option<DeliveryErrorClass>,
    gave_up_at: Option<std::time::SystemTime>,
    #[allow(dead_code)]
    gave_up_reason: Option<GaveUpReason>,
    #[allow(dead_code)]
    abandoned_reason: Option<AbandonedReason>,
}

impl StoredDeliveryClaim {
    fn as_claim(&self) -> DeliveryClaim {
        DeliveryClaim {
            claimant: self.claimant.clone(),
            owner: self.owner.clone(),
            generation: self.generation,
            claimed_at: self.claimed_at,
            delivery_attempt_count: self.delivery_attempt_count,
            status: self.status,
        }
    }
}

/// `(tenant, task_id, event_sequence)` lookup key for the
/// in-memory pending-dispatch map. Matches the tuple
/// `A2aPushDeliveryStore::record_pending_dispatch` writes.
type PendingDispatchKey = (String, String, u64);

/// In-memory event-log shape: per `EventKey`, the full history of
/// `(event_sequence, event)` pairs the atomic store appended.
type EventLog = Vec<(u64, StreamEvent)>;

/// In-memory A2A storage backend.
/// Implements A2aTaskStorage, A2aPushNotificationStorage, A2aEventStore,
/// and A2aPushDeliveryStore on the same struct — enforcing the
/// same-backend requirement from ADR-009.
#[derive(Clone)]
pub struct InMemoryA2aStorage {
    tasks: Arc<RwLock<HashMap<TaskKey, StoredTask>>>,
    push_configs: Arc<RwLock<HashMap<PushConfigKey, StoredPushConfig>>>,
    events: Arc<RwLock<HashMap<EventKey, EventLog>>>,
    event_counters: Arc<RwLock<HashMap<EventKey, u64>>>,
    push_delivery_claims: Arc<RwLock<HashMap<PushDeliveryKey, StoredDeliveryClaim>>>,
    pending_dispatches: Arc<RwLock<HashMap<PendingDispatchKey, StoredPendingDispatch>>>,
    /// ADR-013 §4.3 opt-in: when true, `update_task_status_with_events`
    /// writes an `a2a_push_pending_dispatches` row atomically with the
    /// task/event commit for terminal `StreamEvent::StatusUpdate` events.
    /// When false (default), the atomic commit is task + events only —
    /// non-push deployments never touch the pending-dispatches map.
    push_dispatch_enabled: bool,
    #[cfg(test)]
    cas_hook: Arc<RwLock<Option<CasHook>>>,
}

#[cfg(test)]
#[derive(Clone)]
struct CasHook {
    paused: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

/// Backing row for a pending-dispatch marker. Mirrors the public
/// `PendingDispatch` shape plus the owner/tenant the reclaim
/// sweeper needs to call owner-scoped `get_task`.
#[derive(Debug, Clone)]
struct StoredPendingDispatch {
    owner: String,
    recorded_at: std::time::SystemTime,
}

/// Backing row for a push notification config plus the causal-floor
/// stamp written at CAS time (ADR-013 §6.3). The proto shape is what
/// the wire format requires; `registered_after_event_sequence` is
/// storage-internal and used only by the eligibility filter.
#[derive(Debug, Clone)]
struct StoredPushConfig {
    config: turul_a2a_proto::TaskPushNotificationConfig,
    registered_after_event_sequence: u64,
}

impl InMemoryA2aStorage {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            push_configs: Arc::new(RwLock::new(HashMap::new())),
            events: Arc::new(RwLock::new(HashMap::new())),
            event_counters: Arc::new(RwLock::new(HashMap::new())),
            push_delivery_claims: Arc::new(RwLock::new(HashMap::new())),
            pending_dispatches: Arc::new(RwLock::new(HashMap::new())),
            push_dispatch_enabled: false,
            #[cfg(test)]
            cas_hook: Arc::new(RwLock::new(None)),
        }
    }

    /// Opt in to atomic pending-dispatch marker writes (ADR-013 §4.3).
    ///
    /// Set to `true` when the deployment wires `push_delivery_store`; the
    /// server builder rejects any configuration where the flag and the
    /// delivery store disagree, in either direction.
    pub fn with_push_dispatch_enabled(mut self, enabled: bool) -> Self {
        self.push_dispatch_enabled = enabled;
        self
    }

    #[cfg(test)]
    async fn install_cas_race_hook(&self) -> CasHook {
        let hook = CasHook {
            paused: Arc::new(tokio::sync::Notify::new()),
            resume: Arc::new(tokio::sync::Notify::new()),
        };
        *self.cas_hook.write().await = Some(hook.clone());
        hook
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
            cancel_requested: false,
            latest_event_sequence: 0,
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
                // Preserve cancel_requested marker (monotonic; never
                // cleared by a full-task replacement) and latest_event_sequence
                // (monotonic; maintained by atomic commits).
                let cancel_requested = stored.cancel_requested;
                let latest_event_sequence = stored.latest_event_sequence;
                tasks.insert(
                    key,
                    StoredTask {
                        tenant: tenant.to_string(),
                        owner: owner.to_string(),
                        task,
                        updated_at: now_iso(),
                        cancel_requested,
                        latest_event_sequence,
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
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.task.id().cmp(a.task.id()))
        });

        let total_size = filtered.len() as i32;
        let page_size = filter.page_size.map(|ps| ps.clamp(1, 100)).unwrap_or(50);

        // Cursor-based pagination: token is "updated_at|task_id" of last item.
        // Next page starts after items that sort before this cursor.
        let start_idx = if let Some(ref token) = filter.page_token {
            if let Some((cursor_time, cursor_id)) = token.split_once('|') {
                filtered
                    .iter()
                    .position(|s| (s.updated_at.as_str(), s.task.id()) < (cursor_time, cursor_id))
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
            filtered
                .get(end_idx - 1)
                .map(|s| format!("{}|{}", s.updated_at, s.task.id()))
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
            .map_err(A2aStorageError::TypeError)?;

        let new_state = new_status.state().map_err(A2aStorageError::TypeError)?;

        // Validate state machine transition
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

        // Update status on proto
        let mut proto = stored.task.as_proto().clone();
        proto.status = Some(new_status.into_proto());
        let updated_task = Task::try_from(proto).map_err(A2aStorageError::TypeError)?;

        // Preserve cancel_requested (monotonic) and latest_event_sequence
        // (ADR-013 §6.3 — monotonic, maintained by atomic commits only).
        let cancel_requested = stored.cancel_requested;
        let latest_event_sequence = stored.latest_event_sequence;
        tasks.insert(
            key,
            StoredTask {
                tenant: tenant.to_string(),
                owner: owner.to_string(),
                task: updated_task.clone(),
                updated_at: now_iso(),
                cancel_requested,
                latest_event_sequence,
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

    async fn set_cancel_requested(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
    ) -> Result<(), A2aStorageError> {
        let key = (tenant.to_string(), task_id.to_string());
        let mut tasks = self.tasks.write().await;
        let stored = tasks
            .get_mut(&key)
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?;
        // Owner mismatch → anti-enumeration: return TaskNotFound.
        if stored.owner != owner {
            return Err(A2aStorageError::TaskNotFound(task_id.to_string()));
        }
        // Terminal → reject per ADR-012 contract. Uses TerminalState
        // (the state-machine-style signal); router maps to 409 at wire.
        let state = stored
            .task
            .status()
            .and_then(|s| s.state().ok())
            .ok_or_else(|| A2aStorageError::TaskNotFound(task_id.to_string()))?;
        if turul_a2a_types::state_machine::is_terminal(state) {
            return Err(A2aStorageError::TerminalState(state));
        }
        // Idempotent: already true → no-op success.
        stored.cancel_requested = true;
        Ok(())
    }
}

#[async_trait]
impl crate::storage::A2aCancellationSupervisor for InMemoryA2aStorage {
    fn backend_name(&self) -> &'static str {
        "in-memory"
    }

    async fn supervisor_get_cancel_requested(
        &self,
        tenant: &str,
        task_id: &str,
    ) -> Result<bool, A2aStorageError> {
        let key = (tenant.to_string(), task_id.to_string());
        let tasks = self.tasks.read().await;
        let Some(stored) = tasks.get(&key) else {
            return Ok(false);
        };
        // Ignore the marker if the task is already terminal — supervisor
        // treats "already terminal" and "no cancel needed" identically.
        let state = stored.task.status().and_then(|s| s.state().ok());
        let terminal = state
            .map(turul_a2a_types::state_machine::is_terminal)
            .unwrap_or(false);
        Ok(stored.cancel_requested && !terminal)
    }

    async fn supervisor_list_cancel_requested(
        &self,
        tenant: &str,
        task_ids: &[String],
    ) -> Result<Vec<String>, A2aStorageError> {
        let tasks = self.tasks.read().await;
        let mut out = Vec::new();
        for task_id in task_ids {
            let key = (tenant.to_string(), task_id.clone());
            if let Some(stored) = tasks.get(&key) {
                let state = stored.task.status().and_then(|s| s.state().ok());
                let terminal = state
                    .map(turul_a2a_types::state_machine::is_terminal)
                    .unwrap_or(false);
                if stored.cancel_requested && !terminal {
                    out.push(task_id.clone());
                }
            }
        }
        Ok(out)
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
        let task_key = (tenant.to_string(), config.task_id.clone());
        let push_key = (
            tenant.to_string(),
            config.task_id.clone(),
            config.id.clone(),
        );

        // ADR-013 §6.4 causal-floor CAS. Two-phase:
        //   1. Read `latest_event_sequence` (brief `tasks` read guard).
        //   2. Re-read under a held `tasks` read guard and compare; if
        //      unchanged, insert the config + stamp under a `push_configs`
        //      write guard held concurrently with the `tasks` read guard.
        //      If the re-read disagrees, a concurrent atomic commit
        //      advanced the floor — retry with bounded backoff.
        //
        // The two-phase structure leaves an observable window between
        // phase 1 and phase 2 that the test hook instruments; in
        // production the window is nanoseconds.
        const MAX_ATTEMPTS: u32 = 5;
        let backoffs_ms: [u64; 4] = [10, 50, 250, 1000];
        for attempt in 0..MAX_ATTEMPTS {
            let seq_read = {
                let tasks = self.tasks.read().await;
                tasks
                    .get(&task_key)
                    .map(|t| t.latest_event_sequence)
                    .unwrap_or(0)
            };

            #[cfg(test)]
            {
                // Test-only: allow a deterministic interleaving between
                // the initial read and the re-read below so the CAS
                // retry path is exercised.
                let hook = { self.cas_hook.read().await.clone() };
                if let Some(h) = hook {
                    h.paused.notify_waiters();
                    h.resume.notified().await;
                }
            }

            // Phase 2: re-acquire tasks read guard. While we hold it,
            // no `update_task_*_with_events` can acquire tasks.write().
            // Re-read under the guard; if it matches `seq_read`, the
            // CAS has succeeded and we insert while the guard is held.
            let tasks_guard = self.tasks.read().await;
            let seq_now = tasks_guard
                .get(&task_key)
                .map(|t| t.latest_event_sequence)
                .unwrap_or(0);
            if seq_now != seq_read {
                drop(tasks_guard);
                if attempt + 1 >= MAX_ATTEMPTS {
                    return Err(A2aStorageError::CreateConfigCasTimeout {
                        tenant: tenant.into(),
                        task_id: config.task_id.clone(),
                    });
                }
                let ms = backoffs_ms[attempt as usize];
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                continue;
            }

            self.push_configs.write().await.insert(
                push_key,
                StoredPushConfig {
                    config: config.clone(),
                    registered_after_event_sequence: seq_read,
                },
            );
            drop(tasks_guard);
            return Ok(config);
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
        let key = (
            tenant.to_string(),
            task_id.to_string(),
            config_id.to_string(),
        );
        Ok(self
            .push_configs
            .read()
            .await
            .get(&key)
            .map(|s| s.config.clone()))
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
            .map(|(_, v)| v.config.clone())
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
        let key = (
            tenant.to_string(),
            task_id.to_string(),
            config_id.to_string(),
        );
        self.push_configs.write().await.remove(&key);
        Ok(()) // Idempotent
    }

    async fn list_configs_eligible_at_event(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        page_token: Option<&str>,
        page_size: Option<i32>,
    ) -> Result<PushConfigListPage, A2aStorageError> {
        let configs = self.push_configs.read().await;
        let mut matching: Vec<_> = configs
            .iter()
            .filter(|((t, tid, _), stored)| {
                t == tenant
                    && tid == task_id
                    && stored.registered_after_event_sequence < event_sequence
            })
            .map(|(_, v)| v.config.clone())
            .collect();

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

    async fn latest_sequence(&self, tenant: &str, task_id: &str) -> Result<u64, A2aStorageError> {
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
        // Acquire all locks in consistent order: tasks → event_counters → events
        let mut tasks = self.tasks.write().await;
        let mut counters = self.event_counters.write().await;
        let mut event_map = self.events.write().await;

        let task_key = (tenant.to_string(), task.id().to_string());
        let event_key = (tenant.to_string(), task.id().to_string());

        // Append events with atomic sequence assignment first so we
        // know the max sequence to stamp on the task row.
        let mut sequences = Vec::with_capacity(events.len());
        let task_events = event_map.entry(event_key.clone()).or_default();
        let counter = counters.entry(event_key).or_insert(0);

        for event in events {
            *counter += 1;
            let seq = *counter;
            sequences.push(seq);
            task_events.push((seq, event));
        }

        // latest_event_sequence = max(0, new_seqs). Maintained
        // UNCONDITIONALLY per ADR-013 §6.3 so the causal floor stays
        // valid if push is enabled later.
        let latest_event_sequence = sequences.iter().copied().max().unwrap_or(0);

        tasks.insert(
            task_key,
            StoredTask {
                tenant: tenant.to_string(),
                owner: owner.to_string(),
                task: task.clone(),
                updated_at: now_iso(),
                cancel_requested: false,
                latest_event_sequence,
            },
        );

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
        // Acquire all locks in consistent order:
        // tasks → event_counters → events → pending_dispatches.
        // The last guard is held only when the push_dispatch opt-in is on
        // so the non-push path never contends on it.
        let mut tasks = self.tasks.write().await;
        let mut counters = self.event_counters.write().await;
        let mut event_map = self.events.write().await;
        let mut pending_dispatches = if self.push_dispatch_enabled {
            Some(self.pending_dispatches.write().await)
        } else {
            None
        };

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
                        current_state: crate::storage::terminal_cas::task_state_wire_name(s)
                            .to_string(),
                    }
                }
                other => A2aStorageError::TypeError(other),
            },
        )?;

        // Update task status
        let mut proto = stored.task.as_proto().clone();
        proto.status = Some(new_status.into_proto());
        let updated_task = Task::try_from(proto).map_err(A2aStorageError::TypeError)?;

        // Preserve cancel_requested (monotonic) and snapshot previous
        // latest_event_sequence; the final value is max(prev, new_seqs).
        let cancel_requested = stored.cancel_requested;
        let prev_latest = stored.latest_event_sequence;

        // Append events with atomic sequence assignment. When the
        // push_dispatch opt-in is on, also write a pending-dispatch
        // marker for each terminal StatusUpdate in the same atomic
        // boundary (ADR-013 §4.3 / §5.1).
        let mut sequences = Vec::with_capacity(events.len());
        let task_events = event_map.entry(event_key.clone()).or_default();
        let counter = counters.entry(event_key).or_insert(0);

        for event in events {
            *counter += 1;
            let seq = *counter;
            sequences.push(seq);

            if let Some(guard) = pending_dispatches.as_mut() {
                if event.is_terminal() && matches!(event, StreamEvent::StatusUpdate { .. }) {
                    guard.insert(
                        (tenant.to_string(), task_id.to_string(), seq),
                        StoredPendingDispatch {
                            owner: owner.to_string(),
                            recorded_at: std::time::SystemTime::now(),
                        },
                    );
                }
            }

            task_events.push((seq, event));
        }

        // Maintain latest_event_sequence UNCONDITIONALLY — ADR-013 §6.3.
        let new_latest = sequences
            .iter()
            .copied()
            .max()
            .map(|m| m.max(prev_latest))
            .unwrap_or(prev_latest);

        tasks.insert(
            task_key,
            StoredTask {
                tenant: tenant.to_string(),
                owner: owner.to_string(),
                task: updated_task.clone(),
                updated_at: now_iso(),
                cancel_requested,
                latest_event_sequence: new_latest,
            },
        );

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

        // Terminal-preservation CAS (ADR-010 §7.1 extension): if the
        // persisted task is already terminal, refuse the write and
        // commit nothing — a full-task replacement must not silently
        // overwrite a terminal committed by a concurrent writer.
        if let Some(stored) = tasks.get(&task_key) {
            if let Some(status) = stored.task.status() {
                if let Ok(state) = status.state() {
                    if turul_a2a_types::state_machine::is_terminal(state) {
                        return Err(A2aStorageError::TerminalStateAlreadySet {
                            task_id: task.id().to_string(),
                            current_state: crate::storage::terminal_cas::task_state_wire_name(
                                state,
                            )
                            .to_string(),
                        });
                    }
                }
            }
        }

        // Preserve cancel_requested (monotonic) and snapshot
        // latest_event_sequence so it can extend (never shrink) even
        // when the full-task replacement rewrites task JSON.
        let (cancel_requested, prev_latest) = tasks
            .get(&task_key)
            .map(|s| (s.cancel_requested, s.latest_event_sequence))
            .unwrap_or((false, 0));

        // Append events with atomic sequence assignment.
        let mut sequences = Vec::with_capacity(events.len());
        let task_events = event_map.entry(event_key.clone()).or_default();
        let counter = counters.entry(event_key).or_insert(0);

        for event in events {
            *counter += 1;
            let seq = *counter;
            sequences.push(seq);
            task_events.push((seq, event));
        }

        // Maintain latest_event_sequence UNCONDITIONALLY — ADR-013 §6.3.
        let new_latest = sequences
            .iter()
            .copied()
            .max()
            .map(|m| m.max(prev_latest))
            .unwrap_or(prev_latest);

        tasks.insert(
            task_key,
            StoredTask {
                tenant: tenant.to_string(),
                owner: owner.to_string(),
                task,
                updated_at: now_iso(),
                cancel_requested,
                latest_event_sequence: new_latest,
            },
        );

        Ok(sequences)
    }
}

#[async_trait]
impl A2aPushDeliveryStore for InMemoryA2aStorage {
    fn backend_name(&self) -> &'static str {
        "in-memory"
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
        let key: PushDeliveryKey = (
            tenant.to_string(),
            task_id.to_string(),
            event_sequence,
            config_id.to_string(),
        );
        let now = std::time::SystemTime::now();
        let expires_at = now + claim_expiry;
        let mut claims = self.push_delivery_claims.write().await;

        if let Some(existing) = claims.get(&key) {
            let is_terminal = matches!(
                existing.status,
                ClaimStatus::Succeeded | ClaimStatus::GaveUp | ClaimStatus::Abandoned
            );
            let still_live = existing.expires_at > now;
            if is_terminal || still_live {
                return Err(A2aStorageError::ClaimAlreadyHeld {
                    tenant: tenant.to_string(),
                    task_id: task_id.to_string(),
                    event_sequence,
                    config_id: config_id.to_string(),
                });
            }
            // Re-claim after expiry: advance generation; preserve
            // delivery_attempt_count AND owner (owner is a property of
            // the underlying push config, not the claimant); reset
            // status to Pending; refresh claimed_at and expires_at;
            // replace claimant.
            let new_claim = StoredDeliveryClaim {
                claimant: claimant.to_string(),
                owner: existing.owner.clone(),
                generation: existing.generation + 1,
                claimed_at: now,
                expires_at,
                delivery_attempt_count: existing.delivery_attempt_count,
                status: ClaimStatus::Pending,
                first_attempted_at: existing.first_attempted_at,
                last_attempted_at: existing.last_attempted_at,
                last_http_status: existing.last_http_status,
                last_error_class: existing.last_error_class,
                gave_up_at: None,
                gave_up_reason: None,
                abandoned_reason: None,
            };
            let out = new_claim.as_claim();
            claims.insert(key, new_claim);
            return Ok(out);
        }

        let fresh = StoredDeliveryClaim {
            claimant: claimant.to_string(),
            owner: owner.to_string(),
            generation: 1,
            claimed_at: now,
            expires_at,
            delivery_attempt_count: 0,
            status: ClaimStatus::Pending,
            first_attempted_at: None,
            last_attempted_at: None,
            last_http_status: None,
            last_error_class: None,
            gave_up_at: None,
            gave_up_reason: None,
            abandoned_reason: None,
        };
        let out = fresh.as_claim();
        claims.insert(key, fresh);
        Ok(out)
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
        let key: PushDeliveryKey = (
            tenant.to_string(),
            task_id.to_string(),
            event_sequence,
            config_id.to_string(),
        );
        let now = std::time::SystemTime::now();
        let mut claims = self.push_delivery_claims.write().await;
        let row = claims
            .get_mut(&key)
            .ok_or_else(|| A2aStorageError::StaleDeliveryClaim {
                tenant: tenant.to_string(),
                task_id: task_id.to_string(),
                event_sequence,
                config_id: config_id.to_string(),
            })?;
        if row.claimant != claimant || row.generation != claim_generation {
            return Err(A2aStorageError::StaleDeliveryClaim {
                tenant: tenant.to_string(),
                task_id: task_id.to_string(),
                event_sequence,
                config_id: config_id.to_string(),
            });
        }
        // Terminal rows are frozen: a matching (claimant, generation)
        // that has already committed a terminal cannot restart an
        // attempt. Same signal as fencing — the retry loop must stop.
        if matches!(
            row.status,
            ClaimStatus::Succeeded | ClaimStatus::GaveUp | ClaimStatus::Abandoned
        ) {
            return Err(A2aStorageError::StaleDeliveryClaim {
                tenant: tenant.to_string(),
                task_id: task_id.to_string(),
                event_sequence,
                config_id: config_id.to_string(),
            });
        }
        row.delivery_attempt_count = row.delivery_attempt_count.saturating_add(1);
        if matches!(row.status, ClaimStatus::Pending) {
            row.status = ClaimStatus::Attempting;
        }
        if row.first_attempted_at.is_none() {
            row.first_attempted_at = Some(now);
        }
        row.last_attempted_at = Some(now);
        Ok(row.delivery_attempt_count)
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
        let key: PushDeliveryKey = (
            tenant.to_string(),
            task_id.to_string(),
            event_sequence,
            config_id.to_string(),
        );
        let now = std::time::SystemTime::now();
        let mut claims = self.push_delivery_claims.write().await;
        let row = claims
            .get_mut(&key)
            .ok_or_else(|| A2aStorageError::StaleDeliveryClaim {
                tenant: tenant.to_string(),
                task_id: task_id.to_string(),
                event_sequence,
                config_id: config_id.to_string(),
            })?;
        if row.claimant != claimant || row.generation != claim_generation {
            return Err(A2aStorageError::StaleDeliveryClaim {
                tenant: tenant.to_string(),
                task_id: task_id.to_string(),
                event_sequence,
                config_id: config_id.to_string(),
            });
        }

        // Terminal rows are frozen: once Succeeded/GaveUp/Abandoned
        // has been committed, any subsequent outcome — same terminal
        // (idempotent) or different (cross-over) — is a no-op. The
        // stored row is never mutated away from its terminal. This
        // protects against dispatcher double-dispatch AND against a
        // worker that confuses its own state.
        if matches!(
            row.status,
            ClaimStatus::Succeeded | ClaimStatus::GaveUp | ClaimStatus::Abandoned
        ) {
            return Ok(());
        }

        match outcome {
            DeliveryOutcome::Succeeded { http_status } => {
                row.status = ClaimStatus::Succeeded;
                row.last_http_status = Some(http_status);
                // Ensure first/last_attempted_at exist even if the
                // worker skipped record_attempt_started (e.g.,
                // immediate success paths in tests).
                if row.first_attempted_at.is_none() {
                    row.first_attempted_at = Some(now);
                }
                row.last_attempted_at = Some(now);
            }
            DeliveryOutcome::Retry {
                http_status,
                error_class,
                next_attempt_at: _,
            } => {
                // Retry never changes generation/claimant; keeps the
                // claim in Attempting and updates diagnostics. Does
                // NOT increment delivery_attempt_count.
                row.status = ClaimStatus::Attempting;
                row.last_http_status = http_status;
                row.last_error_class = Some(error_class);
                row.last_attempted_at = Some(now);
            }
            DeliveryOutcome::GaveUp {
                reason,
                last_error_class,
                last_http_status,
            } => {
                row.status = ClaimStatus::GaveUp;
                row.gave_up_at = Some(now);
                row.gave_up_reason = Some(reason);
                row.last_error_class = Some(last_error_class);
                row.last_http_status = last_http_status;
                // Pre-POST giveups (SSRF, payload too large) never
                // recorded first/last_attempted_at; use claim time
                // so FailedDelivery has valid timestamps.
                if row.first_attempted_at.is_none() {
                    row.first_attempted_at = Some(row.claimed_at);
                }
                if row.last_attempted_at.is_none() {
                    row.last_attempted_at = Some(now);
                }
            }
            DeliveryOutcome::Abandoned { reason } => {
                row.status = ClaimStatus::Abandoned;
                row.abandoned_reason = Some(reason);
            }
        }
        Ok(())
    }

    async fn sweep_expired_claims(&self) -> Result<u64, A2aStorageError> {
        let now = std::time::SystemTime::now();
        let claims = self.push_delivery_claims.read().await;
        let mut count: u64 = 0;
        for row in claims.values() {
            if matches!(row.status, ClaimStatus::Pending | ClaimStatus::Attempting)
                && row.expires_at < now
            {
                count += 1;
            }
        }
        Ok(count)
    }

    async fn list_reclaimable_claims(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::push::claim::ReclaimableClaim>, A2aStorageError> {
        let now = std::time::SystemTime::now();
        let claims = self.push_delivery_claims.read().await;
        let mut out: Vec<crate::push::claim::ReclaimableClaim> = claims
            .iter()
            .filter_map(|((tenant, task_id, event_sequence, config_id), row)| {
                if !matches!(row.status, ClaimStatus::Pending | ClaimStatus::Attempting) {
                    return None;
                }
                if row.expires_at >= now {
                    return None;
                }
                Some(crate::push::claim::ReclaimableClaim {
                    tenant: tenant.clone(),
                    owner: row.owner.clone(),
                    task_id: task_id.clone(),
                    event_sequence: *event_sequence,
                    config_id: config_id.clone(),
                })
            })
            .collect();
        out.truncate(limit);
        Ok(out)
    }

    async fn record_pending_dispatch(
        &self,
        tenant: &str,
        owner: &str,
        task_id: &str,
        event_sequence: u64,
    ) -> Result<(), A2aStorageError> {
        let key = (tenant.to_string(), task_id.to_string(), event_sequence);
        let mut map = self.pending_dispatches.write().await;
        // Idempotent: a repeat record refreshes recorded_at so
        // in-progress redispatches don't trip the staleness threshold.
        map.insert(
            key,
            StoredPendingDispatch {
                owner: owner.to_string(),
                recorded_at: std::time::SystemTime::now(),
            },
        );
        Ok(())
    }

    async fn delete_pending_dispatch(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
    ) -> Result<(), A2aStorageError> {
        let key = (tenant.to_string(), task_id.to_string(), event_sequence);
        let mut map = self.pending_dispatches.write().await;
        map.remove(&key);
        Ok(())
    }

    async fn list_stale_pending_dispatches(
        &self,
        older_than_recorded_at: std::time::SystemTime,
        limit: usize,
    ) -> Result<Vec<crate::push::claim::PendingDispatch>, A2aStorageError> {
        let map = self.pending_dispatches.read().await;
        let mut out: Vec<crate::push::claim::PendingDispatch> = map
            .iter()
            .filter_map(|((tenant, task_id, seq), row)| {
                if row.recorded_at <= older_than_recorded_at {
                    Some(crate::push::claim::PendingDispatch {
                        tenant: tenant.clone(),
                        owner: row.owner.clone(),
                        task_id: task_id.clone(),
                        event_sequence: *seq,
                        recorded_at: row.recorded_at,
                    })
                } else {
                    None
                }
            })
            .collect();
        out.truncate(limit);
        Ok(out)
    }

    async fn list_failed_deliveries(
        &self,
        tenant: &str,
        since: std::time::SystemTime,
        limit: usize,
    ) -> Result<Vec<FailedDelivery>, A2aStorageError> {
        let claims = self.push_delivery_claims.read().await;
        let mut out: Vec<FailedDelivery> = claims
            .iter()
            .filter_map(|((row_tenant, task_id, event_sequence, config_id), row)| {
                if row_tenant != tenant || !matches!(row.status, ClaimStatus::GaveUp) {
                    return None;
                }
                let gave_up_at = row.gave_up_at?;
                if gave_up_at < since {
                    return None;
                }
                Some(FailedDelivery {
                    task_id: task_id.clone(),
                    config_id: config_id.clone(),
                    event_sequence: *event_sequence,
                    first_attempted_at: row.first_attempted_at.unwrap_or(row.claimed_at),
                    last_attempted_at: row.last_attempted_at.unwrap_or(gave_up_at),
                    gave_up_at,
                    delivery_attempt_count: row.delivery_attempt_count,
                    last_http_status: row.last_http_status,
                    last_error_class: row
                        .last_error_class
                        .unwrap_or(DeliveryErrorClass::NetworkError),
                })
            })
            .collect();
        // Newest-first by gave_up_at.
        out.sort_by_key(|r| std::cmp::Reverse(r.gave_up_at));
        out.truncate(limit);
        Ok(out)
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

    // Terminal-write CAS (ADR-010 §7.1) parity tests.

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
    async fn test_update_task_with_events_rejects_terminal_already_set() {
        let s = storage();
        parity_tests::test_update_task_with_events_rejects_terminal_already_set(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_invalid_transition_distinct_from_terminal_already_set() {
        let s = storage();
        parity_tests::test_invalid_transition_distinct_from_terminal_already_set(&s, &s).await;
    }

    // Cancel-marker parity (ADR-012).

    #[tokio::test]
    async fn test_cancel_marker_roundtrip() {
        let s = storage();
        parity_tests::test_cancel_marker_roundtrip(&s, &s).await;
    }

    #[tokio::test]
    async fn test_supervisor_list_cancel_requested_parity() {
        let s = storage();
        parity_tests::test_supervisor_list_cancel_requested_parity(&s, &s, &s).await;
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

    // =========================================================
    // A2aPushDeliveryStore parity (ADR-011 §10).
    // =========================================================

    #[tokio::test]
    async fn test_push_claim_is_exclusive() {
        let s = storage();
        parity_tests::test_push_claim_is_exclusive(&s).await;
    }

    #[tokio::test]
    async fn test_push_claim_expired_is_reclaimable() {
        let s = storage();
        parity_tests::test_push_claim_expired_is_reclaimable(&s).await;
    }

    #[tokio::test]
    async fn test_push_outcome_fenced_to_current_claim() {
        let s = storage();
        parity_tests::test_push_outcome_fenced_to_current_claim(&s).await;
    }

    #[tokio::test]
    async fn test_push_claim_terminal_succeeded_blocks_reclaim() {
        let s = storage();
        parity_tests::test_push_claim_terminal_succeeded_blocks_reclaim(&s).await;
    }

    #[tokio::test]
    async fn test_push_claim_terminal_gaveup_blocks_reclaim() {
        let s = storage();
        parity_tests::test_push_claim_terminal_gaveup_blocks_reclaim(&s).await;
    }

    #[tokio::test]
    async fn test_push_claim_terminal_abandoned_blocks_reclaim_and_not_listed() {
        let s = storage();
        parity_tests::test_push_claim_terminal_abandoned_blocks_reclaim_and_not_listed(&s).await;
    }

    #[tokio::test]
    async fn test_push_attempt_started_advances_count_and_status() {
        let s = storage();
        parity_tests::test_push_attempt_started_advances_count_and_status(&s).await;
    }

    #[tokio::test]
    async fn test_push_attempt_started_is_fenced() {
        let s = storage();
        parity_tests::test_push_attempt_started_is_fenced(&s).await;
    }

    #[tokio::test]
    async fn test_push_retry_outcome_keeps_claim_open() {
        let s = storage();
        parity_tests::test_push_retry_outcome_keeps_claim_open(&s).await;
    }

    #[tokio::test]
    async fn test_push_outcome_idempotent_on_terminal() {
        let s = storage();
        parity_tests::test_push_outcome_idempotent_on_terminal(&s).await;
    }

    #[tokio::test]
    async fn test_push_sweep_counts_expired_nonterminal_and_preserves_status() {
        let s = storage();
        parity_tests::test_push_sweep_counts_expired_nonterminal_and_preserves_status(&s).await;
    }

    #[tokio::test]
    async fn test_push_list_reclaimable_filters_and_returns_identity() {
        let s = storage();
        parity_tests::test_push_list_reclaimable_filters_and_returns_identity(&s).await;
    }

    #[tokio::test]
    async fn test_push_list_failed_filters_and_orders() {
        let s = storage();
        parity_tests::test_push_list_failed_filters_and_orders(&s).await;
    }

    #[tokio::test]
    async fn test_push_list_failed_is_tenant_scoped() {
        let s = storage();
        parity_tests::test_push_list_failed_is_tenant_scoped(&s).await;
    }

    #[tokio::test]
    async fn test_push_failed_delivery_diagnostics_roundtrip() {
        let s = storage();
        parity_tests::test_push_failed_delivery_diagnostics_roundtrip(&s).await;
    }

    #[tokio::test]
    async fn test_push_attempt_started_rejected_after_terminal() {
        let s = storage();
        parity_tests::test_push_attempt_started_rejected_after_terminal(&s).await;
    }

    #[tokio::test]
    async fn test_push_outcome_does_not_overwrite_terminal() {
        let s = storage();
        parity_tests::test_push_outcome_does_not_overwrite_terminal(&s).await;
    }

    #[tokio::test]
    async fn test_push_concurrent_claim_race() {
        let s = std::sync::Arc::new(storage());
        parity_tests::test_push_concurrent_claim_race(s).await;
    }

    // Atomic pending-dispatch marker parity (ADR-013 §4.3 / §10.1).

    fn opted_in_storage() -> InMemoryA2aStorage {
        InMemoryA2aStorage::new().with_push_dispatch_enabled(true)
    }

    #[tokio::test]
    async fn test_atomic_marker_written_for_terminal_status() {
        let s = opted_in_storage();
        parity_tests::test_atomic_marker_written_for_terminal_status(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_marker_skipped_for_non_terminal_status() {
        let s = opted_in_storage();
        parity_tests::test_atomic_marker_skipped_for_non_terminal_status(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_marker_skipped_for_artifact_event() {
        let s = opted_in_storage();
        parity_tests::test_atomic_marker_skipped_for_artifact_event(&s, &s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_marker_absent_when_opt_in_off() {
        let s = storage(); // default: push_dispatch_enabled=false
        parity_tests::test_atomic_marker_absent_when_opt_in_off(&s, &s, &s).await;
    }

    // Causal-floor eligibility parity (ADR-013 §4.5 / §10.3 / §10.4).

    #[tokio::test]
    async fn test_config_registered_at_or_after_event_not_eligible() {
        let s = storage();
        parity_tests::test_config_registered_at_or_after_event_not_eligible(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_late_create_config_stamps_advanced_sequence() {
        let s = storage();
        parity_tests::test_late_create_config_stamps_advanced_sequence(&s, &s, &s).await;
    }

    /// ADR-013 §10.4 — real interleaving race.
    ///
    /// Uses the `CasHook` instrumentation to force `create_config` to
    /// pause AFTER its initial `latest_event_sequence` read and BEFORE
    /// the re-read/insert. While paused, a concurrent
    /// `update_task_status_with_events` commits a terminal event that
    /// advances the floor. When `create_config` resumes, its CAS
    /// re-read detects the mismatch, the backoff fires, and the next
    /// attempt stamps the config with the ADVANCED sequence — not the
    /// stale one. Fan-out for the terminal event then excludes the
    /// config.
    ///
    /// A backend without a CAS retry loop would fail this test:
    /// create_config would write the stale sequence and the config
    /// would incorrectly appear in the terminal event's eligible set.
    #[tokio::test]
    async fn test_create_config_cas_retries_under_interleaving() {
        use crate::storage::A2aAtomicStore;
        use crate::streaming::{ArtifactUpdatePayload, StreamEvent};
        use turul_a2a_types::TaskState;

        let s = storage();
        let tenant = "t-race";
        let task_id = "task-race";
        let owner = "owner-1";

        // Seed Working, bump latest_event_sequence to 3.
        let task =
            Task::new(task_id, TaskStatus::new(TaskState::Working)).with_context_id("ctx-race");
        A2aTaskStorage::create_task(&s, tenant, owner, task.clone())
            .await
            .expect("seed");
        for i in 0..3 {
            let evt = StreamEvent::ArtifactUpdate {
                artifact_update: ArtifactUpdatePayload {
                    task_id: task_id.into(),
                    context_id: "ctx-race".into(),
                    artifact: serde_json::json!({"id": format!("a-{i}"), "parts": []}),
                    append: false,
                    last_chunk: true,
                },
            };
            s.update_task_with_events(tenant, owner, task.clone(), vec![evt])
                .await
                .expect("bump");
        }

        // Install the CAS race hook. create_config will pause after
        // reading latest_event_sequence (=3) and await `resume`.
        let hook = s.install_cas_race_hook().await;

        let storage_for_task = s.clone();
        let create_handle = tokio::spawn(async move {
            A2aPushNotificationStorage::create_config(
                &storage_for_task,
                tenant,
                turul_a2a_proto::TaskPushNotificationConfig {
                    tenant: tenant.into(),
                    id: "cfg-race".into(),
                    task_id: task_id.into(),
                    url: "https://example.invalid/race".into(),
                    token: String::new(),
                    authentication: None,
                },
            )
            .await
        });

        // Wait for create_config to reach the pause point.
        hook.paused.notified().await;

        // Sanity: create_config has NOT yet inserted — the config map
        // must still be empty at this moment.
        {
            let pc = s.push_configs.read().await;
            assert!(
                pc.is_empty(),
                "create_config must not have written yet; state: {pc:?}"
            );
        }

        // Concurrent commit advances latest_event_sequence to 4
        // (terminal). The hook is cleared so this call itself does
        // not re-pause.
        *s.cas_hook.write().await = None;
        let terminal = StreamEvent::StatusUpdate {
            status_update: crate::streaming::StatusUpdatePayload {
                task_id: task_id.into(),
                context_id: "ctx-race".into(),
                status: serde_json::to_value(TaskStatus::new(TaskState::Completed)).unwrap(),
            },
        };
        let (_t, seqs) = s
            .update_task_status_with_events(
                tenant,
                task_id,
                owner,
                TaskStatus::new(TaskState::Completed),
                vec![terminal],
            )
            .await
            .expect("terminal commit");
        let terminal_seq = seqs[0];
        assert_eq!(terminal_seq, 4);

        // Release create_config. Its re-read will see seq=4 ≠ 3, retry,
        // and stamp the config with seq=4.
        hook.resume.notify_waiters();
        create_handle
            .await
            .expect("join")
            .expect("create_config eventually succeeds after retry");

        // Proof: the created config is NOT eligible for the terminal
        // event. Strict less-than: 4 < 4 is false.
        let eligible = A2aPushNotificationStorage::list_configs_eligible_at_event(
            &s,
            tenant,
            task_id,
            terminal_seq,
            None,
            Some(100),
        )
        .await
        .expect("list eligible");
        assert!(
            eligible.configs.is_empty(),
            "config written concurrently with the terminal commit MUST NOT be eligible \
             for that terminal event (CAS retry must have advanced its floor); got {:?}",
            eligible.configs.iter().map(|c| &c.id).collect::<Vec<_>>()
        );

        // And the config IS actually stored (registered_after_event_sequence = 4).
        let cfg = A2aPushNotificationStorage::get_config(&s, tenant, task_id, "cfg-race")
            .await
            .expect("get");
        assert!(cfg.is_some(), "config must exist after CAS retry completed");
        let pc = s.push_configs.read().await;
        let stored = pc
            .get(&(tenant.to_string(), task_id.to_string(), "cfg-race".into()))
            .expect("stored row");
        assert_eq!(
            stored.registered_after_event_sequence, terminal_seq,
            "CAS retry must have re-stamped with the advanced sequence (4), not the stale one (3)"
        );
    }
}
