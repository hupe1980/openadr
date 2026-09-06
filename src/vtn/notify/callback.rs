//! What makes a webhook callback URL acceptable.
//!
//! The specification's webhook chapter is a threat model, and the threat is the VTN itself: a
//! subscriber names a URL, and the VTN then makes requests to it from inside the operator's network
//! `[Def §Webhooks]`.
//!
//! The rule has two halves, and both matter:
//!
//! * **The URL as written** is checked when the subscription is created and again before every
//!   delivery. It catches the scheme and any literal address, and it costs no DNS, so the API's
//!   latency does not depend on a resolver.
//! * **What the name resolves to** is checked *inside the connector*, by
//!   [`WebhookNotifier`](super::WebhookNotifier)'s resolver, on the answer the socket is actually
//!   opened against. Resolving separately and then letting the HTTP client resolve again is a check
//!   with a race in it: the second answer is the one that matters and nothing was looking at it.
//!
//! [`is_private`] is the rule itself, and both halves call it.

use std::net::IpAddr;

/// Why a callback URL was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallbackRejected {
    /// Not a URL at all.
    #[error("callbackUrl is not a URL: {0}")]
    Malformed(String),
    /// Not HTTPS.
    #[error("callbackUrl must use https")]
    NotHttps,
    /// No host to connect to.
    #[error("callbackUrl has no host")]
    NoHost,
    /// The host is, or resolves to, an address the VTN must not reach out to.
    #[error("callbackUrl host {host} is a loopback, private or link-local address")]
    PrivateAddress {
        /// The offending host or address.
        host: String,
    },
    /// The host could not be resolved, or resolved to nothing usable.
    #[error("callbackUrl host {host} could not be resolved: {detail}")]
    Unresolvable {
        /// The host that failed.
        host: String,
        /// Why.
        detail: String,
    },
}

/// Which callback URLs a VTN will accept and deliver to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackPolicy {
    /// Require `https`. The specification says the VTN **MUST** validate this.
    pub require_https: bool,
    /// Permit loopback, private and link-local destinations.
    ///
    /// Off in every deployment. On, it re-enables exactly the server-side request forgery the rest
    /// of this module exists to prevent — which is why it is useful in tests and nowhere else.
    pub allow_private_addresses: bool,
}

impl Default for CallbackPolicy {
    fn default() -> Self {
        Self {
            require_https: true,
            allow_private_addresses: false,
        }
    }
}

impl CallbackPolicy {
    /// A policy that permits loopback callbacks, for tests and local development.
    pub fn permissive() -> Self {
        Self {
            require_https: false,
            allow_private_addresses: true,
        }
    }

    /// Check the URL as written, without touching DNS.
    ///
    /// Used when a subscription is created: it catches the obvious cases immediately, and it does
    /// not make the API's latency depend on a resolver.
    pub fn check_literal(&self, callback_url: &str) -> Result<url::Url, CallbackRejected> {
        let url = url::Url::parse(callback_url)
            .map_err(|e| CallbackRejected::Malformed(e.to_string()))?;
        if self.require_https && url.scheme() != "https" {
            return Err(CallbackRejected::NotHttps);
        }
        // `url.host()` is typed. `host_str()` would hand back `[::1]` for an IPv6 literal, brackets
        // and all, which does not parse as an address — so a loopback callback would slip through.
        let host = url.host().ok_or(CallbackRejected::NoHost)?;
        if self.allow_private_addresses {
            return Ok(url);
        }
        let private = match &host {
            url::Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
            url::Host::Ipv4(v4) => is_private(IpAddr::V4(*v4)),
            url::Host::Ipv6(v6) => is_private(IpAddr::V6(*v6)),
        };
        if private {
            return Err(CallbackRejected::PrivateAddress {
                host: host.to_string(),
            });
        }
        Ok(url)
    }
}

