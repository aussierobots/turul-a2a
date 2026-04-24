//! Callback Agent binary — runs the A2A server on :3003 and a
//! co-located webhook receiver on :3004, so the whole push-delivery
//! cycle happens in one process. See `lib.rs` for the
//! `CallbackExecutor` and receiver helper.
//!
//! Not realistic for production: the webhook should live on a
//! separate host (otherwise an attacker who reaches the webhook
//! can also reach the agent). It's fine for a demo — see
//! README.md §"Splitting for production".

use callback_agent::{CallbackExecutor, spawn_webhook_receiver};
use tokio::sync::mpsc;
use turul_a2a::A2aServer;
use turul_a2a::storage::InMemoryA2aStorage;

const AGENT_PORT: u16 = 3003;
const WEBHOOK_PORT: u16 = 3004;
const DEMO_TOKEN: &str = "demo-secret-change-me";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // Receiver side first so the agent can deliver to it the moment
    // the executor completes.
    let webhook_listener = tokio::net::TcpListener::bind(("127.0.0.1", WEBHOOK_PORT)).await?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _webhook_handle = spawn_webhook_receiver(webhook_listener, tx, DEMO_TOKEN);

    // Print received webhooks to stdout as they arrive.
    tokio::spawn(async move {
        while let Some(call) = rx.recv().await {
            println!(
                "\n[webhook] token-header={:?} sequence={:?} body={}",
                call.token_header,
                call.event_sequence_header,
                serde_json::to_string_pretty(&call.body).unwrap_or_default()
            );
        }
    });

    // Agent side with push delivery wired. Storage must be Clone so
    // the builder can Arc-wrap internally; passing `storage.clone()`
    // to `.storage()` and the original to `.push_delivery_store()`
    // satisfies the same-backend requirement because both hold the
    // same underlying in-memory maps.
    let storage = InMemoryA2aStorage::new().with_push_dispatch_enabled(true);
    let server = A2aServer::builder()
        .executor(CallbackExecutor { delay_ms: 1500 })
        .storage(storage.clone())
        .push_delivery_store(storage)
        .allow_insecure_push_urls(true) // localhost demo; NEVER in production
        .bind(([0, 0, 0, 0], AGENT_PORT))
        .build()?;

    println!("Callback Agent listening on http://0.0.0.0:{AGENT_PORT}");
    println!("Webhook receiver on http://127.0.0.1:{WEBHOOK_PORT}/webhook");
    println!();
    println!("Demo sequence (run in another terminal):");
    println!();
    println!("  # 1. Send a non-blocking message — returns immediately with a task_id");
    println!(
        "  TASK_ID=$(curl -s http://localhost:{AGENT_PORT}/message:send \\\n    -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \\\n    -d '{{\"message\":{{\"messageId\":\"m1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"hi\"}}]}},\"configuration\":{{\"returnImmediately\":true}}}}' \\\n    | jq -r .task.id)"
    );
    println!("  echo \"task_id=$TASK_ID\"");
    println!();
    println!("  # 2. Register the webhook + shared secret against that task");
    println!(
        "  curl -s http://localhost:{AGENT_PORT}/tasks/$TASK_ID/pushNotificationConfigs \\\n    -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \\\n    -d '{{\"taskId\":\"'$TASK_ID'\",\"url\":\"http://127.0.0.1:{WEBHOOK_PORT}/webhook\",\"token\":\"{DEMO_TOKEN}\"}}'"
    );
    println!();
    println!("  # 3. Wait ~1.5s for the executor to finish — watch this terminal");
    println!("  #    for the [webhook] line showing the terminal task payload.");
    println!();
    println!(
        "Token validation: the receiver rejects any POST whose \
         X-Turul-Push-Token header doesn't match '{DEMO_TOKEN}'."
    );

    server.run().await?;
    Ok(())
}
