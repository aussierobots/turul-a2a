//! gRPC client CLI for the gRPC Agent example.
//!
//! Demonstrates the `turul_a2a_client::grpc::A2aGrpcClient` verb surface
//! against the companion `grpc-agent` server.
//!
//! Usage:
//!   cargo run -p grpc-agent --bin grpc-client -- send    "your text"
//!   cargo run -p grpc-agent --bin grpc-client -- stream  "your text"
//!   cargo run -p grpc-agent --bin grpc-client -- list
//!   cargo run -p grpc-agent --bin grpc-client -- get     TASK_ID
//!
//! Endpoint defaults to http://127.0.0.1:3005 — override with
//! A2A_GRPC_ENDPOINT=http://host:port.

use std::env;

use futures::StreamExt;
use turul_a2a_client::grpc::A2aGrpcClient;
use turul_a2a_proto as pb;

fn env_endpoint() -> String {
    env::var("A2A_GRPC_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:3005".to_string())
}

fn simple_message(text: &str) -> pb::Message {
    pb::Message {
        message_id: uuid::Uuid::now_v7().to_string(),
        context_id: String::new(),
        task_id: String::new(),
        role: pb::Role::User.into(),
        parts: vec![pb::Part {
            content: Some(pb::part::Content::Text(text.into())),
            metadata: None,
            filename: String::new(),
            media_type: String::new(),
        }],
        metadata: None,
        extensions: vec![],
        reference_task_ids: vec![],
    }
}

async fn cmd_send(text: String) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = A2aGrpcClient::connect(env_endpoint()).await?;
    let resp = client.send_message(simple_message(&text)).await?;
    match resp.payload {
        Some(pb::send_message_response::Payload::Task(t)) => {
            let state = t.status.as_ref().map(|s| s.state());
            println!("task {} state={state:?}", t.id);
            for a in &t.artifacts {
                for p in &a.parts {
                    if let Some(pb::part::Content::Text(text)) = &p.content {
                        println!("  artifact {}: {text}", a.artifact_id);
                    }
                }
            }
        }
        other => println!("unexpected payload: {other:?}"),
    }
    Ok(())
}

async fn cmd_stream(text: String) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = A2aGrpcClient::connect(env_endpoint()).await?;
    let mut stream = client.send_streaming_message(simple_message(&text)).await?;
    let mut count = 0usize;
    while let Some(item) = stream.next().await {
        count += 1;
        match item {
            Ok(resp) => match resp.payload {
                Some(pb::stream_response::Payload::StatusUpdate(upd)) => {
                    let state = upd.status.as_ref().map(|s| s.state());
                    println!("[{count}] status -> {state:?}");
                }
                Some(pb::stream_response::Payload::ArtifactUpdate(art)) => {
                    let artifact_id = art.artifact.as_ref().map(|a| &a.artifact_id);
                    println!(
                        "[{count}] artifact id={artifact_id:?} append={} last_chunk={}",
                        art.append, art.last_chunk
                    );
                    if let Some(a) = art.artifact {
                        for p in &a.parts {
                            if let Some(pb::part::Content::Text(text)) = &p.content {
                                println!("        text: {text:?}");
                            }
                        }
                    }
                }
                other => println!("[{count}] other: {other:?}"),
            },
            Err(e) => {
                println!("[{count}] stream error: {e}");
                return Err(Box::new(e));
            }
        }
    }
    println!("stream closed after {count} event(s)");
    Ok(())
}

async fn cmd_list() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = A2aGrpcClient::connect(env_endpoint()).await?;
    let resp = client.list_tasks(Some(20), None).await?;
    println!("total={} returned={}", resp.total_size, resp.tasks.len());
    for t in resp.tasks {
        let state = t.status.as_ref().map(|s| s.state());
        println!("  {} state={state:?}", t.id);
    }
    Ok(())
}

async fn cmd_get(task_id: String) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = A2aGrpcClient::connect(env_endpoint()).await?;
    let task = client.get_task(task_id, None).await?;
    println!(
        "task {} state={:?} artifacts={}",
        task.id,
        task.status.as_ref().map(|s| s.state()),
        task.artifacts.len()
    );
    Ok(())
}

fn usage() -> ! {
    eprintln!(
        "usage:\n  grpc-client send <text>\n  grpc-client stream <text>\n  grpc-client list\n  grpc-client get <task-id>"
    );
    std::process::exit(2);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, text] if cmd == "send" => cmd_send(text.clone()).await,
        [cmd, text] if cmd == "stream" => cmd_stream(text.clone()).await,
        [cmd] if cmd == "list" => cmd_list().await,
        [cmd, task_id] if cmd == "get" => cmd_get(task_id.clone()).await,
        _ => usage(),
    }
}
