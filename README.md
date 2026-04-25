# turul-a2a

[![crates.io](https://img.shields.io/crates/v/turul-a2a.svg)](https://crates.io/crates/turul-a2a)
[![docs.rs](https://docs.rs/turul-a2a/badge.svg)](https://docs.rs/turul-a2a)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Rust implementation of the [A2A (Agent-to-Agent) Protocol v1.0](https://a2a-protocol.org/).

Proto-first: types are generated from the normative `proto/a2a.proto` (package `lf.a2a.v1`) via `prost` + `pbjson`, then wrapped in ergonomic Rust types. Wire format is interop-tested against the official [Go A2A SDK](https://github.com/a2aproject/a2a-go).

## Crates

| Crate | docs.rs | Description |
|-------|---------|-------------|
| `turul-a2a-proto` | [📖](https://docs.rs/turul-a2a-proto) | Proto-generated types with camelCase JSON via pbjson |
| `turul-a2a-types` | [📖](https://docs.rs/turul-a2a-types) | Ergonomic wrappers, state machine, `#[non_exhaustive]` |
| `turul-a2a` | [📖](https://docs.rs/turul-a2a) | Server framework: HTTP + JSON-RPC + SSE, storage, middleware |
| `turul-a2a-auth` | [📖](https://docs.rs/turul-a2a-auth) | Bearer JWT and API key middleware |
| `turul-a2a-client` | [📖](https://docs.rs/turul-a2a-client) | Client library: discovery, send, stream, subscribe |
| `turul-a2a-aws-lambda` | [📖](https://docs.rs/turul-a2a-aws-lambda) | AWS Lambda adapter |
| `turul-jwt-validator` | [📖](https://docs.rs/turul-jwt-validator) | Local JWT validator with JWKS caching |

## Quick start

```toml
[dependencies]
turul-a2a = "0.1"
turul-a2a-client = "0.1"
tokio = { version = "1", features = ["full"] }
```

```bash
cargo run -p echo-agent     # runs on :3000

curl -X POST http://localhost:3000/message:send \
  -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"hello"}]}}'
```

## Server

```rust
use turul_a2a::prelude::*;

struct MyAgent;

#[async_trait::async_trait]
impl AgentExecutor for MyAgent {
    async fn execute(&self, task: &mut Task, message: &Message, _ctx: &ExecutionContext)
        -> Result<(), A2aError>
    {
        task.push_text_artifact("result", "Response", format!("You said: {}", message.joined_text()));
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("My Agent", "1.0.0").build().unwrap()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    A2aServer::builder().executor(MyAgent).bind(([0, 0, 0, 0], 3000)).build()?.run().await?;
    Ok(())
}
```

See [`crates/turul-a2a/README.md`](crates/turul-a2a/README.md) for storage wiring, durable streaming, and feature flags.

## Client

```rust
use turul_a2a_client::{A2aClient, MessageBuilder};

let client = A2aClient::discover("http://localhost:3000").await?;
let response = client.send_message(MessageBuilder::new().text("hello").build()).await?;
```

See [`crates/turul-a2a-client/README.md`](crates/turul-a2a-client/README.md) for streaming, push config, and tenant routing.

## Storage

Four parity-tested backends. Pick one with a feature flag and pass the instance to `.storage()`:

| Backend | Feature | Notes |
|---|---|---|
| In-memory | `in-memory` (default) | Volatile, single-process. |
| SQLite | `sqlite` | File-backed, atomic task + event writes. |
| PostgreSQL | `postgres` | Multi-instance ready. |
| DynamoDB | `dynamodb` | TTL-aware, AWS deployments. |

## Auth

```rust
use turul_a2a_auth::{ApiKeyMiddleware, BearerMiddleware};

let auth = ApiKeyMiddleware::new(Arc::new(my_lookup), "X-API-Key");
// or: BearerMiddleware::new(jwt_validator, "sub");

A2aServer::builder().executor(MyAgent).middleware(Arc::new(auth)).build()?;
```

## Transports

| Transport | Default | Where to read |
|---|---|---|
| HTTP + JSON | yes | This crate. |
| JSON-RPC | yes | This crate (`POST /jsonrpc`, 11 methods). |
| SSE streaming | yes | This crate. |
| gRPC | feature `grpc` | [`crates/turul-a2a/README.md`](crates/turul-a2a/README.md#grpc) |
| AWS Lambda | separate crate | [`crates/turul-a2a-aws-lambda/README.md`](crates/turul-a2a-aws-lambda/README.md) |

## Interoperability

Verified against the official [Go A2A SDK](https://github.com/a2aproject/a2a-go) (v2, spec v1.0):

```bash
cd interop/go-sdk
TURUL_SERVER_URL=http://localhost:3000 go test -v
```

## Contributing

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

Architecture rationale lives under `docs/adr/`. Per-crate READMEs cover crate-local concerns.

## License

MIT OR Apache-2.0
