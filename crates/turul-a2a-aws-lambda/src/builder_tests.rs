//! Builder wiring tests for same-backend requirements on the Lambda
//! adapter. Proves that:
//!
//! - `.storage()` requires `A2aCancellationSupervisor` on the bundled
//!   backend (bound at the type-system level).
//! - `build()` rejects configurations where the cancellation supervisor
//!   is omitted — no silent fallback to a different backend.
//! - `build()` rejects same-backend mismatches that cross the supervisor
//!   boundary.
//!
//! Without these guarantees a production Lambda deployment would write
//! cancel markers to DynamoDB/Postgres while the supervisor reads from
//! an unrelated in-memory store, silently breaking cross-instance
//! cancellation.

use async_trait::async_trait;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::storage::{
    A2aAtomicStore, A2aCancellationSupervisor, A2aEventStore, A2aPushNotificationStorage,
    A2aStorageError, A2aTaskStorage, InMemoryA2aStorage,
};
use turul_a2a_types::{Message, Task};

use crate::LambdaA2aHandler;

struct DummyExecutor;

#[async_trait]
impl AgentExecutor for DummyExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// Like [`DummyExecutor`], but its card declares push notifications, so
/// push-config CRUD is allowed (A2A v1.0.1 §3.3.4).
struct PushCapableExecutor;

#[async_trait]
impl AgentExecutor for PushCapableExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            capabilities: Some(turul_a2a_proto::AgentCapabilities {
                push_notifications: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

/// Stand-in second-backend type used to exercise same-backend enforcement.
/// Implements every storage trait so we can plug it into the individual
/// setters while reporting a distinct `backend_name`.
#[derive(Clone, Default)]
struct FakeBackend;

#[async_trait]
impl A2aTaskStorage for FakeBackend {
    fn backend_name(&self) -> &'static str {
        "fake-backend"
    }
    async fn create_task(&self, _t: &str, _o: &str, task: Task) -> Result<Task, A2aStorageError> {
        Ok(task)
    }
    async fn get_task(
        &self,
        _t: &str,
        _tid: &str,
        _o: &str,
        _h: Option<i32>,
    ) -> Result<Option<Task>, A2aStorageError> {
        Ok(None)
    }
    async fn update_task(&self, _t: &str, _o: &str, _task: Task) -> Result<(), A2aStorageError> {
        Ok(())
    }
    async fn delete_task(&self, _t: &str, _tid: &str, _o: &str) -> Result<bool, A2aStorageError> {
        Ok(false)
    }
    async fn list_tasks(
        &self,
        _f: turul_a2a::storage::TaskFilter,
    ) -> Result<turul_a2a::storage::TaskListPage, A2aStorageError> {
        Ok(turul_a2a::storage::TaskListPage {
            tasks: vec![],
            next_page_token: String::new(),
            page_size: 0,
            total_size: 0,
        })
    }
    async fn update_task_status(
        &self,
        _t: &str,
        _tid: &str,
        _o: &str,
        _s: turul_a2a_types::TaskStatus,
    ) -> Result<Task, A2aStorageError> {
        unimplemented!()
    }
    async fn append_message(
        &self,
        _t: &str,
        _tid: &str,
        _o: &str,
        _m: Message,
    ) -> Result<(), A2aStorageError> {
        Ok(())
    }
    async fn append_artifact(
        &self,
        _t: &str,
        _tid: &str,
        _o: &str,
        _a: turul_a2a_types::Artifact,
        _append: bool,
        _last: bool,
    ) -> Result<(), A2aStorageError> {
        Ok(())
    }
    async fn task_count(&self) -> Result<usize, A2aStorageError> {
        Ok(0)
    }
    async fn maintenance(&self) -> Result<(), A2aStorageError> {
        Ok(())
    }
    async fn set_cancel_requested(
        &self,
        _t: &str,
        _tid: &str,
        _o: &str,
    ) -> Result<(), A2aStorageError> {
        Ok(())
    }
}

#[async_trait]
impl A2aPushNotificationStorage for FakeBackend {
    fn backend_name(&self) -> &'static str {
        "fake-backend"
    }
    async fn create_config(
        &self,
        _t: &str,
        c: turul_a2a_proto::TaskPushNotificationConfig,
    ) -> Result<turul_a2a_proto::TaskPushNotificationConfig, A2aStorageError> {
        Ok(c)
    }
    async fn get_config(
        &self,
        _t: &str,
        _tid: &str,
        _c: &str,
    ) -> Result<Option<turul_a2a_proto::TaskPushNotificationConfig>, A2aStorageError> {
        Ok(None)
    }
    async fn list_configs(
        &self,
        _t: &str,
        _tid: &str,
        _p: Option<&str>,
        _ps: Option<i32>,
    ) -> Result<turul_a2a::storage::PushConfigListPage, A2aStorageError> {
        Ok(turul_a2a::storage::PushConfigListPage {
            configs: vec![],
            next_page_token: String::new(),
        })
    }
    async fn delete_config(&self, _t: &str, _tid: &str, _c: &str) -> Result<(), A2aStorageError> {
        Ok(())
    }
    async fn list_configs_eligible_at_event(
        &self,
        _t: &str,
        _tid: &str,
        _seq: u64,
        _p: Option<&str>,
        _ps: Option<i32>,
    ) -> Result<turul_a2a::storage::PushConfigListPage, A2aStorageError> {
        Ok(turul_a2a::storage::PushConfigListPage {
            configs: vec![],
            next_page_token: String::new(),
        })
    }
}

