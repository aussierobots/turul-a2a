# grpc-agent

Worked example of the A2A gRPC transport (ADR-014). Ships two binaries
in a single crate:

- **`grpc-agent`** — a tonic server exposing the full
  `lf.a2a.v1.A2AService` on port 3005.
- **`grpc-client`** — a CLI that talks to it via
  `turul_a2a_client::grpc::A2aGrpcClient`.

The example demonstrates:

- `A2aServer::into_tonic_router()` — the single public gRPC entry
  point, which always layers the Tower auth stack (ADR-014 §2.2 /
  §2.4).
- Server-streaming via `SendStreamingMessage` using the durable
  event store + broker (ADR-009 shared with SSE, no parallel event
  pipeline).
- `last_chunk` chunked artifacts delivered through the gRPC stream
  (ADR-006 / ADR-014 §2.3 two-layer persistence).
- The ergonomic client wrapper — auth + tenant + metadata handling.

## Run

In terminal A:

```bash
cargo run -p grpc-agent --bin grpc-agent
```

The server prints its listen address and three example `grpc-client`
invocations.

In terminal B:

```bash
cargo run -p grpc-agent --bin grpc-client -- send   "hello world"
cargo run -p grpc-agent --bin grpc-client -- stream "stream this"
cargo run -p grpc-agent --bin grpc-client -- list
cargo run -p grpc-agent --bin grpc-client -- get    <task-id-from-send>
```

Override the endpoint with `A2A_GRPC_ENDPOINT=http://host:port`.

### Expected streaming output

`cargo run ... stream "hi"` should print something like:

```
[1] status -> Some(Submitted)
[2] status -> Some(Working)
[3] artifact id=Some("grpc-out") append=false last_chunk=false
        text: "echo"
[4] artifact id=Some("grpc-out") append=true last_chunk=true
        text: ": hi"
[5] status -> Some(Completed)
stream closed after 5 event(s)
```

The first persisted event is `SUBMITTED` (no synthetic `Task` snapshot
precedes it on `SendStreamingMessage` — ADR-014 §2.3 narrows that
spec §3.1.6 first-event-Task rule to `SubscribeToTask`).

## What this example is NOT

- **Not production-ready.** `A2aServer::builder()` uses the in-memory
  storage default; tasks disappear on restart. Wire a persistent
  backend (SQLite / PostgreSQL / DynamoDB per ADR-009) for real use.
- **Not TLS-terminated.** gRPC in production MUST run under TLS
  (A2A v1.0 §7.1); this framework defers termination to your reverse
  proxy / service mesh (ADR-014 §2.8). The example uses
  `http://127.0.0.1:3005` for local loopback only.
- **Not Lambda-compatible.** Lambda lacks persistent HTTP/2; gRPC is
  out of scope for `turul-a2a-aws-lambda` in the current release
  (ADR-014 §2.6). Host this example on ECS / Fargate / AppRunner /
  Kubernetes / a self-managed VM.

## See also

- `examples/echo-agent` — HTTP+JSON + JSON-RPC equivalent.
- `examples/streaming-agent` — SSE streaming over HTTP.
- `docs/adr/ADR-014-grpc-transport.md` — full design.
