//! A2A transport compliance middleware.
//!
//! Validates the `A2A-Version` service parameter and the Content-Type of
//! POST requests. Runs as the outermost Tower layer, before auth.
//!
//! Rejections on the JSON-RPC endpoint are JSON-RPC error envelopes (HTTP
//! 200, `id: null` because the body has not been read); every other path
//! gets the HTTP+JSON error body.

use std::task::{Context, Poll};

use axum::body::Body;
use axum::response::IntoResponse;
use http::{Request, Response};
use tower::{Layer, Service};
use turul_a2a_types::wire::protocol;

use crate::error::A2aError;

/// Whether `requested` names `target`'s `Major.Minor`.
///
/// Only `Major.Minor` is compared: spec Section 3.6 says patch numbers
/// "MUST not be considered when clients and servers negotiate protocol
/// versions", so `1.0.1` is treated as `1.0`.
fn same_major_minor(requested: &str, target: &str) -> bool {
    let mut ours = target.split('.');
    let mut theirs = requested.trim().split('.');
    ours.next() == theirs.next()
        && ours.next() == theirs.next()
        && theirs.all(|patch| !patch.is_empty() && patch.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether `requested` names the protocol version this server speaks.
pub(crate) fn version_supported(requested: &str) -> bool {
    same_major_minor(requested, protocol::VERSION)
}

/// The legacy protocol version served on the JSON-RPC endpoint by the
/// `compat-v03` feature.
const LEGACY_VERSION: &str = "0.3";

/// Whether `requested` names the legacy 0.3 protocol.
pub(crate) fn is_legacy_version(requested: &str) -> bool {
    same_major_minor(requested, LEGACY_VERSION)
}

/// Whether the HTTP transports serve the legacy 0.3 protocol.
const HTTP_SERVES_V03: bool = cfg!(feature = "compat-v03");

/// Validate the requested `A2A-Version` (header, gRPC metadata or query
/// parameter value).
///
/// Spec Section 3.6.2: an absent or empty value means version 0.3, and an
/// agent MUST return `VersionNotSupportedError` for a version the
/// interface does not support. `serves_v03` says whether this interface
/// serves 0.3; when it does, an absent, empty or explicit `0.3` is
/// accepted, otherwise all three are rejected.
pub(crate) fn check_version(requested: Option<&str>, serves_v03: bool) -> Result<(), A2aError> {
    match requested.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) if version_supported(v) => Ok(()),
        Some(v) if serves_v03 && is_legacy_version(v) => Ok(()),
        Some(v) => Err(A2aError::VersionNotSupported {
            version: v.to_string(),
        }),
        None if serves_v03 => Ok(()),
        None => Err(A2aError::VersionNotSupported {
            version: "0.3 (A2A-Version absent or empty)".to_string(),
        }),
    }
}

/// The requested version on an HTTP request: the `A2A-Version` header, or
/// failing that the `A2A-Version` query parameter (spec Section 3.6.1:
/// "Clients MAY provide the `A2A-Version` as a request parameter instead
/// of a header"). Service-parameter names are case-insensitive.
fn requested_version<B>(req: &Request<B>) -> Option<String> {
    if let Some(v) = req.headers().get(protocol::VERSION_PARAM) {
        return Some(v.to_str().unwrap_or_default().to_string());
    }
    req.uri().query()?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        key.eq_ignore_ascii_case(protocol::VERSION_PARAM)
            .then(|| value.to_string())
    })
}

/// Whether a Content-Type names a JSON body this server accepts:
/// `application/json` or the registered `application/a2a+json` (spec
/// Section 14.1), with any media-type parameters such as `charset`.
fn is_accepted_json_media_type(content_type: &str) -> bool {
    let essence = content_type.split(';').next().unwrap_or_default().trim();
    essence.eq_ignore_ascii_case(protocol::MEDIA_TYPE_JSON)
        || essence.eq_ignore_ascii_case(protocol::MEDIA_TYPE_A2A_JSON)
}