#[async_trait]
impl A2aEventStore for FakeBackend {
    fn backend_name(&self) -> &'static str {
        "fake-backend"
    }
    async fn append_event(
        &self,
        _t: &str,
        _tid: &str,
        _e: turul_a2a::streaming::StreamEvent,
    ) -> Result<u64, A2aStorageError> {
        Ok(0)
    }
    async fn get_events_after(
        &self,
        _t: &str,
        _tid: &str,
        _s: u64,
    ) -> Result<Vec<(u64, turul_a2a::streaming::StreamEvent)>, A2aStorageError> {
        Ok(vec![])
    }
    async fn latest_sequence(&self, _t: &str, _tid: &str) -> Result<u64, A2aStorageError> {
        Ok(0)
    }
    async fn cleanup_expired(&self) -> Result<u64, A2aStorageError> {
        Ok(0)
    }
}

#[async_trait]
impl A2aAtomicStore for FakeBackend {
    fn backend_name(&self) -> &'static str {
        "fake-backend"
    }
    async fn create_task_with_events(
        &self,
        _t: &str,
        _o: &str,
        task: Task,
        _e: Vec<turul_a2a::streaming::StreamEvent>,
    ) -> Result<(Task, Vec<u64>), A2aStorageError> {
        Ok((task, vec![]))
    }
    async fn update_task_status_with_events(
        &self,
        _t: &str,
        _tid: &str,
        _o: &str,
        _s: turul_a2a_types::TaskStatus,
        _e: Vec<turul_a2a::streaming::StreamEvent>,
    ) -> Result<(Task, Vec<u64>), A2aStorageError> {
        unimplemented!()
    }
    async fn update_task_with_events(
        &self,
        _t: &str,
        _o: &str,
        _task: Task,
        _e: Vec<turul_a2a::streaming::StreamEvent>,
    ) -> Result<Vec<u64>, A2aStorageError> {
        Ok(vec![])
    }
}

