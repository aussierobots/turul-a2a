//! `SkillRegistry` trait, `SkillDescriptor`, and the default in-memory
//! implementation. Registry entries enforce a single-source-of-truth
//! invariant on `params_schema`: for manifest-backed skills it is derived
//! from the manifest's input schema; for programmatic skills it is supplied
//! once at registration time. There is no second authoritative surface.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use turul_a2a_proto::AgentSkill;

use crate::error::SkillError;
use crate::handler::SkillHandler;
use crate::manifest::SkillCard;

/// Returned by `SkillRegistry::describe`.
///
/// `params_schema` is **Turul-local runtime-planning metadata** — never on
/// the wire. Single-source-of-truth invariant: for manifest-backed skills
/// it is derived from the manifest input schema; for programmatic skills
/// it is supplied once at registration. There is no second authoritative
/// surface.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SkillDescriptor {
    pub id: String,
    pub agent_skill: AgentSkill,
    pub params_schema: Option<Value>,
}

/// Registry mapping `AgentSkill.id` to a registered `SkillHandler`.
#[async_trait]
pub trait SkillRegistry: Send + Sync {
    /// Register a manifest-backed skill. The `params_schema` field of the
    /// returned descriptor is derived from `card.input_schema` (single
    /// source of truth).
    async fn register_manifest(
        &self,
        card: SkillCard,
        handler: Arc<dyn SkillHandler>,
    ) -> Result<(), SkillError>;

    /// Register a programmatic skill with an optional one-shot schema.
    /// The supplied schema becomes `params_schema`; there is no second
    /// surface that can override it.
    async fn register_programmatic(
        &self,
        agent_skill: AgentSkill,
        params_schema: Option<Value>,
        handler: Arc<dyn SkillHandler>,
    ) -> Result<(), SkillError>;

    /// Describe a registered skill.
    async fn describe(&self, id: &str) -> Option<SkillDescriptor>;

    /// List all registered descriptors.
    async fn list(&self) -> Vec<SkillDescriptor>;

    /// Look up the handler for `id`.
    async fn handler(&self, id: &str) -> Option<Arc<dyn SkillHandler>>;
}

struct Registration {
    descriptor: SkillDescriptor,
    handler: Arc<dyn SkillHandler>,
}

/// Default in-memory `SkillRegistry`.
#[derive(Default)]
pub struct InMemorySkillRegistry {
    inner: Mutex<BTreeMap<String, Registration>>,
}

impl InMemorySkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SkillRegistry for InMemorySkillRegistry {
    async fn register_manifest(
        &self,
        card: SkillCard,
        handler: Arc<dyn SkillHandler>,
    ) -> Result<(), SkillError> {
        // Single source of truth: params_schema IS the manifest's
        // input_schema, by construction.
        let agent_skill = card.to_agent_skill();
        let descriptor = SkillDescriptor {
            id: card.id.clone(),
            agent_skill,
            params_schema: card.input_schema.clone(),
        };
        self.insert(descriptor, handler)
    }

    async fn register_programmatic(
        &self,
        agent_skill: AgentSkill,
        params_schema: Option<Value>,
        handler: Arc<dyn SkillHandler>,
    ) -> Result<(), SkillError> {
        let descriptor = SkillDescriptor {
            id: agent_skill.id.clone(),
            agent_skill,
            params_schema,
        };
        self.insert(descriptor, handler)
    }

    async fn describe(&self, id: &str) -> Option<SkillDescriptor> {
        let guard = self.inner.lock().ok()?;
        guard.get(id).map(|r| r.descriptor.clone())
    }

    async fn list(&self) -> Vec<SkillDescriptor> {
        match self.inner.lock() {
            Ok(g) => g.values().map(|r| r.descriptor.clone()).collect(),
            Err(_) => Vec::new(),
        }
    }

    async fn handler(&self, id: &str) -> Option<Arc<dyn SkillHandler>> {
        let guard = self.inner.lock().ok()?;
        guard.get(id).map(|r| r.handler.clone())
    }
}

impl InMemorySkillRegistry {
    fn insert(
        &self,
        descriptor: SkillDescriptor,
        handler: Arc<dyn SkillHandler>,
    ) -> Result<(), SkillError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| SkillError::Internal(format!("registry mutex poisoned: {e}")))?;
        if descriptor.id.is_empty() {
            return Err(SkillError::InvalidRequest(
                "skill id must not be empty".to_string(),
            ));
        }
        guard.insert(
            descriptor.id.clone(),
            Registration {
                descriptor,
                handler,
            },
        );
        Ok(())
    }
}
