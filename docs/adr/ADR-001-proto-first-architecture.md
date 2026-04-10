# ADR-001: Proto-First Architecture with Generated Proto and Wrapper Layer

- **Status:** Accepted
- **Date:** 2026-04-10

## Context

The A2A Protocol v1.0 designates `a2a.proto` (package `lf.a2a.v1`) as the single normative definition for all message types and service interfaces. Any Rust implementation must decide how to represent these types internally while maintaining wire-format compliance.

Three approaches were evaluated:

| Approach | Trade-offs |
|----------|------------|
| Generate from proto with `prost` + `tonic` | Tight gRPC coupling; serde JSON mapping requires manual work; no camelCase JSON out of the box. |
| Hand-code types with `serde`, test against proto | Maximum ergonomics but proto drift risk. Testing found 15+ wire-format violations in early prototypes. |
| Generate from proto with `prost` + `pbjson`, wrap in ergonomic Rust types | Two-layer system adds conversion boilerplate but anchors correctness to the normative source. |

## Decision

Generate from proto with `prost` + `pbjson`, then wrap in ergonomic Rust types. The project uses a two-crate architecture:

1. **`turul-a2a-proto`** -- generates Rust types via `prost-build` + `pbjson-build`. The proto file is vendored at `proto/a2a.proto`. Google well-known types are mapped to `pbjson_types` via `compile_well_known_types()` + `extern_path`. `pbjson` provides `serde::Serialize` / `serde::Deserialize` implementations with camelCase JSON field names, matching the proto JSON mapping specification.

2. **`turul-a2a-types`** -- wraps generated proto types with ergonomic Rust types that provide:
   - `From` / `TryFrom` conversions between proto and wrapper layers.
   - State machine enforcement for task lifecycle transitions.
   - Builder patterns for constructing valid instances.

gRPC support via `tonic` is deferred and will be feature-gated behind `grpc` when needed.

## Consequences

- **Proto updates are a single `cargo build`.** Regeneration happens automatically; no manual sync step.
- **Wire-format compliance is anchored to the normative source.** The 15+ violations found in hand-coded approaches are structurally eliminated.
- **Two-layer type system adds conversion boilerplate.** Every proto type that surfaces in the public API has a corresponding wrapper, with `From` or `TryFrom` conversions in both directions.
- **`pbjson` handles JSON serialization.** camelCase field names, `oneof` representations, and well-known type mappings all follow the proto JSON mapping spec without manual `#[serde(rename)]` annotations.
- **`#[non_exhaustive]` on all public wrapper types.** New fields can be added to wrapper types in minor versions without breaking downstream consumers.
- **gRPC is not an immediate dependency.** The `tonic` code-gen path exists but is gated, keeping the default dependency tree lighter for HTTP/JSON-only deployments.