#[async_trait]
impl A2aCancellationSupervisor for FakeBackend {
    fn backend_name(&self) -> &'static str {
        "fake-backend"
    }
    async fn supervisor_get_cancel_requested(
        &self,
        _t: &str,
        _tid: &str,
    ) -> Result<bool, A2aStorageError> {
        Ok(false)
    }
    async fn supervisor_list_cancel_requested(
        &self,
        _t: &str,
        _tids: &[String],
    ) -> Result<Vec<String>, A2aStorageError> {
        Ok(vec![])
    }
    async fn supervisor_renew_executor_leases(
        &self,
        _t: &str,
        _tids: &[String],
        _expires_at_micros: Option<i64>,
    ) -> Result<(), A2aStorageError> {
        Ok(())
    }
    async fn supervisor_list_expired_executor_leases(
        &self,
        _now_micros: i64,
    ) -> Result<Vec<turul_a2a::storage::ExecutorHeldTask>, A2aStorageError> {
        Ok(vec![])
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

/// Unified `.storage()` with InMemoryA2aStorage — fully wires the
/// cancellation supervisor from the same backend. Builds cleanly.
#[test]
fn storage_bundle_requires_cancellation_supervisor_trait_bound() {
    // Compile-time check: .storage() accepts InMemoryA2aStorage which now
    // implements A2aCancellationSupervisor. If the trait bound on
    // `.storage()` were relaxed, a backend lacking the supervisor would
    // be accepted and silently wired — the builder test below proves
    // the runtime rejection.
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(InMemoryA2aStorage::new())
        .build();
    assert!(result.is_ok(), "unified storage bundle should build");
}

/// Omitting `cancellation_supervisor` while still supplying all other
/// stores individually MUST fail at build(). No silent in-memory fallback.
#[test]
fn build_rejects_missing_cancellation_supervisor() {
    let storage = InMemoryA2aStorage::new();
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage)
        // NOTE: no .cancellation_supervisor() — this is what we're testing.
        .build();
    match result {
        Err(A2aError::Internal(msg)) => {
            assert!(
                msg.contains("cancellation_supervisor"),
                "error message should mention cancellation_supervisor: {msg}"
            );
        }
        Ok(_) => {
            panic!("expected Internal error about missing cancellation_supervisor, got Ok(handler)")
        }
        Err(other) => panic!("expected Internal error, got different error: {other}"),
    }
}

/// Mismatched backend on cancellation_supervisor MUST be rejected at
/// build() with a message mentioning the mismatch. Prevents the
/// "DynamoDB marker write, in-memory supervisor read" silent failure mode.
#[test]
fn build_rejects_cancellation_supervisor_backend_mismatch() {
    let storage = InMemoryA2aStorage::new();
    let wrong_supervisor = FakeBackend;
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage)
        .cancellation_supervisor(wrong_supervisor)
        .build();
    match result {
        Err(A2aError::Internal(msg)) => {
            assert!(
                msg.contains("backend mismatch") || msg.contains("Storage backend mismatch"),
                "error should mention backend mismatch: {msg}"
            );
            assert!(
                msg.contains("cancellation_supervisor"),
                "error should mention which field mismatched: {msg}"
            );
        }
        Ok(_) => panic!("expected same-backend rejection, got Ok(handler)"),
        Err(other) => panic!("expected Internal mismatch error, got different error: {other}"),
    }
}

/// Positive test: individual setters including `.cancellation_supervisor()`
/// on the same backend accept the combination and produce a handler
/// without error.
///
/// Scope: this test proves only what `build()` returns — i.e., that the
/// same-backend check accepts the matching supervisor and that the
/// required-field validation is satisfied. It does NOT inspect
/// `AppState` directly (the `LambdaA2aHandler` wraps the router with no
/// test-only accessor). The corresponding AppState-wiring coverage lives
/// in `crates/turul-a2a/src/server/mod.rs::tests::runtime_config_setters_survive_build`
/// for the main server builder, and in `tests/cancellation_tests.rs`
/// which exercises the supervisor via the full cancel flow.
/// Together those give end-to-end proof that the Arc reaches the
/// router's handler state; this test is the compile-time + build-time
/// slice for the Lambda builder's setter surface.
#[test]
fn build_succeeds_with_explicit_cancellation_supervisor_same_backend() {
    let storage = InMemoryA2aStorage::new();
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage.clone())
        .cancellation_supervisor(storage)
        .build();
    assert!(
        result.is_ok(),
        "same-backend individual setters including cancellation_supervisor should build: {:?}",
        result.err()
    );
}

// ---------------------------------------------------------------------
// Lambda builder mirror of main-server push-dispatch consistency.
// ---------------------------------------------------------------------

