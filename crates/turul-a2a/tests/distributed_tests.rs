//! Distributed multi-instance verification.
//!
//! Two router instances sharing the same storage backend.
//! Requests alternate between instances to prove request/response
//! correctness across instances with shared state.

use std::sync::Arc;

use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::middleware::MiddlewareStack;
use turul_a2a::router::{build_router, AppState};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a::streaming::TaskEventBroker;
use turul_a2a_types::{Message, Task};

struct TestExecutor;

#[async_trait::async_trait]
impl AgentExecutor for TestExecutor {
    async fn execute(&self, task: &mut Task, _msg: &Message, _ctx: &turul_a2a::executor::ExecutionContext) -> Result<(), A2aError> {
        task.push_text_artifact("dist-art", "Result", "distributed result");
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Distributed Test Agent", "1.0.0")
            .description("Tests multi-instance behavior")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .unwrap()
    }
}

/// Create two router instances sharing the SAME storage backend.
/// Each has its own event broker (simulating separate processes).
fn two_instances() -> (axum::Router, axum::Router) {
    let shared_storage = InMemoryA2aStorage::new();

    let state_a = AppState {
        executor: Arc::new(TestExecutor),
        task_storage: Arc::new(shared_storage.clone()),
        push_storage: Arc::new(shared_storage.clone()),
        event_store: Arc::new(shared_storage.clone()),
        atomic_store: Arc::new(shared_storage.clone()),
        event_broker: TaskEventBroker::new(), // instance A's local broker
        middleware_stack: Arc::new(MiddlewareStack::new(vec![])),
        runtime_config: turul_a2a::server::RuntimeConfig::default(),
    };

    let state_b = AppState {
        executor: Arc::new(TestExecutor),
        task_storage: Arc::new(shared_storage.clone()),
        push_storage: Arc::new(shared_storage.clone()),
        event_store: Arc::new(shared_storage.clone()),
        atomic_store: Arc::new(shared_storage),
        event_broker: TaskEventBroker::new(), // instance B's separate broker
        middleware_stack: Arc::new(MiddlewareStack::new(vec![])),
        runtime_config: turul_a2a::server::RuntimeConfig::default(),
    };

    (build_router(state_a), build_router(state_b))
}

fn send_body(id: &str) -> String {
    serde_json::json!({"message":{"messageId":id,"role":"ROLE_USER","parts":[{"text":"hello"}]}})
        .to_string()
}

async fn json_response(router: axum::Router, req: Request<Body>) -> (u16, serde_json::Value) {
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

// =========================================================
// Create on A, fetch on B
// =========================================================

#[tokio::test]
async fn create_on_a_fetch_on_b() {
    let (router_a, router_b) = two_instances();

    // Create task on instance A
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body("dist-1")))
        .unwrap();
    let (status, body) = json_response(router_a, req).await;
    assert_eq!(status, 200);
    let task_id = body["task"]["id"].as_str().unwrap();

    // Fetch the same task on instance B
    let req = Request::get(&format!("/tasks/{task_id}"))
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router_b, req).await;
    assert_eq!(status, 200, "Instance B should see task created by instance A");
    assert_eq!(body["id"].as_str().unwrap(), task_id);
}

// =========================================================
// Create on A, list on B
// =========================================================

#[tokio::test]
async fn create_on_a_list_on_b() {
    let (router_a, router_b) = two_instances();

    // Create 3 tasks on instance A
    for i in 0..3 {
        let req = Request::post("/message:send")
            .header("content-type", "application/json")
            .header("a2a-version", "1.0")
            .body(Body::from(send_body(&format!("list-{i}"))))
            .unwrap();
        json_response(router_a.clone(), req).await;
    }

    // List tasks on instance B
    let req = Request::get("/tasks")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router_b, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["totalSize"], 3, "Instance B should see all 3 tasks from instance A");
}

// =========================================================
// Create on A, cancel on B
// =========================================================

#[tokio::test]
async fn create_on_a_cancel_on_b() {
    let (router_a, router_b) = two_instances();

    // Create task on A (executor completes it)
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body("cancel-dist")))
        .unwrap();
    let (_, body) = json_response(router_a, req).await;
    let task_id = body["task"]["id"].as_str().unwrap();

    // Cancel on B — task is already completed, so this should return 409
    let req = Request::post(&format!("/tasks/{task_id}:cancel"))
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router_b, req).await;
    assert_eq!(status, 409, "Cancel on B should see A's completed task");
    assert_eq!(body["error"]["details"][0]["reason"], "TASK_NOT_CANCELABLE");
}

// =========================================================
// Tenant isolation across instances
// =========================================================

#[tokio::test]
async fn tenant_isolation_across_instances() {
    let (router_a, router_b) = two_instances();

    // Create task under tenant "acme" on instance A
    let req = Request::post("/acme/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body("tenant-dist")))
        .unwrap();
    let (_, body) = json_response(router_a, req).await;
    let task_id = body["task"]["id"].as_str().unwrap();

    // Instance B under "acme" can see it
    let req = Request::get(&format!("/acme/tasks/{task_id}"))
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let (status, _) = json_response(router_b.clone(), req).await;
    assert_eq!(status, 200, "Same tenant on B should see A's task");

    // Instance B under different tenant cannot see it
    let req = Request::get(&format!("/other/tasks/{task_id}"))
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let (status, _) = json_response(router_b.clone(), req).await;
    assert_eq!(status, 404, "Different tenant on B should not see A's task");

    // Instance B default tenant cannot see it
    let req = Request::get(&format!("/tasks/{task_id}"))
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let (status, _) = json_response(router_b, req).await;
    assert_eq!(status, 404, "Default tenant on B should not see acme's task");
}

// =========================================================
// Push config created on A, visible on B
// =========================================================

#[tokio::test]
async fn push_config_across_instances() {
    let (router_a, router_b) = two_instances();

    // Create task on A
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body("push-dist")))
        .unwrap();
    let (_, body) = json_response(router_a.clone(), req).await;
    let task_id = body["task"]["id"].as_str().unwrap();

    // Create push config on A
    let config_body = serde_json::json!({
        "taskId": task_id,
        "url": "https://example.com/webhook"
    })
    .to_string();
    let req = Request::post(&format!("/tasks/{task_id}/pushNotificationConfigs"))
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(config_body))
        .unwrap();
    let (status, body) = json_response(router_a, req).await;
    assert_eq!(status, 200);
    let config_id = body["id"].as_str().unwrap();

    // Get push config on B
    let req = Request::get(&format!("/tasks/{task_id}/pushNotificationConfigs/{config_id}"))
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router_b, req).await;
    assert_eq!(status, 200, "Instance B should see push config created by A");
    assert_eq!(body["id"].as_str().unwrap(), config_id);
}

// =========================================================
// JSON-RPC on B sees A's data
// =========================================================

#[tokio::test]
async fn jsonrpc_cross_instance() {
    let (router_a, router_b) = two_instances();

    // Create task on A via HTTP
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body("jrpc-dist")))
        .unwrap();
    let (_, body) = json_response(router_a, req).await;
    let task_id = body["task"]["id"].as_str().unwrap();

    // Get task on B via JSON-RPC
    let jrpc = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "GetTask",
        "params": {"id": task_id},
        "id": 1
    })
    .to_string();
    let req = Request::post("/jsonrpc")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(jrpc))
        .unwrap();
    let (status, body) = json_response(router_b, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["id"].as_str().unwrap(), task_id);
}
