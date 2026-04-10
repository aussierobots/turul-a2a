# ADR-002: Wrapper Boundary and Validation Strategy

- **Status:** Accepted
- **Date:** 2026-04-10

## Context

Proto-generated types produced by `prost` are intentionally permissive: all message fields are `Option<T>`, enums are represented as `i32`, and no domain invariants are enforced. This is correct behaviour for a serialization layer -- any bytes on the wire should round-trip without loss.

However, the A2A specification imposes stronger requirements:

- Several fields are marked **REQUIRED** (e.g., `Task.id`, `Task.status`, `Message.role`).
- The `TaskState` enum includes an `UNSPECIFIED` variant that must never appear in valid application data.
- Task status transitions follow a state machine (e.g., a `completed` task cannot move back to `working`).

The wrapper layer introduced in ADR-001 needs a clear policy on where and how these constraints are enforced.

## Decision

**Validation happens at the boundary between proto and wrapper types.**

Specific rules:

1. **`Task::try_from(proto::Task)`** rejects instances with an empty `id` or a missing/`UNSPECIFIED` status. There is no infallible `From<proto::Task>` -- only `TryFrom`.

2. **`Message::try_from(proto::Message)`** rejects instances with an `UNSPECIFIED` role.

3. **`TaskState`** (the wrapper enum) excludes the `UNSPECIFIED` variant entirely. The `TryFrom<i32>` conversion maps the proto `UNSPECIFIED` value to an error rather than a variant.

4. **State machine transitions** are enforced in `state_machine.rs`. Storage implementations delegate to this module before persisting status changes, ensuring that invalid transitions (e.g., `completed` to `working`) are rejected regardless of the caller.

5. **Serde deserialization** for wrapper types delegates to the `pbjson`-generated `Deserialize` impl on the proto type, then applies `TryFrom`. This means invalid JSON is rejected at parse time with a meaningful error, not silently accepted.

## Consequences

- **Any wrapper `Task` or `Message` that exists is guaranteed valid.** Code that receives a `Task` value can assume `id` is non-empty and `status` is a real state -- no defensive `unwrap_or_default` needed.
- **Storage and handler layers operate exclusively on validated types.** They do not need to re-check REQUIRED field constraints.
- **`From<proto>` is unavailable for types with REQUIRED fields.** Only `TryFrom` is implemented. This is a deliberate trade-off: compile-time visibility of fallibility outweighs the ergonomic cost of `.try_from()?.` chains.
- **Error messages surface at the boundary.** A malformed JSON payload from a remote agent produces a clear validation error at deserialization time, not a panic deep in business logic.
- **State machine is a single enforcement point.** All storage backends share the same transition logic, preventing inconsistencies between in-memory and persistent stores.
