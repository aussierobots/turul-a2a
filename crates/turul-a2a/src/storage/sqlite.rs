//! SQLite storage backend for A2A tasks, push configs, events, and atomic operations.

use async_trait::async_trait;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use turul_a2a_types::{Artifact, Message, Task, TaskState, TaskStatus};

use crate::push::{
    A2aPushDeliveryStore, AbandonedReason, ClaimStatus, DeliveryClaim, DeliveryErrorClass,
    DeliveryOutcome, FailedDelivery, GaveUpReason,
};
use crate::streaming::StreamEvent;
use super::atomic::A2aAtomicStore;
use super::error::A2aStorageError;
use super::event_store::A2aEventStore;
use super::filter::{PushConfigListPage, TaskFilter, TaskListPage};
use super::traits::{A2aPushNotificationStorage, A2aTaskStorage};

/// SQLite storage configuration.
#[derive(Debug, Clone)]
pub struct SqliteConfig {
    pub database_url: String,
    pub max_connections: u32,
}

impl Default for SqliteConfig {
    fn default() -> Self {
        Self {
            database_url: "sqlite::memory:".into(),
            max_connections: 5,
        }
    }
}

/// SQLite-backed A2A storage.
#[derive(Clone)]
pub struct SqliteA2aStorage {
    pool: SqlitePool,
    /// ADR-013 §4.3 opt-in — see `InMemoryA2aStorage::push_dispatch_enabled`.
    push_dispatch_enabled: bool,
}

