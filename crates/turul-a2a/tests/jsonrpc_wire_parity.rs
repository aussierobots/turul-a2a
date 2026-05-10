//! JSON-RPC wire-format parity tests.
//!
//! These tests pin the **exact** wire shape of A2A's `/jsonrpc` endpoint
//! before any internal dispatch refactor. Each assertion targets a specific
//! representational quirk that diverges from generic JSON-RPC 2.0 frameworks:
//!
//! - `id: null` literal preservation in parse-error and rejection replies
//! - explicit `id: null` rejected as -32600 Invalid Request (0.1.17+)
//! - `id: 0` numeric (zero is a valid id, must not be coerced to null)
//! - `id: "abc"` string preservation
//! - missing `id` key → notification → 204 No Content (NOT `id: null`)
//! - `params` array → -32602 (A2A is stricter than JSON-RPC 2.0)
//! - `params` scalar → -32602
//! - `params: null` → empty-object semantics, no error
//! - `params` absent → empty-object semantics, no error
//! - method-not-found message is exactly `"Method not found"` (no method name)
//! - error envelope carries `data` with `google.rpc.ErrorInfo` (`@type`, `reason`, `domain`)
//!
//! These goldens MUST NOT be edited unless the current behavior is proven
//! wrong against the A2A v1.0 normative contract (proto + spec) — not merely
//! inconvenient for any specific dispatcher implementation.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::router::{AppState, build_router};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

struct NoopExecutor;

#[async_trait::async_trait]
impl AgentExecutor for NoopExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        _message: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: "Parity Test Agent".into(),
            description: "wire parity".into(),
            supported_interfaces: vec![turul_a2a_proto::AgentInterface {
                url: "http://localhost:0".into(),
                protocol_binding: "JSONRPC".into(),
                tenant: String::new(),
                protocol_version: "1.0".into(),
            }],
            provider: None,
            version: "0.0.0".into(),
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
            skills: vec![],
            signatures: vec![],
            icon_url: None,
        }
    }
}

fn test_state() -> AppState {
    let s = InMemoryA2aStorage::new();
    AppState {
        executor: Arc::new(NoopExecutor),
        task_storage: Arc::new(s.clone()),
        push_storage: Arc::new(s.clone()),
        event_store: Arc::new(s.clone()),
        atomic_store: Arc::new(s),
        event_broker: turul_a2a::streaming::TaskEventBroker::new(),
        middleware_stack: Arc::new(turul_a2a::middleware::MiddlewareStack::new(vec![])),
        runtime_config: turul_a2a::server::RuntimeConfig::default(),
        in_flight: Arc::new(turul_a2a::server::in_flight::InFlightRegistry::new()),
        cancellation_supervisor: Arc::new(InMemoryA2aStorage::new()),
        push_delivery_store: None,
        push_dispatcher: None,
        durable_executor_queue: None,
    }
}

/// Send `body` to `/jsonrpc` and return (status, raw body bytes, parsed JSON).
async fn call(body: &str) -> (u16, Vec<u8>, serde_json::Value) {
    let router = build_router(test_state());
    let req = Request::post("/jsonrpc")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, bytes, json)
}

// ──────────────────────────────────────────────────────────────────────
// id literal preservation
// ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn parse_error_emits_literal_id_null() {
    // Parse error per JSON-RPC 2.0 §5.1: id MUST be the literal `null`.
    // RequestId enums that lack a Null variant cannot represent this.
    let (_, bytes, body) = call("not valid json{{{").await;
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["error"]["code"], -32700);
    assert!(body["id"].is_null(), "id must be null Value");
    let raw = std::str::from_utf8(&bytes).unwrap();
    assert!(
        raw.contains("\"id\":null"),
        "raw body must contain literal \"id\":null, got: {raw}"
    );
}

#[tokio::test]
async fn explicit_id_null_is_rejected_as_invalid_request() {
    // 0.1.17+ wire tightening: explicit `id: null` (key present, value null)
    // is rejected as -32600 Invalid Request. Notifications (id key absent)
    // are unaffected. The rejection response echoes `id: null` per
    // JSON-RPC 2.0 §5.1 ("MUST be Null when id cannot be detected/used").
    let body_str = r#"{"jsonrpc":"2.0","method":"GetTask","params":{},"id":null}"#.to_string();
    let (status, bytes, body) = call(&body_str).await;
    assert_eq!(
        status, 200,
        "rejection uses 200 with JSON-RPC error envelope"
    );
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["error"]["code"], -32600);
    assert!(
        body["id"].is_null(),
        "rejection response must carry id: null"
    );
    let raw = std::str::from_utf8(&bytes).unwrap();
    assert!(
        raw.contains("\"id\":null"),
        "raw body must contain literal \"id\":null, got: {raw}"
    );
}

