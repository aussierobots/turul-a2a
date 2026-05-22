//! Skill-invocation dispatcher profile — extension activation tests.
//!
//! These tests pin the four-point profile shape:
//!   1. Declaration of the extension URI on the AgentCard.
//!   2. Activation via the `A2A-Extensions` HTTP request header
//!      (comma-separated URI list, whitespace tolerant).
//!   3. Echo of activated URIs on the response.
//!   4. Rejection with `UnsupportedOperationError` when an advertised
//!      `required = true` extension is not activated.
//!
//! The dispatcher itself is wire-bookkeeping only — payload routing
//! (reading `Message.metadata["a2a.skillId"]`) is adopter code in the
//! `AgentExecutor`. Tests therefore exercise the header / card surface
//! and do not assert on per-skill payload dispatch.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use http::{Method, Request};
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::profile_dispatch::{
    SKILL_INVOCATION_PROFILE_V1, parse_a2a_extensions, response_header_value, validate_activation,
};
use turul_a2a::server::A2aServer;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

const RESPONSE_HEADER: &str = "a2a-extensions";

// ---------------------------------------------------------------------------
// Pure-unit coverage of the parser + validator
// ---------------------------------------------------------------------------

#[test]
fn parse_a2a_extensions_handles_empty_input() {
    assert!(parse_a2a_extensions(None).is_empty());
    assert!(parse_a2a_extensions(Some("")).is_empty());
    assert!(parse_a2a_extensions(Some("   ")).is_empty());
}

#[test]
fn parse_a2a_extensions_handles_single_uri() {
    let set = parse_a2a_extensions(Some(SKILL_INVOCATION_PROFILE_V1));
    assert_eq!(set.len(), 1);
    assert!(set.contains(SKILL_INVOCATION_PROFILE_V1));
}

#[test]
fn parse_a2a_extensions_handles_multi_uri_with_whitespace() {
    let raw = format!(" {SKILL_INVOCATION_PROFILE_V1}  ,   https://other.example/ext/v1 ");
    let set = parse_a2a_extensions(Some(&raw));
    assert_eq!(set.len(), 2);
    assert!(set.contains(SKILL_INVOCATION_PROFILE_V1));
    assert!(set.contains("https://other.example/ext/v1"));
}

#[test]
fn parse_a2a_extensions_drops_empty_segments() {
    let set = parse_a2a_extensions(Some(",  ,foo, ,"));
    assert_eq!(set.len(), 1);
    assert!(set.contains("foo"));
}

#[test]
fn validate_activation_clean_intersection() {
    let advertised = vec![skill_invocation_extension(false)];
    let mut activated = HashSet::new();
    activated.insert(SKILL_INVOCATION_PROFILE_V1.to_string());

    let result = validate_activation(&activated, &advertised).expect("intersection ok");
    assert_eq!(result.len(), 1);
    assert!(result.contains(SKILL_INVOCATION_PROFILE_V1));
}

#[test]
fn validate_activation_required_but_not_activated_is_err() {
    let advertised = vec![skill_invocation_extension(true)];
    let activated = HashSet::new();

    let err = validate_activation(&activated, &advertised)
        .expect_err("required extension not activated must error");
    assert!(
        matches!(err, A2aError::UnsupportedOperation { .. }),
        "expected UnsupportedOperationError, got {err:?}"
    );
}

#[test]
fn validate_activation_required_other_uri_does_not_satisfy() {
    let advertised = vec![skill_invocation_extension(true)];
    let mut activated = HashSet::new();
    activated.insert("https://other.example/ext/v1".to_string());

    let err = validate_activation(&activated, &advertised)
        .expect_err("activating a different URI does not satisfy a required ext");
    assert!(matches!(err, A2aError::UnsupportedOperation { .. }));
}

#[test]
fn validate_activation_unknown_uri_is_ignored_when_no_required() {
    let advertised = vec![skill_invocation_extension(false)];
    let mut activated = HashSet::new();
    activated.insert("https://unknown.example/ext/v9".to_string());

    // No required extension advertised, so an unknown activation just
    // produces an empty intersection — silently ignored, no error.
    let result = validate_activation(&activated, &advertised).expect("unknown uri ignored");
    assert!(result.is_empty());
}

#[test]
fn response_header_value_round_trip() {
    let mut set = HashSet::new();
    set.insert(SKILL_INVOCATION_PROFILE_V1.to_string());
    let value = response_header_value(&set).expect("non-empty set yields Some");
    assert_eq!(value, SKILL_INVOCATION_PROFILE_V1);

    let empty: HashSet<String> = HashSet::new();
    assert!(response_header_value(&empty).is_none());
}

