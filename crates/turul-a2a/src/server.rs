//! A2aServer builder and runtime.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::error::A2aError;
use crate::executor::AgentExecutor;
use crate::router::{build_router, AppState};
use crate::storage::{A2aPushNotificationStorage, A2aTaskStorage, InMemoryA2aStorage};
use crate::streaming::TaskEventBroker;

/// Builder for configuring and running an A2A server.
pub struct A2aServerBuilder {
    executor: Option<Arc<dyn AgentExecutor>>,
    task_storage: Option<Arc<dyn A2aTaskStorage>>,
    push_storage: Option<Arc<dyn A2aPushNotificationStorage>>,
    bind_addr: SocketAddr,
}

impl A2aServerBuilder {
    pub fn new() -> Self {
        Self {
            executor: None,
            task_storage: None,
            push_storage: None,
            bind_addr: ([0, 0, 0, 0], 3000).into(),
        }
    }

    pub fn executor(mut self, executor: impl AgentExecutor + 'static) -> Self {
        self.executor = Some(Arc::new(executor));
        self
    }

    pub fn storage(mut self, storage: InMemoryA2aStorage) -> Self {
        self.task_storage = Some(Arc::new(storage.clone()));
        self.push_storage = Some(Arc::new(storage));
        self
    }

    pub fn task_storage(mut self, storage: impl A2aTaskStorage + 'static) -> Self {
        self.task_storage = Some(Arc::new(storage));
        self
    }

    pub fn push_storage(mut self, storage: impl A2aPushNotificationStorage + 'static) -> Self {
        self.push_storage = Some(Arc::new(storage));
        self
    }

    pub fn bind(mut self, addr: impl Into<SocketAddr>) -> Self {
        self.bind_addr = addr.into();
        self
    }

    pub fn build(self) -> Result<A2aServer, A2aError> {
        let executor = self.executor.ok_or(A2aError::Internal(
            "executor is required".into(),
        ))?;

        let default_storage = InMemoryA2aStorage::new();
        let task_storage = self
            .task_storage
            .unwrap_or_else(|| Arc::new(default_storage.clone()));
        let push_storage = self
            .push_storage
            .unwrap_or_else(|| Arc::new(default_storage));

        Ok(A2aServer {
            state: AppState {
                executor,
                task_storage,
                push_storage,
                event_broker: TaskEventBroker::new(),
                middleware_stack: Arc::new(crate::middleware::MiddlewareStack::new(vec![])),
            },
            bind_addr: self.bind_addr,
        })
    }
}

impl Default for A2aServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A configured A2A server ready to run.
pub struct A2aServer {
    state: AppState,
    bind_addr: SocketAddr,
}

impl A2aServer {
    pub fn builder() -> A2aServerBuilder {
        A2aServerBuilder::new()
    }

    /// Build the axum router without binding — useful for testing.
    pub fn into_router(self) -> axum::Router {
        build_router(self.state)
    }

    /// Run the server, binding to the configured address.
    pub async fn run(self) -> Result<(), A2aError> {
        let app = build_router(self.state);
        let listener = tokio::net::TcpListener::bind(self.bind_addr)
            .await
            .map_err(|e| A2aError::Internal(format!("Failed to bind: {e}")))?;
        tracing::info!("A2A server listening on {}", self.bind_addr);
        axum::serve(listener, app)
            .await
            .map_err(|e| A2aError::Internal(format!("Server error: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::A2aError;
    use crate::executor::AgentExecutor;
    use turul_a2a_types::{Message, Task};

    struct DummyExecutor;

    #[async_trait::async_trait]
    impl AgentExecutor for DummyExecutor {
        async fn execute(&self, _task: &mut Task, _msg: &Message) -> Result<(), A2aError> {
            Ok(())
        }
        fn agent_card(&self) -> turul_a2a_proto::AgentCard {
            turul_a2a_proto::AgentCard::default()
        }
    }

    #[test]
    fn builder_requires_executor() {
        let result = A2aServer::builder().build();
        assert!(result.is_err());
    }

    #[test]
    fn builder_with_executor_defaults_storage() {
        let server = A2aServer::builder()
            .executor(DummyExecutor)
            .build()
            .unwrap();
        // Should have default in-memory storage
        let _ = server.into_router();
    }

    #[test]
    fn builder_with_explicit_storage() {
        let storage = InMemoryA2aStorage::new();
        let server = A2aServer::builder()
            .executor(DummyExecutor)
            .storage(storage)
            .bind(([127, 0, 0, 1], 8080))
            .build()
            .unwrap();
        let _ = server.into_router();
    }
}
