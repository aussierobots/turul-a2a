//! gRPC transport (ADR-014).
//!
//! Third thin adapter over the shared core handlers (ADR-005 extended).
//! All 11 `lf.a2a.v1.A2AService` RPCs dispatch into the same `core_*`
//! functions that HTTP and JSON-RPC call; this module contains only
//! transport-level adaptation (error mapping, metadata, streaming
//! serialization).
//!
//! Enabled by the `grpc` Cargo feature. `grpc-reflection` and
//! `grpc-health` are optional sub-features that add the
//! `grpc.reflection.v1alpha.ServerReflection` and
//! `grpc.health.v1.Health` services respectively (off by default).

pub mod error;
pub mod service;
pub mod streaming;

pub use service::GrpcService;
