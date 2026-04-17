//! Structured-event target declarations for framework observability.
//!
//! 0.1.x ships structured `tracing` events rather than a committed metrics
//! backend. The targets below are the stable contract adopters subscribe to
//! when wiring their own telemetry. Future phases add events at additional
//! targets; the naming scheme is `turul_a2a::<component>_<event>`.
//!
//! Event-emission guidelines:
//! - Every framework-internal event uses a target from this module.
//! - Events carry typed fields (`task_id`, `tenant`, `config_id`, etc.).
//! - Secret values (credentials, tokens) MUST NOT appear in any field.
//!
//! Tests intercept events via a capturing `tracing_subscriber::Layer` and
//! assert on `(target, level, fields)` — not on counter increments. See
//! `tests/runtime_substrate_tests.rs` for the canonical capture pattern.

/// Fired at ERROR level when a supervisor task panics and [`crate::server::in_flight::SupervisorSentinel`]
/// runs cleanup via `Drop` during the unwind. Fields: `tenant`, `task_id`.
///
/// Adopters SHOULD alert on non-zero event rate at this target — a panic is a
/// framework invariant violation, not normal operation.
pub const TARGET_SUPERVISOR_PANIC: &str = "turul_a2a::supervisor_panic";
