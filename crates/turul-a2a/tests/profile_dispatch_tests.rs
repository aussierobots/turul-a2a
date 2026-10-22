//! Skill-invocation dispatcher profile — what the framework enforces.
//!
//! The framework owns the wire contract of the profile:
//!   1. Declaration of the extension URI on the AgentCard (adopter
//!      fixture here).
//!   2. Activation via the `A2A-Extensions` request header
//!      (comma-separated URI list, whitespace tolerant) on every send
//!      transport: HTTP blocking + streaming, JSON-RPC, gRPC.
//!   3. Echo of the negotiated URIs on the response.
//!   4. Rejection with `ExtensionSupportRequiredError` (JSON-RPC -32008,
//!      HTTP 400, gRPC FAILED_PRECONDITION; A2A v1.0.1 §3.3.4) when an
//!      advertised `required = true` extension is not activated.
//!   5. Request shape: once the profile is negotiated,
//!      `Message.metadata["a2a.skillId"]` must be a string, otherwise
//!      InvalidParams (HTTP 400 / JSON-RPC -32602 / gRPC
//!      INVALID_ARGUMENT) before any task is created.
//!
//! Routing to a skill — looking up `a2a.skillId`, deciding what an
//! unknown id means, consuming `a2a.skillParams` — is adopter code in
//! the `AgentExecutor` (see `examples/skill-dispatch-profile-agent`).
//! These tests therefore assert that the framework hands the message to
//! the executor unchanged, not which skill runs.

mod common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use http::{Method, Request};
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::profile_dispatch::{
    SKILL_ID_METADATA_KEY, SKILL_INVOCATION_PROFILE_V1, check_skill_invocation_request,
    parse_a2a_extensions, response_header_value, validate_activation,
};
use turul_a2a::server::A2aServer;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::pbjson::value_to_json;
use turul_a2a_types::wire::errors;
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
    let activated = HashSet::from([SKILL_INVOCATION_PROFILE_V1.to_string()]);
    let result = validate_activation(&activated, &advertised).expect("intersection ok");
    assert_eq!(
        result,
        HashSet::from([SKILL_INVOCATION_PROFILE_V1.to_string()])
    );
}

#[test]
fn validate_activation_required_but_not_activated_is_err() {
    let advertised = vec![skill_invocation_extension(true)];
    let err = validate_activation(&HashSet::new(), &advertised)
        .expect_err("required extension not activated must error");
    assert!(
        matches!(&err, A2aError::ExtensionSupportRequired { extension }
            if extension == SKILL_INVOCATION_PROFILE_V1),
        "expected ExtensionSupportRequiredError naming the URI, got {err:?}"
    );
}

#[test]
fn validate_activation_required_other_uri_does_not_satisfy() {
    let advertised = vec![skill_invocation_extension(true)];
    let activated = HashSet::from(["https://other.example/ext/v1".to_string()]);
    let err = validate_activation(&activated, &advertised)
        .expect_err("activating a different URI does not satisfy a required ext");
    assert!(matches!(err, A2aError::ExtensionSupportRequired { .. }));
}

#[test]
fn validate_activation_unknown_uri_is_ignored_when_no_required() {
    let advertised = vec![skill_invocation_extension(false)];
    let activated = HashSet::from(["https://unknown.example/ext/v9".to_string()]);
    // No required extension advertised, so an unknown activation just
    // produces an empty intersection — silently ignored, no error.
    let result = validate_activation(&activated, &advertised).expect("unknown uri ignored");
    assert!(result.is_empty());
}

#[test]
fn response_header_value_round_trip() {
    let set = HashSet::from([SKILL_INVOCATION_PROFILE_V1.to_string()]);
    assert_eq!(
        response_header_value(&set).as_deref(),
        Some(SKILL_INVOCATION_PROFILE_V1)
    );
    assert!(response_header_value(&HashSet::new()).is_none());
}

