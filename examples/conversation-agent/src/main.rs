//! Conversation Agent — demonstrates task refinement within a single
//! `contextId` per the A2A "Life of a Task" spec.
//!
//! Run: cargo run -p conversation-agent
//!
//! See the crate-level docs in `lib.rs` and the smoke test in
//! `tests/smoke.rs` for the full flow (originating task → refinement
//! task in the same context, same artifact name with a new artifactId).

use conversation_agent::ConversationExecutor;
use turul_a2a::A2aServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let server = A2aServer::builder()
        .executor(ConversationExecutor)
        .bind(([0, 0, 0, 0], 3007))
        .build()?;

    println!("Conversation Agent listening on http://0.0.0.0:3007");
    println!("Agent card: http://localhost:3007/.well-known/agent-card.json");
    println!();
    println!("Step 1 — originate a task:");
    println!("  curl -sS -X POST http://localhost:3007/message:send \\");
    println!("    -H 'a2a-version: 1.0' -H 'content-type: application/json' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"u1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"sailboat\"}}]}}}}'"
    );
    println!();
    println!("Step 2 — refine within the same contextId, referencing task1:");
    println!("  curl -sS -X POST http://localhost:3007/message:send \\");
    println!("    -H 'a2a-version: 1.0' -H 'content-type: application/json' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"u2\",\"role\":\"ROLE_USER\",\"contextId\":\"<ctx>\",\"referenceTaskIds\":[\"<task1.id>\"],\"parts\":[{{\"text\":\"make it red\"}}]}}}}'"
    );

    server.run().await?;
    Ok(())
}