#[test]
fn lambda_builder_rejects_push_consumer_without_dispatch_enabled() {
    // push_delivery_store wired + atomic_store.push_dispatch_enabled() = false
    // ⇒ build error with the pinned main-server wording.
    let storage = InMemoryA2aStorage::new(); // flag defaults to false
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage.clone())
        .cancellation_supervisor(storage.clone())
        .push_delivery_store(storage)
        .build();
    let err = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("orphan delivery store must be rejected"),
    };
    assert!(
        err.contains("push_delivery_store wired")
            && err.contains("push_dispatch_enabled")
            && err.contains("with_push_dispatch_enabled(true)"),
        "lambda error must mirror main server wording: {err}"
    );
}

#[test]
fn lambda_builder_rejects_push_dispatch_without_consumer() {
    // push_dispatch_enabled=true + no push_delivery_store ⇒ build error.
    let storage = InMemoryA2aStorage::new().with_push_dispatch_enabled(true);
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage.clone())
        .cancellation_supervisor(storage)
        // deliberately no .push_delivery_store(...)
        .build();
    let err = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("orphan dispatch flag must be rejected"),
    };
    assert!(
        err.contains("push_dispatch_enabled() is true") && err.contains("no consumer"),
        "lambda error must cite the orphaned-marker rationale: {err}"
    );
}

#[test]
fn lambda_builder_accepts_push_fully_wired() {
    // `.storage()` wires storage traits only; push
    // delivery is an explicit opt-in via `.push_delivery_store()`. The
    // storage instance itself must also opt in via
    // `with_push_dispatch_enabled(true)` so atomic commits write markers.
    let storage = InMemoryA2aStorage::new().with_push_dispatch_enabled(true);
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(storage.clone())
        .push_delivery_store(storage)
        .build();
    assert!(
        result.is_ok(),
        "lambda push fully wired must build: {:?}",
        result.err()
    );
}

#[test]
fn lambda_builder_accepts_non_push_deployment() {
    let storage = InMemoryA2aStorage::new(); // flag defaults to false
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage.clone())
        .cancellation_supervisor(storage)
        .build();
    assert!(
        result.is_ok(),
        "lambda non-push deployment must build: {:?}",
        result.err()
    );
}

#[test]
fn lambda_builder_rejects_retry_horizon_violation() {
    // push_claim_expiry <= max_attempts * backoff_cap must fail fast
    //. Mirror of the main server's pinning test.
    use std::time::Duration;
    use turul_a2a::server::RuntimeConfig;

    let storage = InMemoryA2aStorage::new().with_push_dispatch_enabled(true);
    let mut cfg = RuntimeConfig::default();
    cfg.push_max_attempts = 10;
    cfg.push_backoff_cap = Duration::from_secs(60);
    // 10 * 60s = 600s — equal to claim expiry, <= so rejected.
    cfg.push_claim_expiry = Duration::from_secs(600);
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(storage.clone())
        .push_delivery_store(storage)
        .runtime_config(cfg)
        .build();
    let err = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("retry horizon violation must be rejected"),
    };
    assert!(
        err.contains("retry horizon") || err.contains("push_claim_expiry"),
        "lambda error must cite the retry horizon: {err}"
    );
}

#[test]
fn lambda_builder_clears_executor_lease_and_stall_timeout() {
    use std::time::Duration;
    use turul_a2a::server::RuntimeConfig;

    let mut cfg = RuntimeConfig::default();
    cfg.executor_lease_ttl = Some(Duration::from_secs(30));
    cfg.executor_stall_timeout = Some(Duration::from_secs(60));
    let handler = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(InMemoryA2aStorage::new())
        .runtime_config(cfg)
        .build()
        .expect("builds");
    assert!(handler.state.runtime_config.executor_lease_ttl.is_none());
    assert!(
        handler
            .state
            .runtime_config
            .executor_stall_timeout
            .is_none()
    );
}

// --------------------------------------------------------------------------
// strip_path_prefix — API Gateway stage + resource tree prefix stripping
// --------------------------------------------------------------------------

fn agent_card_request(path: &str) -> lambda_http::Request {
    http::Request::builder()
        .method("GET")
        .uri(path)
        .header("accept", "application/json")
        .header("a2a-version", "1.0")
        .body(lambda_http::Body::Empty)
        .unwrap()
}