#[tokio::test]
async fn id_zero_numeric_is_preserved() {
    // Zero is a valid request id and must not be coerced to null.
    let body_str = r#"{"jsonrpc":"2.0","method":"NoSuchMethod","params":{},"id":0}"#.to_string();
    let (_, bytes, body) = call(&body_str).await;
    assert_eq!(body["id"], 0);
    let raw = std::str::from_utf8(&bytes).unwrap();
    assert!(raw.contains("\"id\":0"), "raw body must contain \"id\":0");
}

#[tokio::test]
async fn id_string_is_preserved() {
    let body_str =
        r#"{"jsonrpc":"2.0","method":"NoSuchMethod","params":{},"id":"abc"}"#.to_string();
    let (_, _, body) = call(&body_str).await;
    assert_eq!(body["id"], "abc");
}

#[tokio::test]
async fn missing_id_is_notification_returns_204_no_body() {
    // Missing id key (NOT id:null) → notification → 204, empty body.
    let body_str = r#"{"jsonrpc":"2.0","method":"NoSuchMethod","params":{}}"#.to_string();
    let (status, bytes, _) = call(&body_str).await;
    assert_eq!(status, 204);
    assert!(bytes.is_empty(), "204 must have empty body, got {bytes:?}");
}

// ──────────────────────────────────────────────────────────────────────
// params shape validation (A2A is stricter than JSON-RPC 2.0)
// ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn params_array_returns_invalid_params() {
    // JSON-RPC 2.0 allows positional params; A2A rejects with -32602.
    let body_str = r#"{"jsonrpc":"2.0","method":"GetTask","params":[1,2,3],"id":1}"#.to_string();
    let (_, _, body) = call(&body_str).await;
    assert_eq!(body["error"]["code"], -32602);
    assert_eq!(body["id"], 1);
}

#[tokio::test]
async fn params_scalar_returns_invalid_params() {
    let body_str = r#"{"jsonrpc":"2.0","method":"GetTask","params":5,"id":2}"#.to_string();
    let (_, _, body) = call(&body_str).await;
    assert_eq!(body["error"]["code"], -32602);
    assert_eq!(body["id"], 2);
}

#[tokio::test]
async fn params_null_and_params_absent_are_envelope_equivalent() {
    // Invariant: at the envelope-validation layer, `params: null` and an
    // absent `params` key produce identical downstream behavior. Both must
    // pass envelope validation (not be rejected as "params shape") and
    // reach the same dispatch outcome.
    //
    // We compare the two responses directly. The dispatcher may legitimately
    // reject for missing required method fields with -32602; that's a
    // method-level concern, not an envelope-level one. The pin here is
    // equivalence between the two forms, not the specific outcome.
    let null_body = r#"{"jsonrpc":"2.0","method":"GetTask","params":null,"id":3}"#.to_string();
    let absent_body = r#"{"jsonrpc":"2.0","method":"GetTask","id":3}"#.to_string();
    let (_, _, body_null) = call(&null_body).await;
    let (_, _, body_absent) = call(&absent_body).await;
    assert_eq!(
        body_null, body_absent,
        "params:null and params-absent must produce identical responses; null={body_null}, absent={body_absent}"
    );
}

// ──────────────────────────────────────────────────────────────────────
// error message wording and ErrorInfo envelope
// ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn method_not_found_message_is_exactly_method_not_found() {
    // A2A emits "Method not found" with NO method name interpolation.
    // Generic JSON-RPC dispatchers typically emit "Method 'X' not found".
    let body_str = r#"{"jsonrpc":"2.0","method":"DoesNotExist","params":{},"id":7}"#.to_string();
    let (_, _, body) = call(&body_str).await;
    assert_eq!(body["error"]["code"], -32601);
    assert_eq!(
        body["error"]["message"], "Method not found",
        "wording must be exact, no method-name interpolation"
    );
    assert!(
        !body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("DoesNotExist"),
        "method name must NOT appear in message"
    );
}

#[tokio::test]
async fn a2a_error_envelope_carries_google_rpc_errorinfo() {
    // GetTask on missing id → TaskNotFound → -32001 with ErrorInfo in `data`.
    // GetTaskRequest.id is the required field per proto/a2a.proto §654.
    let params = serde_json::json!({"id": "does-not-exist"});
    let body_str = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "GetTask",
        "params": params,
        "id": 8,
    })
    .to_string();
    let (_, _, body) = call(&body_str).await;
    assert_eq!(body["error"]["code"], -32001);
    let data = &body["error"]["data"];
    assert_eq!(
        data["@type"], "type.googleapis.com/google.rpc.ErrorInfo",
        "ErrorInfo @type must be the canonical google.rpc.ErrorInfo URL"
    );
    assert_eq!(data["reason"], "TASK_NOT_FOUND");
    assert_eq!(data["domain"], "a2a-protocol.org");
}
