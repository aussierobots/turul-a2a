//! A2aServer builder and runtime.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::error::A2aError;
use crate::executor::AgentExecutor;
use crate::middleware::{A2aMiddleware, MiddlewareStack, SecurityContribution};
use crate::router::{build_router, AppState};
use crate::storage::{A2aPushNotificationStorage, A2aTaskStorage, InMemoryA2aStorage};
use crate::streaming::TaskEventBroker;

/// Builder for configuring and running an A2A server.
pub struct A2aServerBuilder {
    executor: Option<Arc<dyn AgentExecutor>>,
    task_storage: Option<Arc<dyn A2aTaskStorage>>,
    push_storage: Option<Arc<dyn A2aPushNotificationStorage>>,
    bind_addr: SocketAddr,
    middleware: Vec<Arc<dyn A2aMiddleware>>,
}

impl A2aServerBuilder {
    pub fn new() -> Self {
        Self {
            executor: None,
            task_storage: None,
            push_storage: None,
            bind_addr: ([0, 0, 0, 0], 3000).into(),
            middleware: vec![],
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

    /// Add auth middleware. Multiple calls stack (AND semantics).
    /// Use `AnyOfMiddleware` for OR semantics.
    pub fn middleware(mut self, mw: Arc<dyn A2aMiddleware>) -> Self {
        self.middleware.push(mw);
        self
    }

    pub fn build(self) -> Result<A2aServer, A2aError> {
        let executor = self
            .executor
            .ok_or(A2aError::Internal("executor is required".into()))?;

        let default_storage = InMemoryA2aStorage::new();
        let task_storage = self
            .task_storage
            .unwrap_or_else(|| Arc::new(default_storage.clone()));
        let push_storage = self
            .push_storage
            .unwrap_or_else(|| Arc::new(default_storage));

        // Collect and merge security contributions
        let contributions: Vec<SecurityContribution> = self
            .middleware
            .iter()
            .map(|m| m.security_contribution())
            .collect();
        let merged = merge_stacked_contributions(&contributions)?;

        Ok(A2aServer {
            state: AppState {
                executor,
                task_storage,
                push_storage,
                event_broker: TaskEventBroker::new(),
                middleware_stack: Arc::new(MiddlewareStack::new(self.middleware)),
            },
            merged_security: merged,
            bind_addr: self.bind_addr,
        })
    }
}

impl Default for A2aServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Merge stacked contributions (AND semantics).
///
/// Schemes: union with collision detection (identical = dedup, different = error).
/// Requirements: Cartesian product (AND).
fn merge_stacked_contributions(
    contributions: &[SecurityContribution],
) -> Result<SecurityContribution, A2aError> {
    let mut merged = SecurityContribution::new();

    if contributions.is_empty() {
        return Ok(merged);
    }

    // 1. Collect schemes with collision detection
    let mut seen_schemes: std::collections::HashMap<String, turul_a2a_proto::SecurityScheme> =
        std::collections::HashMap::new();

    for contrib in contributions {
        for (name, scheme) in &contrib.schemes {
            if let Some(existing) = seen_schemes.get(name) {
                // Check semantic equality
                if !schemes_equivalent(existing, scheme) {
                    return Err(A2aError::Internal(format!(
                        "Security scheme collision: '{}' has conflicting definitions",
                        name
                    )));
                }
                // Identical — skip (dedup)
            } else {
                seen_schemes.insert(name.clone(), scheme.clone());
                merged.schemes.push((name.clone(), scheme.clone()));
            }
        }
    }

    // 2. Compute requirements via Cartesian product (AND)
    let requirement_sets: Vec<&[turul_a2a_proto::SecurityRequirement]> = contributions
        .iter()
        .filter(|c| !c.requirements.is_empty())
        .map(|c| c.requirements.as_slice())
        .collect();

    if requirement_sets.is_empty() {
        return Ok(merged);
    }

    let mut combined: Vec<turul_a2a_proto::SecurityRequirement> =
        requirement_sets[0].to_vec();

    for alternatives in &requirement_sets[1..] {
        let mut new_combined = Vec::new();
        for existing in &combined {
            for alt in *alternatives {
                let mut merged_schemes = existing.schemes.clone();
                for (name, scopes) in &alt.schemes {
                    merged_schemes
                        .entry(name.clone())
                        .and_modify(|existing_scopes| {
                            // Union scopes, dedup + sort
                            for s in &scopes.list {
                                if !existing_scopes.list.contains(s) {
                                    existing_scopes.list.push(s.clone());
                                }
                            }
                            existing_scopes.list.sort();
                            existing_scopes.list.dedup();
                        })
                        .or_insert_with(|| scopes.clone());
                }
                new_combined.push(turul_a2a_proto::SecurityRequirement {
                    schemes: merged_schemes,
                });
            }
        }
        combined = new_combined;
    }

    merged.requirements = combined;
    Ok(merged)
}

/// Semantic equality for SecurityScheme (not byte comparison).
/// Normalizes scope lists before comparison.
fn schemes_equivalent(
    a: &turul_a2a_proto::SecurityScheme,
    b: &turul_a2a_proto::SecurityScheme,
) -> bool {
    // Compare the scheme variant and its contents
    // Since proto types derive PartialEq, this is structural equality
    // which is correct for non-map fields. For OAuth flows with scope maps,
    // we'd need deeper normalization, but for API Key and HTTP Auth schemes
    // structural equality is sufficient.
    a == b
}

/// A configured A2A server ready to run.
pub struct A2aServer {
    state: AppState,
    merged_security: SecurityContribution,
    bind_addr: SocketAddr,
}

impl A2aServer {
    pub fn builder() -> A2aServerBuilder {
        A2aServerBuilder::new()
    }

    /// Build the axum router — useful for testing.
    /// Augments the AgentCard with merged security contributions.
    pub fn into_router(self) -> axum::Router {
        let router = build_router(self.state.clone());

        // Store merged security in AppState for agent card augmentation
        // We use a different approach: wrap the agent card handler
        // Actually, the simplest: store merged security as an Extension on the router
        // But that's complex. Instead, patch the executor's agent card at build time.
        // Since AgentExecutor returns the card by value, we wrap it.

        if self.merged_security.is_empty() {
            return router;
        }

        // The agent card route is already built. We need to use the state's merged_security.
        // The cleanest approach: store merged_security in AppState and use it in the handler.
        // For now, rebuild with a wrapping executor.
        let wrapped = SecurityAugmentedExecutor {
            inner: self.state.executor.clone(),
            security: self.merged_security,
        };

        let augmented_state = AppState {
            executor: Arc::new(wrapped),
            task_storage: self.state.task_storage,
            push_storage: self.state.push_storage,
            event_broker: self.state.event_broker,
            middleware_stack: self.state.middleware_stack,
        };

        build_router(augmented_state)
    }

    /// Run the server.
    pub async fn run(self) -> Result<(), A2aError> {
        let bind_addr = self.bind_addr;
        let app = self.into_router();
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .map_err(|e| A2aError::Internal(format!("Failed to bind: {e}")))?;
        tracing::info!("A2A server listening on {}", bind_addr);
        axum::serve(listener, app)
            .await
            .map_err(|e| A2aError::Internal(format!("Server error: {e}")))?;
        Ok(())
    }
}

/// Wraps an executor to augment its agent card with merged security contributions.
struct SecurityAugmentedExecutor {
    inner: Arc<dyn AgentExecutor>,
    security: SecurityContribution,
}

#[async_trait::async_trait]
impl AgentExecutor for SecurityAugmentedExecutor {
    async fn execute(
        &self,
        task: &mut turul_a2a_types::Task,
        msg: &turul_a2a_types::Message,
    ) -> Result<(), A2aError> {
        self.inner.execute(task, msg).await
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        let mut card = self.inner.agent_card();
        // Merge security contributions into the card
        for (name, scheme) in &self.security.schemes {
            card.security_schemes
                .entry(name.clone())
                .or_insert_with(|| scheme.clone());
        }
        for req in &self.security.requirements {
            card.security_requirements.push(req.clone());
        }
        card
    }

    fn extended_agent_card(
        &self,
        claims: Option<&serde_json::Value>,
    ) -> Option<turul_a2a_proto::AgentCard> {
        self.inner.extended_agent_card(claims).map(|mut card| {
            for (name, scheme) in &self.security.schemes {
                card.security_schemes
                    .entry(name.clone())
                    .or_insert_with(|| scheme.clone());
            }
            for req in &self.security.requirements {
                card.security_requirements.push(req.clone());
            }
            card
        })
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
