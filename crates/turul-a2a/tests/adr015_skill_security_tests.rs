//! ADR-015 skill-level `security_requirements` — advertisement vs.
//! enforcement.
//!
//! These tests pin the post-merge truthfulness invariants defined in
//! ADR-015 §2.3 and the declaration-only runtime invariant in §4.2.
//! All tests that need to populate `AgentSkill.security_requirements`
//! use raw `turul_a2a_proto::AgentSkill` construction today; once
//! ADR-015 §5.5 lands the builder path
//! (`AgentSkillBuilder::security_requirements(...)` +
//! `AgentCardBuilder::security_schemes(...)` /
//! `::security_requirements(...)`) becomes the canonical adopter API
//! and these tests should migrate. Using raw proto here keeps the
//! test file compilable against current `main` so the post-merge
//! validator can be observed in its red phase.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use http::{Method, Request};
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::middleware::{A2aMiddleware, MiddlewareError, RequestContext, SecurityContribution};
use turul_a2a::server::A2aServer;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A skill whose proto is fully adopter-controlled so we can populate
/// `security_requirements` without depending on a builder setter that
/// does not yet exist on `main`.
fn skill_with_requirement(id: &str, scheme_name: &str) -> turul_a2a_proto::AgentSkill {
    let mut schemes = HashMap::new();
    schemes.insert(
        scheme_name.to_string(),
        turul_a2a_proto::StringList { list: vec![] },
    );
    turul_a2a_proto::AgentSkill {
        id: id.into(),
        name: id.into(),
        description: "skill used by ADR-015 tests".into(),
        tags: vec!["adr015".into()],
        examples: vec![],
        input_modes: vec!["text/plain".into()],
        output_modes: vec!["text/plain".into()],
        security_requirements: vec![turul_a2a_proto::SecurityRequirement { schemes }],
    }
}

fn bearer_scheme() -> turul_a2a_proto::SecurityScheme {
    turul_a2a_proto::SecurityScheme {
        scheme: Some(
            turul_a2a_proto::security_scheme::Scheme::HttpAuthSecurityScheme(
                turul_a2a_proto::HttpAuthSecurityScheme {
                    description: String::new(),
                    scheme: "Bearer".into(),
                    bearer_format: "JWT".into(),
                },
            ),
        ),
    }
}

fn api_key_scheme() -> turul_a2a_proto::SecurityScheme {
    turul_a2a_proto::SecurityScheme {
        scheme: Some(
            turul_a2a_proto::security_scheme::Scheme::ApiKeySecurityScheme(
                turul_a2a_proto::ApiKeySecurityScheme {
                    description: String::new(),
                    location: "header".into(),
                    name: "X-API-Key".into(),
                },
            ),
        ),
    }
}

fn base_card(skills: Vec<turul_a2a_proto::AgentSkill>) -> turul_a2a_proto::AgentCard {
    turul_a2a_proto::AgentCard {
        name: "ADR-015 Test Agent".into(),
        description: "Agent used by ADR-015 tests".into(),
        supported_interfaces: vec![turul_a2a_proto::AgentInterface {
            url: "http://localhost".into(),
            protocol_binding: "JSONRPC".into(),
            tenant: String::new(),
            protocol_version: "1.0".into(),
        }],
        provider: None,
        version: "1.0.0".into(),
        documentation_url: None,
        capabilities: Some(turul_a2a_proto::AgentCapabilities {
            streaming: Some(false),
            push_notifications: Some(false),
            extensions: vec![],
            extended_agent_card: Some(false),
        }),
        security_schemes: HashMap::new(),
        security_requirements: vec![],
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills,
        signatures: vec![],
        icon_url: None,
    }
}

/// Executor whose cards are parameterised: the `public` card is
/// returned from `agent_card()`, the `extended` card is returned from
/// `extended_agent_card(None)` (wrapped in `Some` when non-empty).
struct CardExecutor {
    public: turul_a2a_proto::AgentCard,
    extended: Option<turul_a2a_proto::AgentCard>,
}

