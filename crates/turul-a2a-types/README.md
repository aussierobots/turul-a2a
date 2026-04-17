# turul-a2a-types

Ergonomic Rust wrappers over the prost-generated A2A Protocol v1.0 types.

Provides:

- [`Task`], [`TaskStatus`], [`TaskState`] with state-machine enforcement.
- [`Message`], [`Part`], [`Role`] for conversational payloads.
- [`Artifact`] for task outputs.
- [`wire`] module with JSON-RPC method constants and SSE event shapes.

All public types are `#[non_exhaustive]` — new fields are additive, not
breaking. Raw proto access is available via the [`proto`] re-export for
advanced use cases.

This crate is transport- and storage-agnostic. See
[`turul-a2a`](https://crates.io/crates/turul-a2a) for the server framework and
[`turul-a2a-client`](https://crates.io/crates/turul-a2a-client) for the
client library.

See the [workspace README](https://github.com/aussierobots/turul-a2a#readme)
for the full project overview.

## License

Licensed under either [MIT](../../LICENSE-MIT) or
[Apache 2.0](../../LICENSE-APACHE) at your option.