/// Whether an address is one a webhook must never be sent to.
///
/// The list is "anything not reachable from the public internet", not "anything RFC 1918". A cloud
/// metadata service is the target that matters and it has lived at a link-local address, a
/// carrier-grade-NAT address and a unique-local one depending on the provider.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                // Carrier-grade NAT, 100.64.0.0/10 — where a cloud metadata service often lives.
                || (a == 100 && (64..128).contains(&b))
                // IETF protocol assignments, 192.0.0.0/24.
                || (a == 192 && b == 0 && v4.octets()[2] == 0)
                // Benchmarking, 198.18.0.0/15.
                || (a == 198 && (18..20).contains(&b))
                // Reserved, 240.0.0.0/4.
                || a >= 240
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique local, fc00::/7.
                || (segments[0] & 0xfe00) == 0xfc00
                // Link-local, fe80::/10.
                || (segments[0] & 0xffc0) == 0xfe80
                // Documentation, 2001:db8::/32.
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                // NAT64, 64:ff9b::/96 — an IPv4 destination wearing an IPv6 address.
                || (segments[0] == 0x0064
                    && segments[1] == 0xff9b
                    && is_private(
                        std::net::Ipv4Addr::new(
                            (segments[6] >> 8) as u8,
                            segments[6] as u8,
                            (segments[7] >> 8) as u8,
                            segments[7] as u8,
                        )
                        .into(),
                    ))
                // An IPv4 address in an IPv6 wrapper is still that address, in either form.
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private(v4.into()))
                || (segments[..6].iter().all(|s| *s == 0)
                    && v6.to_ipv4().is_some_and(|v4| is_private(v4.into())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_carrier_grade_addresses_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "100.100.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "198.18.0.1",
            "192.0.0.1",
            "64:ff9b::a00:1",
            "2001:db8::1",
            "ff02::1",
        ] {
            assert!(is_private(ip.parse().unwrap()), "{ip} should be refused");
        }
        for ip in ["8.8.8.8", "93.184.216.34", "2001:4860:4860::8888"] {
            assert!(!is_private(ip.parse().unwrap()), "{ip} should be allowed");
        }
    }

    #[test]
    fn the_default_policy_requires_https_and_a_public_host() {
        let policy = CallbackPolicy::default();
        assert_eq!(
            policy.check_literal("http://example.com/hook"),
            Err(CallbackRejected::NotHttps)
        );
        for url in [
            "https://localhost/hook",
            "https://127.0.0.1/hook",
            "https://10.0.0.1/hook",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/hook",
        ] {
            assert!(
                matches!(
                    policy.check_literal(url),
                    Err(CallbackRejected::PrivateAddress { .. })
                ),
                "{url} should be refused"
            );
        }
        assert!(policy.check_literal("https://example.com/hook").is_ok());
    }

    #[test]
    fn an_ipv6_literal_is_recognised_despite_its_brackets() {
        // Regression: `host_str()` returns `[::1]`, which does not parse as an address, so a
        // string-based check silently accepts every IPv6 loopback and unique-local callback.
        let policy = CallbackPolicy::default();
        for url in [
            "https://[::1]/hook",
            "https://[fd00::1]/hook",
            "https://[fe80::1]/hook",
            "https://[::ffff:127.0.0.1]/hook",
        ] {
            assert!(
                matches!(
                    policy.check_literal(url),
                    Err(CallbackRejected::PrivateAddress { .. })
                ),
                "{url} should be refused"
            );
        }
        assert!(
            policy
                .check_literal("https://[2001:4860:4860::8888]/hook")
                .is_ok()
        );
    }

    #[test]
    fn the_permissive_policy_is_for_tests_only() {
        let policy = CallbackPolicy::permissive();
        assert!(policy.check_literal("http://127.0.0.1:8080/hook").is_ok());
    }

    #[test]
    fn a_malformed_url_is_refused_before_anything_else() {
        assert!(matches!(
            CallbackPolicy::default().check_literal("not a url"),
            Err(CallbackRejected::Malformed(_))
        ));
    }
}