async fn response_status(
    handler: &LambdaA2aHandler,
    req: lambda_http::Request,
) -> http::StatusCode {
    let resp = handler.handle(req).await.expect("handle() must not error");
    resp.status()
}

/// Agent card request under an API-Gateway stage-prefixed path routes
/// through to the A2A router when `strip_path_prefix` is configured.
#[tokio::test]
async fn strip_path_prefix_routes_agent_card_under_apigw_stage_prefix() {
    let handler = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(InMemoryA2aStorage::new())
        .strip_path_prefix("/stage/agent")
        .build()
        .expect("builder must accept a valid strip_path_prefix");
    let req = agent_card_request("/stage/agent/.well-known/agent-card.json");
    assert_eq!(
        response_status(&handler, req).await,
        http::StatusCode::OK,
        "prefixed agent-card path must reach the router after strip"
    );
}

/// Without `strip_path_prefix`, the same prefixed path 404s —
/// demonstrates the failure mode the builder method fixes.
#[tokio::test]
async fn without_strip_path_prefix_apigw_stage_prefixed_path_is_404() {
    let handler = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(InMemoryA2aStorage::new())
        .build()
        .expect("builder must accept no-prefix config");
    let req = agent_card_request("/stage/agent/.well-known/agent-card.json");
    assert_eq!(
        response_status(&handler, req).await,
        http::StatusCode::NOT_FOUND,
        "baseline — without strip, the prefixed path must 404"
    );
}

/// With `strip_path_prefix` configured, an un-prefixed request still
/// falls through unchanged — non-matching paths do not get stripped.
/// The router therefore sees the genuine path and either serves it
/// (if it happens to match root-routed paths) or 404s honestly.
#[tokio::test]
async fn strip_path_prefix_passes_through_root_routed_request() {
    let handler = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(InMemoryA2aStorage::new())
        .strip_path_prefix("/stage/agent")
        .build()
        .unwrap();
    let req = agent_card_request("/.well-known/agent-card.json");
    assert_eq!(
        response_status(&handler, req).await,
        http::StatusCode::OK,
        "un-prefixed root request must still reach the router"
    );
}

/// Invalid prefix shapes are rejected at build time with a message that
/// names the builder method — no silent acceptance.
#[test]
fn strip_path_prefix_rejects_missing_leading_slash() {
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(InMemoryA2aStorage::new())
        .strip_path_prefix("stage/agent")
        .build();
    let err = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("must reject a prefix without a leading slash"),
    };
    assert!(
        err.contains("strip_path_prefix"),
        "error must name strip_path_prefix: {err}"
    );
    assert!(
        err.contains("start with '/'"),
        "error must explain the rule: {err}"
    );
}

#[test]
fn strip_path_prefix_rejects_trailing_slash() {
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(InMemoryA2aStorage::new())
        .strip_path_prefix("/stage/agent/")
        .build();
    let err = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("must reject a prefix ending with a slash"),
    };
    assert!(
        err.contains("strip_path_prefix"),
        "error must name strip_path_prefix: {err}"
    );
    assert!(
        err.contains("not end with '/'"),
        "error must explain the rule: {err}"
    );
}

#[test]
fn strip_path_prefix_treats_slash_as_noop() {
    let handler = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(InMemoryA2aStorage::new())
        .strip_path_prefix("/")
        .build();
    assert!(
        handler.is_ok(),
        "single-slash prefix must build — treated as no-op"
    );
}

// --------------------------------------------------------------------------
// return_immediately capability. The Lambda environment can freeze once
// the response is flushed, so `build()` turns the capability off unless
// a durable executor queue is wired; wiring one turns it back on.
// --------------------------------------------------------------------------

/// Captures enqueued jobs; never fails.
struct CapturingQueue {
    jobs: std::sync::Mutex<Vec<turul_a2a::durable_executor::QueuedExecutorJob>>,
}

