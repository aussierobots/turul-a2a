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
    if let Some(ctx) = parts.extensions.get::<lambda_http::request::RequestContext>() {
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
    let body_bytes = body.collect().await
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
        assert_eq!(
            headers.get("x-authorizer-userid").unwrap(),
            "user-123"
        );
    }
}
