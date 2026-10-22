//! Skill-level `security_requirements` — advertisement vs. enforcement.
//!
//! These tests pin two invariants:
//!
//! - Post-merge truthfulness: every scheme name advertised on the
//!   built card (agent-level or skill-level, public or extended) must
//!   resolve to an installed middleware contribution. The server
//!   builder rejects mismatches.
//! - Declaration-only runtime: advertising a skill-level
//!   `security_requirement` does NOT install runtime enforcement —
//!   the framework publishes the requirement on the card and trusts
//!   the executor to gate.
//!
//! Builder paths (`AgentSkillBuilder::security_requirements(...)`,
//! `AgentCardBuilder::security_schemes(...)`,
//! `AgentCardBuilder::security_requirements(...)`) are the canonical
//! adopter API; tests use them where available and fall back to raw
//! `turul_a2a_proto::AgentSkill` construction otherwise.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use http::{Method, Request};
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::card_builder::AgentSkillBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::middleware::{A2aMiddleware, MiddlewareError, RequestContext, SecurityContribution};
use turul_a2a::server::A2aServer;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A skill carrying one requirement on `scheme_name` (no scopes), built
/// through the adopter-facing `AgentSkillBuilder`.
fn skill_with_requirement(id: &str, scheme_name: &str) -> turul_a2a_proto::AgentSkill {
    let schemes = HashMap::from([(
        scheme_name.to_string(),
        turul_a2a_proto::StringList { list: vec![] },
    )]);
    AgentSkillBuilder::new(id, id, "skill used by skill-security tests")
        .tags(vec!["skill-security"])
        .input_modes(vec!["text/plain"])
        .output_modes(vec!["text/plain"])
        .security_requirements(vec![turul_a2a_proto::SecurityRequirement { schemes }])
        .build()
}