#[async_trait]
impl turul_a2a::durable_executor::DurableExecutorQueue for CapturingQueue {
    fn max_payload_bytes(&self) -> usize {
        256 * 1024
    }
    async fn enqueue(
        &self,
        job: turul_a2a::durable_executor::QueuedExecutorJob,
    ) -> Result<(), turul_a2a::durable_executor::QueueError> {
        self.jobs.lock().unwrap().push(job);
        Ok(())
    }
    fn kind(&self) -> &'static str {
        "capturing"
    }
}

fn return_immediately_send(path: &str, jsonrpc: bool) -> lambda_http::Request {
    let params = serde_json::json!({
        "message": {
            "messageId": uuid::Uuid::now_v7().to_string(),
            "role": "ROLE_USER",
            "parts": [{"text": "hello"}],
        },
        "configuration": {"returnImmediately": true},
    });
    let body = if jsonrpc {
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": params})
    } else {
        params
    };
    http::Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(lambda_http::Body::Text(body.to_string()))
        .unwrap()
}

async fn response_json(
    handler: &LambdaA2aHandler,
    req: lambda_http::Request,
) -> (http::StatusCode, serde_json::Value) {
    let resp = handler.handle(req).await.expect("handle() must not error");
    let status = resp.status();
    let body = match resp.body() {
        lambda_http::Body::Text(s) => serde_json::from_str(s).unwrap_or_default(),
        _ => serde_json::Value::Null,
    };
    (status, body)
}

