//! Push notification delivery (ADR-011).
//!
//! This module hosts the framework's push-delivery surface: the
//! delivery-coordination storage contract, the worker that dispatches
//! outbound HTTP POSTs, and the supporting invariants around SSRF
//! safety and secret redaction.
//!
//! The storage contract — [`A2aPushDeliveryStore`] — is additive to
//! [`crate::storage::A2aPushNotificationStorage`]: configs are still
//! CRUD'd through the push-notification-storage trait; this new trait
//! handles only the per-delivery claim bookkeeping that multi-instance
//! deployments need. Adopters that do not wire a delivery worker do
//! not need to implement the new trait — configs remain stored, just
//! not delivered (the current framework behaviour).
//!
//! See ADR-011 for the full design. The trait docstrings carry the
//! normative contract; backend parity tests in
//! [`crate::storage::parity_tests`] gate per-backend acceptance.

pub mod claim;
pub mod delivery;
pub mod dispatcher;
pub mod secret;
pub mod ssrf;

pub use claim::{
    A2aPushDeliveryStore, AbandonedReason, ClaimStatus, DeliveryClaim, DeliveryErrorClass,
    DeliveryOutcome, FailedDelivery, GaveUpReason, PendingDispatch, ReclaimableClaim,
};
pub use dispatcher::PushDispatcher;
pub use secret::{redact_in_str, Secret};
pub use ssrf::{decide as ssrf_decide, is_blocked_ip, SsrfBlockReason, SsrfDecision};
