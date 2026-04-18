//! SSRF (Server-Side Request Forgery) defence for push delivery
//! (ADR-011 §R3).
//!
//! Push delivery is an outbound HTTP request whose destination is
//! client-controlled — the `url` field of a push config. Without
//! these guards, a malicious or misconfigured client could register
//! an internal URL and turn the framework into a proxy into the
//! deployment's private network.
//!
//! # Coverage
//!
//! - **Destination-IP blocklist**: loopback, link-local, RFC1918
//!   private, IPv6 unique-local, multicast, broadcast, unspecified,
//!   AWS / GCP metadata service addresses.
//! - **DNS-rebinding defence**: resolve the hostname once per
//!   attempt, validate the resolved IPs against the blocklist
//!   immediately. Connect to a specific resolved IP so a
//!   subsequent DNS answer cannot swap the destination between
//!   validation and connect.
//! - **Scheme enforcement**: production allows only `https://`.
//!   `allow_insecure_push_urls` (dev-only flag) disables both the
//!   private-IP blocklist and the https-only requirement so
//!   localhost testing works.
//! - **Outbound allowlist hook**: an optional
//!   `Fn(&url::Url) -> Result<(), String>` consulted before DNS.
//!   Deployments with a strict destination allowlist can reject
//!   everything outside a known vendor list.
//!
//! # Not here (by design)
//!
//! The actual POST with a validated destination IP lives in the
//! delivery worker (`push/delivery.rs`). This module is the
//! pure-logic validator: given a parsed URL and a set of resolved
//! IPs, decide whether to proceed.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use url::Url;

/// Operator-configurable outbound URL validator.
///
/// Called before any DNS resolution. Returning `Err(reason)` rejects
/// the delivery with error class `SSRFBlocked`; the reason string is
/// recorded in structured logs (never on the wire) so operators can
/// see why a specific URL was refused. Deployments without a
/// validator leave this as `None`, and only the IP blocklist applies.
pub type OutboundUrlValidator = Arc<dyn Fn(&Url) -> Result<(), String> + Send + Sync>;

/// Decision on whether a push delivery is allowed to proceed to
/// the network.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SsrfDecision {
    /// Allowed. `resolved_ip` is the IP the worker should connect
    /// to directly (bypassing re-resolution on the reqwest side so
    /// a DNS rebind cannot swap destinations).
    Allow { resolved_ip: IpAddr },
    /// Rejected. `reason_token` is a fixed identifier (not
    /// operator-visible reason text — that goes to logs).
    Block(SsrfBlockReason),
}

/// Classified reason for an SSRF block. Maps to
/// [`crate::push::DeliveryErrorClass::SSRFBlocked`] when the
/// delivery records the outcome. Enum-only so no free-text path
/// can leak URLs, credentials, or hostnames into failure records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SsrfBlockReason {
    /// Resolved IP is in a private / loopback / link-local / metadata
    /// address range.
    PrivateIp,
    /// `http://` URL rejected in production mode.
    InsecureScheme,
    /// Operator allowlist validator rejected the URL.
    ValidatorDenied,
    /// URL could not be parsed or has no host.
    InvalidUrl,
    /// DNS resolution returned no addresses, or all were blocked.
    DnsResolutionFailed,
}

/// Validate a destination IP against the production SSRF blocklist.
///
/// Returns `true` if the address is in a range the framework
/// refuses to connect to (regardless of URL or scheme). Public so
/// the delivery worker can check each DNS answer individually.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

fn is_blocked_ipv4(v4: Ipv4Addr) -> bool {
    let octets = v4.octets();
    // Unspecified: 0.0.0.0
    if v4.is_unspecified() {
        return true;
    }
    // Broadcast: 255.255.255.255
    if v4.is_broadcast() {
        return true;
    }
    // Loopback: 127.0.0.0/8
    if v4.is_loopback() {
        return true;
    }
    // Link-local: 169.254.0.0/16 (includes AWS/GCP metadata
    // service addresses 169.254.169.254).
    if v4.is_link_local() {
        return true;
    }
    // Private: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16.
    if v4.is_private() {
        return true;
    }
    // Multicast: 224.0.0.0/4.
    if v4.is_multicast() {
        return true;
    }
    // Carrier-grade NAT (shared address space): 100.64.0.0/10 —
    // typically not reachable on the public internet but used by
    // some private deployments. Treat as private.
    if octets[0] == 100 && (octets[1] & 0xc0) == 0x40 {
        return true;
    }
    false
}