#[test]
fn skill_id_check_applies_only_when_profile_negotiated() {
    let no_skill = serde_json::json!({"message": {"messageId": "m", "metadata": {}}});
    check_skill_invocation_request(&HashSet::new(), &no_skill)
        .expect("profile not negotiated: no request-shape check");

    let negotiated = HashSet::from([SKILL_INVOCATION_PROFILE_V1.to_string()]);
    let err = check_skill_invocation_request(&negotiated, &no_skill).expect_err("missing id");
    assert!(matches!(err, A2aError::InvalidRequest { .. }), "{err:?}");
    assert_eq!(err.jsonrpc_code(), -32602);
    assert_eq!(err.http_status(), 400);

    let non_string = serde_json::json!({"message": {"metadata": {SKILL_ID_METADATA_KEY: 7}}});
    assert!(check_skill_invocation_request(&negotiated, &non_string).is_err());

    let ok = serde_json::json!({"message": {"metadata": {SKILL_ID_METADATA_KEY: "demo"}}});
    check_skill_invocation_request(&negotiated, &ok).expect("string skill id accepted");
}

// ---------------------------------------------------------------------------
// HTTP blocking send
// ---------------------------------------------------------------------------

/// Not activated, `required = false`: the default executor receives the
/// message unchanged and there is no echo.
#[tokio::test]
async fn extension_not_activated_runs_default_executor() {
    let (router, seen) = http_router(false);
    let resp = router
        .oneshot(http_post("/message:send", &send_body(None), &[]))
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers().get(RESPONSE_HEADER).is_none(),
        "missing activation header must produce no echo"
    );
    let body = json_body(resp).await;
    assert_eq!(
        body.pointer("/task/status/state").and_then(|v| v.as_str()),
        Some("TASK_STATE_COMPLETED")
    );
    assert_eq!(
        seen.skill_ids(),
        vec![None],
        "executor ran once, no skill id"
    );
}

/// Activated with a skill id: echo header, and the executor receives the
/// skill id in `Message.metadata` for its own routing.
#[tokio::test]
async fn extension_activated_passes_skill_id_to_executor_and_echoes() {
    let (router, seen) = http_router(false);
    let resp = router
        .oneshot(http_post(
            "/message:send",
            &send_body(Some(serde_json::json!("demo"))),
            &[("A2A-Extensions", SKILL_INVOCATION_PROFILE_V1)],
        ))
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get(RESPONSE_HEADER)
            .unwrap()
            .to_str()
            .unwrap(),
        SKILL_INVOCATION_PROFILE_V1
    );
    assert_eq!(seen.skill_ids(), vec![Some("demo".to_string())]);
}