#[async_trait]
impl AgentExecutor for CardExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let mut p = task.as_proto().clone();
        p.status = Some(turul_a2a_proto::TaskStatus {
            state: turul_a2a_proto::TaskState::Completed.into(),
            message: None,
            timestamp: None,
        });
        *task = Task::try_from(p).unwrap();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        self.public.clone()
    }

    fn extended_agent_card(
        &self,
        _claims: Option<&serde_json::Value>,
    ) -> Option<turul_a2a_proto::AgentCard> {
        self.extended.clone()
    }
}

/// Test middleware that contributes a "bearer" scheme via
/// `SecurityContribution` but does NOT reject unauthenticated traffic.
/// Used by tests that exercise scheme fill-in from the middleware
/// stack without requiring a live JWT validator.
struct BearerContributingMiddleware;

#[async_trait]
impl A2aMiddleware for BearerContributingMiddleware {
    async fn before_request(&self, _ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
        Ok(())
    }

    fn security_contribution(&self) -> SecurityContribution {
        SecurityContribution::new().with_scheme("bearer", bearer_scheme(), vec![])
    }
}

// ---------------------------------------------------------------------------
// §4.1 test 2 — public card agent-level path rejected (Red-B)
// ---------------------------------------------------------------------------

/// ADR-015 §4.1 test 3: agent-level `SecurityRequirement` naming a
/// scheme that is not in the merged `security_schemes` map causes
/// server build to fail with `InvalidRequest` citing the offending
/// scheme and "agent-level".
#[test]
fn server_build_rejects_agent_requirement_with_undeclared_scheme() {
    let mut public = base_card(vec![]);
    let mut agent_req = HashMap::new();
    agent_req.insert(
        "bearer".to_string(),
        turul_a2a_proto::StringList { list: vec![] },
    );
    public
        .security_requirements
        .push(turul_a2a_proto::SecurityRequirement { schemes: agent_req });

    let result = A2aServer::builder()
        .executor(CardExecutor {
            public,
            extended: None,
        })
        .storage(InMemoryA2aStorage::new())
        .build();

    let err = match result {
        Ok(_) => panic!("server build MUST reject agent-level undeclared scheme"),
        Err(e) => e,
    };
    let msg = format!("{err:?}");
    assert!(
        matches!(err, A2aError::InvalidRequest { .. }),
        "must be InvalidRequest; got {msg}"
    );
    assert!(
        msg.contains("bearer"),
        "error MUST name the offending scheme; got {msg}"
    );
    assert!(
        msg.to_lowercase().contains("agent")
            || msg.contains("agent-level")
            || msg.to_lowercase().contains("agent_card"),
        "error MUST identify which surface failed (agent-level / agent_card); got {msg}"
    );
}

/// ADR-015 §4.1 test 2: public-card skill-level `SecurityRequirement`
/// naming a scheme that is not in the merged `security_schemes` map
/// causes server build to fail with `InvalidRequest` citing the
/// offending scheme AND the skill id.
#[test]
fn server_build_rejects_skill_requirement_with_undeclared_scheme() {
    let public = base_card(vec![skill_with_requirement("search", "bearer")]);

    let result = A2aServer::builder()
        .executor(CardExecutor {
            public,
            extended: None,
        })
        .storage(InMemoryA2aStorage::new())
        .build();

    let err = match result {
        Ok(_) => panic!("server build MUST reject skill-level undeclared scheme"),
        Err(e) => e,
    };
    let msg = format!("{err:?}");
    assert!(
        matches!(err, A2aError::InvalidRequest { .. }),
        "must be InvalidRequest; got {msg}"
    );
    assert!(
        msg.contains("bearer"),
        "error MUST name the offending scheme; got {msg}"
    );
    assert!(
        msg.contains("search"),
        "error MUST name the offending skill id; got {msg}"
    );
}