// ---------------------------------------------------------------------------
// End-to-end through the router
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_send_echoes_activated_extension() {
    let server = build_test_server(false);
    let router = server.into_router();

    let body = sample_send_body();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/message:send")
        .header("Content-Type", "application/json")
        .header("A2A-Version", "1.0")
        .header("A2A-Extensions", SKILL_INVOCATION_PROFILE_V1)
        .body(Body::from(body))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let echo = resp
        .headers()
        .get(RESPONSE_HEADER)
        .expect("response must echo A2A-Extensions");
    assert_eq!(echo.to_str().unwrap(), SKILL_INVOCATION_PROFILE_V1);
}

#[tokio::test]
async fn http_send_without_header_does_not_echo() {
    let server = build_test_server(false);
    let router = server.into_router();

    let body = sample_send_body();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/message:send")
        .header("Content-Type", "application/json")
        .header("A2A-Version", "1.0")
        .body(Body::from(body))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers().get(RESPONSE_HEADER).is_none(),
        "missing activation header must produce no echo"
    );
}

#[tokio::test]
async fn http_send_required_extension_missing_returns_unsupported_operation() {
    let server = build_test_server(true);
    let router = server.into_router();

    let body = sample_send_body();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/message:send")
        .header("Content-Type", "application/json")
        .header("A2A-Version", "1.0")
        .body(Body::from(body))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "UnsupportedOperationError maps to HTTP 400 per A2A error model"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let reason = body
        .pointer("/error/details/0/reason")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        reason, "UNSUPPORTED_OPERATION",
        "error body must report UNSUPPORTED_OPERATION reason; got {body}"
    );
}

#[tokio::test]
async fn http_send_unknown_extension_is_silently_ignored() {
    let server = build_test_server(false);
    let router = server.into_router();

    let body = sample_send_body();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/message:send")
        .header("Content-Type", "application/json")
        .header("A2A-Version", "1.0")
        .header(
            "A2A-Extensions",
            "https://example.invalid/never-heard-of/v9",
        )
        .body(Body::from(body))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers().get(RESPONSE_HEADER).is_none(),
        "unknown extension URI must not appear in the echo"
    );
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn skill_invocation_extension(required: bool) -> turul_a2a_proto::AgentExtension {
    turul_a2a_proto::AgentExtension {
        uri: SKILL_INVOCATION_PROFILE_V1.into(),
        description: "skill-invocation dispatcher test fixture".into(),
        required,
        params: None,
    }
}

fn base_card(extensions: Vec<turul_a2a_proto::AgentExtension>) -> turul_a2a_proto::AgentCard {
    turul_a2a_proto::AgentCard {
        name: "Profile Dispatch Test Agent".into(),
        description: "Agent used by profile-dispatch tests".into(),
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
            extensions,
            extended_agent_card: Some(false),
        }),
        security_schemes: std::collections::HashMap::new(),
        security_requirements: vec![],
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![],
        signatures: vec![],
        icon_url: None,
    }
}

struct ProfileTestExecutor {
    card: turul_a2a_proto::AgentCard,
}

#[async_trait]
impl AgentExecutor for ProfileTestExecutor {
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
        self.card.clone()
    }
}

fn build_test_server(required: bool) -> A2aServer {
    let card = base_card(vec![skill_invocation_extension(required)]);
    A2aServer::builder()
        .executor(ProfileTestExecutor { card })
        .storage(InMemoryA2aStorage::new())
        .build()
        .expect("server build should succeed")
}

fn sample_send_body() -> String {
    let req = turul_a2a_proto::SendMessageRequest {
        message: Some(turul_a2a_proto::Message {
            message_id: "msg-prof-1".into(),
            role: turul_a2a_proto::Role::User.into(),
            parts: vec![turul_a2a_proto::Part {
                content: Some(turul_a2a_proto::part::Content::Text(
                    "hello dispatcher".into(),
                )),
                metadata: None,
                filename: String::new(),
                media_type: String::new(),
            }],
            context_id: String::new(),
            task_id: String::new(),
            extensions: vec![],
            metadata: None,
            reference_task_ids: vec![],
        }),
        configuration: None,
        metadata: None,
        tenant: String::new(),
    };
    serde_json::to_string(&req).expect("request serializes")
}

#[allow(dead_code)]
fn _arc_typecheck(s: Arc<InMemoryA2aStorage>) -> Arc<InMemoryA2aStorage> {
    s
}