/// Tower Layer for A2A transport compliance.
#[derive(Clone)]
pub struct TransportComplianceLayer;

impl<S> Layer<S> for TransportComplianceLayer {
    type Service = TransportComplianceService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TransportComplianceService { inner }
    }
}

#[derive(Clone)]
pub struct TransportComplianceService<S> {
    inner: S,
}

impl<S> Service<Request<Body>> for TransportComplianceService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();

        Box::pin(async move {
            if let Err(err) = check_request(&req) {
                let response = if crate::jsonrpc::is_jsonrpc_path(req.uri().path()) {
                    axum::Json(err.to_jsonrpc_error(None)).into_response()
                } else {
                    err.into_response()
                };
                return Ok(response);
            }
            inner.call(req).await
        })
    }
}

fn check_request<B>(req: &Request<B>) -> Result<(), A2aError> {
    // Discovery is public and version-independent.
    if !super::is_bypass_path(req.uri().path()) {
        check_version(requested_version(req).as_deref(), HTTP_SERVES_V03)?;
    }

    // A POST without a Content-Type is allowed (e.g. cancel with an empty
    // body); one that declares a type must declare JSON.
    if req.method() == http::Method::POST
        && let Some(value) = req.headers().get(http::header::CONTENT_TYPE)
    {
        let content_type = value.to_str().unwrap_or_default();
        if !is_accepted_json_media_type(content_type) {
            return Err(A2aError::ContentTypeNotSupported {
                content_type: content_type.to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_match_ignores_patch() {
        assert!(version_supported("1.0"));
        assert!(version_supported("1.0.1"));
        assert!(version_supported(" 1.0 "));
        assert!(!version_supported("1.1"));
        assert!(!version_supported("0.3"));
        assert!(!version_supported("1"));
        assert!(!version_supported("1.0.x"));
        assert!(!version_supported("10.0"));
    }

    #[test]
    fn version_0_3_absent_empty_or_explicit_follows_serves_v03() {
        for requested in [None, Some(""), Some("  "), Some("0.3"), Some("0.3.0")] {
            assert!(check_version(requested, true).is_ok(), "{requested:?}");
            assert!(
                matches!(
                    check_version(requested, false),
                    Err(A2aError::VersionNotSupported { .. })
                ),
                "{requested:?}"
            );
        }
        for serves_v03 in [true, false] {
            assert!(check_version(Some("1.0"), serves_v03).is_ok());
            assert!(matches!(
                check_version(Some("0.4"), serves_v03),
                Err(A2aError::VersionNotSupported { .. })
            ));
        }
    }

    #[test]
    fn http_serves_v03_only_with_compat_feature() {
        assert_eq!(HTTP_SERVES_V03, cfg!(feature = "compat-v03"));
    }

    #[test]
    fn version_query_parameter_is_read_when_header_absent() {
        let req = Request::get("/tasks?pageSize=5&a2a-version=1.0")
            .body(())
            .unwrap();
        assert_eq!(requested_version(&req).as_deref(), Some("1.0"));

        let req = Request::get("/tasks?A2A-Version=0.9")
            .header("A2A-Version", "1.0")
            .body(())
            .unwrap();
        assert_eq!(
            requested_version(&req).as_deref(),
            Some("1.0"),
            "header wins over query parameter"
        );
    }

    #[test]
    fn json_media_types() {
        assert!(is_accepted_json_media_type("application/json"));
        assert!(is_accepted_json_media_type("application/a2a+json"));
        assert!(is_accepted_json_media_type(
            "application/a2a+json; charset=utf-8"
        ));
        assert!(is_accepted_json_media_type(
            "Application/JSON;charset=UTF-8"
        ));
        assert!(!is_accepted_json_media_type("application/jsonx"));
        assert!(!is_accepted_json_media_type("text/plain; application/json"));
        assert!(!is_accepted_json_media_type("text/xml"));
        assert!(!is_accepted_json_media_type(""));
    }
}