#[tokio::test]
async fn build_disables_return_immediately_without_durable_executor() {
    use turul_a2a::server::RuntimeConfig;

    // Even a runtime config asking for the capability is overridden.
    let mut cfg = RuntimeConfig::default();
    cfg.supports_return_immediately = true;
    let handler = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(InMemoryA2aStorage::new())
        .runtime_config(cfg)
        .build()
        .expect("build");
    assert!(!handler.state.runtime_config.supports_return_immediately);

    // HTTP: 400 with ErrorInfo reason UNSUPPORTED_OPERATION.
    let (status, body) =
        response_json(&handler, return_immediately_send("/message:send", false)).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body.pointer("/error/details/0/reason")
            .and_then(|v| v.as_str()),
        Some("UNSUPPORTED_OPERATION"),
        "{body}"
    );

    // JSON-RPC: -32004.
    let (status, body) = response_json(&handler, return_immediately_send("/jsonrpc", true)).await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(
        body.pointer("/error/code").and_then(|v| v.as_i64()),
        Some(-32004),
        "{body}"
    );

    // Refused before any write.
    let page = handler
        .state
        .task_storage
        .list_tasks(turul_a2a::storage::TaskFilter {
            tenant: Some(String::new()),
            owner: Some("anonymous".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(page.tasks.is_empty());
}

#[tokio::test]
async fn build_enables_return_immediately_with_durable_executor() {
    let queue = std::sync::Arc::new(CapturingQueue {
        jobs: std::sync::Mutex::new(Vec::new()),
    });
    let handler = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(InMemoryA2aStorage::new())
        .with_durable_executor(queue.clone())
        .build()
        .expect("build");
    assert!(handler.state.runtime_config.supports_return_immediately);

    let (status, body) =
        response_json(&handler, return_immediately_send("/message:send", false)).await;
    assert_eq!(status, http::StatusCode::OK, "{body}");
    assert_eq!(
        body.pointer("/task/status/state").and_then(|v| v.as_str()),
        Some("TASK_STATE_WORKING")
    );
    let jobs = queue.jobs.lock().unwrap();
    assert_eq!(jobs.len(), 1, "the turn is enqueued, not run in-process");
    assert_eq!(
        Some(jobs[0].task_id.as_str()),
        body.pointer("/task/id").and_then(|v| v.as_str())
    );
}

#[cfg(feature = "sqs")]
#[test]
fn with_sqs_return_immediately_enables_the_capability() {
    // Building an SQS client makes no network call.
    let config = aws_sdk_sqs::Config::builder()
        .region(aws_sdk_sqs::config::Region::new("us-east-1"))
        .behavior_version(aws_sdk_sqs::config::BehaviorVersion::latest())
        .build();
    let client = std::sync::Arc::new(aws_sdk_sqs::Client::from_conf(config));
    let handler = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(InMemoryA2aStorage::new())
        .with_sqs_return_immediately(
            "https://sqs.us-east-1.amazonaws.com/000000000000/fake-queue",
            client,
        )
        .build()
        .expect("build");
    assert!(handler.state.runtime_config.supports_return_immediately);
    assert_eq!(
        handler
            .state
            .durable_executor_queue
            .as_ref()
            .map(|q| q.kind()),
        Some("sqs")
    );
}

// --------------------------------------------------------------------------
// Outbound URL allowlist installed on the Lambda builder.
// --------------------------------------------------------------------------

/// The Lambda builder's validator reaches the delivery worker: a webhook
/// URL it refuses is never POSTed and is recorded as an SSRF-blocked
/// failed delivery, while an allowed URL on the same task is delivered.
#[tokio::test]
async fn lambda_builder_outbound_url_validator_refuses_unlisted_webhooks() {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};
    use turul_a2a::push::{A2aPushDeliveryStore, DeliveryErrorClass};
    use turul_a2a::server::RuntimeConfig;
    use turul_a2a_types::{TaskState, TaskStatus};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;

    let storage = InMemoryA2aStorage::new().with_push_dispatch_enabled(true);
    let mut cfg = RuntimeConfig::default();
    cfg.allow_insecure_push_urls = true;
    cfg.cancel_handler_grace = Duration::from_millis(50);
    cfg.cancel_handler_poll_interval = Duration::from_millis(10);
    cfg.push_max_attempts = 1;
    cfg.push_backoff_cap = Duration::from_millis(10);
    cfg.push_claim_expiry = Duration::from_secs(5);
    let handler = LambdaA2aHandler::builder()
        .executor(PushCapableExecutor)
        .storage(storage.clone())
        .push_delivery_store(storage.clone())
        .runtime_config(cfg)
        .outbound_url_validator(Arc::new(|url: &url::Url| {
            if url.path() == "/allowed" {
                Ok(())
            } else {
                Err("not on the allowlist".into())
            }
        }))
        .build()
        .expect("lambda build");

    let task_id = "task-lambda-push-allowlist";
    let task = Task::new(task_id, TaskStatus::new(TaskState::Working))
        .with_context_id("ctx-lambda-allowlist");
    storage
        .create_task("", "anonymous", task)
        .await
        .expect("seed task");

    for (id, suffix) in [("cfg-allowed", "allowed"), ("cfg-refused", "refused")] {
        let body = serde_json::json!({
            "id": id,
            "taskId": task_id,
            "url": format!("{}/{suffix}", mock.uri()),
        });
        let req = http::Request::builder()
            .method("POST")
            .uri(format!("/tasks/{task_id}/pushNotificationConfigs"))
            .header("content-type", "application/json")
            .header("a2a-version", "1.0")
            .body(lambda_http::Body::Text(body.to_string()))
            .unwrap();
        let resp = handler.handle(req).await.expect("handle");
        assert!(
            resp.status().is_success(),
            "create config: {}",
            resp.status()
        );
    }

    let cancel = http::Request::builder()
        .method("POST")
        .uri(format!("/tasks/{task_id}:cancel"))
        .header("a2a-version", "1.0")
        .body(lambda_http::Body::Empty)
        .unwrap();
    let resp = handler.handle(cancel).await.expect("handle");
    assert_eq!(resp.status(), http::StatusCode::OK, "cancel must succeed");

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let failed = loop {
        let failed = storage
            .list_failed_deliveries("", SystemTime::UNIX_EPOCH, 10)
            .await
            .unwrap();
        let posted = mock.received_requests().await.unwrap().len();
        if failed.len() == 1 && posted == 1 {
            break failed;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out: {} failed, {posted} posted",
            failed.len()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let reqs = mock.received_requests().await.unwrap();
    assert_eq!(reqs[0].url.path(), "/allowed");
    assert_eq!(failed[0].config_id, "cfg-refused");
    assert_eq!(failed[0].last_error_class, DeliveryErrorClass::SSRFBlocked);
    assert_eq!(failed[0].delivery_attempt_count, 0, "never POSTed");
}