/// ADR-015 §4.1 test 3a: an extended-card skill-level requirement
/// naming an undeclared scheme MUST also fail build. Without this
/// surface covered, the extended card could ship a reference that the
/// public card would have caught.
#[test]
fn server_build_rejects_extended_card_skill_requirement_with_undeclared_scheme() {
    let public = base_card(vec![]);
    let extended = base_card(vec![skill_with_requirement("ext-only", "bearer")]);

    let result = A2aServer::builder()
        .executor(CardExecutor {
            public,
            extended: Some(extended),
        })
        .storage(InMemoryA2aStorage::new())
        .build();

    let err = match result {
        Ok(_) => panic!("server build MUST reject extended-card undeclared scheme"),
        Err(e) => e,
    };
    let msg = format!("{err:?}");
    assert!(
        matches!(err, A2aError::InvalidRequest { .. }),
        "must be InvalidRequest; got {msg}"
    );
    assert!(msg.contains("bearer"), "error MUST name scheme; got {msg}");
    assert!(
        msg.contains("ext-only"),
        "error MUST name the skill id; got {msg}"
    );
    assert!(
        msg.contains("extended") || msg.contains("extended_agent_card"),
        "error MUST identify the failing surface as the extended card; got {msg}"
    );
}

// ---------------------------------------------------------------------------
// §4.1 tests 4-6 — post-merge acceptance paths
// ---------------------------------------------------------------------------

/// ADR-015 §4.1 test 4: a skill advertises a requirement naming
/// `"bearer"`; the adopter card declares no schemes; a middleware
/// contributes `"bearer"` via `SecurityContribution`. The validator
/// sees the merged map and accepts the card. `build()` returns Ok.
///
/// This test guards against the failure mode where the post-merge
/// validator runs against an un-merged card snapshot and rejects a
/// legitimate configuration.
#[tokio::test]
async fn server_build_accepts_skill_requirement_satisfied_by_middleware() {
    let public = base_card(vec![skill_with_requirement("search", "bearer")]);

    let server = A2aServer::builder()
        .executor(CardExecutor {
            public,
            extended: None,
        })
        .storage(InMemoryA2aStorage::new())
        .middleware(Arc::new(BearerContributingMiddleware))
        .build()
        .expect("build must succeed when middleware supplies the scheme");

    // The served public card MUST show exactly one `bearer` scheme.
    let router = server.into_router();
    let req = Request::builder()
        .method(Method::GET)
        .uri("/.well-known/agent-card.json")
        .header("A2A-Version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let card: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let schemes = card
        .get("securitySchemes")
        .and_then(|v| v.as_object())
        .expect("securitySchemes object (pbjson camelCase)");
    assert_eq!(
        schemes.len(),
        1,
        "exactly one scheme (bearer) should appear; got {schemes:?}"
    );
    assert!(
        schemes.contains_key("bearer"),
        "merged card must expose bearer; got {schemes:?}"
    );
    // The skill-level requirement must survive verbatim.
    let skill_reqs = card
        .get("skills")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|s| s.get("securityRequirements"))
        .and_then(|v| v.as_array())
        .expect("skills[0].securityRequirements (pbjson camelCase)");
    assert_eq!(skill_reqs.len(), 1);
    let only = &skill_reqs[0];
    assert!(only.get("schemes").and_then(|v| v.get("bearer")).is_some());
}

