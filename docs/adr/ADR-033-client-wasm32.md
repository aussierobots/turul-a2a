# ADR-033: turul-a2a-client on wasm32

- **Status:** Accepted
- **Date:** 2026-11-20
- **Related:** ADR-014 (gRPC transport)

## Context

Callers running in a browser, or in a JavaScript runtime such as Node, need an A2A client compiled to WebAssembly. `turul-a2a-client` did not build for `wasm32-unknown-unknown`:

1. It depended on the workspace `tokio` with `features = ["full"]`. That enables `net`, which pulls in `mio`, and `mio` has no wasm32 backend. The library code never used tokio; only the tests did.
2. `uuid` needs a randomness source on `wasm32-unknown-unknown`, and that target has no OS RNG.
3. `SseStream` and `TypedSseStream` box a `dyn Stream + Send`. reqwest's wasm body stream is backed by a JS `ReadableStream`, which is not `Send`.

The client spawns no tasks and uses no timers or clocks, so it needs no runtime abstraction.

## Decision

1. **Target.** `wasm32-unknown-unknown` on a JavaScript host. reqwest 0.13 picks its `fetch` backend for wasm32, so the HTTP+JSON client and SSE streaming work unchanged. No new feature is added; dependencies are gated by `cfg(target_arch)`.
2. **No runtime dependency.** `tokio` moves to native-only dev-dependencies, together with `wiremock`.
3. **Randomness.** On `cfg(all(target_arch = "wasm32", target_os = "unknown"))`, `uuid` is built with its `js` feature (the JS crypto API).
4. **Stream bound.** The crate-private `BoxedStream<T>` alias in `sse.rs` is the one definition of the boxed event stream behind both public stream types. It is `Send` on native targets and drops `Send` on wasm32. `sse_events` states no `Send` bound; the compiler infers it from the byte stream. Native public types keep `Send`.
5. **gRPC is native-only.** tonic's transport and its dependencies (`mio`, `socket2`) do not build for wasm32, so `--features grpc` fails to compile on wasm32. A `compile_error!` in this crate would never be reached, because dependencies build first.
6. **CORS is out of scope.** A browser calling a server on another origin needs CORS response headers. The server framework does not send them; adopters add a CORS layer to the router, or a gateway in front of it. Adding CORS to the framework is a separate decision.

## Verification

`tests/wasm.rs` (`#![cfg(target_arch = "wasm32")]`) runs card discovery, a blocking send and a streaming send under Node with `wasm-bindgen-test-runner`, against a live `post-task-hook-agent`. The server URL comes from `TURUL_WASM_E2E_URL` at compile time, and an unset URL fails the test rather than skipping it. The release gate adds `cargo clippy -p turul-a2a-client --target wasm32-unknown-unknown --all-targets -- -D warnings` and this wasm test. Native `static_assertions` checks keep `SseStream` and `TypedSseStream` `Send`.

## Consequences

- Native adopters see no API or behaviour change.
- On wasm, the stream types are not `Send`.
- A browser test (headless Firefox) is not part of the gate. It would need a CORS-enabled fixture.

## Revision log

- 2026-11-20: Accepted.