/// The `InvalidRequest` message of a rejected server build.
fn build_error_message(result: Result<A2aServer, A2aError>) -> String {
    match result {
        Ok(_) => panic!("server build MUST be rejected"),
        Err(A2aError::InvalidRequest { message }) => message,
        Err(other) => panic!("server build must fail with InvalidRequest; got {other:?}"),
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
// Post-merge truthfulness — agent-level path
// ---------------------------------------------------------------------------

/// Agent-level `SecurityRequirement` naming a scheme that is not in
/// the merged `security_schemes` map causes server build to fail with
/// `InvalidRequest` naming the failing surface (`agent_card`), the
/// agent-level requirement, and the offending scheme.
#[test]
fn server_build_rejects_agent_requirement_with_undeclared_scheme() {
    let mut public = base_card(vec![]);
    public
        .security_requirements
        .push(turul_a2a_proto::SecurityRequirement {
            schemes: HashMap::from([(
                "bearer".to_string(),
                turul_a2a_proto::StringList { list: vec![] },
            )]),
        });

    let msg = build_error_message(
        A2aServer::builder()
            .executor(CardExecutor {
                public,
                extended: None,
            })
            .storage(InMemoryA2aStorage::new())
            .build(),
    );
    assert!(
        msg.starts_with("agent_card: "),
        "error MUST name the public card surface; got {msg}"
    );
    assert!(msg.contains("agent-level"), "got {msg}");
    assert!(
        msg.contains("'bearer'"),
        "error MUST name the scheme; got {msg}"
    );
}

/// Public-card skill-level `SecurityRequirement` naming a scheme that
/// is not in the merged `security_schemes` map causes server build to
/// fail with `InvalidRequest` naming the surface, the skill id and the
/// scheme.
#[test]
fn server_build_rejects_skill_requirement_with_undeclared_scheme() {
    let public = base_card(vec![skill_with_requirement("search", "bearer")]);

    let msg = build_error_message(
        A2aServer::builder()
            .executor(CardExecutor {
                public,
                extended: None,
            })
            .storage(InMemoryA2aStorage::new())
            .build(),
    );
    assert!(
        msg.starts_with("agent_card: "),
        "error MUST name the public card surface; got {msg}"
    );
    assert!(
        msg.contains("skill 'search'"),
        "error MUST name the skill id; got {msg}"
    );
    assert!(
        msg.contains("'bearer'"),
        "error MUST name the scheme; got {msg}"
    );
}

/// An extended-card skill-level requirement naming an undeclared
/// scheme MUST also fail build, and the error must say it was the
/// extended card — the public card here is valid. Without this surface
/// covered, the extended card could ship a reference the public card
/// would have caught.
#[test]
fn server_build_rejects_extended_card_skill_requirement_with_undeclared_scheme() {
    let public = base_card(vec![]);
    let extended = base_card(vec![skill_with_requirement("ext-only", "bearer")]);

    let msg = build_error_message(
        A2aServer::builder()
            .executor(CardExecutor {
                public,
                extended: Some(extended),
            })
            .storage(InMemoryA2aStorage::new())
            .build(),
    );
    assert!(
        msg.starts_with("extended_agent_card: "),
        "error MUST name the extended card surface; got {msg}"
    );
    assert!(
        msg.contains("skill 'ext-only'"),
        "error MUST name the skill id; got {msg}"
    );
    assert!(
        msg.contains("'bearer'"),
        "error MUST name the scheme; got {msg}"
    );
}

// ---------------------------------------------------------------------------
// Post-merge acceptance paths
// ---------------------------------------------------------------------------

/// A skill advertises a requirement naming `"bearer"`; the adopter
/// card declares no schemes; a middleware contributes `"bearer"` via
/// `SecurityContribution`. The validator sees the merged map and
/// accepts the card. `build()` returns Ok.
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

/// The adopter both declares `security_schemes["bearer"]` and
/// references it from one skill's `security_requirements`. No
/// middleware. Build must succeed. The served public card
/// round-trips the skill-level requirement
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

    // The skill requirement round-trips exactly: one requirement naming
    // `bearer` with an empty scope list (proto JSON omits the empty
    // `list`, leaving an empty StringList object).
    assert_eq!(
        card.pointer("/skills/0/securityRequirements"),
        Some(&serde_json::json!([{ "schemes": { "bearer": {} } }])),
        "got {card}"
    );
}

/// Bearer middleware + an adopter-supplied skill-level requirement
/// naming the same scheme. The merged card must:
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
// Declaration-only runtime invariant
// ---------------------------------------------------------------------------

/// Advertising a skill-level `SecurityRequirement` MUST NOT, on its
/// own, install any runtime gatekeeper. With no middleware in the stack
/// a request without any Authorization header succeeds and the executor
/// runs. The message carries no skill-targeting metadata: the A2A proto
/// has no normative way for a message to target a skill.
///
/// The invariant under test is precisely that advertising
/// skill-level requirements is declaration-only; authorization is
/// governed solely by the installed middleware stack. If a future
/// spec revision introduces a normative way for a message to target
/// a specific skill AND enables runtime enforcement keyed on that
/// targeting, the assertion in this test flips.
#[tokio::test]
async fn advertised_skill_requirements_do_not_install_middleware() {
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
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body.pointer("/task/status/state").and_then(|v| v.as_str()),
        Some("TASK_STATE_COMPLETED"),
        "the executor ran ungated; got {body}"
    );
}

/// An extended-card skill requirement naming a scheme only a middleware
/// contributes: build accepts it (the extended card is validated after
/// the same merge as the public card), and the served extended card
/// carries both the merged scheme and the skill requirement.
#[tokio::test]
async fn server_build_accepts_extended_card_skill_requirement_satisfied_by_middleware() {
    let mut public = base_card(vec![]);
    if let Some(caps) = public.capabilities.as_mut() {
        caps.extended_agent_card = Some(true);
    }
    let extended = base_card(vec![skill_with_requirement("ext-only", "bearer")]);

    let server = A2aServer::builder()
        .executor(CardExecutor {
            public,
            extended: Some(extended),
        })
        .storage(InMemoryA2aStorage::new())
        .middleware(Arc::new(BearerContributingMiddleware))
        .build()
        .expect("build must succeed when middleware supplies the extended card's scheme");

    let req = Request::builder()
        .method(Method::GET)
        .uri("/extendedAgentCard")
        .header("A2A-Version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = server.into_router().oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let card: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let schemes = card
        .get("securitySchemes")
        .and_then(|v| v.as_object())
        .expect("securitySchemes object");
    assert_eq!(schemes.keys().collect::<Vec<_>>(), ["bearer"], "got {card}");
    assert_eq!(
        card.pointer("/skills/0/securityRequirements"),
        Some(&serde_json::json!([{ "schemes": { "bearer": {} } }])),
        "got {card}"
    );
}
