//! Secret-string handling for push delivery (ADR-011 §4a).
//!
//! Two values flow through the push pipeline that are secrets:
//! - `AuthenticationInfo.credentials` — the material after the HTTP
//!   auth scheme in the outbound `Authorization` header.
//! - `TaskPushNotificationConfig.token` — the receiver-validation
//!   token sent as `X-Turul-Push-Token`.
//!
//! Both are wrapped in [`Secret`] inside the delivery worker so
//! accidental formatting paths (`{}`, `{:?}`, serde default, tracing
//! field capture) cannot leak the value. The wrapper's `Debug` and
//! `Display` impls print `[REDACTED]`. To use a secret, callers
//! explicitly call `.expose()` at the exact point where the
//! underlying `&str` is placed into an outbound header or validated
//! against a sentinel in tests.
//!
//! # Scope
//!
//! This is the internal framework wrapper. On the wire the push
//! CRUD API continues to expose credentials in the proto JSON as
//! the spec defines — that's the CRUD contract and adopters
//! opting into CRUD accept it. The rule is: no credential or
//! token may appear in any framework-side log, metric label, span
//! attribute, error value, or persisted failure record.

use std::fmt;

/// A string held privately — its contents never appear in `Debug`,
/// `Display`, structured logs, metric labels, or any other
/// formatting path except through an explicit `.expose()` call.
///
/// `#[derive(Clone)]` so the delivery worker can copy secrets per
/// attempt without enabling arbitrary `.to_string()` leaks.
/// `PartialEq` intentionally NOT derived — comparison paths are an
/// accidental leakage vector (e.g., test failure messages printing
/// both sides of an `assert_eq!`). Tests that need to check a
/// secret use a sentinel-string search on captured output.
#[derive(Clone)]
pub struct Secret<T: Clone = String> {
    inner: T,
}

impl<T: Clone> Secret<T> {
    pub fn new(v: T) -> Self {
        Self { inner: v }
    }

    /// Borrow the raw secret value. Callers must place the returned
    /// reference ONLY into the outbound wire context (an HTTP header
    /// builder, a stored-credential comparator, etc.), never into a
    /// format macro, trace event, or error value.
    pub fn expose(&self) -> &T {
        &self.inner
    }
}

impl<T: Clone> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<T: Clone> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<T: Clone> From<T> for Secret<T> {
    fn from(v: T) -> Self {
        Self { inner: v }
    }
}

/// Scrub a log-bound string of known secret substrings, replacing
/// them with `[REDACTED]`. Last-resort defence for formatted error
/// strings the framework passes through (e.g., a reqwest error
/// body that might echo back a request header). The primary
/// defence is never to put secrets into formatted strings at all;
/// this is a belt-and-suspenders layer.
pub fn redact_in_str(s: &str, secrets: &[&str]) -> String {
    let mut out = s.to_string();
    for secret in secrets {
        if !secret.is_empty() && out.contains(secret) {
            out = out.replace(secret, "[REDACTED]");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s: Secret = Secret::new("SECRET-VALUE-DO-NOT-LEAK".into());
        let formatted = format!("{s:?}");
        assert_eq!(formatted, "[REDACTED]");
        assert!(!formatted.contains("SECRET-VALUE"));
    }

    #[test]
    fn display_is_redacted() {
        let s: Secret = Secret::new("SECRET-VALUE-DO-NOT-LEAK".into());
        let formatted = format!("{s}");
        assert_eq!(formatted, "[REDACTED]");
    }

    #[test]
    fn debug_of_containing_struct_is_redacted() {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Carrier {
            name: String,
            credential: Secret,
        }
        let c = Carrier {
            name: "bearer".into(),
            credential: Secret::new("sk_live_abcdef1234567890".into()),
        };
        let formatted = format!("{c:?}");
        assert!(!formatted.contains("sk_live"));
        assert!(formatted.contains("[REDACTED]"));
    }

    #[test]
    fn expose_returns_the_raw_value() {
        let s: Secret = Secret::new("x".into());
        assert_eq!(s.expose(), "x");
    }

    #[test]
    fn redact_in_str_replaces_known_secrets() {
        let log_line = "Authorization: Bearer sk_live_abcdef sent to webhook.example.com";
        let cleaned = redact_in_str(log_line, &["sk_live_abcdef"]);
        assert_eq!(
            cleaned,
            "Authorization: Bearer [REDACTED] sent to webhook.example.com"
        );
    }

    #[test]
    fn redact_in_str_with_empty_secret_is_identity() {
        let log_line = "some log line";
        let cleaned = redact_in_str(log_line, &[""]);
        assert_eq!(cleaned, log_line);
    }

    #[test]
    fn redact_in_str_handles_multiple_secrets() {
        let log_line = "cred=sk_live_abc, token=tok_xyz, other=fine";
        let cleaned = redact_in_str(log_line, &["sk_live_abc", "tok_xyz"]);
        assert_eq!(cleaned, "cred=[REDACTED], token=[REDACTED], other=fine");
    }
}
