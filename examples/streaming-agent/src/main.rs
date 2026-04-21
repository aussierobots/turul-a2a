//! Streaming Agent binary — runs the example on 0.0.0.0:3002.
//! See `lib.rs` for the `StreamingExecutor` implementation.

use streaming_agent::StreamingExecutor;
use turul_a2a::A2aServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let server = A2aServer::builder()
        .executor(StreamingExecutor { delay_ms: 100 })
        .bind(([0, 0, 0, 0], 3002))
        .build()?;

    println!("Streaming Agent listening on http://0.0.0.0:3002");
    println!("Agent card: http://localhost:3002/.well-known/agent-card.json");
    println!();
    println!("Streaming (watch tokens arrive in real time — --no-buffer is required):");
    println!("  curl --no-buffer http://localhost:3002/message:stream \\");
    println!("    -H 'Content-Type: application/json' \\");
    println!("    -H 'a2a-version: 1.0' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"stream please\"}}]}}}}'"
    );
    println!();
    println!("Subscribe to an existing task (replace TASK_ID):");
    println!("  curl --no-buffer http://localhost:3002/tasks/TASK_ID:subscribe \\");
    println!("    -H 'a2a-version: 1.0'");

    server.run().await?;
    Ok(())
}