fn is_blocked_ipv6(v6: Ipv6Addr) -> bool {
    // Unspecified: ::
    if v6.is_unspecified() {
        return true;
    }
    // Loopback: ::1
    if v6.is_loopback() {
        return true;
    }
    // Multicast: ff00::/8
    if v6.is_multicast() {
        return true;
    }
    let segments = v6.segments();
    // Link-local: fe80::/10
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // Unique local: fc00::/7
    if (segments[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // IPv4-mapped IPv6: ::ffff:0:0/96 → validate the embedded v4.
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_blocked_ipv4(v4);
    }
    // AWS IPv6 metadata service: fd00:ec2::254 (EC2's unique-local
    // metadata address) is already caught by the fc00::/7 check,
    // but documenting explicitly.
    false
}

/// Validate a URL's scheme against the SSRF policy.
///
/// - `https://` is always allowed.
/// - `http://` is allowed only when `allow_insecure` is true
///   (dev-only `allow_insecure_push_urls` flag).
/// - All other schemes (`file://`, `gopher://`, etc.) are rejected.
pub fn validate_scheme(url: &Url, allow_insecure: bool) -> Result<(), SsrfBlockReason> {
    match url.scheme() {
        "https" => Ok(()),
        "http" if allow_insecure => Ok(()),
        _ => Err(SsrfBlockReason::InsecureScheme),
    }
}

/// Apply the SSRF decision to a parsed URL with a pre-resolved list
/// of destination IPs.
///
/// Callers: the delivery worker resolves the hostname (via a
/// single `tokio::net::lookup_host` or an injected DNS resolver for
/// testability), passes the resolved IPs here, and receives either
/// `Allow { resolved_ip }` — which the worker then uses to open a
/// connection directly to that IP — or `Block(reason)`.
///
/// The allowlist validator, if present, runs BEFORE DNS; it sees
/// the URL pre-resolution so deployments can reject by hostname
/// regardless of how it resolves.
pub fn decide(
    url: &Url,
    resolved_ips: &[IpAddr],
    allow_insecure: bool,
    outbound_validator: Option<&OutboundUrlValidator>,
) -> SsrfDecision {
    // URL shape: must have a host.
    if url.host_str().is_none() {
        return SsrfDecision::Block(SsrfBlockReason::InvalidUrl);
    }
    if let Err(reason) = validate_scheme(url, allow_insecure) {
        return SsrfDecision::Block(reason);
    }
    if let Some(validator) = outbound_validator {
        if validator(url).is_err() {
            return SsrfDecision::Block(SsrfBlockReason::ValidatorDenied);
        }
    }
    // Pick the first IP that passes the blocklist. When
    // `allow_insecure` is true we also bypass the blocklist so
    // `http://127.0.0.1` works in local tests.
    for ip in resolved_ips {
        if allow_insecure {
            return SsrfDecision::Allow { resolved_ip: *ip };
        }
        if !is_blocked_ip(*ip) {
            return SsrfDecision::Allow { resolved_ip: *ip };
        }
    }
    if resolved_ips.is_empty() {
        SsrfDecision::Block(SsrfBlockReason::DnsResolutionFailed)
    } else {
        SsrfDecision::Block(SsrfBlockReason::PrivateIp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipv4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn blocks_ipv4_loopback_and_localhost() {
        assert!(is_blocked_ip(ipv4(127, 0, 0, 1)));
        assert!(is_blocked_ip(ipv4(127, 255, 255, 254)));
    }

    #[test]
    fn blocks_ipv4_rfc1918_private() {
        assert!(is_blocked_ip(ipv4(10, 0, 0, 5)));
        assert!(is_blocked_ip(ipv4(10, 255, 255, 255)));
        assert!(is_blocked_ip(ipv4(172, 16, 0, 1)));
        assert!(is_blocked_ip(ipv4(172, 31, 255, 255)));
        assert!(is_blocked_ip(ipv4(192, 168, 1, 1)));
    }

    #[test]
    fn blocks_ipv4_link_local_and_metadata() {
        assert!(is_blocked_ip(ipv4(169, 254, 0, 1)));
        // AWS metadata service — the canonical SSRF target.
        assert!(is_blocked_ip(ipv4(169, 254, 169, 254)));
    }

    #[test]
    fn blocks_ipv4_unspecified_broadcast_multicast() {
        assert!(is_blocked_ip(ipv4(0, 0, 0, 0)));
        assert!(is_blocked_ip(ipv4(255, 255, 255, 255)));
        assert!(is_blocked_ip(ipv4(224, 0, 0, 1))); // multicast
        assert!(is_blocked_ip(ipv4(239, 255, 255, 255)));
    }

    #[test]
    fn blocks_ipv4_carrier_grade_nat() {
        // 100.64.0.0/10
        assert!(is_blocked_ip(ipv4(100, 64, 0, 1)));
        assert!(is_blocked_ip(ipv4(100, 127, 255, 255)));
        // Outside CGN → allowed
        assert!(!is_blocked_ip(ipv4(100, 63, 0, 0)));
        assert!(!is_blocked_ip(ipv4(100, 128, 0, 0)));
    }

    #[test]
    fn allows_ipv4_public() {
        assert!(!is_blocked_ip(ipv4(8, 8, 8, 8)));
        assert!(!is_blocked_ip(ipv4(1, 1, 1, 1)));
        assert!(!is_blocked_ip(ipv4(203, 0, 113, 10))); // TEST-NET-3
    }

    #[test]
    fn blocks_ipv6_loopback_and_link_local() {
        assert!(is_blocked_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_blocked_ip(IpAddr::V6("fe80::1234".parse().unwrap())));
    }

    #[test]
    fn blocks_ipv6_unique_local_and_multicast() {
        assert!(is_blocked_ip(IpAddr::V6("fc00::1".parse().unwrap())));
        assert!(is_blocked_ip(IpAddr::V6("fd00:ec2::254".parse().unwrap())));
        assert!(is_blocked_ip(IpAddr::V6("ff02::1".parse().unwrap())));
    }

    #[test]
    fn blocks_ipv6_mapped_private() {
        // ::ffff:127.0.0.1
        assert!(is_blocked_ip(IpAddr::V6(
            "::ffff:7f00:0001".parse().unwrap()
        )));
        assert!(is_blocked_ip(IpAddr::V6("::ffff:10.0.0.5".parse().unwrap())));
    }

    #[test]
    fn allows_ipv6_public() {
        assert!(!is_blocked_ip(IpAddr::V6(
            "2001:db8::1".parse().unwrap()
        ))); // doc range, fine for testing intent
        assert!(!is_blocked_ip(IpAddr::V6("2606:4700:4700::1111".parse().unwrap())));
    }

    #[test]
    fn scheme_https_always_allowed() {
        let url = Url::parse("https://webhook.example.com/deliver").unwrap();
        assert!(validate_scheme(&url, false).is_ok());
        assert!(validate_scheme(&url, true).is_ok());
    }

    #[test]
    fn scheme_http_rejected_in_production() {
        let url = Url::parse("http://webhook.example.com/deliver").unwrap();
        assert!(matches!(
            validate_scheme(&url, false),
            Err(SsrfBlockReason::InsecureScheme)
        ));
    }

    #[test]
    fn scheme_http_allowed_in_dev_mode() {
        let url = Url::parse("http://localhost:8080/").unwrap();
        assert!(validate_scheme(&url, true).is_ok());
    }

    #[test]
    fn scheme_other_always_rejected() {
        for s in ["file:///etc/passwd", "gopher://x", "ftp://x", "ldap://x"] {
            let url = Url::parse(s).unwrap();
            assert!(matches!(
                validate_scheme(&url, true),
                Err(SsrfBlockReason::InsecureScheme)
            ));
        }
    }

    #[test]
    fn decide_allows_public_https() {
        let url = Url::parse("https://webhook.example.com/deliver").unwrap();
        let ips = vec![ipv4(203, 0, 113, 10)];
        assert!(matches!(
            decide(&url, &ips, false, None),
            SsrfDecision::Allow { .. }
        ));
    }

    #[test]
    fn decide_blocks_private_ip() {
        let url = Url::parse("https://metadata.attacker.example/").unwrap();
        let ips = vec![ipv4(169, 254, 169, 254)];
        assert!(matches!(
            decide(&url, &ips, false, None),
            SsrfDecision::Block(SsrfBlockReason::PrivateIp)
        ));
    }

    #[test]
    fn decide_prefers_first_public_ip_when_mixed() {
        // DNS answer with a private IP first, a public IP second.
        // The guard should pick the public one rather than block
        // outright — matches the ADR-011 §R3 "first passing IP" rule.
        let url = Url::parse("https://webhook.example.com/").unwrap();
        let ips = vec![ipv4(10, 0, 0, 5), ipv4(203, 0, 113, 10)];
        match decide(&url, &ips, false, None) {
            SsrfDecision::Allow { resolved_ip } => {
                assert_eq!(resolved_ip, ipv4(203, 0, 113, 10));
            }
            other => panic!("expected Allow on public IP, got {other:?}"),
        }
    }

    #[test]
    fn decide_allows_private_in_insecure_mode() {
        let url = Url::parse("http://localhost:3000/").unwrap();
        let ips = vec![ipv4(127, 0, 0, 1)];
        assert!(matches!(
            decide(&url, &ips, true, None),
            SsrfDecision::Allow { .. }
        ));
    }

    #[test]
    fn decide_empty_dns_is_blocked() {
        let url = Url::parse("https://nothing.example/").unwrap();
        assert!(matches!(
            decide(&url, &[], false, None),
            SsrfDecision::Block(SsrfBlockReason::DnsResolutionFailed)
        ));
    }

    #[test]
    fn decide_respects_outbound_validator() {
        let url = Url::parse("https://evil.attacker.com/").unwrap();
        let validator: OutboundUrlValidator = Arc::new(|u: &Url| {
            if u.host_str() == Some("webhook.example.com") {
                Ok(())
            } else {
                Err(format!("host {} not in allowlist", u.host_str().unwrap_or("")))
            }
        });
        let ips = vec![ipv4(203, 0, 113, 10)];
        assert!(matches!(
            decide(&url, &ips, false, Some(&validator)),
            SsrfDecision::Block(SsrfBlockReason::ValidatorDenied)
        ));

        // And the allowed host goes through.
        let ok_url = Url::parse("https://webhook.example.com/").unwrap();
        assert!(matches!(
            decide(&ok_url, &ips, false, Some(&validator)),
            SsrfDecision::Allow { .. }
        ));
    }

    #[test]
    fn decide_rejects_non_https_in_production() {
        let url = Url::parse("http://webhook.example.com/").unwrap();
        let ips = vec![ipv4(203, 0, 113, 10)];
        assert!(matches!(
            decide(&url, &ips, false, None),
            SsrfDecision::Block(SsrfBlockReason::InsecureScheme)
        ));
    }

    #[test]
    fn decide_rejects_malformed_url() {
        // `data:` URLs have no host; url crate allows parsing.
        let url = Url::parse("data:text/plain,hello").unwrap();
        let ips = vec![ipv4(203, 0, 113, 10)];
        assert!(matches!(
            decide(&url, &ips, false, None),
            SsrfDecision::Block(SsrfBlockReason::InvalidUrl)
        ));
    }
}
