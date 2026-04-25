//! Interrupting Agent — demonstrates the `INPUT_REQUIRED` interrupted
//! state per the A2A "Life of a Task" spec.
//!
//! Run: cargo run -p interrupting-agent
//!
//! See the crate-level docs in `lib.rs` and the smoke test in
//! `tests/smoke.rs` for the full flow (turn 1 yields INPUT_REQUIRED;
//! turn 2 reuses the **same taskId** to provide the answer and
//! completes the task).

use interrupting_agent::FlightBookingExecutor;
use turul_a2a::A2aServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let server = A2aServer::builder()
        .executor(FlightBookingExecutor)
        .bind(([0, 0, 0, 0], 3008))
        .build()?;

    println!("Interrupting Agent listening on http://0.0.0.0:3008");
    println!("Agent card: http://localhost:3008/.well-known/agent-card.json");
    println!();
    println!("Turn 1 — start booking, expect INPUT_REQUIRED:");
    println!("  curl -sS -X POST http://localhost:3008/message:send \\");
    println!("    -H 'a2a-version: 1.0' -H 'content-type: application/json' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"u1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"book a flight\"}}]}}}}'"
    );
    println!();
    println!("Turn 2 — answer, same taskId + contextId, expect COMPLETED:");
    println!("  curl -sS -X POST http://localhost:3008/message:send \\");
    println!("    -H 'a2a-version: 1.0' -H 'content-type: application/json' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"u2\",\"role\":\"ROLE_USER\",\"taskId\":\"<task.id>\",\"contextId\":\"<ctx>\",\"parts\":[{{\"text\":\"Helsinki\"}}]}}}}'"
    );

    server.run().await?;
    Ok(())
}
