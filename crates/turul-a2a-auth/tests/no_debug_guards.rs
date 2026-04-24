//! ADR-016 §4 test #5 — type-level guard.
//!
//! Static assertion that `ApiKeyMiddleware`, `BearerMiddleware`, and
//! `StaticApiKeyLookup` do not implement `Debug`. Prevents a drive-by
//! `#[derive(Debug)]` from silently exposing credential material (the
//! raw keys in `StaticApiKeyLookup`, the inner `Arc<dyn ApiKeyLookup>`
//! in `ApiKeyMiddleware`, or the `Arc<JwtValidator>` in `BearerMiddleware`).
//!
//! This is enforced at compile time — if any of these types later gain
//! a `Debug` impl, this crate will fail to build.

use static_assertions::assert_not_impl_any;
use turul_a2a_auth::{ApiKeyMiddleware, BearerMiddleware, StaticApiKeyLookup};

assert_not_impl_any!(ApiKeyMiddleware: std::fmt::Debug);
assert_not_impl_any!(BearerMiddleware: std::fmt::Debug);
assert_not_impl_any!(StaticApiKeyLookup: std::fmt::Debug);