/// The framework does not know which skills exist: an id the agent does
/// not recognise still reaches the executor, which decides between
/// InvalidParams and falling through to default behaviour.
#[tokio::test]
async fn extension_activated_unknown_skill_id_is_left_to_the_executor() {
    let (router, seen) = http_router(false);
    let resp = router
        .oneshot(http_post(
            "/message:send",
            &send_body(Some(serde_json::json!("no-such-skill"))),
            &[("A2A-Extensions", SKILL_INVOCATION_PROFILE_V1)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(seen.skill_ids(), vec![Some("no-such-skill".to_string())]);
}

/// Activated without `a2a.skillId`: InvalidParams (HTTP 400) and the
/// executor never runs.
#[tokio::test]
async fn extension_activated_missing_skill_id_returns_invalid_params() {
    let (router, seen) = http_router(false);
    let resp = router
        .oneshot(http_post(
            "/message:send",
            &send_body(None),
            &[("A2A-Extensions", SKILL_INVOCATION_PROFILE_V1)],
        ))
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 400);
    let body = json_body(resp).await;
    assert_eq!(
        body.pointer("/error/code").and_then(|v| v.as_u64()),
        Some(400),
        "got {body}"
    );
    let message = body
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(message.contains(SKILL_ID_METADATA_KEY), "got {body}");
    assert!(seen.skill_ids().is_empty(), "executor must not run");
}

#[tokio::test]
async fn extension_activated_non_string_skill_id_returns_invalid_params() {
    let (router, seen) = http_router(false);
    let resp = router
        .oneshot(http_post(
            "/message:send",
            &send_body(Some(serde_json::json!(42))),
            &[("A2A-Extensions", SKILL_INVOCATION_PROFILE_V1)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    assert!(seen.skill_ids().is_empty(), "executor must not run");
}

/// The skill-id check belongs to the profile: an agent that does not
/// advertise it ignores the header and applies no request-shape check.
#[tokio::test]
async fn profile_header_on_agent_without_profile_is_ignored() {
    let seen = Seen::default();
    let router = A2aServer::builder()
        .executor(ProfileTestExecutor {
            card: base_card(vec![], false),
            seen: seen.clone(),
        })
        .storage(InMemoryA2aStorage::new())
        .build()
        .unwrap()
        .into_router();
    let resp = router
        .oneshot(http_post(
            "/message:send",
            &send_body(None),
            &[("A2A-Extensions", SKILL_INVOCATION_PROFILE_V1)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp.headers().get(RESPONSE_HEADER).is_none());
    assert_eq!(seen.skill_ids(), vec![None]);
}

/// A required extension the client did not activate:
/// `ExtensionSupportRequiredError` (HTTP 400, `FAILED_PRECONDITION`) on
/// blocking and streaming sends, default and tenant routes, before the
/// executor runs.
#[tokio::test]
async fn http_send_required_extension_missing_returns_extension_support_required() {
    for path in ["/message:send", "/message:stream", "/acme/message:send"] {
        let (router, seen) =
            router_with_card(base_card(vec![skill_invocation_extension(true)], true));
        let resp = router
            .oneshot(http_post(path, &send_body(None), &[]))
            .await
            .unwrap();

        assert_eq!(
            resp.status().as_u16(),
            errors::HTTP_EXTENSION_SUPPORT_REQUIRED,
            "{path}"
        );
        let body = json_body(resp).await;
        assert_eq!(
            body.pointer("/error/details/0/reason")
                .and_then(|v| v.as_str()),
            Some(errors::REASON_EXTENSION_SUPPORT_REQUIRED),
            "{path}: {body}"
        );
        assert_eq!(
            body["error"]["status"], "FAILED_PRECONDITION",
            "{path}: {body}"
        );
        assert!(seen.skill_ids().is_empty(), "{path}: executor must not run");
    }
}

#[tokio::test]
async fn jsonrpc_send_required_extension_missing_returns_extension_support_required() {
    for method in ["SendMessage", "SendStreamingMessage"] {
        let (router, seen) =
            router_with_card(base_card(vec![skill_invocation_extension(true)], true));
        let resp = router
            .oneshot(http_post("/jsonrpc", &jsonrpc_call(method, None), &[]))
            .await
            .unwrap();
        let body = json_body(resp).await;
        assert_eq!(
            body["error"]["code"],
            errors::JSONRPC_EXTENSION_SUPPORT_REQUIRED,
            "{method}: {body}"
        );
        assert_eq!(
            body["error"]["data"][0]["reason"],
            errors::REASON_EXTENSION_SUPPORT_REQUIRED,
            "{method}: {body}"
        );
        assert!(
            seen.skill_ids().is_empty(),
            "{method}: executor must not run"
        );
    }
}

/// The negotiated set reaches the executor: the profile URI when the
/// client activated it, nothing otherwise.
#[tokio::test]
async fn executor_sees_activated_extensions() {
    let (router, seen) = http_router(false);
    router
        .clone()
        .oneshot(http_post(
            "/message:send",
            &send_body(Some(serde_json::json!("demo"))),
            &[("A2A-Extensions", SKILL_INVOCATION_PROFILE_V1)],
        ))
        .await
        .unwrap();
    router
        .oneshot(http_post("/message:send", &send_body(None), &[]))
        .await
        .unwrap();
    assert_eq!(
        seen.activated(),
        vec![vec![SKILL_INVOCATION_PROFILE_V1.to_string()], vec![]]
    );
}

#[tokio::test]
async fn http_send_unknown_extension_is_silently_ignored() {
    let (router, _) = http_router(false);
    let resp = router
        .oneshot(http_post(
            "/message:send",
            &send_body(None),
            &[(
                "A2A-Extensions",
                "https://example.invalid/never-heard-of/v9",
            )],
        ))
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers().get(RESPONSE_HEADER).is_none(),
        "unknown extension URI must not appear in the echo"
    );
}

// ---------------------------------------------------------------------------
// HTTP streaming send
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_streaming_send_echoes_activated_extension() {
    let (router, _) = streaming_http_router();
    let resp = router
        .oneshot(http_post(
            "/message:stream",
            &send_body(Some(serde_json::json!("demo"))),
            &[("A2A-Extensions", SKILL_INVOCATION_PROFILE_V1)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get(RESPONSE_HEADER)
            .unwrap()
            .to_str()
            .unwrap(),
        SKILL_INVOCATION_PROFILE_V1
    );
}

#[tokio::test]
async fn http_streaming_send_missing_skill_id_returns_invalid_params() {
    let (router, seen) = streaming_http_router();
    let resp = router
        .oneshot(http_post(
            "/message:stream",
            &send_body(None),
            &[("A2A-Extensions", SKILL_INVOCATION_PROFILE_V1)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    assert!(seen.skill_ids().is_empty(), "executor must not run");
}

#[tokio::test]
async fn tenant_scoped_send_missing_skill_id_returns_invalid_params() {
    let (router, seen) = http_router(false);
    let resp = router
        .oneshot(http_post(
            "/acme/message:send",
            &send_body(None),
            &[("A2A-Extensions", SKILL_INVOCATION_PROFILE_V1)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    assert!(seen.skill_ids().is_empty(), "executor must not run");
}

// ---------------------------------------------------------------------------
// JSON-RPC
// ---------------------------------------------------------------------------

fn jsonrpc_send(skill_id: Option<serde_json::Value>) -> String {
    jsonrpc_call("SendMessage", skill_id)
}

fn jsonrpc_call(method: &str, skill_id: Option<serde_json::Value>) -> String {
    let params: serde_json::Value = serde_json::from_str(&send_body(skill_id)).unwrap();
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": method,
        "params": params,
    })
    .to_string()
}

#[tokio::test]
async fn jsonrpc_send_echoes_activated_extension() {
    let (router, seen) = http_router(false);
    let resp = router
        .oneshot(http_post(
            "/jsonrpc",
            &jsonrpc_send(Some(serde_json::json!("demo"))),
            &[("A2A-Extensions", SKILL_INVOCATION_PROFILE_V1)],
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get(RESPONSE_HEADER)
            .unwrap()
            .to_str()
            .unwrap(),
        SKILL_INVOCATION_PROFILE_V1
    );
    let body = json_body(resp).await;
    assert!(body.get("result").is_some(), "got {body}");
    assert_eq!(seen.skill_ids(), vec![Some("demo".to_string())]);
}

#[tokio::test]
async fn jsonrpc_send_missing_skill_id_returns_invalid_params() {
    let (router, seen) = http_router(false);
    let resp = router
        .oneshot(http_post(
            "/jsonrpc",
            &jsonrpc_send(None),
            &[("A2A-Extensions", SKILL_INVOCATION_PROFILE_V1)],
        ))
        .await
        .unwrap();
    let body = json_body(resp).await;
    assert_eq!(body["id"], 7, "got {body}");
    assert_eq!(body["error"]["code"], -32602, "got {body}");
    assert!(seen.skill_ids().is_empty(), "executor must not run");
}

#[tokio::test]
async fn jsonrpc_streaming_send_missing_skill_id_returns_invalid_params() {
    let (router, seen) = streaming_http_router();
    let resp = router
        .oneshot(http_post(
            "/jsonrpc",
            &jsonrpc_call("SendStreamingMessage", None),
            &[("A2A-Extensions", SKILL_INVOCATION_PROFILE_V1)],
        ))
        .await
        .unwrap();
    let body = json_body(resp).await;
    assert_eq!(body["id"], 7, "got {body}");
    assert_eq!(body["error"]["code"], -32602, "got {body}");
    assert!(seen.skill_ids().is_empty(), "executor must not run");
}

// ---------------------------------------------------------------------------
// gRPC
// ---------------------------------------------------------------------------

#[cfg(feature = "grpc")]
mod grpc {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::Code;
    use turul_a2a_proto::grpc::A2aServiceClient;

    async fn client(seen: Seen) -> A2aServiceClient<common::VersionedChannel> {
        client_requiring(false, seen).await
    }

    async fn client_requiring(
        required: bool,
        seen: Seen,
    ) -> A2aServiceClient<common::VersionedChannel> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = A2aServer::builder()
            .executor(ProfileTestExecutor {
                card: base_card(vec![skill_invocation_extension(required)], true),
                seen,
            })
            .storage(InMemoryA2aStorage::new())
            .bind(([127, 0, 0, 1], 0))
            .build()
            .unwrap()
            .into_tonic_router();
        tokio::spawn(async move {
            let _ = router
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect_lazy();
        A2aServiceClient::with_interceptor(channel, common::with_a2a_version as _)
    }

    fn activated_request(
        skill_id: Option<serde_json::Value>,
    ) -> tonic::Request<turul_a2a_proto::SendMessageRequest> {
        let body: turul_a2a_proto::SendMessageRequest =
            serde_json::from_str(&send_body(skill_id)).unwrap();
        let mut req = tonic::Request::new(body);
        req.metadata_mut().insert(
            "a2a-extensions",
            SKILL_INVOCATION_PROFILE_V1.parse().unwrap(),
        );
        req
    }

    fn echo<T>(resp: &tonic::Response<T>) -> Option<String> {
        resp.metadata()
            .get(RESPONSE_HEADER)
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn grpc_send_echoes_activated_extension() {
        let seen = Seen::default();
        let mut client = client(seen.clone()).await;
        let resp = client
            .send_message(activated_request(Some(serde_json::json!("demo"))))
            .await
            .expect("send");
        assert_eq!(echo(&resp).as_deref(), Some(SKILL_INVOCATION_PROFILE_V1));
        assert_eq!(seen.skill_ids(), vec![Some("demo".to_string())]);
    }

    #[tokio::test]
    async fn grpc_streaming_send_echoes_activated_extension() {
        let mut client = client(Seen::default()).await;
        let resp = client
            .send_streaming_message(activated_request(Some(serde_json::json!("demo"))))
            .await
            .expect("stream");
        assert_eq!(echo(&resp).as_deref(), Some(SKILL_INVOCATION_PROFILE_V1));
    }

    #[tokio::test]
    async fn grpc_send_missing_skill_id_is_invalid_argument() {
        let seen = Seen::default();
        let mut client = client(seen.clone()).await;
        let status = client
            .send_message(activated_request(None))
            .await
            .expect_err("missing skill id must be rejected");
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(seen.skill_ids().is_empty(), "executor must not run");
    }

    #[tokio::test]
    async fn grpc_send_required_extension_missing_is_extension_support_required() {
        use tonic_types::StatusExt;
        let seen = Seen::default();
        let mut client = client_requiring(true, seen.clone()).await;
        let request = || {
            let body: turul_a2a_proto::SendMessageRequest =
                serde_json::from_str(&send_body(None)).unwrap();
            tonic::Request::new(body)
        };
        let unary = client
            .send_message(request())
            .await
            .expect_err("required extension not activated");
        let streaming = client
            .send_streaming_message(request())
            .await
            .expect_err("required extension not activated");
        for status in [unary, streaming] {
            assert_eq!(status.code(), Code::FailedPrecondition, "{status:?}");
            let info = status.get_details_error_info().expect("ErrorInfo");
            assert_eq!(info.reason, errors::REASON_EXTENSION_SUPPORT_REQUIRED);
        }
        assert!(seen.skill_ids().is_empty(), "executor must not run");
    }

    #[tokio::test]
    async fn grpc_executor_sees_activated_extensions() {
        let seen = Seen::default();
        let mut client = client(seen.clone()).await;
        client
            .send_message(activated_request(Some(serde_json::json!("demo"))))
            .await
            .expect("send");
        assert_eq!(
            seen.activated(),
            vec![vec![SKILL_INVOCATION_PROFILE_V1.to_string()]]
        );
    }

    #[tokio::test]
    async fn grpc_streaming_send_missing_skill_id_is_invalid_argument() {
        let seen = Seen::default();
        let mut client = client(seen.clone()).await;
        let status = client
            .send_streaming_message(activated_request(None))
            .await
            .expect_err("missing skill id must be rejected");
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(seen.skill_ids().is_empty(), "executor must not run");
    }
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

fn base_card(
    extensions: Vec<turul_a2a_proto::AgentExtension>,
    streaming: bool,
) -> turul_a2a_proto::AgentCard {
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
            streaming: Some(streaming),
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

/// What the executor saw: one entry per execution, holding the
/// `a2a.skillId` metadata value if present and
/// `ExecutionContext::activated_extensions`.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<Execution>>>);

/// `(a2a.skillId, activated_extensions)` for one execution.
type Execution = (Option<String>, Vec<String>);

impl Seen {
    fn skill_ids(&self) -> Vec<Option<String>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn activated(&self) -> Vec<Vec<String>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, activated)| activated.clone())
            .collect()
    }
}

struct ProfileTestExecutor {
    card: turul_a2a_proto::AgentCard,
    seen: Seen,
}

#[async_trait]
impl AgentExecutor for ProfileTestExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let skill_id = msg
            .metadata()
            .and_then(|m| m.fields.get(SKILL_ID_METADATA_KEY))
            .and_then(|v| value_to_json(v.clone()).as_str().map(str::to_string));
        self.seen
            .0
            .lock()
            .unwrap()
            .push((skill_id, ctx.activated_extensions.clone()));
        ctx.events.complete(None).await?;
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        self.card.clone()
    }
}

fn http_router(required: bool) -> (axum::Router, Seen) {
    router_with_card(base_card(vec![skill_invocation_extension(required)], false))
}

fn streaming_http_router() -> (axum::Router, Seen) {
    router_with_card(base_card(vec![skill_invocation_extension(false)], true))
}

fn router_with_card(card: turul_a2a_proto::AgentCard) -> (axum::Router, Seen) {
    let seen = Seen::default();
    let router = A2aServer::builder()
        .executor(ProfileTestExecutor {
            card,
            seen: seen.clone(),
        })
        .storage(InMemoryA2aStorage::new())
        .build()
        .expect("server build should succeed")
        .into_router();
    (router, seen)
}

fn http_post(uri: &str, body: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("Content-Type", "application/json")
        .header("A2A-Version", "1.0");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn json_body(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// A `SendMessageRequest` body; `skill_id` populates
/// `message.metadata["a2a.skillId"]` when given.
fn send_body(skill_id: Option<serde_json::Value>) -> String {
    let mut message = serde_json::json!({
        "messageId": uuid::Uuid::now_v7().to_string(),
        "role": "ROLE_USER",
        "parts": [{"text": "hello dispatcher"}],
    });
    if let Some(id) = skill_id {
        message["metadata"] = serde_json::json!({ SKILL_ID_METADATA_KEY: id });
    }
    serde_json::json!({ "message": message }).to_string()
}
