//! Request/response conversion between Lambda and axum.

use axum::body::Body;
use http_body_util::BodyExt;

/// Header prefix for authorizer context. Client-supplied headers with this
/// prefix are stripped (anti-spoofing per ADR-008).
pub const AUTHORIZER_HEADER_PREFIX: &str = "x-authorizer-";

/// Convert a Lambda HTTP request to an axum request.
///
/// - Strips any client-supplied x-authorizer-* headers (anti-spoofing)
/// - Injects authorizer context from Lambda requestContext as x-authorizer-* headers
/// - Preserves all other headers, method, URI, body
pub fn lambda_to_axum_request(
    event: lambda_http::Request,
) -> Result<http::Request<Body>, lambda_http::Error> {
    let (mut parts, body) = event.into_parts();

    // Anti-spoofing: strip ALL client-supplied x-authorizer-* headers
    let keys_to_remove: Vec<http::header::HeaderName> = parts
        .headers
        .keys()
        .filter(|k| k.as_str().starts_with(AUTHORIZER_HEADER_PREFIX))
        .cloned()
        .collect();
    for key in keys_to_remove {
        parts.headers.remove(&key);
    }

    // Extract authorizer context from Lambda extensions and inject as headers.
    // The exact structure depends on the API Gateway type (v1, v2, ALB).
    // We use a generic approach: look for the RequestContext extension and
    // extract authorizer fields from it.
    if let Some(ctx) = parts
        .extensions
        .get::<lambda_http::request::RequestContext>()
    {
        match ctx {
            lambda_http::request::RequestContext::ApiGatewayV2(apigw) => {
                if let Some(ref authorizer) = apigw.authorizer {
                    // JWT authorizer: claims are in jwt.claims (HashMap<String, String>)
                    if let Some(ref jwt) = authorizer.jwt {
                        for (key, value) in &jwt.claims {
                            inject_authorizer_header(&mut parts.headers, key, value);
                        }
                    }
                    // Lambda authorizer: context fields
                    // In lambda_http v1, these may be in authorizer.fields or similar
                    // For now, JWT claims are the primary path
                }
            }
            lambda_http::request::RequestContext::ApiGatewayV1(apigw) => {
                // V1 authorizer context fields
                for (key, value) in &apigw.authorizer.fields {
                    if let serde_json::Value::String(s) = value {
                        inject_authorizer_header(&mut parts.headers, key, s);
                    }
                }
            }
            _ => {} // ALB, etc. — no authorizer extraction
        }
    }

    // Convert body
    let body_bytes: bytes::Bytes = match body {
        lambda_http::Body::Empty => bytes::Bytes::new(),
        lambda_http::Body::Text(s) => bytes::Bytes::from(s),
        lambda_http::Body::Binary(b) => bytes::Bytes::from(b),
        _ => bytes::Bytes::new(),
    };

    let req = http::Request::from_parts(parts, Body::from(body_bytes));
    Ok(req)
}

/// Rewrite `req.uri()` to drop a leading `prefix` from the path, if
/// present. Preserves query string, scheme, authority. If `prefix` is
/// `/` or the path does not start with `prefix`, the request is
/// returned unchanged (callers downstream will 404 on a genuinely
/// unknown path — that's the correct failure mode).
///
/// A path that equals `prefix` exactly becomes `/` so root-routed
/// handlers reach the router.
pub(crate) fn strip_request_path_prefix(
    mut req: http::Request<Body>,
    prefix: &str,
) -> http::Request<Body> {
    if prefix.is_empty() || prefix == "/" {
        return req;
    }
    let uri = req.uri().clone();
    let path = uri.path();
    if !path.starts_with(prefix) {
        return req;
    }
    let rest = &path[prefix.len()..];
    let new_path = if rest.is_empty() || !rest.starts_with('/') {
        // path == prefix → "/"; or prefix matches mid-segment
        // (`/devs` vs `/dev`) → do not strip.
        if rest.is_empty() {
            "/"
        } else {
            return req;
        }
    } else {
        rest
    };

    let path_and_query = match uri.query() {
        Some(q) => format!("{new_path}?{q}"),
        None => new_path.to_string(),
    };

    let mut parts = uri.into_parts();
    parts.path_and_query = match http::uri::PathAndQuery::from_maybe_shared(path_and_query) {
        Ok(pq) => Some(pq),
        Err(_) => return req,
    };
    if let Ok(new_uri) = http::Uri::from_parts(parts) {
        *req.uri_mut() = new_uri;
    }
    req
}

fn inject_authorizer_header(headers: &mut http::HeaderMap, key: &str, value: &str) {
    let header_name = format!("{AUTHORIZER_HEADER_PREFIX}{}", key.to_lowercase());
    if let Ok(name) = http::header::HeaderName::from_bytes(header_name.as_bytes()) {
        if let Ok(val) = http::header::HeaderValue::from_str(value) {
            headers.insert(name, val);
        }
    }
}

