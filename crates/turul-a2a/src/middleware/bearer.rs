//! Bearer token extraction — hardened parser.
//! Design sourced from turul-mcp-framework/crates/turul-http-mcp-server/src/middleware/bearer.rs.

/// Extract a Bearer token from an Authorization header value.
///
/// Returns None if the scheme is not "Bearer" or the token is malformed.
/// Hardened: case-insensitive scheme, rejects whitespace and control characters.
pub fn extract_bearer_token(auth_header: &str) -> Option<String> {
    let trimmed = auth_header.trim();
    let (scheme, rest) = trimmed.split_once([' ', '\t'])?;

    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }

    let token = rest.trim();

    if token.is_empty() {
        return None;
    }

    // Reject tokens with whitespace (multi-token)
    if token.contains(|c: char| c.is_ascii_whitespace()) {
        return None;
    }

    // Reject ASCII control characters
    if token.contains(|c: char| c.is_ascii_control()) {
        return None;
    }

    Some(token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_bearer_token() {
        assert_eq!(
            extract_bearer_token("Bearer eyJhbGciOiJSUzI1NiJ9.test"),
            Some("eyJhbGciOiJSUzI1NiJ9.test".to_string())
        );
    }

    #[test]
    fn case_insensitive_scheme() {
        assert!(extract_bearer_token("bearer token123").is_some());
        assert!(extract_bearer_token("BEARER token123").is_some());
        assert!(extract_bearer_token("Bearer token123").is_some());
    }

    #[test]
    fn non_bearer_scheme_returns_none() {
        assert!(extract_bearer_token("Basic dXNlcjpwYXNz").is_none());
        assert!(extract_bearer_token("Digest nonce=abc").is_none());
    }

    #[test]
    fn empty_token_returns_none() {
        assert!(extract_bearer_token("Bearer ").is_none());
        assert!(extract_bearer_token("Bearer").is_none());
    }

    #[test]
    fn whitespace_in_token_rejected() {
        assert!(extract_bearer_token("Bearer token with spaces").is_none());
    }

    #[test]
    fn control_chars_rejected() {
        assert!(extract_bearer_token("Bearer token\x00bad").is_none());
        assert!(extract_bearer_token("Bearer token\x1Fbad").is_none());
    }

    #[test]
    fn leading_trailing_whitespace_trimmed() {
        assert_eq!(
            extract_bearer_token("  Bearer   mytoken  "),
            Some("mytoken".to_string())
        );
    }
}
