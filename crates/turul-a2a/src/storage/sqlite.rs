//! SQLite storage backend for A2A tasks, push configs, events, and atomic operations.

use async_trait::async_trait;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use turul_a2a_types::{Artifact, Message, Task, TaskState, TaskStatus};

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
}

impl SqliteA2aStorage {
    pub async fn new(config: SqliteConfig) -> Result<Self, A2aStorageError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(config.max_connections)
            .connect(&config.database_url)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let storage = Self { pool };
        storage.create_tables().await?;
        Ok(storage)
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
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (tenant, task_id)
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS a2a_push_configs (
                tenant TEXT NOT NULL DEFAULT '',
                task_id TEXT NOT NULL,
                config_id TEXT NOT NULL,
                config_json TEXT NOT NULL,
                PRIMARY KEY (tenant, task_id, config_id)
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

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

        sqlx::query(
            "INSERT OR REPLACE INTO a2a_push_configs (tenant, task_id, config_id, config_json)
             VALUES (?, ?, ?, ?)",
        )
        .bind(tenant)
        .bind(&config.task_id)
        .bind(&config.id)
        .bind(&json)
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        Ok(config)
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
        // SQLite: no TTL tracking in v0.1. No-op.
        Ok(0)
    }
}

#[async_trait]
impl A2aAtomicStore for SqliteA2aStorage {
    fn backend_name(&self) -> &'static str {
        "sqlite"
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

        // Append events
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

            sequences.push(seq.0 as u64);
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

        // Verify task exists and owner matches
        let result = sqlx::query(
            "UPDATE a2a_tasks SET task_json = ?, status_state = ?, context_id = ?, updated_at = datetime('now')
             WHERE tenant = ? AND task_id = ? AND owner = ?",
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
            return Err(A2aStorageError::TaskNotFound(task.id().to_string()));
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

        tx.commit().await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        Ok(sequences)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::parity_tests;

    async fn storage() -> SqliteA2aStorage {
        SqliteA2aStorage::new(SqliteConfig::default()).await.unwrap()
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

    // Terminal-write CAS (ADR-010 §7.1) — phase B parity.

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
    async fn test_terminal_cas_rejects_sequential_second_terminal() {
        let s = storage().await;
        parity_tests::test_terminal_cas_rejects_sequential_second_terminal(&s, &s, &s).await;
    }

    #[tokio::test]
    async fn test_invalid_transition_distinct_from_terminal_already_set() {
        let s = storage().await;
        parity_tests::test_invalid_transition_distinct_from_terminal_already_set(&s, &s).await;
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