/// Convert an axum response to a Lambda HTTP response.
pub async fn axum_to_lambda_response(
    resp: http::Response<Body>,
) -> Result<lambda_http::Response<lambda_http::Body>, lambda_http::Error> {
    let (parts, body) = resp.into_parts();
    let body_bytes = body
        .collect()
        .await
        .map_err(|e| lambda_http::Error::from(format!("Body collect error: {e}")))?
        .to_bytes();

    let lambda_body = if body_bytes.is_empty() {
        lambda_http::Body::Empty
    } else {
        lambda_http::Body::Text(String::from_utf8_lossy(&body_bytes).into_owned())
    };

    let resp = http::Response::from_parts(parts, lambda_body);
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_client_supplied_authorizer_headers() {
        let req: lambda_http::Request = http::Request::builder()
            .method("POST")
            .uri("/message:send")
            .header("content-type", "application/json")
            .header("a2a-version", "1.0")
            .header("x-authorizer-userid", "forged-admin")
            .header("x-authorizer-role", "superuser")
            .header("x-real-header", "keep-this")
            .body(lambda_http::Body::Text("{}".into()))
            .unwrap();

        let axum_req = lambda_to_axum_request(req).unwrap();

        // Forged headers must be stripped
        assert!(
            axum_req.headers().get("x-authorizer-userid").is_none(),
            "Forged x-authorizer-userid must be stripped"
        );
        assert!(
            axum_req.headers().get("x-authorizer-role").is_none(),
            "Forged x-authorizer-role must be stripped"
        );

        // Legitimate headers preserved
        assert_eq!(
            axum_req.headers().get("x-real-header").unwrap(),
            "keep-this"
        );
        assert_eq!(
            axum_req.headers().get("content-type").unwrap(),
            "application/json"
        );
    }

    #[test]
    fn inject_authorizer_header_works() {
        let mut headers = http::HeaderMap::new();
        inject_authorizer_header(&mut headers, "userId", "user-123");
        assert_eq!(headers.get("x-authorizer-userid").unwrap(), "user-123");
    }

    fn uri_only(path_and_query: &str) -> http::Request<Body> {
        http::Request::builder()
            .method("GET")
            .uri(path_and_query)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn strip_path_prefix_removes_api_gateway_stage_and_resource_prefix() {
        let req = uri_only("/stage/agent/message:send");
        let out = strip_request_path_prefix(req, "/stage/agent");
        assert_eq!(out.uri().path(), "/message:send");
    }

    #[test]
    fn strip_path_prefix_preserves_query_string() {
        let req = uri_only("/stage/agent/tasks/abc?historyLength=5");
        let out = strip_request_path_prefix(req, "/stage/agent");
        assert_eq!(out.uri().path(), "/tasks/abc");
        assert_eq!(out.uri().query(), Some("historyLength=5"));
    }

    #[test]
    fn strip_path_prefix_handles_root_after_strip() {
        // Path exactly equals prefix (e.g. agent-card at the prefix root).
        let req = uri_only("/stage/agent");
        let out = strip_request_path_prefix(req, "/stage/agent");
        assert_eq!(out.uri().path(), "/");
    }

    #[test]
    fn strip_path_prefix_passes_through_when_prefix_absent() {
        let req = uri_only("/message:send");
        let out = strip_request_path_prefix(req, "/stage/agent");
        assert_eq!(
            out.uri().path(),
            "/message:send",
            "non-matching prefix must not mutate path"
        );
    }

    #[test]
    fn strip_path_prefix_rejects_mid_segment_match() {
        // "/dev" is a prefix of "/devs/foo" as a string, but not as a
        // path segment — must not strip.
        let req = uri_only("/devs/foo");
        let out = strip_request_path_prefix(req, "/dev");
        assert_eq!(out.uri().path(), "/devs/foo");
    }

    #[test]
    fn strip_path_prefix_noop_on_empty_or_slash_config() {
        let req = uri_only("/message:send");
        let out = strip_request_path_prefix(req, "/");
        assert_eq!(out.uri().path(), "/message:send");
        let req = uri_only("/message:send");
        let out = strip_request_path_prefix(req, "");
        assert_eq!(out.uri().path(), "/message:send");
    }

    #[test]
    fn strip_path_prefix_well_known_and_jsonrpc() {
        let prefix = "/stage/agent";
        for (input, expected) in [
            (
                "/stage/agent/.well-known/agent-card.json",
                "/.well-known/agent-card.json",
            ),
            ("/stage/agent/jsonrpc", "/jsonrpc"),
            ("/stage/agent/tasks/abc:cancel", "/tasks/abc:cancel"),
            ("/stage/agent/extendedAgentCard", "/extendedAgentCard"),
        ] {
            let req = uri_only(input);
            let out = strip_request_path_prefix(req, prefix);
            assert_eq!(out.uri().path(), expected, "input: {input}");
        }
    }
}
