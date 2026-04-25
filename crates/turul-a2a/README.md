# turul-a2a

A2A Protocol v1.0 server framework.

Provides the server side of the
[A2A Protocol](https://github.com/a2aproject/A2A): a typed `AgentExecutor`
trait, HTTP + JSON-RPC transports, Server-Sent Events streaming, and a
storage abstraction with four parity-proven backends.

## Feature flags

| Flag         | Purpose                                                          |
|--------------|------------------------------------------------------------------|
| `in-memory`  | Default. Volatile in-process storage.                            |
| `sqlite`     | SQLx SQLite backend with atomic task + event writes.             |
| `postgres`   | SQLx PostgreSQL backend.                                         |
| `dynamodb`   | AWS DynamoDB backend with TTL.                                   |
| `grpc`       | Optional gRPC transport (tonic). HTTP-only deployments pay zero tonic weight. |
| `compat-v03` | Opt-in compatibility shim for `a2a-sdk 0.3.x` / Strands clients. |

## Storage wiring

Push delivery is strict opt-in at both levels:

- **Storage**: `InMemoryA2aStorage::new().with_push_dispatch_enabled(true)` (and equivalents per backend) so atomic commits write the pending-dispatch marker.
- **Builder**: `.push_delivery_store(storage.clone())` to register the consumer.

`.storage(storage)` wires the framework's storage traits (task storage, event store, atomic store, push delivery store, cancellation supervisor) from a single backend. Cross-instance correctness for streaming and push delivery requires they all live on the same backend; the builder enforces this.

```rust
let storage = InMemoryA2aStorage::new(); // or SqliteA2aStorage, PostgresA2aStorage, DynamoDbA2aStorage
A2aServer::builder()
    .executor(MyAgent)
    .storage(storage)
    .build()?;
```

## Durable event coordination

Task state and streaming events are written atomically via
`storage::A2aAtomicStore`. The event store is the source of truth; the
in-process broker is a local wake-up signal. Terminal replay and
Last-Event-ID reconnection work across instances when all replicas
share a backend (PostgreSQL, DynamoDB). Subscribers attached while a
task is non-terminal receive replay of prior events and then live
delivery; subscribing to an already-terminal task returns
`UnsupportedOperationError` per A2A v1.0 §3.1.6 — use `GetTask` to
retrieve the final state.

## Multi-instance streaming

The in-process event broker provides local fanout for attached
clients on the same instance only. Shared task storage solves
request/response correctness across instances; cross-instance
streaming coordination relies on the durable event store with
monotonic IDs and replay semantics.

## gRPC

A third transport alongside HTTP+JSON and JSON-RPC, behind the
`grpc` feature. The default dependency tree does not include tonic.

```bash
cargo build -p turul-a2a --features grpc

# Example server + client
cargo run -p grpc-agent --bin grpc-agent
cargo run -p grpc-agent --bin grpc-client -- send "hello"
cargo run -p grpc-agent --bin grpc-client -- stream "hello"
```

All 11 `lf.a2a.v1.A2AService` RPCs are served via
`A2aServer::into_tonic_router()` — the single public entry point,
which always layers the same Tower auth stack as the HTTP path.
Streaming consumes the same durable event store as SSE, with
`a2a-last-event-id` ASCII metadata for resume. `grpc-reflection` and
`grpc-health` are optional sub-features.

The client wrapper `turul_a2a_client::grpc::A2aGrpcClient` ships
under the same feature flag. Not available under
`turul-a2a-aws-lambda` (Lambda lacks persistent HTTP/2).

## Examples

See `examples/echo-agent`, `examples/auth-agent`, and `examples/grpc-agent`
in the repository for runnable end-to-end demos.

See the [workspace README](../../README.md) for the project overview
and crate map.

## License

Licensed under either [MIT](../../LICENSE-MIT) or
[Apache 2.0](../../LICENSE-APACHE) at your option.
