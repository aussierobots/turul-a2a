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

pub mod auth;
pub mod error;
pub mod service;
pub mod streaming;

pub use auth::GrpcAuthLayer;
pub use service::GrpcService;

use std::sync::Arc;

use crate::middleware::MiddlewareStack;
use crate::router::AppState;

/// Build a fully-wired tonic router with auth layered and the
/// [`GrpcService`] registered.
///
/// Auth wraps the entire tonic server (ADR-014 §2.4): every RPC runs
/// `MiddlewareStack::before_request` before dispatch and receives a
/// populated [`crate::middleware::context::RequestContext`] in the
/// request extensions. Failures surface as `tonic::Status::unauthenticated`
/// / `tonic::Status::permission_denied` — no A2A error model, no HTTP
/// JSON body (ADR-004).
///
/// This is the only public entry point to the gRPC surface. Returning
/// the raw service without the auth layer would permit adopters to
/// silently bypass authentication (ADR-014 §2.2).
pub fn make_grpc_router(
    state: AppState,
    middleware_stack: Arc<MiddlewareStack>,
) -> LayeredGrpcRouter {
    let service = turul_a2a_proto::grpc::A2aServiceServer::new(GrpcService::new(state));
    tonic::transport::Server::builder()
        .layer(GrpcAuthLayer::new(middleware_stack))
        .add_service(service)
}

/// Return type of [`make_grpc_router`]: a tonic router with the A2A
/// auth layer already applied. The concrete name is exposed so the
/// server builder can return the same type from
/// [`crate::A2aServer::into_tonic_router`].
pub type LayeredGrpcRouter = tonic::transport::server::Router<
    tower::layer::util::Stack<GrpcAuthLayer, tower::layer::util::Identity>,
>;
