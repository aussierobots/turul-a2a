# turul-a2a-client

Independent, typed client library for the A2A Protocol v1.0.

- Discover agent cards via `/.well-known/agent-card.json`.
- Send messages (blocking and streaming SSE).
- Get, list, cancel, and subscribe to tasks.
- Push notification config CRUD.
- Tenant scoping, API-key and Bearer auth.
- Wrapper-first request/response types with typed streaming events.

This crate has no dependency on the server framework. It ships separately
so you can call any A2A v1.0 agent — built with `turul-a2a` or any other
compliant server.

See the [workspace README](https://github.com/aussierobots/turul-a2a#readme)
for the full project overview.

## License

Licensed under either [MIT](../../LICENSE-MIT) or
[Apache 2.0](../../LICENSE-APACHE) at your option.