impl SqliteA2aStorage {
    pub async fn new(config: SqliteConfig) -> Result<Self, A2aStorageError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(config.max_connections)
            .connect(&config.database_url)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let storage = Self {
            pool,
            push_dispatch_enabled: false,
        };
        storage.create_tables().await?;
        Ok(storage)
    }

    /// Opt in to atomic pending-dispatch marker writes (ADR-013 §4.3).
    pub fn with_push_dispatch_enabled(mut self, enabled: bool) -> Self {
        self.push_dispatch_enabled = enabled;
        self
    }

    async fn create_tables(&self) -> Result<(), A2aStorageError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS a2a_tasks (
                tenant TEXT NOT NULL DEFAULT '',
                task_id TEXT NOT NULL,
                owner TEXT NOT NULL,
                task_json TEXT NOT NULL,
                context_id TEXT NOT NULL DEFAULT '',
                status_state TEXT NOT NULL DEFAULT '',
                cancel_requested INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (tenant, task_id)
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        // Additive migration for pre-0.1.4 deployments: add the ADR-012
        // cancel_requested column if the table already exists without it.
        // SQLite has no conditional ADD COLUMN, so we attempt the ALTER
        // and ignore the duplicate-column error. This is the standard
        // idempotent migration pattern for SQLite.
        let alter_result =
            sqlx::query("ALTER TABLE a2a_tasks ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0")
                .execute(&self.pool)
                .await;
        match alter_result {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column") => {
                // Column already present — expected on fresh installs.
            }
            Err(e) => return Err(A2aStorageError::DatabaseError(e.to_string())),
        }

        // ADR-013 §6.3: unconditional latest_event_sequence column.
        // Legacy rows default to 0; the first post-migration commit
        // extends it monotonically via MAX(existing, new).
        let alter_latest =
            sqlx::query("ALTER TABLE a2a_tasks ADD COLUMN latest_event_sequence INTEGER NOT NULL DEFAULT 0")
                .execute(&self.pool)
                .await;
        match alter_latest {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column") => {}
            Err(e) => return Err(A2aStorageError::DatabaseError(e.to_string())),
        }

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS a2a_push_configs (
                tenant TEXT NOT NULL DEFAULT '',
                task_id TEXT NOT NULL,
                config_id TEXT NOT NULL,
                config_json TEXT NOT NULL,
                registered_after_event_sequence INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (tenant, task_id, config_id)
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        // ADR-013 §6.3: additive migration for pre-0.1.4 push config
        // rows — default 0 is permissive by design (legacy configs
        // become eligible for any event with seq > 0).
        let alter_registered = sqlx::query(
            "ALTER TABLE a2a_push_configs \
             ADD COLUMN registered_after_event_sequence INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await;
        match alter_registered {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column") => {}
            Err(e) => return Err(A2aStorageError::DatabaseError(e.to_string())),
        }

        // Index to make eligibility scans cheap (ADR-013 §6.3).
        let _ = sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_a2a_push_configs_eligibility \
             ON a2a_push_configs (tenant, task_id, registered_after_event_sequence)",
        )
        .execute(&self.pool)
        .await;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS a2a_task_events (
                tenant TEXT NOT NULL DEFAULT '',
                task_id TEXT NOT NULL,
                event_sequence INTEGER NOT NULL,
                event_data TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (tenant, task_id, event_sequence)
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        // Push-delivery claim table (ADR-011 §10). One row per
        // `(tenant, task_id, event_sequence, config_id)` tuple. The
        // status column is the enum Debug form; timestamps are
        // epoch micros for deterministic ordering without TZ ambiguity.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS a2a_push_deliveries (
                tenant TEXT NOT NULL,
                task_id TEXT NOT NULL,
                event_sequence INTEGER NOT NULL,
                config_id TEXT NOT NULL,
                claimant TEXT NOT NULL,
                owner TEXT NOT NULL DEFAULT '',
                generation INTEGER NOT NULL,
                claimed_at_micros INTEGER NOT NULL,
                expires_at_micros INTEGER NOT NULL,
                delivery_attempt_count INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL,
                first_attempted_at_micros INTEGER,
                last_attempted_at_micros INTEGER,
                last_http_status INTEGER,
                last_error_class TEXT,
                gave_up_at_micros INTEGER,
                gave_up_reason TEXT,
                abandoned_reason TEXT,
                PRIMARY KEY (tenant, task_id, event_sequence, config_id)
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS a2a_push_pending_dispatches (
                tenant TEXT NOT NULL,
                task_id TEXT NOT NULL,
                event_sequence INTEGER NOT NULL,
                owner TEXT NOT NULL,
                recorded_at_micros INTEGER NOT NULL,
                PRIMARY KEY (tenant, task_id, event_sequence)
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        Ok(())
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

    /// Apply history_length trimming to a task.
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
impl A2aTaskStorage for SqliteA2aStorage {
    fn backend_name(&self) -> &'static str {
        "sqlite"
    }

    async fn create_task(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
    ) -> Result<Task, A2aStorageError> {
        let json = Self::task_to_json(&task)?;
        let state_str = Self::status_state_str(&task);
        sqlx::query(
            "INSERT INTO a2a_tasks (tenant, task_id, owner, task_json, context_id, status_state, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, datetime('now'))",
        )
        .bind(tenant)
        .bind(task.id())
        .bind(owner)
        .bind(&json)
        .bind(task.context_id())
        .bind(&state_str)
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(task)
    }

    async fn get_task(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        history_length: Option<i32>,
    ) -> Result<Option<Task>, A2aStorageError> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT task_json FROM a2a_tasks WHERE tenant = ? AND task_id = ? AND owner = ?",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(owner)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        match row {
            Some((json,)) => {
                let task = Self::task_from_json(&json)?;
                Ok(Some(Self::trim_task(task, history_length, true)))
            }
            None => Ok(None),
        }
    }

    async fn update_task(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
    ) -> Result<(), A2aStorageError> {
        let json = Self::task_to_json(&task)?;
        let state_str = Self::status_state_str(&task);
        let result = sqlx::query(
            "UPDATE a2a_tasks SET task_json = ?, status_state = ?, context_id = ?, updated_at = datetime('now')
             WHERE tenant = ? AND task_id = ? AND owner = ?",
        )
        .bind(&json)
        .bind(&state_str)
        .bind(task.context_id())
        .bind(tenant)
        .bind(task.id())
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(A2aStorageError::TaskNotFound(task.id().to_string()));
        }
        Ok(())
    }

    async fn delete_task(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
    ) -> Result<bool, A2aStorageError> {
        let result = sqlx::query(
            "DELETE FROM a2a_tasks WHERE tenant = ? AND task_id = ? AND owner = ?",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(result.rows_affected() > 0)
    }

    async fn list_tasks(&self, filter: TaskFilter) -> Result<TaskListPage, A2aStorageError> {
        let tenant = filter.tenant.as_deref().unwrap_or("");
        let owner = filter.owner.as_deref().unwrap_or("");

        // Build WHERE clause
        let mut conditions = vec!["tenant = ?".to_string(), "owner = ?".to_string()];
        if filter.context_id.is_some() {
            conditions.push("context_id = ?".to_string());
        }
        if filter.status.is_some() {
            conditions.push("status_state = ?".to_string());
        }

        let where_clause = conditions.join(" AND ");

        // Count total
        let count_sql = format!("SELECT COUNT(*) FROM a2a_tasks WHERE {where_clause}");
        let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql)
            .bind(tenant)
            .bind(owner);
        if let Some(ref ctx) = filter.context_id {
            count_query = count_query.bind(ctx);
        }
        if let Some(ref status) = filter.status {
            count_query = count_query.bind(format!("{status:?}"));
        }
        let total_size: i64 = count_query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let page_size = filter.page_size.map(|ps| ps.clamp(1, 100)).unwrap_or(50);

        // Fetch page — sorted by updated_at DESC, task_id DESC (spec §3.1.4)
        // Cursor token: "updated_at|task_id" of last item on previous page
        let (select_sql, cursor_parts) = if let Some(ref token) = filter.page_token {
            let parts: Vec<&str> = token.splitn(2, '|').collect();
            let (cursor_time, cursor_id) = if parts.len() == 2 {
                (parts[0].to_string(), parts[1].to_string())
            } else {
                // Legacy cursor — treat as task_id
                (String::new(), token.clone())
            };
            (
                format!(
                    "SELECT task_json, updated_at FROM a2a_tasks WHERE {where_clause} \
                     AND (updated_at < ? OR (updated_at = ? AND task_id < ?)) \
                     ORDER BY updated_at DESC, task_id DESC LIMIT ?"
                ),
                Some((cursor_time, cursor_id)),
            )
        } else {
            (
                format!(
                    "SELECT task_json, updated_at FROM a2a_tasks WHERE {where_clause} \
                     ORDER BY updated_at DESC, task_id DESC LIMIT ?"
                ),
                None,
            )
        };

        let mut select_query = sqlx::query_as::<_, (String, String)>(&select_sql)
            .bind(tenant)
            .bind(owner);
        if let Some(ref ctx) = filter.context_id {
            select_query = select_query.bind(ctx);
        }
        if let Some(ref status) = filter.status {
            select_query = select_query.bind(format!("{status:?}"));
        }
        if let Some((ref ct, ref ci)) = cursor_parts {
            select_query = select_query.bind(ct).bind(ct).bind(ci);
        }
        select_query = select_query.bind(page_size);

        let rows: Vec<(String, String)> = select_query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let include_artifacts = filter.include_artifacts.unwrap_or(false);
        let tasks_with_times: Vec<(Task, String)> = rows
            .iter()
            .filter_map(|(json, updated_at)| {
                Self::task_from_json(json)
                    .ok()
                    .map(|t| (Self::trim_task(t, filter.history_length, include_artifacts), updated_at.clone()))
            })
            .collect();

        let tasks: Vec<Task> = tasks_with_times.iter().map(|(t, _)| t.clone()).collect();

        let next_page_token = if tasks.len() as i32 >= page_size {
            tasks_with_times.last().map(|(t, updated_at)| {
                format!("{}|{}", updated_at, t.id())
            }).unwrap_or_default()
        } else {
            String::new()
        };

        Ok(TaskListPage {
            tasks,
            next_page_token,
            page_size,
            total_size: total_size as i32,
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
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM a2a_tasks")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(count.0 as usize)
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
        // Single SQL round-trip: only update if the row exists with this
        // owner AND is non-terminal. rows_affected == 0 requires a second
        // SELECT to classify (not-found/wrong-owner vs terminal).
        let result = sqlx::query(
            "UPDATE a2a_tasks SET cancel_requested = 1
             WHERE tenant = ? AND task_id = ? AND owner = ?
               AND status_state NOT IN ('Completed', 'Failed', 'Canceled', 'Rejected')",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        if result.rows_affected() > 0 {
            return Ok(());
        }

        // Classify the zero-rows case: read the row (ignoring owner) to
        // decide between TaskNotFound and TerminalState.
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT owner, status_state FROM a2a_tasks
             WHERE tenant = ? AND task_id = ?",
        )
        .bind(tenant)
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        match row {
            None => Err(A2aStorageError::TaskNotFound(task_id.to_string())),
            Some((row_owner, _)) if row_owner != owner => {
                // Anti-enumeration: wrong owner surfaces as TaskNotFound.
                Err(A2aStorageError::TaskNotFound(task_id.to_string()))
            }
            Some((_, state_str)) => {
                // Owner matched but state was terminal.
                let state = match state_str.as_str() {
                    "Completed" => turul_a2a_types::TaskState::Completed,
                    "Failed" => turul_a2a_types::TaskState::Failed,
                    "Canceled" => turul_a2a_types::TaskState::Canceled,
                    "Rejected" => turul_a2a_types::TaskState::Rejected,
                    // Idempotent no-op on a non-terminal row (this arm
                    // is reachable if the UPDATE affected 0 rows because
                    // cancel_requested was already set AND... no actually
                    // the UPDATE sets = 1 regardless of prior value, so
                    // rows_affected is 1 as long as WHERE matches. Falling
                    // here implies something odd; return generic error.
                    other => return Err(A2aStorageError::DatabaseError(format!(
                        "unexpected status_state on set_cancel_requested classify: {other}"
                    ))),
                };
                Err(A2aStorageError::TerminalState(state))
            }
        }
    }
}

#[async_trait]
impl crate::storage::A2aCancellationSupervisor for SqliteA2aStorage {
    fn backend_name(&self) -> &'static str {
        "sqlite"
    }

    async fn supervisor_get_cancel_requested(
        &self,
        tenant: &str,
        task_id: &str,
    ) -> Result<bool, A2aStorageError> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT cancel_requested FROM a2a_tasks
             WHERE tenant = ? AND task_id = ?
               AND status_state NOT IN ('Completed', 'Failed', 'Canceled', 'Rejected')",
        )
        .bind(tenant)
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(row.map(|(v,)| v != 0).unwrap_or(false))
    }

    async fn supervisor_list_cancel_requested(
        &self,
        tenant: &str,
        task_ids: &[String],
    ) -> Result<Vec<String>, A2aStorageError> {
        if task_ids.is_empty() {
            return Ok(Vec::new());
        }
        // Build an `IN (?,?,?...)` clause of the right width. sqlx does
        // not support Vec binding directly for SQLite; explicit parameter
        // expansion is the supported pattern.
        let placeholders = task_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT task_id FROM a2a_tasks
             WHERE tenant = ? AND task_id IN ({placeholders})
               AND cancel_requested = 1
               AND status_state NOT IN ('Completed', 'Failed', 'Canceled', 'Rejected')"
        );
        let mut query = sqlx::query_as::<_, (String,)>(&sql).bind(tenant);
        for id in task_ids {
            query = query.bind(id);
        }
        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }
}

