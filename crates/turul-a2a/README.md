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
| `compat-v03` | Opt-in compatibility shim for `a2a-sdk 0.3.x` / Strands clients. |

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

## Examples

See `examples/echo-agent` and `examples/auth-agent` in the repository for
runnable end-to-end demos.

See the [workspace README](https://github.com/aussierobots/turul-a2a#readme)
for the full project overview.

## License

Licensed under either [MIT](../../LICENSE-MIT) or
[Apache 2.0](../../LICENSE-APACHE) at your option.
