//! Offline smoke test for the skill-dispatch profile example agent.
//!
//! Runs the already-built agent binary (`CARGO_BIN_EXE_*`) on a free
//! loopback port per test, so tests run in parallel without nested
//! `cargo run` or fixed ports. Covers the advertised card, dispatch to
//! both skills (with the activation echo), the framework's InvalidParams
//! rejection when the profile is activated without `a2a.skillId`, and
//! the executor's own failure paths (profile not activated, unknown
//! skill id).
//!
//! No network egress.

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};

const PROFILE_URI: &str = "https://turul.dev/a2a/extensions/skill-invocation/v1";

/// The card name the spawned binary advertises.
const AGENT_NAME: &str = "Skill Dispatch Profile Agent";

struct AgentGuard {
    child: Child,
    base: String,
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A port the OS reports free right now, never handed out twice in this
/// test binary (its tests run in parallel). The listener is dropped
/// before the agent binds the port.
fn free_port() -> u16 {
    static ISSUED: Mutex<Vec<u16>> = Mutex::new(Vec::new());
    let mut issued = ISSUED.lock().expect("port registry");
    loop {
        let port = std::net::TcpListener::bind("0.0.0.0:0")
            .and_then(|l| l.local_addr())
            .expect("reserve free port")
            .port();
        if !issued.contains(&port) {
            issued.push(port);
            return port;
        }
    }
}

/// Poll the agent card until this test's own child serves it. Fails fast
/// if the child exits (for example because its port was taken in the
/// meantime) and refuses a card served by a different listener.
async fn wait_until_ready(guard: &mut AgentGuard) {
    let url = format!("{}/.well-known/agent-card.json", guard.base);
    for _ in 0..150 {
        if let Some(status) = guard.child.try_wait().expect("poll agent process") {
            panic!("agent exited before it was ready ({status})");
        }
        if let Ok(r) = reqwest::get(&url).await
            && r.status().is_success()
        {
            let card: Value = r.json().await.expect("agent card JSON");
            assert_eq!(
                card["name"], AGENT_NAME,
                "{url} is served by another process"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("agent did not become ready at {url} within 15s");
}

async fn spawn_agent() -> AgentGuard {
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_skill-dispatch-profile-agent"))
        .env("A2A_PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent");
    let mut guard = AgentGuard {
        child,
        base: format!("http://127.0.0.1:{port}"),
    };
    wait_until_ready(&mut guard).await;
    guard
}

async fn send_message(base: &str, body: Value, activate_profile: bool) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/message:send"))
        .header("a2a-version", "1.0")
        .json(&body);
    if activate_profile {
        req = req.header("A2A-Extensions", PROFILE_URI);
    }
    req.send().await.expect("send req")
}

/// The first text part of the first artifact of a spec
/// `SendMessageResponse` (`{"task": {...}}`).
fn first_artifact_text(resp: &Value) -> &str {
    resp.pointer("/task/artifacts/0/parts/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected SendMessageResponse.task with an artifact: {resp:#}"))
}

fn task_state(resp: &Value) -> &str {
    resp.pointer("/task/status/state")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected SendMessageResponse.task.status.state: {resp:#}"))
}

fn status_text(resp: &Value) -> &str {
    resp.pointer("/task/status/message/parts/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn dispatch_body(skill_id: &str, params: Value) -> Value {
    json!({
        "message": {
            "messageId": format!("smoke-{skill_id}"),
            "role": "ROLE_USER",
            "parts": [ { "text": format!("dispatch:{skill_id}") } ],
            "metadata": {
                "a2a.skillId": skill_id,
                "a2a.skillParams": params,
            }
        }
    })
}

fn assert_echo(resp: &reqwest::Response) {
    let echo = resp
        .headers()
        .get("a2a-extensions")
        .expect("response must echo A2A-Extensions");
    assert_eq!(echo.to_str().unwrap(), PROFILE_URI);
}

#[tokio::test]
async fn agent_card_advertises_profile_extension_as_not_required() {
    let agent = spawn_agent().await;
    let card: Value = reqwest::get(format!("{}/.well-known/agent-card.json", agent.base))
        .await
        .expect("card req")
        .json()
        .await
        .expect("card json");

    let extensions = card["capabilities"]["extensions"]
        .as_array()
        .expect("capabilities.extensions array");
    let profile = extensions
        .iter()
        .find(|e| e["uri"] == PROFILE_URI)
        .unwrap_or_else(|| panic!("card must advertise profile URI: {extensions:?}"));
    // proto3 JSON omits a false bool; anything else must be `false`.
    assert!(
        matches!(profile.get("required"), None | Some(Value::Bool(false))),
        "profile must be advertised with required = false: {profile:#}"
    );

    let mut ids: Vec<&str> = card["skills"]
        .as_array()
        .expect("skills array")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, ["echo_loud", "reverse"]);
}

#[tokio::test]
async fn dispatches_to_echo_loud() {
    let agent = spawn_agent().await;
    let resp = send_message(
        &agent.base,
        dispatch_body("echo_loud", json!({ "text": "hello" })),
        true,
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_echo(&resp);

    let payload: Value = resp.json().await.expect("send json");
    assert_eq!(task_state(&payload), "TASK_STATE_COMPLETED");
    let artifact: Value =
        serde_json::from_str(first_artifact_text(&payload)).expect("artifact is JSON");
    assert_eq!(artifact, json!({ "shouted": "HELLO" }));
}

#[tokio::test]
async fn dispatches_to_reverse() {
    let agent = spawn_agent().await;
    let resp = send_message(
        &agent.base,
        dispatch_body("reverse", json!({ "text": "abc" })),
        true,
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_echo(&resp);

    let payload: Value = resp.json().await.expect("send json");
    assert_eq!(task_state(&payload), "TASK_STATE_COMPLETED");
    let artifact: Value =
        serde_json::from_str(first_artifact_text(&payload)).expect("artifact is JSON");
    assert_eq!(artifact, json!({ "reversed": "cba" }));
}

/// The executor owns routing: an id no skill is registered under fails
/// the task, citing the id.
#[tokio::test]
async fn unknown_skill_id_fails_task() {
    let agent = spawn_agent().await;
    let resp = send_message(
        &agent.base,
        dispatch_body("no_such_skill", json!({ "text": "x" })),
        true,
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let payload: Value = resp.json().await.expect("send json");
    assert_eq!(task_state(&payload), "TASK_STATE_FAILED", "{payload:#}");
    assert!(
        status_text(&payload).contains("no_such_skill"),
        "failure message should cite the unknown id: {payload:#}"
    );
}

/// Activated without `a2a.skillId`: the framework rejects the request
/// as InvalidParams (HTTP 400) before the executor runs.
#[tokio::test]
async fn activated_without_skill_id_is_invalid_params() {
    let agent = spawn_agent().await;
    let body = json!({
        "message": {
            "messageId": "smoke-activated-missing",
            "role": "ROLE_USER",
            "parts": [ { "text": "no dispatch intent here" } ]
        }
    });
    let resp = send_message(&agent.base, body, true).await;
    assert_eq!(resp.status().as_u16(), 400);
    let payload: Value = resp.json().await.expect("error json");
    let message = payload
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(message.contains("a2a.skillId"), "{payload:#}");
}

/// Not activated (the profile is optional): the executor's default path
/// fails the task explaining how to target a skill.
#[tokio::test]
async fn not_activated_fails_task_with_guidance() {
    let agent = spawn_agent().await;
    let body = json!({
        "message": {
            "messageId": "smoke-missing",
            "role": "ROLE_USER",
            "parts": [ { "text": "no dispatch intent here" } ]
        }
    });
    let resp = send_message(&agent.base, body, false).await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "task-level failure is reported via TaskStatus, not HTTP error"
    );
    assert!(resp.headers().get("a2a-extensions").is_none());
    let payload: Value = resp.json().await.expect("send json");
    assert_eq!(task_state(&payload), "TASK_STATE_FAILED", "{payload:#}");
    assert!(
        status_text(&payload).contains("a2a.skillId"),
        "failure message should cite the missing metadata key: {payload:#}"
    );
}

/// `a2a.skillId` without the activation header: the executor cannot see
/// the negotiated extension set, so it routes on metadata alone. The
/// framework did not apply the profile, so nothing is echoed.
#[tokio::test]
async fn skill_id_without_activation_header_still_dispatches() {
    let agent = spawn_agent().await;
    let resp = send_message(
        &agent.base,
        dispatch_body("reverse", json!({ "text": "abc" })),
        false,
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp.headers().get("a2a-extensions").is_none());

    let payload: Value = resp.json().await.expect("send json");
    assert_eq!(task_state(&payload), "TASK_STATE_COMPLETED", "{payload:#}");
    let artifact: Value =
        serde_json::from_str(first_artifact_text(&payload)).expect("artifact is JSON");
    assert_eq!(artifact, json!({ "reversed": "cba" }));
}
