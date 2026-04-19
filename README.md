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
use turul_a2a::prelude::*;

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

### Lambda push-notification recovery (ADR-013)

Push-notification delivery on Lambda requires **external triggers**.
`tokio::spawn` continuations created inside a Lambda invocation are
opportunistic only: the execution environment may be frozen
indefinitely between invocations, so the in-process
`PushDeliveryWorker` reclaim loop from the binary server (ADR-011)
has no equivalent. Correctness is carried by an atomic pending-dispatch
marker (ADR-013 §4.3) plus two external workers:

| Worker | Trigger | Role |
|---|---|---|
| `LambdaStreamRecoveryHandler` | DynamoDB Stream on `a2a_push_pending_dispatches` (NEW_IMAGE) | Fast path — fires within ~1s of the marker commit. DynamoDB backends only. |
| `LambdaScheduledRecoveryHandler` | EventBridge Scheduler (e.g. every 5 min) | Mandatory backstop for all backends. For SQLite/Postgres/in-memory this is the **only** recovery path. |

Minimal wiring — see `examples/lambda-stream-worker` and
`examples/lambda-scheduled-worker`:

```rust
// Stream worker (DynamoDB only)
use turul_a2a_aws_lambda::LambdaStreamRecoveryHandler;
let handler = LambdaStreamRecoveryHandler::new(dispatcher);
// Lambda input type: aws_lambda_events::dynamodb::Event
// Response type: aws_lambda_events::streams::DynamoDbEventResponse

// Scheduled worker (all backends)
use turul_a2a_aws_lambda::{LambdaScheduledRecoveryHandler, LambdaScheduledRecoveryConfig};
let handler = LambdaScheduledRecoveryHandler::new(dispatcher, delivery_store)
    .with_config(LambdaScheduledRecoveryConfig::default());
// Lambda input type: aws_lambda_events::eventbridge::EventBridgeEvent
// Response: LambdaScheduledRecoveryResponse (per-stage counts + error sample)
```

Release-gate invariants (pinned by tests in `turul-a2a-aws-lambda/src/stream_recovery_tests.rs` and `scheduled_recovery_tests.rs`):

- Valid INSERT + successful redispatch → no `BatchItemFailure`, one POST per config.
- `get_task → Ok(None)` (task deleted) → delete marker, return success.
- Transient storage error → `BatchItemFailure` with the record's `SequenceNumber`.
- Unparseable NEW_IMAGE → `BatchItemFailure` (logged).
- Duplicate stream records → exactly one POST (claim fencing).
- Scheduled sweep: stale markers recovered, transient errors counted and markers retained, reclaimable claims redispatched, batch limits honoured.

Operator responsibilities (not framework-managed):

1. Provision the DynamoDB Stream (NEW_IMAGE view) on `a2a_push_pending_dispatches` — DynamoDB backends only.
2. Create an event-source mapping with `FunctionResponseTypes: ["ReportBatchItemFailures"]`, a DLQ, and `MaximumRetryAttempts`.
3. Create an EventBridge Scheduler schedule (recommended every 5 min) targeting the scheduled worker.
4. Grant IAM: stream-worker reads the stream, scheduled-worker scans + deletes markers, both read tasks and configs.
5. Set TTL on `a2a_push_pending_dispatches` to outlast the worst-case scheduler outage.

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
