//! PostgreSQL storage backend for A2A tasks, push configs, events, and atomic operations.

use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgPoolOptions};
use turul_a2a_types::{Artifact, Message, Task, TaskState, TaskStatus};

use crate::streaming::StreamEvent;
use super::atomic::A2aAtomicStore;
use super::error::A2aStorageError;
use super::event_store::A2aEventStore;
use super::filter::{PushConfigListPage, TaskFilter, TaskListPage};
use super::traits::{A2aPushNotificationStorage, A2aTaskStorage};

/// PostgreSQL storage configuration.
#[derive(Debug, Clone)]
pub struct PostgresConfig {
    pub database_url: String,
    pub max_connections: u32,
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            database_url: "postgres://localhost/a2a".into(),
            max_connections: 10,
        }
    }
}

/// PostgreSQL-backed A2A storage.
#[derive(Clone)]
pub struct PostgresA2aStorage {
    pool: PgPool,
}

impl PostgresA2aStorage {
    pub async fn new(config: PostgresConfig) -> Result<Self, A2aStorageError> {
        let pool = PgPoolOptions::new()
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
                task_json JSONB NOT NULL,
                context_id TEXT NOT NULL DEFAULT '',
                status_state TEXT NOT NULL DEFAULT '',
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (tenant, task_id)
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        // Migration: add updated_at column if not present (for existing tables)
        let _ = sqlx::query(
            "ALTER TABLE a2a_tasks ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()",
        )
        .execute(&self.pool)
        .await;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS a2a_push_configs (
                tenant TEXT NOT NULL DEFAULT '',
                task_id TEXT NOT NULL,
                config_id TEXT NOT NULL,
                config_json JSONB NOT NULL,
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
                event_sequence BIGINT NOT NULL,
                event_data JSONB NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (tenant, task_id, event_sequence)
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        Ok(())
    }

    fn task_to_json(task: &Task) -> Result<serde_json::Value, A2aStorageError> {
        serde_json::to_value(task).map_err(|e| A2aStorageError::SerializationError(e.to_string()))
    }

    fn task_from_json(json: &serde_json::Value) -> Result<Task, A2aStorageError> {
        let proto: turul_a2a_proto::Task =
            serde_json::from_value(json.clone()).map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;
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
impl A2aTaskStorage for PostgresA2aStorage {
    fn backend_name(&self) -> &'static str {
        "postgres"
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
             VALUES ($1, $2, $3, $4, $5, $6, NOW())",
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
        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT task_json FROM a2a_tasks WHERE tenant = $1 AND task_id = $2 AND owner = $3",
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
            "UPDATE a2a_tasks SET task_json = $1, status_state = $2, context_id = $3, updated_at = NOW()
             WHERE tenant = $4 AND task_id = $5 AND owner = $6",
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
            "DELETE FROM a2a_tasks WHERE tenant = $1 AND task_id = $2 AND owner = $3",
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
        let page_size = filter.page_size.map(|ps| ps.clamp(1, 100)).unwrap_or(50);

        // Count total with filters
        let total_size: i64 = if let (Some(ctx), Some(status)) = (&filter.context_id, &filter.status) {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM a2a_tasks WHERE tenant = $1 AND owner = $2 AND context_id = $3 AND status_state = $4",
            )
            .bind(tenant).bind(owner).bind(ctx).bind(format!("{status:?}"))
            .fetch_one(&self.pool).await
        } else if let Some(ctx) = filter.context_id.as_ref() {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM a2a_tasks WHERE tenant = $1 AND owner = $2 AND context_id = $3",
            )
            .bind(tenant).bind(owner).bind(ctx)
            .fetch_one(&self.pool).await
        } else if let Some(status) = filter.status.as_ref() {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM a2a_tasks WHERE tenant = $1 AND owner = $2 AND status_state = $3",
            )
            .bind(tenant).bind(owner).bind(format!("{status:?}"))
            .fetch_one(&self.pool).await
        } else {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM a2a_tasks WHERE tenant = $1 AND owner = $2",
            )
            .bind(tenant).bind(owner)
            .fetch_one(&self.pool).await
        }.map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?
        .unwrap_or(0);

        // Build dynamic query — sorted by updated_at DESC, task_id DESC (spec §3.1.4)
        let mut param_idx = 3u32; // $1=tenant, $2=owner already used
        let mut extra_where = String::new();
        let mut bind_values: Vec<String> = vec![];

        if let Some(ref ctx) = filter.context_id {
            extra_where.push_str(&format!(" AND context_id = ${param_idx}"));
            bind_values.push(ctx.clone());
            param_idx += 1;
        }
        if let Some(ref status) = filter.status {
            extra_where.push_str(&format!(" AND status_state = ${param_idx}"));
            bind_values.push(format!("{status:?}"));
            param_idx += 1;
        }

        // Cursor: "updated_at|task_id" — fetch items that sort after cursor in DESC order
        let cursor_parts = filter.page_token.as_ref().and_then(|t| {
            t.split_once('|').map(|(time, id)| (time.to_string(), id.to_string()))
        });
        if let Some((ref cursor_time, ref cursor_id)) = cursor_parts {
            extra_where.push_str(&format!(
                " AND (updated_at < ${p1}::timestamptz OR (updated_at = ${p1}::timestamptz AND task_id < ${p2}))",
                p1 = param_idx, p2 = param_idx + 1
            ));
            bind_values.push(cursor_time.clone());
            bind_values.push(cursor_id.clone());
            param_idx += 2;
        }

        let select_sql = format!(
            "SELECT task_json, updated_at::text FROM a2a_tasks WHERE tenant = $1 AND owner = $2{extra_where} \
             ORDER BY updated_at DESC, task_id DESC LIMIT ${param_idx}"
        );

        let mut query = sqlx::query_as::<_, (serde_json::Value, String)>(&select_sql)
            .bind(tenant)
            .bind(owner);
        for val in &bind_values {
            query = query.bind(val);
        }
        query = query.bind(page_size);

        let rows: Vec<(serde_json::Value, String)> = query
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
        let count: Option<i64> = sqlx::query_scalar("SELECT COUNT(*) FROM a2a_tasks")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(count.unwrap_or(0) as usize)
    }

    async fn maintenance(&self) -> Result<(), A2aStorageError> {
        Ok(())
    }
}

