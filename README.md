# turul-a2a

Rust implementation of the [A2A (Agent-to-Agent) Protocol v1.0](https://a2a-protocol.org/).

Proto-first architecture: types generated from the normative `proto/a2a.proto` (package `lf.a2a.v1`) using `prost` + `pbjson`, wrapped in ergonomic Rust types.

## Crates

| Crate | Description |
|-------|-------------|
| `turul-a2a-proto` | Proto-generated types from `a2a.proto` with camelCase JSON via pbjson |
| `turul-a2a-types` | Ergonomic Rust wrappers, state machine enforcement, `#[non_exhaustive]` |
| `turul-a2a` | Server framework: HTTP + JSON-RPC + SSE, storage backends, auth middleware |
| `turul-a2a-auth` | Auth middleware: Bearer JWT (`turul-jwt-validator`), API key |
| `turul-a2a-client` | Client library: discovery, send, stream, subscribe, push config |
| `turul-a2a-aws-lambda` | Lambda adapter: same router, streaming via durable event store |
| `turul-jwt-validator` | Local JWT validator with JWKS caching |

## Quick Start

```bash
# Run the echo agent
cargo run -p echo-agent

# Test it
curl -X POST http://localhost:3000/message:send \
  -H 'Content-Type: application/json' \
  -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"hello"}]}}'
```

## Client Usage

```rust
use turul_a2a_client::{A2aClient, MessageBuilder};

let client = A2aClient::discover("http://localhost:3000").await?;

// Send a message
let request = MessageBuilder::new().text("hello agent").build();
let response = client.send_message(request).await?;

// Stream a message (SSE)
let request = MessageBuilder::new().text("stream this").build();
let mut stream = client.send_streaming_message(request).await?;
while let Some(event) = futures::StreamExt::next(&mut stream).await {
    let event = event?;
    println!("Event: {:?}", event.data);
}
```

## Server Usage

```rust
use turul_a2a::server::A2aServer;
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::error::A2aError;
use turul_a2a_types::{Task, Message};

struct MyAgent;

#[async_trait::async_trait]
impl AgentExecutor for MyAgent {
    async fn execute(
        &self,
        task: &mut Task,
        message: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let input = message.joined_text();
        task.push_text_artifact("result", "Response", format!("You said: {input}"));
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("My Agent", "1.0.0")
            .description("An example A2A agent")
            .url("http://localhost:3000/jsonrpc", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .unwrap()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    A2aServer::builder()
        .executor(MyAgent)
        .bind(([0, 0, 0, 0], 3000))
        .build()?
        .run()
        .await?;
    Ok(())
}
```

## Storage Backends

Four parity-tested backends, all implementing task storage, push notification storage, event store, and atomic task+event writes:

```bash
# In-memory (default)
cargo run -p echo-agent

# SQLite
cargo test --features sqlite

# PostgreSQL (requires DATABASE_URL)
cargo test --features postgres --test-threads=1

# DynamoDB (requires AWS credentials)
cargo test --features dynamodb --test-threads=1
```

Use `.storage()` on the builder for unified same-backend enforcement (ADR-009):

```rust
let storage = InMemoryA2aStorage::new(); // or SqliteA2aStorage, PostgresA2aStorage, DynamoDbA2aStorage
A2aServer::builder()
    .executor(MyAgent)
    .storage(storage)  // wires all 4 storage traits from one instance
    .build()?;
```

## Auth

```rust
use turul_a2a_auth::{ApiKeyMiddleware, BearerMiddleware};

// API key
let auth = ApiKeyMiddleware::new(Arc::new(my_lookup), "X-API-Key");

// Bearer JWT
let auth = BearerMiddleware::new(jwt_validator, "sub");

A2aServer::builder()
    .executor(MyAgent)
    .middleware(Arc::new(auth))
    .build()?;
```

## Lambda

```rust
use turul_a2a_aws_lambda::LambdaA2aServerBuilder;

let handler = LambdaA2aServerBuilder::new()
    .executor(MyAgent)
    .storage(my_dynamodb_storage)
    .build()?;

lambda_http::run(lambda_http::service_fn(move |event| {
    let h = handler.clone();
    async move { h.handle(event).await }
})).await?;
```

## Interoperability

Verified against the official [Go A2A SDK](https://github.com/a2aproject/a2a-go) (v2, spec v1.0):

```bash
# Start the echo agent, then:
cd interop/go-sdk
TURUL_SERVER_URL=http://localhost:3000 go test -v
```

## Development

```bash
cargo build --workspace
cargo test --workspace                # 377+ tests
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

## Architecture Decision Records

Documented under `docs/adr/`. Key decisions: proto-first types (ADR-001), storage traits (ADR-003), dual transport (ADR-005), SSE streaming (ADR-006), auth middleware (ADR-007), Lambda adapter (ADR-008), durable event coordination (ADR-009).

## License

MIT OR Apache-2.0
