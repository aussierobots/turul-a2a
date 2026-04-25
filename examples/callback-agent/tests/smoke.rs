//! End-to-end smoke test for the push-notification flow.
//!
//! Asserts the ADR-011 invariants the example exists to teach:
//!
//! 1. Push delivery actually fires when a task terminates.
//! 2. The framework echoes the configured `token` in the
//!    `X-Turul-Push-Token` header, so receivers can authenticate the
//!    caller.
//! 3. The webhook receiver rejects (401) any call whose header
//!    doesn't match — forged or stale callbacks don't get through.
//! 4. The delivered body is a proto-JSON `Task` in terminal state.
//!
//! Both spawned servers (the A2A agent and the receiver) keep their
//! JoinHandles and abort at test end — no leaked listening sockets.

use std::net::TcpListener;
use std::time::Duration;

use callback_agent::{CallbackExecutor, ReceivedWebhook, TOKEN_HEADER, spawn_webhook_receiver};
use tokio::sync::mpsc;
use turul_a2a::A2aServer;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_client::{A2aClient, MessageBuilder};

const TEST_TOKEN: &str = "smoke-secret-correct";
const WRONG_TOKEN: &str = "smoke-secret-wrong";

fn reserve_port() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.set_nonblocking(true).expect("set_nonblocking");
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

async fn tokio_listener(std_listener: TcpListener) -> tokio::net::TcpListener {
    tokio::net::TcpListener::from_std(std_listener).expect("std -> tokio listener")
}

#[tokio::test]
async fn terminal_fires_webhook_and_token_is_validated() {
    // Receiver listens on a random port; we feed the correct token
    // to it, so only correctly-signed deliveries pass the header
    // check.
    let (receiver_std, receiver_port) = reserve_port();
    let (tx, mut rx) = mpsc::unbounded_channel::<ReceivedWebhook>();
    let receiver_handle =
        spawn_webhook_receiver(tokio_listener(receiver_std).await, tx, TEST_TOKEN);

    // Agent server with push delivery wired, on another random port.
    let (agent_std, agent_port) = reserve_port();
    let agent_listener = tokio_listener(agent_std).await;
    let storage = InMemoryA2aStorage::new().with_push_dispatch_enabled(true);
    let server = A2aServer::builder()
        .executor(CallbackExecutor { delay_ms: 500 })
        .storage(storage.clone())
        .push_delivery_store(storage)
        .allow_insecure_push_urls(true)
        .bind(([127, 0, 0, 1], agent_port))
        .build()
        .expect("build agent server");
    let router = server.into_router();
    let agent_handle = tokio::spawn(async move {
        let _ = axum::serve(agent_listener, router).await;
    });

    let client = A2aClient::new(format!("http://127.0.0.1:{agent_port}"));

    // Step 1: non-blocking send so we get the task_id back before
    // the executor finishes its delay. The client library doesn't
    // expose return_immediately as a typed knob on
    // `send_message`, so we set it on the raw proto request
    // builder.
    let mut request = MessageBuilder::new().text("hello").build();
    request.configuration = Some(turul_a2a_proto::SendMessageConfiguration {
        return_immediately: true,
        ..Default::default()
    });
    let response = client
        .send_message(request)
        .await
        .expect("non-blocking send succeeds");
    let task = response
        .into_task()
        .expect("return_immediately send must yield a Task response");
    let task_id = task.id().to_string();

    // Step 2: register the push config with the CORRECT token. This
    // must land before the executor's 500 ms delay elapses.
    let _ = client
        .create_push_config(
            &task_id,
            format!("http://127.0.0.1:{receiver_port}/webhook"),
            TEST_TOKEN,
        )
        .await
        .expect("create_push_config ok");

    // Step 3: wait for the receiver to observe the delivery. The
    // executor completes ~500 ms after send; push delivery adds a
    // small network hop. Give it a generous ceiling for CI jitter.
    let received = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("webhook fires within 5s")
        .expect("receiver channel open");

    assert_eq!(
        received.token_header.as_deref(),
        Some(TEST_TOKEN),
        "framework must echo the configured token in the {TOKEN_HEADER} header"
    );
    assert!(
        received.event_sequence_header.is_some(),
        "framework must include the event sequence header for replay/debug"
    );

    // Body is the proto-JSON form of the terminal Task.
    let status_state = received
        .body
        .get("status")
        .and_then(|s| s.get("state"))
        .and_then(|v| v.as_str());
    assert_eq!(
        status_state,
        Some("TASK_STATE_COMPLETED"),
        "delivered body must be a terminal task; got: {}",
        serde_json::to_string(&received.body).unwrap_or_default()
    );
    let body_task_id = received.body.get("id").and_then(|v| v.as_str());
    assert_eq!(
        body_task_id,
        Some(task_id.as_str()),
        "delivered task_id must match the task we sent"
    );

    // Invariant 3: the receiver rejects mismatched tokens. Fire a
    // manual POST with the WRONG token against the receiver and
    // expect 401 + a rejection marker on the channel. This proves
    // the validation path is actually enforced rather than being
    // dead code.
    let http = reqwest::Client::new();
    let resp = http
        .post(format!("http://127.0.0.1:{receiver_port}/webhook"))
        .header(TOKEN_HEADER, WRONG_TOKEN)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("direct webhook POST");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "receiver must reject tokens that don't match the configured secret"
    );
    let rejected = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("rejected call surfaced on channel")
        .expect("channel open");
    assert_eq!(rejected.token_header.as_deref(), Some(WRONG_TOKEN));
    assert_eq!(
        rejected
            .body
            .get("_rejected_reason")
            .and_then(|v| v.as_str()),
        Some("token mismatch"),
        "rejection should be tagged so tests can distinguish it from real deliveries"
    );

    agent_handle.abort();
    receiver_handle.abort();
    let _ = agent_handle.await;
    let _ = receiver_handle.await;
}
