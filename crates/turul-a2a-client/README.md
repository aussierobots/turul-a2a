# turul-a2a-client

Independent, typed client library for the A2A Protocol v1.0.

This crate has no dependency on the server framework. It ships separately
so you can call any A2A v1.0 agent — built with `turul-a2a` or any other
compliant server.

## Features

- Discover agent cards via `/.well-known/agent-card.json`.
- Send messages (blocking and streaming SSE).
- Get, list, cancel, and subscribe to tasks.
- Push notification config CRUD.
- Tenant scoping, API-key and Bearer auth.
- Wrapper-first request/response types with typed streaming events.
- Optional gRPC transport via the `grpc` feature.

## Quick start

```rust
use turul_a2a_client::{A2aClient, MessageBuilder};

let client = A2aClient::discover("http://localhost:3000").await?;
let response = client
    .send_message(MessageBuilder::new().text("hello").build())
    .await?;
println!("task: {}", response.task.unwrap().id);
```

`discover` performs `GET /.well-known/agent-card.json` and caches the
agent card. Skip discovery with `A2aClient::new(base_url)` if you
already know the routes.

## Streaming

```rust
use futures::StreamExt;

let mut stream = client
    .send_streaming_message(MessageBuilder::new().text("stream this").build())
    .await?;

while let Some(event) = stream.next().await {
    let event = event?;
    println!("event: {:?}", event);
}
```

`subscribe_to_task(task_id)` reattaches to an existing non-terminal task
and replays missed events from the durable event store. Subscribing to
an already-terminal task returns `UnsupportedOperationError`; use
`get_task` for the final state.

## Push notification config

Register a webhook against an existing task:

```rust
use turul_a2a_proto as pb;

let task_id = response.task.unwrap().id;
client
    .create_push_config(
        &task_id,
        pb::TaskPushNotificationConfig {
            task_id: task_id.clone(),
            url: "https://my-webhook.example/a2a-callback".into(),
            token: "shared-secret-token".into(),
            ..Default::default()
        },
    )
    .await?;
```

For inline registration on the same `send_message` call (server assigns
the task id during atomic creation), use the `MessageBuilder` helper:

```rust
let request = MessageBuilder::new()
    .text("hello")
    .inline_push_config(
        "https://my-webhook.example/a2a-callback",
        "shared-secret-token",
    )
    .build();
client.send_message(request).await?;
```

The server delivers a POST to the registered URL when the task reaches
a terminal state, with the token echoed in the
`X-Turul-Push-Token` header so the receiver can validate the call.

## Auth

```rust
use turul_a2a_client::{A2aClient, ClientAuth};

// API key (the header name is server-configured; default `X-API-Key`).
let client = A2aClient::new("http://agent.example")
    .with_auth(ClientAuth::ApiKey {
        header: "X-API-Key".into(),
        key: "my-key".into(),
    });

// Bearer JWT.
let client = A2aClient::new("http://agent.example")
    .with_auth(ClientAuth::Bearer("eyJ...".into()));
```

## Tenant scoping

Multi-tenant agents route per-tenant via path prefix
(`/{tenant}/message:send`). Set the tenant once on the client:

```rust
let client = A2aClient::new("http://agent.example").with_tenant("acme");
```

All subsequent calls automatically include the tenant prefix.

## Errors

All client methods return `Result<T, A2aClientError>`. Common variants:

| Variant | Cause |
|---|---|
| `Http { status, message }` | Non-2xx HTTP response without an A2A error envelope. |
| `A2aError { status, message, reason }` | A2A-typed error (`TaskNotFound`, `TaskNotCancelable`, `UnsupportedOperation`, …) with the `ErrorInfo.reason` from the server. Use `.reason()` to read it. |
| `Request(_)` | reqwest/transport error before a response arrived. |
| `Json(_)` | Response payload didn't parse as expected JSON. |
| `Conversion(_)` | Proto → wrapper type conversion failed. |
| `Sse(_)` / `StreamClosed` | SSE-specific errors during streaming. |

`A2aClientError` is `#[non_exhaustive]`. With the `grpc` feature it
also carries `Grpc` and `GrpcTransport` variants.

## Examples

- `examples/echo-agent` — round-trip a message, blocking.
- `examples/conversation-agent` — multi-turn refinement with `referenceTaskIds`.
- `examples/interrupting-agent` — pause + resume via `INPUT_REQUIRED`.
- `examples/callback-agent` — register a push-notification webhook.
- `examples/grpc-agent` — gRPC transport (`--features grpc`).

See the [workspace README](../../README.md) for the project overview
and crate map.

## License

Licensed under either [MIT](../../LICENSE-MIT) or
[Apache 2.0](../../LICENSE-APACHE) at your option.