#[async_trait]
impl A2aPushNotificationStorage for SqliteA2aStorage {
    fn backend_name(&self) -> &'static str {
        "sqlite"
    }

    async fn create_config(
        &self,
        tenant: &str,
        mut config: turul_a2a_proto::TaskPushNotificationConfig,
    ) -> Result<turul_a2a_proto::TaskPushNotificationConfig, A2aStorageError> {
        if config.id.is_empty() {
            config.id = uuid::Uuid::now_v7().to_string();
        }
        let json = serde_json::to_string(&config)
            .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

        // ADR-013 §6.4 causal-floor CAS: read `latest_event_sequence`
        // and INSERT the config row with
        // `registered_after_event_sequence = seq_read` in the SAME
        // transaction, conditional on `latest_event_sequence` still
        // equalling `seq_read`. SQLite serialises writers, so the
        // conditional UPDATE that re-writes `latest_event_sequence`
        // to itself within the tx is a cheap detection: if another
        // writer committed a concurrent event commit between our
        // read and our insert, our follow-up zero-row UPDATE surfaces
        // it as a ConcurrentModification signal; the outer retry loop
        // backs off and re-reads.
        const MAX_ATTEMPTS: u32 = 5;
        let backoffs_ms: [u64; 4] = [10, 50, 250, 1000];
        for attempt in 0..MAX_ATTEMPTS {
            let mut tx = self.pool.begin().await
                .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            let latest: Option<(i64,)> = sqlx::query_as(
                "SELECT latest_event_sequence FROM a2a_tasks
                 WHERE tenant = ? AND task_id = ?",
            )
            .bind(tenant)
            .bind(&config.task_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            let seq_read = latest.map(|(s,)| s).unwrap_or(0);

            sqlx::query(
                "INSERT OR REPLACE INTO a2a_push_configs
                    (tenant, task_id, config_id, config_json, registered_after_event_sequence)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(tenant)
            .bind(&config.task_id)
            .bind(&config.id)
            .bind(&json)
            .bind(seq_read)
            .execute(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            // CAS: succeed only if latest_event_sequence still matches
            // what we read. The UPDATE rewrites the same value (idempotent)
            // but the WHERE clause detects a concurrent advance.
            let cas = sqlx::query(
                "UPDATE a2a_tasks SET latest_event_sequence = latest_event_sequence
                 WHERE tenant = ? AND task_id = ? AND latest_event_sequence = ?",
            )
            .bind(tenant)
            .bind(&config.task_id)
            .bind(seq_read)
            .execute(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            // If the task row is missing (seq_read=0, no task yet),
            // cas.rows_affected()=0 is not a CAS loss — it's just
            // "no task row". Accept the insert.
            let task_exists = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM a2a_tasks WHERE tenant = ? AND task_id = ?",
            )
            .bind(tenant)
            .bind(&config.task_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            if task_exists > 0 && cas.rows_affected() == 0 {
                // Concurrent event commit advanced latest_event_sequence.
                // Roll back and retry.
                drop(tx);
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

            tx.commit().await
                .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            return Ok(config);
        }
        // Unreachable — loop returns on every path.
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
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT config_json FROM a2a_push_configs
             WHERE tenant = ? AND task_id = ? AND config_id = ?",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(config_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        match row {
            Some((json,)) => {
                let config = serde_json::from_str(&json)
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

        let (sql, has_token) = if page_token.is_some() {
            (
                "SELECT config_json FROM a2a_push_configs
                 WHERE tenant = ? AND task_id = ? AND config_id > ?
                 ORDER BY config_id LIMIT ?",
                true,
            )
        } else {
            (
                "SELECT config_json FROM a2a_push_configs
                 WHERE tenant = ? AND task_id = ?
                 ORDER BY config_id LIMIT ?",
                false,
            )
        };

        let mut query = sqlx::query_as::<_, (String,)>(sql)
            .bind(tenant)
            .bind(task_id);
        if has_token {
            query = query.bind(page_token.unwrap_or(""));
        }
        query = query.bind(page_size);

        let rows: Vec<(String,)> = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let configs: Vec<turul_a2a_proto::TaskPushNotificationConfig> = rows
            .iter()
            .filter_map(|(json,)| serde_json::from_str(json).ok())
            .collect();

        let next_page_token = if configs.len() as i32 >= page_size {
            configs.last().map(|c| c.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };

        Ok(PushConfigListPage {
            configs,
            next_page_token,
        })
    }

    async fn delete_config(
        &self,
        tenant: &str,
        task_id: &str,
        config_id: &str,
    ) -> Result<(), A2aStorageError> {
        sqlx::query(
            "DELETE FROM a2a_push_configs WHERE tenant = ? AND task_id = ? AND config_id = ?",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(config_id)
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
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
        let (sql, has_token) = if page_token.is_some() {
            (
                "SELECT config_json FROM a2a_push_configs
                 WHERE tenant = ? AND task_id = ?
                   AND registered_after_event_sequence < ?
                   AND config_id > ?
                 ORDER BY config_id LIMIT ?",
                true,
            )
        } else {
            (
                "SELECT config_json FROM a2a_push_configs
                 WHERE tenant = ? AND task_id = ?
                   AND registered_after_event_sequence < ?
                 ORDER BY config_id LIMIT ?",
                false,
            )
        };

        let mut q = sqlx::query_as::<_, (String,)>(sql)
            .bind(tenant)
            .bind(task_id)
            .bind(event_sequence as i64);
        if has_token {
            q = q.bind(page_token.unwrap_or(""));
        }
        q = q.bind(page_size);

        let rows: Vec<(String,)> = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let configs: Vec<turul_a2a_proto::TaskPushNotificationConfig> = rows
            .iter()
            .filter_map(|(json,)| serde_json::from_str(json).ok())
            .collect();

        let next_page_token = if configs.len() as i32 >= page_size {
            configs.last().map(|c| c.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };

        Ok(PushConfigListPage {
            configs,
            next_page_token,
        })
    }
}

#[async_trait]
impl A2aEventStore for SqliteA2aStorage {
    fn backend_name(&self) -> &'static str {
        "sqlite"
    }

    async fn append_event(
        &self,
        tenant: &str,
        task_id: &str,
        event: StreamEvent,
    ) -> Result<u64, A2aStorageError> {
        let event_data = serde_json::to_string(&event)
            .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

        // Single transaction: allocate sequence + insert event.
        // SQLite single-writer model makes MAX+1 safe.
        let mut tx = self.pool.begin().await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let seq: (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(event_sequence), 0) + 1 FROM a2a_task_events
             WHERE tenant = ? AND task_id = ?",
        )
        .bind(tenant)
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        sqlx::query(
            "INSERT INTO a2a_task_events (tenant, task_id, event_sequence, event_data)
             VALUES (?, ?, ?, ?)",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(seq.0)
        .bind(&event_data)
        .execute(&mut *tx)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        tx.commit().await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        Ok(seq.0 as u64)
    }

    async fn get_events_after(
        &self,
        tenant: &str,
        task_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, StreamEvent)>, A2aStorageError> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT event_sequence, event_data FROM a2a_task_events
             WHERE tenant = ? AND task_id = ? AND event_sequence > ?
             ORDER BY event_sequence",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(after_sequence as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let mut events = Vec::with_capacity(rows.len());
        for (seq, data) in rows {
            let event: StreamEvent = serde_json::from_str(&data)
                .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;
            events.push((seq as u64, event));
        }
        Ok(events)
    }

    async fn latest_sequence(
        &self,
        tenant: &str,
        task_id: &str,
    ) -> Result<u64, A2aStorageError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(event_sequence), 0) FROM a2a_task_events
             WHERE tenant = ? AND task_id = ?",
        )
        .bind(tenant)
        .bind(task_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(row.0 as u64)
    }

    async fn cleanup_expired(&self) -> Result<u64, A2aStorageError> {
        // SQLite has no TTL tracking; cleanup is a no-op.
        Ok(0)
    }
}

#[async_trait]
impl A2aAtomicStore for SqliteA2aStorage {
    fn backend_name(&self) -> &'static str {
        "sqlite"
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
        let task_json = Self::task_to_json(&task)?;
        let state_str = Self::status_state_str(&task);

        let mut tx = self.pool.begin().await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        // Insert task
        sqlx::query(
            "INSERT INTO a2a_tasks (tenant, task_id, owner, task_json, context_id, status_state, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, datetime('now'))",
        )
        .bind(tenant)
        .bind(task.id())
        .bind(owner)
        .bind(&task_json)
        .bind(task.context_id())
        .bind(&state_str)
        .execute(&mut *tx)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        // Append events with sequence allocation
        let mut sequences = Vec::with_capacity(events.len());
        for event in &events {
            let seq: (i64,) = sqlx::query_as(
                "SELECT COALESCE(MAX(event_sequence), 0) + 1 FROM a2a_task_events
                 WHERE tenant = ? AND task_id = ?",
            )
            .bind(tenant)
            .bind(task.id())
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            let event_data = serde_json::to_string(event)
                .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

            sqlx::query(
                "INSERT INTO a2a_task_events (tenant, task_id, event_sequence, event_data)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(tenant)
            .bind(task.id())
            .bind(seq.0)
            .bind(&event_data)
            .execute(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            sequences.push(seq.0 as u64);
        }

        // ADR-013 §6.3: maintain latest_event_sequence UNCONDITIONALLY.
        if let Some(&max_seq) = sequences.iter().max() {
            sqlx::query(
                "UPDATE a2a_tasks SET latest_event_sequence = ?
                 WHERE tenant = ? AND task_id = ? AND latest_event_sequence < ?",
            )
            .bind(max_seq as i64)
            .bind(tenant)
            .bind(task.id())
            .bind(max_seq as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        }

        tx.commit().await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

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
        let mut tx = self.pool.begin().await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        // Read current task within transaction
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT task_json FROM a2a_tasks WHERE tenant = ? AND task_id = ? AND owner = ?",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let task = match row {
            Some((json,)) => Self::task_from_json(&json)?,
            None => return Err(A2aStorageError::TaskNotFound(task_id.to_string())),
        };

        // Validate state machine transition
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

        // Update task status
        let mut proto = task.as_proto().clone();
        proto.status = Some(new_status.into_proto());
        let updated_task = Task::try_from(proto).map_err(A2aStorageError::TypeError)?;

        let task_json = Self::task_to_json(&updated_task)?;
        let state_str = Self::status_state_str(&updated_task);

        // Terminal-write CAS (ADR-010 §7.1): the UPDATE's WHERE clause
        // rejects a row whose status_state is already terminal. If rows
        // affected == 0 AFTER we established existence + owner above, a
        // concurrent transaction raced us to commit a terminal first —
        // return TerminalStateAlreadySet (with the current persisted
        // state) and let the transaction roll back; no events persisted.
        let result = sqlx::query(
            "UPDATE a2a_tasks
               SET task_json = ?, status_state = ?, context_id = ?, updated_at = datetime('now')
             WHERE tenant = ? AND task_id = ? AND owner = ?
               AND status_state NOT IN ('Completed', 'Failed', 'Canceled', 'Rejected')",
        )
        .bind(&task_json)
        .bind(&state_str)
        .bind(updated_task.context_id())
        .bind(tenant)
        .bind(task_id)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        if result.rows_affected() == 0 {
            // Concurrent terminal write won. Re-read the now-terminal state
            // for the error. The transaction is dropped without commit.
            let current: Option<(String,)> = sqlx::query_as(
                "SELECT status_state FROM a2a_tasks WHERE tenant = ? AND task_id = ?",
            )
            .bind(tenant)
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            let current_state_str = current
                .map(|(s,)| crate::storage::terminal_cas::debug_state_to_wire_name(&s))
                .unwrap_or_else(|| "TASK_STATE_UNKNOWN".to_string());
            return Err(A2aStorageError::TerminalStateAlreadySet {
                task_id: task_id.to_string(),
                current_state: current_state_str,
            });
        }

        // Append events and, when the push_dispatch opt-in is on,
        // insert a pending-dispatch marker row for each terminal
        // StatusUpdate in the SAME transaction (ADR-013 §4.3).
        let mut sequences = Vec::with_capacity(events.len());
        for event in &events {
            let seq: (i64,) = sqlx::query_as(
                "SELECT COALESCE(MAX(event_sequence), 0) + 1 FROM a2a_task_events
                 WHERE tenant = ? AND task_id = ?",
            )
            .bind(tenant)
            .bind(task_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            let event_data = serde_json::to_string(event)
                .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

            sqlx::query(
                "INSERT INTO a2a_task_events (tenant, task_id, event_sequence, event_data)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(tenant)
            .bind(task_id)
            .bind(seq.0)
            .bind(&event_data)
            .execute(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            if self.push_dispatch_enabled
                && event.is_terminal()
                && matches!(event, StreamEvent::StatusUpdate { .. })
            {
                let now_micros = systime_to_micros(std::time::SystemTime::now());
                sqlx::query(
                    "INSERT INTO a2a_push_pending_dispatches \
                        (tenant, task_id, event_sequence, owner, recorded_at_micros) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT (tenant, task_id, event_sequence) DO UPDATE SET \
                        owner = excluded.owner, \
                        recorded_at_micros = excluded.recorded_at_micros",
                )
                .bind(tenant)
                .bind(task_id)
                .bind(seq.0)
                .bind(owner)
                .bind(now_micros)
                .execute(&mut *tx)
                .await
                .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            }

            sequences.push(seq.0 as u64);
        }

        // ADR-013 §6.3: maintain latest_event_sequence UNCONDITIONALLY.
        if let Some(&max_seq) = sequences.iter().max() {
            sqlx::query(
                "UPDATE a2a_tasks SET latest_event_sequence = ?
                 WHERE tenant = ? AND task_id = ? AND latest_event_sequence < ?",
            )
            .bind(max_seq as i64)
            .bind(tenant)
            .bind(task_id)
            .bind(max_seq as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        }

        tx.commit().await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        Ok((updated_task, sequences))
    }

    async fn update_task_with_events(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
        events: Vec<StreamEvent>,
    ) -> Result<Vec<u64>, A2aStorageError> {
        let task_json = Self::task_to_json(&task)?;
        let state_str = Self::status_state_str(&task);

        let mut tx = self.pool.begin().await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        // Terminal-preservation CAS (ADR-010 §7.1 extension): the UPDATE's
        // WHERE clause excludes terminal status_state values. If the
        // persisted row is terminal the update matches zero rows;
        // a follow-up SELECT disambiguates "task missing" vs "already
        // terminal" so the caller gets the right error.
        let result = sqlx::query(
            "UPDATE a2a_tasks
               SET task_json = ?, status_state = ?, context_id = ?, updated_at = datetime('now')
             WHERE tenant = ? AND task_id = ? AND owner = ?
               AND status_state NOT IN ('Completed', 'Failed', 'Canceled', 'Rejected')",
        )
        .bind(&task_json)
        .bind(&state_str)
        .bind(task.context_id())
        .bind(tenant)
        .bind(task.id())
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        if result.rows_affected() == 0 {
            // Disambiguate: not-found vs terminal-already-set.
            let current: Option<(String,)> = sqlx::query_as(
                "SELECT status_state FROM a2a_tasks WHERE tenant = ? AND task_id = ? AND owner = ?",
            )
            .bind(tenant)
            .bind(task.id())
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            return match current {
                Some((state,)) => Err(A2aStorageError::TerminalStateAlreadySet {
                    task_id: task.id().to_string(),
                    current_state:
                        crate::storage::terminal_cas::debug_state_to_wire_name(&state),
                }),
                None => Err(A2aStorageError::TaskNotFound(task.id().to_string())),
            };
        }

        // Append events
        let mut sequences = Vec::with_capacity(events.len());
        for event in &events {
            let seq: (i64,) = sqlx::query_as(
                "SELECT COALESCE(MAX(event_sequence), 0) + 1 FROM a2a_task_events
                 WHERE tenant = ? AND task_id = ?",
            )
            .bind(tenant)
            .bind(task.id())
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            let event_data = serde_json::to_string(event)
                .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

            sqlx::query(
                "INSERT INTO a2a_task_events (tenant, task_id, event_sequence, event_data)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(tenant)
            .bind(task.id())
            .bind(seq.0)
            .bind(&event_data)
            .execute(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            sequences.push(seq.0 as u64);
        }

        // ADR-013 §6.3: maintain latest_event_sequence UNCONDITIONALLY.
        if let Some(&max_seq) = sequences.iter().max() {
            sqlx::query(
                "UPDATE a2a_tasks SET latest_event_sequence = ?
                 WHERE tenant = ? AND task_id = ? AND latest_event_sequence < ?",
            )
            .bind(max_seq as i64)
            .bind(tenant)
            .bind(task.id())
            .bind(max_seq as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        }

        tx.commit().await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        Ok(sequences)
    }
}

// ===========================================================================
// A2aPushDeliveryStore (ADR-011 §10)
// ===========================================================================

/// Convert `SystemTime` to epoch micros; stored as `INTEGER` so
/// ordering and arithmetic are cheap and deterministic across
/// timezones.
fn systime_to_micros(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

fn micros_to_systime(micros: i64) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_micros(micros.max(0) as u64)
}

fn claim_status_to_str(s: ClaimStatus) -> &'static str {
    match s {
        ClaimStatus::Pending => "Pending",
        ClaimStatus::Attempting => "Attempting",
        ClaimStatus::Succeeded => "Succeeded",
        ClaimStatus::GaveUp => "GaveUp",
        ClaimStatus::Abandoned => "Abandoned",
    }
}

fn claim_status_from_str(s: &str) -> Result<ClaimStatus, A2aStorageError> {
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

fn error_class_to_json(c: DeliveryErrorClass) -> String {
    serde_json::to_string(&ErrorClassWire::from(c)).unwrap_or_else(|_| "{}".into())
}

fn error_class_from_json(s: &str) -> Option<DeliveryErrorClass> {
    serde_json::from_str::<ErrorClassWire>(s).ok().map(Into::into)
}

/// Wire shape for persisting `DeliveryErrorClass` without coupling
/// the trait enum to serde. SQLite-local.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", content = "s")]
enum ErrorClassWire {
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
impl From<DeliveryErrorClass> for ErrorClassWire {
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
impl From<ErrorClassWire> for DeliveryErrorClass {
    fn from(w: ErrorClassWire) -> Self {
        match w {
            ErrorClassWire::NetworkError => DeliveryErrorClass::NetworkError,
            ErrorClassWire::Timeout => DeliveryErrorClass::Timeout,
            ErrorClassWire::HttpError4xx(s) => DeliveryErrorClass::HttpError4xx { status: s },
            ErrorClassWire::HttpError5xx(s) => DeliveryErrorClass::HttpError5xx { status: s },
            ErrorClassWire::HttpError429 => DeliveryErrorClass::HttpError429,
            ErrorClassWire::SSRFBlocked => DeliveryErrorClass::SSRFBlocked,
            ErrorClassWire::PayloadTooLarge => DeliveryErrorClass::PayloadTooLarge,
            ErrorClassWire::ConfigDeleted => DeliveryErrorClass::ConfigDeleted,
            ErrorClassWire::TaskDeleted => DeliveryErrorClass::TaskDeleted,
            ErrorClassWire::TlsRejected => DeliveryErrorClass::TlsRejected,
        }
    }
}

fn gave_up_reason_to_str(r: GaveUpReason) -> &'static str {
    match r {
        GaveUpReason::MaxAttemptsExhausted => "MaxAttemptsExhausted",
        GaveUpReason::NonRetryableHttpStatus => "NonRetryableHttpStatus",
        GaveUpReason::SsrfBlocked => "SsrfBlocked",
        GaveUpReason::PayloadTooLarge => "PayloadTooLarge",
        GaveUpReason::TlsRejected => "TlsRejected",
    }
}

fn abandoned_reason_to_str(r: AbandonedReason) -> &'static str {
    match r {
        AbandonedReason::ConfigDeleted => "ConfigDeleted",
        AbandonedReason::TaskDeleted => "TaskDeleted",
        AbandonedReason::NonHttpsUrlInProduction => "NonHttpsUrlInProduction",
    }
}

#[async_trait]
impl A2aPushDeliveryStore for SqliteA2aStorage {
    fn backend_name(&self) -> &'static str {
        "sqlite"
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
        let expires_at = now + claim_expiry;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let existing: Option<(String, String, i64, i64, i64, i64, String, Option<i64>, Option<i64>, Option<i64>, Option<String>)> =
            sqlx::query_as(
                "SELECT claimant, owner, generation, claimed_at_micros, expires_at_micros, \
                    delivery_attempt_count, status, first_attempted_at_micros, \
                    last_attempted_at_micros, last_http_status, last_error_class \
                 FROM a2a_push_deliveries \
                 WHERE tenant = ?1 AND task_id = ?2 AND event_sequence = ?3 AND config_id = ?4",
            )
            .bind(tenant)
            .bind(task_id)
            .bind(event_sequence as i64)
            .bind(config_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let now_micros = systime_to_micros(now);
        let expires_micros = systime_to_micros(expires_at);

        if let Some((_prev_claimant, prev_owner, prev_gen, _prev_claimed, prev_expires, prev_count, prev_status_s, prev_first, prev_last, prev_http, prev_err)) = existing {
            let prev_status = claim_status_from_str(&prev_status_s)?;
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
            // Re-claim. Conditional update: the row must still be in
            // a non-terminal status AND still expired AND at the
            // generation we just read. Without these guards, a
            // stale read followed by a concurrent terminal commit
            // would let the re-claim UPDATE clobber the terminal.
            // Owner is preserved from the original claim — a re-claim
            // is a recovery hand-off, not a new registration.
            let new_gen = prev_gen + 1;
            let update_result = sqlx::query(
                "UPDATE a2a_push_deliveries SET \
                    claimant = ?1, generation = ?2, claimed_at_micros = ?3, \
                    expires_at_micros = ?4, status = 'Pending', \
                    gave_up_at_micros = NULL, gave_up_reason = NULL, abandoned_reason = NULL \
                 WHERE tenant = ?5 AND task_id = ?6 AND event_sequence = ?7 AND config_id = ?8 \
                   AND generation = ?9 AND status IN ('Pending', 'Attempting') \
                   AND expires_at_micros < ?10",
            )
            .bind(claimant)
            .bind(new_gen)
            .bind(now_micros)
            .bind(expires_micros)
            .bind(tenant)
            .bind(task_id)
            .bind(event_sequence as i64)
            .bind(config_id)
            .bind(prev_gen)
            .bind(now_micros)
            .execute(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            if update_result.rows_affected() == 0 {
                // Someone raced us — terminal commit, another
                // re-claim, or a late recovery. From our caller's
                // perspective, the claim is no longer ours to take.
                return Err(A2aStorageError::ClaimAlreadyHeld {
                    tenant: tenant.to_string(),
                    task_id: task_id.to_string(),
                    event_sequence,
                    config_id: config_id.to_string(),
                });
            }
            tx.commit()
                .await
                .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            let _ = (prev_count, prev_first, prev_last, prev_http, prev_err);
            return Ok(DeliveryClaim {
                claimant: claimant.to_string(),
                owner: prev_owner,
                generation: new_gen as u64,
                claimed_at: now,
                delivery_attempt_count: prev_count as u32,
                status: ClaimStatus::Pending,
            });
        }

        // Fresh claim. ON CONFLICT DO NOTHING collapses concurrent
        // fresh inserts into a one-winner race — the loser sees
        // `rows_affected == 0` and returns `ClaimAlreadyHeld`
        // rather than surfacing a UNIQUE constraint violation.
        let result = sqlx::query(
            "INSERT INTO a2a_push_deliveries \
                (tenant, task_id, event_sequence, config_id, claimant, owner, generation, \
                 claimed_at_micros, expires_at_micros, delivery_attempt_count, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, 0, 'Pending') \
             ON CONFLICT (tenant, task_id, event_sequence, config_id) DO NOTHING",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(event_sequence as i64)
        .bind(config_id)
        .bind(claimant)
        .bind(owner)
        .bind(now_micros)
        .bind(expires_micros)
        .execute(&mut *tx)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        if result.rows_affected() == 0 {
            return Err(A2aStorageError::ClaimAlreadyHeld {
                tenant: tenant.to_string(),
                task_id: task_id.to_string(),
                event_sequence,
                config_id: config_id.to_string(),
            });
        }
        tx.commit()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        Ok(DeliveryClaim {
            claimant: claimant.to_string(),
            owner: owner.to_string(),
            generation: 1,
            claimed_at: now,
            delivery_attempt_count: 0,
            status: ClaimStatus::Pending,
        })
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
        let now = std::time::SystemTime::now();
        let now_micros = systime_to_micros(now);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        // Conditional update fenced on identity AND non-terminal
        // status: a terminal row is frozen and cannot restart an
        // attempt, so the UPDATE filters to {Pending, Attempting}.
        let result = sqlx::query(
            "UPDATE a2a_push_deliveries SET \
                delivery_attempt_count = delivery_attempt_count + 1, \
                status = CASE WHEN status = 'Pending' THEN 'Attempting' ELSE status END, \
                first_attempted_at_micros = COALESCE(first_attempted_at_micros, ?1), \
                last_attempted_at_micros = ?1 \
             WHERE tenant = ?2 AND task_id = ?3 AND event_sequence = ?4 \
               AND config_id = ?5 AND claimant = ?6 AND generation = ?7 \
               AND status IN ('Pending', 'Attempting')",
        )
        .bind(now_micros)
        .bind(tenant)
        .bind(task_id)
        .bind(event_sequence as i64)
        .bind(config_id)
        .bind(claimant)
        .bind(claim_generation as i64)
        .execute(&mut *tx)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        if result.rows_affected() == 0 {
            return Err(A2aStorageError::StaleDeliveryClaim {
                tenant: tenant.to_string(),
                task_id: task_id.to_string(),
                event_sequence,
                config_id: config_id.to_string(),
            });
        }
        let count: (i64,) = sqlx::query_as(
            "SELECT delivery_attempt_count FROM a2a_push_deliveries \
             WHERE tenant = ?1 AND task_id = ?2 AND event_sequence = ?3 AND config_id = ?4",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(event_sequence as i64)
        .bind(config_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(count.0 as u32)
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
        let now = std::time::SystemTime::now();
        let now_micros = systime_to_micros(now);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        // Fetch current row to validate identity and idempotency.
        let row: Option<(String, i64, String, Option<i64>)> = sqlx::query_as(
            "SELECT claimant, generation, status, claimed_at_micros \
             FROM a2a_push_deliveries \
             WHERE tenant = ?1 AND task_id = ?2 AND event_sequence = ?3 AND config_id = ?4",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(event_sequence as i64)
        .bind(config_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let (cur_claimant, cur_gen, cur_status_s, cur_claimed) = match row {
            Some(r) => r,
            None => {
                return Err(A2aStorageError::StaleDeliveryClaim {
                    tenant: tenant.to_string(),
                    task_id: task_id.to_string(),
                    event_sequence,
                    config_id: config_id.to_string(),
                })
            }
        };
        if cur_claimant != claimant || cur_gen as u64 != claim_generation {
            return Err(A2aStorageError::StaleDeliveryClaim {
                tenant: tenant.to_string(),
                task_id: task_id.to_string(),
                event_sequence,
                config_id: config_id.to_string(),
            });
        }
        let cur_status = claim_status_from_str(&cur_status_s)?;

        // Terminal-frozen gate: once Succeeded/GaveUp/Abandoned,
        // further outcomes (same terminal OR any other) are silent
        // no-ops. The row is never mutated away from its terminal.
        if matches!(
            cur_status,
            ClaimStatus::Succeeded | ClaimStatus::GaveUp | ClaimStatus::Abandoned
        ) {
            tx.commit()
                .await
                .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            return Ok(());
        }

        match outcome {
            DeliveryOutcome::Succeeded { http_status } => {
                sqlx::query(
                    "UPDATE a2a_push_deliveries SET status = 'Succeeded', \
                        last_http_status = ?1, \
                        first_attempted_at_micros = COALESCE(first_attempted_at_micros, ?2), \
                        last_attempted_at_micros = ?2 \
                     WHERE tenant = ?3 AND task_id = ?4 AND event_sequence = ?5 AND config_id = ?6",
                )
                .bind(http_status as i64)
                .bind(now_micros)
                .bind(tenant)
                .bind(task_id)
                .bind(event_sequence as i64)
                .bind(config_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            }
            DeliveryOutcome::Retry {
                http_status,
                error_class,
                next_attempt_at: _,
            } => {
                sqlx::query(
                    "UPDATE a2a_push_deliveries SET status = 'Attempting', \
                        last_http_status = ?1, last_error_class = ?2, \
                        last_attempted_at_micros = ?3 \
                     WHERE tenant = ?4 AND task_id = ?5 AND event_sequence = ?6 AND config_id = ?7",
                )
                .bind(http_status.map(|s| s as i64))
                .bind(error_class_to_json(error_class))
                .bind(now_micros)
                .bind(tenant)
                .bind(task_id)
                .bind(event_sequence as i64)
                .bind(config_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            }
            DeliveryOutcome::GaveUp {
                reason,
                last_error_class,
                last_http_status,
            } => {
                sqlx::query(
                    "UPDATE a2a_push_deliveries SET status = 'GaveUp', \
                        gave_up_at_micros = ?1, gave_up_reason = ?2, \
                        last_error_class = ?3, last_http_status = ?4, \
                        first_attempted_at_micros = COALESCE(first_attempted_at_micros, ?5), \
                        last_attempted_at_micros = COALESCE(last_attempted_at_micros, ?1) \
                     WHERE tenant = ?6 AND task_id = ?7 AND event_sequence = ?8 AND config_id = ?9",
                )
                .bind(now_micros)
                .bind(gave_up_reason_to_str(reason))
                .bind(error_class_to_json(last_error_class))
                .bind(last_http_status.map(|s| s as i64))
                .bind(cur_claimed.unwrap_or(now_micros))
                .bind(tenant)
                .bind(task_id)
                .bind(event_sequence as i64)
                .bind(config_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            }
            DeliveryOutcome::Abandoned { reason } => {
                sqlx::query(
                    "UPDATE a2a_push_deliveries SET status = 'Abandoned', \
                        abandoned_reason = ?1 \
                     WHERE tenant = ?2 AND task_id = ?3 AND event_sequence = ?4 AND config_id = ?5",
                )
                .bind(abandoned_reason_to_str(reason))
                .bind(tenant)
                .bind(task_id)
                .bind(event_sequence as i64)
                .bind(config_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            }
        }
        tx.commit()
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(())
    }

    async fn sweep_expired_claims(&self) -> Result<u64, A2aStorageError> {
        let now_micros = systime_to_micros(std::time::SystemTime::now());
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM a2a_push_deliveries \
             WHERE expires_at_micros < ?1 AND status IN ('Pending', 'Attempting')",
        )
        .bind(now_micros)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(row.0.max(0) as u64)
    }

    async fn list_reclaimable_claims(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::push::claim::ReclaimableClaim>, A2aStorageError> {
        let now_micros = systime_to_micros(std::time::SystemTime::now());
        let rows: Vec<(String, String, String, i64, String)> = sqlx::query_as(
            "SELECT tenant, owner, task_id, event_sequence, config_id \
             FROM a2a_push_deliveries \
             WHERE expires_at_micros < ?1 AND status IN ('Pending', 'Attempting') \
             LIMIT ?2",
        )
        .bind(now_micros)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(
                |(tenant, owner, task_id, event_sequence, config_id)| {
                    crate::push::claim::ReclaimableClaim {
                        tenant,
                        owner,
                        task_id,
                        event_sequence: event_sequence.max(0) as u64,
                        config_id,
                    }
                },
            )
            .collect())
    }

    async fn record_pending_dispatch(
        &self,
        tenant: &str,
        owner: &str,
        task_id: &str,
        event_sequence: u64,
    ) -> Result<(), A2aStorageError> {
        let now_micros = systime_to_micros(std::time::SystemTime::now());
        // Idempotent insert-or-refresh: a repeat write on the same
        // tuple updates recorded_at + owner. Using ON CONFLICT DO
        // UPDATE keeps the call cheap for the in-progress dispatch
        // path where the marker may already exist.
        sqlx::query(
            "INSERT INTO a2a_push_pending_dispatches \
                (tenant, task_id, event_sequence, owner, recorded_at_micros) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (tenant, task_id, event_sequence) DO UPDATE SET \
                owner = excluded.owner, \
                recorded_at_micros = excluded.recorded_at_micros",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(event_sequence as i64)
        .bind(owner)
        .bind(now_micros)
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(())
    }

    async fn delete_pending_dispatch(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
    ) -> Result<(), A2aStorageError> {
        sqlx::query(
            "DELETE FROM a2a_push_pending_dispatches \
             WHERE tenant = ?1 AND task_id = ?2 AND event_sequence = ?3",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(event_sequence as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(())
    }

    async fn list_stale_pending_dispatches(
        &self,
        older_than_recorded_at: std::time::SystemTime,
        limit: usize,
    ) -> Result<Vec<crate::push::claim::PendingDispatch>, A2aStorageError> {
        let cutoff_micros = systime_to_micros(older_than_recorded_at);
        let rows: Vec<(String, String, String, i64, i64)> = sqlx::query_as(
            "SELECT tenant, owner, task_id, event_sequence, recorded_at_micros \
             FROM a2a_push_pending_dispatches \
             WHERE recorded_at_micros <= ?1 \
             LIMIT ?2",
        )
        .bind(cutoff_micros)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|(tenant, owner, task_id, event_sequence, recorded_at_micros)| {
                crate::push::claim::PendingDispatch {
                    tenant,
                    owner,
                    task_id,
                    event_sequence: event_sequence.max(0) as u64,
                    recorded_at: micros_to_systime(recorded_at_micros),
                }
            })
            .collect())
    }

    async fn list_failed_deliveries(
        &self,
        tenant: &str,
        since: std::time::SystemTime,
        limit: usize,
    ) -> Result<Vec<FailedDelivery>, A2aStorageError> {
        let since_micros = systime_to_micros(since);
        let rows: Vec<(
            String,
            String,
            i64,
            i64,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            i64,
            Option<i64>,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT task_id, config_id, event_sequence, claimed_at_micros, \
                    first_attempted_at_micros, last_attempted_at_micros, \
                    gave_up_at_micros, delivery_attempt_count, \
                    last_http_status, last_error_class \
             FROM a2a_push_deliveries \
             WHERE tenant = ?1 AND status = 'GaveUp' AND gave_up_at_micros >= ?2 \
             ORDER BY gave_up_at_micros DESC \
             LIMIT ?3",
        )
        .bind(tenant)
        .bind(since_micros)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let mut out = Vec::with_capacity(rows.len());
        for (task_id, config_id, seq, claimed, first, last, gave_up, count, http, err) in rows {
            let gave_up = gave_up.unwrap_or(claimed);
            out.push(FailedDelivery {
                task_id,
                config_id,
                event_sequence: seq.max(0) as u64,
                first_attempted_at: micros_to_systime(first.unwrap_or(claimed)),
                last_attempted_at: micros_to_systime(last.unwrap_or(gave_up)),
                gave_up_at: micros_to_systime(gave_up),
                delivery_attempt_count: count.max(0) as u32,
                last_http_status: http.map(|s| s as u16),
                last_error_class: err
                    .as_deref()
                    .and_then(error_class_from_json)
                    .unwrap_or(DeliveryErrorClass::NetworkError),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::parity_tests;

    async fn storage() -> SqliteA2aStorage {
        // Single-connection in-memory SQLite. The pool is capped at 1 so
        // all queries serialize on one connection — this matches SQLite's
        // actual multi-writer model (one writer at a time) and avoids
        // SQLITE_LOCKED deadlocks on concurrent `BEGIN DEFERRED`
        // transactions. The conditional-UPDATE CAS is still exercised
        // because the test issues multiple SEQUENTIAL tx attempts: once
        // a terminal writer commits, subsequent transactions see the
        // terminal and the conditional UPDATE affects zero rows, returning
        // TerminalStateAlreadySet as required.
        SqliteA2aStorage::new(SqliteConfig {
            database_url: "sqlite::memory:".into(),
            max_connections: 1,
        })
        .await
        .unwrap()
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

    // Terminal-write CAS (ADR-010 §7.1) parity tests.

    #[tokio::test]
    async fn test_terminal_cas_single_winner_on_concurrent_terminals() {
        let s = std::sync::Arc::new(storage().await);
        parity_tests::test_terminal_cas_single_winner_on_concurrent_terminals(
            s.clone(),
            s.clone(),
            s,
        )
        .await;
    }

    #[tokio::test]
    async fn test_terminal_cas_single_winner_from_submitted_includes_rejected() {
        let s = std::sync::Arc::new(storage().await);
        parity_tests::test_terminal_cas_single_winner_from_submitted_includes_rejected(
            s.clone(),
            s.clone(),
            s,
        )
        .await;
    }

    #[tokio::test]
    async fn test_terminal_cas_rejects_sequential_second_terminal() {
        let s = storage().await;
        parity_tests::test_terminal_cas_rejects_sequential_second_terminal(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_update_task_with_events_rejects_terminal_already_set() {
        let s = storage().await;
        parity_tests::test_update_task_with_events_rejects_terminal_already_set(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_invalid_transition_distinct_from_terminal_already_set() {
        let s = storage().await;
        parity_tests::test_invalid_transition_distinct_from_terminal_already_set(&s, &s).await;
    }

    // Cancel-marker parity (ADR-012).

    #[tokio::test]
    async fn test_cancel_marker_roundtrip() {
        let s = storage().await;
        parity_tests::test_cancel_marker_roundtrip(&s, &s).await;
    }

    #[tokio::test]
    async fn test_supervisor_list_cancel_requested_parity() {
        let s = storage().await;
        parity_tests::test_supervisor_list_cancel_requested_parity(&s, &s, &s).await;
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

    // =========================================================
    // A2aPushDeliveryStore parity (ADR-011 §10).
    // =========================================================

    #[tokio::test]
    async fn test_push_claim_is_exclusive() {
        let s = storage().await;
        parity_tests::test_push_claim_is_exclusive(&s).await;
    }

    #[tokio::test]
    async fn test_push_claim_expired_is_reclaimable() {
        let s = storage().await;
        parity_tests::test_push_claim_expired_is_reclaimable(&s).await;
    }

    #[tokio::test]
    async fn test_push_outcome_fenced_to_current_claim() {
        let s = storage().await;
        parity_tests::test_push_outcome_fenced_to_current_claim(&s).await;
    }

    #[tokio::test]
    async fn test_push_claim_terminal_succeeded_blocks_reclaim() {
        let s = storage().await;
        parity_tests::test_push_claim_terminal_succeeded_blocks_reclaim(&s).await;
    }

    #[tokio::test]
    async fn test_push_claim_terminal_gaveup_blocks_reclaim() {
        let s = storage().await;
        parity_tests::test_push_claim_terminal_gaveup_blocks_reclaim(&s).await;
    }

    #[tokio::test]
    async fn test_push_claim_terminal_abandoned_blocks_reclaim_and_not_listed() {
        let s = storage().await;
        parity_tests::test_push_claim_terminal_abandoned_blocks_reclaim_and_not_listed(&s).await;
    }

    #[tokio::test]
    async fn test_push_attempt_started_advances_count_and_status() {
        let s = storage().await;
        parity_tests::test_push_attempt_started_advances_count_and_status(&s).await;
    }

    #[tokio::test]
    async fn test_push_attempt_started_is_fenced() {
        let s = storage().await;
        parity_tests::test_push_attempt_started_is_fenced(&s).await;
    }

    #[tokio::test]
    async fn test_push_retry_outcome_keeps_claim_open() {
        let s = storage().await;
        parity_tests::test_push_retry_outcome_keeps_claim_open(&s).await;
    }

    #[tokio::test]
    async fn test_push_outcome_idempotent_on_terminal() {
        let s = storage().await;
        parity_tests::test_push_outcome_idempotent_on_terminal(&s).await;
    }

    #[tokio::test]
    async fn test_push_sweep_counts_expired_nonterminal_and_preserves_status() {
        let s = storage().await;
        parity_tests::test_push_sweep_counts_expired_nonterminal_and_preserves_status(&s).await;
    }

    #[tokio::test]
    async fn test_push_list_reclaimable_filters_and_returns_identity() {
        let s = storage().await;
        parity_tests::test_push_list_reclaimable_filters_and_returns_identity(&s).await;
    }

    #[tokio::test]
    async fn test_push_list_failed_filters_and_orders() {
        let s = storage().await;
        parity_tests::test_push_list_failed_filters_and_orders(&s).await;
    }

    #[tokio::test]
    async fn test_push_list_failed_is_tenant_scoped() {
        let s = storage().await;
        parity_tests::test_push_list_failed_is_tenant_scoped(&s).await;
    }

    #[tokio::test]
    async fn test_push_failed_delivery_diagnostics_roundtrip() {
        let s = storage().await;
        parity_tests::test_push_failed_delivery_diagnostics_roundtrip(&s).await;
    }

    #[tokio::test]
    async fn test_push_attempt_started_rejected_after_terminal() {
        let s = storage().await;
        parity_tests::test_push_attempt_started_rejected_after_terminal(&s).await;
    }

    #[tokio::test]
    async fn test_push_outcome_does_not_overwrite_terminal() {
        let s = storage().await;
        parity_tests::test_push_outcome_does_not_overwrite_terminal(&s).await;
    }

    #[tokio::test]
    async fn test_push_concurrent_claim_race() {
        let s = std::sync::Arc::new(storage().await);
        parity_tests::test_push_concurrent_claim_race(s).await;
    }

    // Atomic pending-dispatch marker parity (ADR-013 §4.3 / §10.1).

    async fn opted_in_storage() -> SqliteA2aStorage {
        storage().await.with_push_dispatch_enabled(true)
    }

    #[tokio::test]
    async fn test_atomic_marker_written_for_terminal_status() {
        let s = opted_in_storage().await;
        parity_tests::test_atomic_marker_written_for_terminal_status(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_marker_skipped_for_non_terminal_status() {
        let s = opted_in_storage().await;
        parity_tests::test_atomic_marker_skipped_for_non_terminal_status(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_marker_skipped_for_artifact_event() {
        let s = opted_in_storage().await;
        parity_tests::test_atomic_marker_skipped_for_artifact_event(&s, &s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_atomic_marker_absent_when_opt_in_off() {
        let s = storage().await; // default: push_dispatch_enabled=false
        parity_tests::test_atomic_marker_absent_when_opt_in_off(&s, &s, &s).await;
    }

    // Causal-floor eligibility parity (ADR-013 §4.5 / §10.3 / §10.4).

    #[tokio::test]
    async fn test_config_registered_at_or_after_event_not_eligible() {
        let s = storage().await;
        parity_tests::test_config_registered_at_or_after_event_not_eligible(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_late_create_config_stamps_advanced_sequence() {
        let s = storage().await;
        parity_tests::test_late_create_config_stamps_advanced_sequence(&s, &s, &s).await;
    }
}
