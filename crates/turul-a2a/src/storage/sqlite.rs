//! SQLite storage backend for A2A tasks and push notification configs.

use async_trait::async_trait;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use turul_a2a_types::{Artifact, Message, Task, TaskState, TaskStatus};

use super::error::A2aStorageError;
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
            "INSERT INTO a2a_tasks (tenant, task_id, owner, task_json, context_id, status_state)
             VALUES (?, ?, ?, ?, ?, ?)",
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
            "UPDATE a2a_tasks SET task_json = ?, status_state = ?, context_id = ?
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

        // Fetch page
        let select_sql = if filter.page_token.is_some() {
            format!(
                "SELECT task_json FROM a2a_tasks WHERE {where_clause} AND task_id > ? ORDER BY task_id LIMIT ?"
            )
        } else {
            format!(
                "SELECT task_json FROM a2a_tasks WHERE {where_clause} ORDER BY task_id LIMIT ?"
            )
        };

        let mut select_query = sqlx::query_as::<_, (String,)>(&select_sql)
            .bind(tenant)
            .bind(owner);
        if let Some(ref ctx) = filter.context_id {
            select_query = select_query.bind(ctx);
        }
        if let Some(ref status) = filter.status {
            select_query = select_query.bind(format!("{status:?}"));
        }
        if let Some(ref token) = filter.page_token {
            select_query = select_query.bind(token);
        }
        select_query = select_query.bind(page_size);

        let rows: Vec<(String,)> = select_query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| A2aStorageError::DatabaseError(e.to_string()))?;

        let include_artifacts = filter.include_artifacts.unwrap_or(false);
        let tasks: Vec<Task> = rows
            .iter()
            .filter_map(|(json,)| {
                Self::task_from_json(json)
                    .ok()
                    .map(|t| Self::trim_task(t, filter.history_length, include_artifacts))
            })
            .collect();

        let next_page_token = if tasks.len() as i32 >= page_size {
            tasks.last().map(|t| t.id().to_string()).unwrap_or_default()
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
}