#[async_trait]
impl A2aPushNotificationStorage for PostgresA2aStorage {
    fn backend_name(&self) -> &'static str {
        "postgres"
    }

    async fn create_config(
        &self,
        tenant: &str,
        mut config: turul_a2a_proto::TaskPushNotificationConfig,
    ) -> Result<turul_a2a_proto::TaskPushNotificationConfig, A2aStorageError> {
        if config.id.is_empty() {
            config.id = uuid::Uuid::now_v7().to_string();
        }
        let json = serde_json::to_value(&config)
            .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

        sqlx::query(
            "INSERT INTO a2a_push_configs (tenant, task_id, config_id, config_json)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (tenant, task_id, config_id) DO UPDATE SET config_json = EXCLUDED.config_json",
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
        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT config_json FROM a2a_push_configs
             WHERE tenant = $1 AND task_id = $2 AND config_id = $3",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(config_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        match row {
            Some((json,)) => {
                let config = serde_json::from_value(json)
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

        let rows: Vec<(serde_json::Value,)> = if let Some(token) = page_token {
            sqlx::query_as(
                "SELECT config_json FROM a2a_push_configs
                 WHERE tenant = $1 AND task_id = $2 AND config_id > $3
                 ORDER BY config_id LIMIT $4",
            )
            .bind(tenant)
            .bind(task_id)
            .bind(token)
            .bind(page_size)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as(
                "SELECT config_json FROM a2a_push_configs
                 WHERE tenant = $1 AND task_id = $2
                 ORDER BY config_id LIMIT $3",
            )
            .bind(tenant)
            .bind(task_id)
            .bind(page_size)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let configs: Vec<turul_a2a_proto::TaskPushNotificationConfig> = rows
            .iter()
            .filter_map(|(json,)| serde_json::from_value(json.clone()).ok())
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
            "DELETE FROM a2a_push_configs WHERE tenant = $1 AND task_id = $2 AND config_id = $3",
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
impl A2aEventStore for PostgresA2aStorage {
    fn backend_name(&self) -> &'static str {
        "postgres"
    }

    async fn append_event(
        &self,
        tenant: &str,
        task_id: &str,
        event: StreamEvent,
    ) -> Result<u64, A2aStorageError> {
        let event_data = serde_json::to_value(&event)
            .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

        let mut tx = self.pool.begin().await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        // Allocate per-task sequence via MAX+1 within transaction
        let seq: Option<i64> = sqlx::query_scalar(
            "SELECT COALESCE(MAX(event_sequence), 0) + 1 FROM a2a_task_events
             WHERE tenant = $1 AND task_id = $2",
        )
        .bind(tenant)
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let seq = seq.unwrap_or(1);

        sqlx::query(
            "INSERT INTO a2a_task_events (tenant, task_id, event_sequence, event_data)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(tenant)
        .bind(task_id)
        .bind(seq)
        .bind(&event_data)
        .execute(&mut *tx)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        tx.commit().await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        Ok(seq as u64)
    }

    async fn get_events_after(
        &self,
        tenant: &str,
        task_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, StreamEvent)>, A2aStorageError> {
        let rows: Vec<(i64, serde_json::Value)> = sqlx::query_as(
            "SELECT event_sequence, event_data FROM a2a_task_events
             WHERE tenant = $1 AND task_id = $2 AND event_sequence > $3
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
            let event: StreamEvent = serde_json::from_value(data)
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
        let row: Option<i64> = sqlx::query_scalar(
            "SELECT COALESCE(MAX(event_sequence), 0) FROM a2a_task_events
             WHERE tenant = $1 AND task_id = $2",
        )
        .bind(tenant)
        .bind(task_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
        Ok(row.unwrap_or(0) as u64)
    }

    async fn cleanup_expired(&self) -> Result<u64, A2aStorageError> {
        Ok(0)
    }
}

#[async_trait]
impl A2aAtomicStore for PostgresA2aStorage {
    fn backend_name(&self) -> &'static str {
        "postgres"
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

        sqlx::query(
            "INSERT INTO a2a_tasks (tenant, task_id, owner, task_json, context_id, status_state, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, NOW())",
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

        let mut sequences = Vec::with_capacity(events.len());
        for event in &events {
            let seq: Option<i64> = sqlx::query_scalar(
                "SELECT COALESCE(MAX(event_sequence), 0) + 1 FROM a2a_task_events
                 WHERE tenant = $1 AND task_id = $2",
            )
            .bind(tenant)
            .bind(task.id())
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            let seq = seq.unwrap_or(1);

            let event_data = serde_json::to_value(event)
                .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

            sqlx::query(
                "INSERT INTO a2a_task_events (tenant, task_id, event_sequence, event_data)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(tenant)
            .bind(task.id())
            .bind(seq)
            .bind(&event_data)
            .execute(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            sequences.push(seq as u64);
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

        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT task_json FROM a2a_tasks WHERE tenant = $1 AND task_id = $2 AND owner = $3",
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

        let mut proto = task.as_proto().clone();
        proto.status = Some(new_status.into_proto());
        let updated_task = Task::try_from(proto).map_err(A2aStorageError::TypeError)?;

        let task_json = Self::task_to_json(&updated_task)?;
        let state_str = Self::status_state_str(&updated_task);

        // Terminal-write CAS (ADR-010 §7.1). Under PostgreSQL default
        // isolation (READ COMMITTED), two transactions can both read a
        // non-terminal state from the SELECT above, both pass the state
        // machine check, and both reach the UPDATE. The WHERE clause
        // below rejects a row whose status_state is already terminal at
        // UPDATE time, guaranteeing exactly-one-winner semantics. Row
        // count == 0 after the UPDATE ⇒ another writer committed a
        // terminal first; no events persist (the rollback at end-of-scope
        // discards event inserts below too).
        let result = sqlx::query(
            "UPDATE a2a_tasks
               SET task_json = $1, status_state = $2, context_id = $3, updated_at = NOW()
             WHERE tenant = $4 AND task_id = $5 AND owner = $6
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
            // Concurrent terminal write won the race. Re-read the
            // now-terminal state for the error; transaction aborts.
            let current: Option<(String,)> = sqlx::query_as(
                "SELECT status_state FROM a2a_tasks WHERE tenant = $1 AND task_id = $2",
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

        let mut sequences = Vec::with_capacity(events.len());
        for event in &events {
            let seq: Option<i64> = sqlx::query_scalar(
                "SELECT COALESCE(MAX(event_sequence), 0) + 1 FROM a2a_task_events
                 WHERE tenant = $1 AND task_id = $2",
            )
            .bind(tenant)
            .bind(task_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            let seq = seq.unwrap_or(1);

            let event_data = serde_json::to_value(event)
                .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

            sqlx::query(
                "INSERT INTO a2a_task_events (tenant, task_id, event_sequence, event_data)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(tenant)
            .bind(task_id)
            .bind(seq)
            .bind(&event_data)
            .execute(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            sequences.push(seq as u64);
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

        let result = sqlx::query(
            "UPDATE a2a_tasks SET task_json = $1, status_state = $2, context_id = $3, updated_at = NOW()
             WHERE tenant = $4 AND task_id = $5 AND owner = $6",
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

        let mut sequences = Vec::with_capacity(events.len());
        for event in &events {
            let seq: Option<i64> = sqlx::query_scalar(
                "SELECT COALESCE(MAX(event_sequence), 0) + 1 FROM a2a_task_events
                 WHERE tenant = $1 AND task_id = $2",
            )
            .bind(tenant)
            .bind(task.id())
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;
            let seq = seq.unwrap_or(1);

            let event_data = serde_json::to_value(event)
                .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;

            sqlx::query(
                "INSERT INTO a2a_task_events (tenant, task_id, event_sequence, event_data)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(tenant)
            .bind(task.id())
            .bind(seq)
            .bind(&event_data)
            .execute(&mut *tx)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

            sequences.push(seq as u64);
        }

        tx.commit().await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        Ok(sequences)
    }
}

/// PostgreSQL parity tests require a running PostgreSQL instance.
///
/// Run tests:
///   DATABASE_URL=postgres://user:pass@localhost/a2a_test \
///     cargo test --features postgres -p turul-a2a --lib -- storage::postgres --test-threads=1
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::parity_tests;

    async fn storage() -> PostgresA2aStorage {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://localhost/a2a_test".into());
        let storage = PostgresA2aStorage::new(PostgresConfig {
            database_url: url,
            max_connections: 5,
        })
        .await
        .unwrap();

        // Clean tables for test isolation
        sqlx::query("DELETE FROM a2a_task_events")
            .execute(&storage.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM a2a_push_configs")
            .execute(&storage.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM a2a_tasks")
            .execute(&storage.pool)
            .await
            .unwrap();

        storage
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