/// ADR-015 §4.1 test 5: the adopter both declares
/// `security_schemes["bearer"]` and references it from one skill's
/// `security_requirements`. No middleware. Build must succeed. The
/// served public card round-trips the skill-level requirement
/// untouched (camelCase per proto JSON mapping).
#[tokio::test]
async fn server_build_accepts_skill_requirement_declared_by_adopter() {
    let mut public = base_card(vec![skill_with_requirement("search", "bearer")]);
    public
        .security_schemes
        .insert("bearer".into(), bearer_scheme());

    let server = A2aServer::builder()
        .executor(CardExecutor {
            public,
            extended: None,
        })
        .storage(InMemoryA2aStorage::new())
        .build()
        .expect("build must succeed when adopter declares the scheme");

    let router = server.into_router();
    let req = Request::builder()
        .method(Method::GET)
        .uri("/.well-known/agent-card.json")
        .header("A2A-Version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let card: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    // skills[0].securityRequirements[0].schemes.bearer must be present.
    let bearer = card
        .pointer("/skills/0/securityRequirements/0/schemes/bearer")
        .expect("camelCase skill requirement for bearer must be preserved");
    assert!(
        bearer.is_object() || bearer.is_null() || bearer.get("list").is_some(),
        "StringList envelope for scopes expected; got {bearer}"
    );
}

/// ADR-015 §4.1 test 6: bearer middleware + an adopter-supplied
/// skill-level requirement naming the same scheme. The merged card
/// must:
///   - expose `securitySchemes.bearer` exactly once (dedup),
///   - carry the middleware-contributed agent-level requirement,
///   - preserve the skill's requirement verbatim.
#[tokio::test]
async fn middleware_contributions_and_skill_requirements_both_survive() {
    let public = base_card(vec![skill_with_requirement("search", "bearer")]);

    let server = A2aServer::builder()
        .executor(CardExecutor {
            public,
            extended: None,
        })
        .storage(InMemoryA2aStorage::new())
        .middleware(Arc::new(BearerContributingMiddleware))
        .build()
        .expect("build must succeed");

    let router = server.into_router();
    let req = Request::builder()
        .method(Method::GET)
        .uri("/.well-known/agent-card.json")
        .header("A2A-Version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let card: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let schemes = card
        .get("securitySchemes")
        .and_then(|v| v.as_object())
        .expect("securitySchemes object");
    assert_eq!(
        schemes.len(),
        1,
        "bearer must appear exactly once despite being named twice"
    );

    let agent_reqs = card
        .get("securityRequirements")
        .and_then(|v| v.as_array())
        .expect("agent-level securityRequirements array");
    assert!(
        !agent_reqs.is_empty(),
        "middleware-contributed agent requirement must be preserved"
    );
    assert!(
        agent_reqs
            .iter()
            .any(|r| r.pointer("/schemes/bearer").is_some()),
        "agent-level requirement from middleware must reference bearer"
    );

    let skill_reqs = card
        .pointer("/skills/0/securityRequirements")
        .and_then(|v| v.as_array())
        .expect("skills[0].securityRequirements");
    assert_eq!(
        skill_reqs.len(),
        1,
        "skill-level requirement must be preserved verbatim"
    );
    assert!(
        skill_reqs[0].pointer("/schemes/bearer").is_some(),
        "skill-level requirement must still name bearer"
    );
}

// ---------------------------------------------------------------------------
// §4.2 test 7 — declaration-only runtime invariant
// ---------------------------------------------------------------------------

/// ADR-015 §4.2 test 7: advertising a skill-level
/// `SecurityRequirement` MUST NOT, on its own, install any runtime
/// gatekeeper. With no middleware in the stack the request succeeds
/// regardless of whether the wire names any skill or carries any
/// Authorization header — the message body carries no
/// skill-targeting metadata by design.
///
/// The invariant under test is precisely that advertising
/// skill-level requirements is declaration-only; authorization is
/// governed solely by the installed middleware stack. If a successor
/// ADR both (a) introduces a normative way for a message to target a
/// skill and (b) enables runtime enforcement, the assertion in this
/// test flips and this comment's conditional becomes the migration
/// note.
#[tokio::test]
async fn advertised_skill_requirements_do_not_install_middleware() {
    let mut public = base_card(vec![skill_with_requirement("search", "bearer")]);
    public
        .security_schemes
        .insert("bearer".into(), bearer_scheme());
    // Extend the declared schemes with one more so the test is robust
    // to the validator computing a merged map that happens to include
    // additional middleware-contributed names.
    public
        .security_schemes
        .insert("apiKey".into(), api_key_scheme());

    let server = A2aServer::builder()
        .executor(CardExecutor {
            public,
            extended: None,
        })
        .storage(InMemoryA2aStorage::new())
        .build()
        .expect("build must succeed when all scheme refs are declared");
    let router = server.into_router();

    // Plain message body, no skill-targeting content, no Authorization.
    let body = serde_json::json!({
        "message": {
            "messageId": "no-skill-target",
            "role": "ROLE_USER",
            "parts": [{"text": "hello"}],
        }
    })
    .to_string();

    let req = Request::builder()
        .method(Method::POST)
        .uri("/message:send")
        .header("A2A-Version", "1.0")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "declaration-only: no middleware installed, request must pass"
    );
}
