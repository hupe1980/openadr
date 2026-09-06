//! The webhook signature scheme — both ends of it.
//!
//! The Definitions leave payload signing as a guideline and say only "HMAC, in a request header"
//! `[Def §Webhooks, Additional guidelines]`. That is enough for a receiver to tell a genuine
//! notification from a forged one, and not enough to tell it from a *replayed* one: an HMAC over
//! the body alone is valid for ever, so anyone who captures one delivery can post it back at any
//! time and the receiver cannot tell. A dispatch instruction that can be replayed is a dispatch
//! instruction, and "curtail to 0 kW" replayed on a still evening is a real outage.
//!
//! So the signed material is `v1.<unix seconds>.<body>` and the instant travels in its own header,
//! which is the scheme Stripe, GitHub and the Standard Webhooks specification converged on. A
//! receiver checks the signature *and* that the instant is recent; neither alone is sufficient.
//!
//! This module is the whole scheme, and it is deliberately not under [`vtn`](crate::vtn): the
//! sender is a VTN, the receiver is not, and a subscriber should not have to compile an HTTP
//! server to check a signature. It is `no_std` + alloc, takes its time as an argument rather than
//! reading a clock, and allocates one string.
//!
//! ```
//! use openadr::model::Timestamp;
//! use openadr::webhook::{self, Signature, SignatureError};
//!
//! # fn main() -> Result<(), SignatureError> {
//! # let sent_at: Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
//! # let signature_header = webhook::sign(b"shared-secret", sent_at, b"{}");
//! # let timestamp_header = sent_at.as_second().to_string();
//! # let body = b"{}";
//! # let now: Timestamp = "2026-01-01T00:00:05Z".parse().unwrap();
//! // In the handler for your callback URL, over the raw bytes — not a re-serialised body.
//! let sent_at = webhook::parse_timestamp(&timestamp_header).ok_or(SignatureError::Malformed)?;
//! Signature::parse(&signature_header)?
//!     .verify(b"shared-secret", body, sent_at, now, webhook::DEFAULT_TOLERANCE)?;
//! # Ok(())
//! # }
//! ```

use crate::std_shim::{String, ToString};
use core::fmt;

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::model::Timestamp;

/// Header carrying the signature, as `v1=<hex>`.
pub const SIGNATURE_HEADER: &str = "x-openadr-signature";
/// Header carrying the instant the signature covers, as Unix seconds.
pub const TIMESTAMP_HEADER: &str = "x-openadr-timestamp";
/// Header carrying the delivery attempt number, so a receiver can recognise a retry.
pub const ATTEMPT_HEADER: &str = "x-openadr-attempt";

/// The only scheme version defined, and the prefix of every signature.
pub const VERSION: &str = "v1";

/// How far a delivery's timestamp may be from the receiver's clock, in seconds.
///
/// Five minutes: wide enough for the clock difference between two machines that are not
/// synchronised carefully, narrow enough that a captured delivery is worthless by the time anyone
/// has decided to replay it.
pub const DEFAULT_TOLERANCE: i64 = 300;

/// Why a signature was not accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    /// The header was not `v1=<hex>`.
    #[error("malformed signature header: expected `{VERSION}=<hex>`")]
    Malformed,
    /// The scheme version is not one this build knows.
    #[error("unknown signature scheme {version:?}")]
    UnknownVersion {
        /// The version the header named.
        version: String,
    },
    /// The signature does not match the body under this key.
    #[error("signature does not match")]
    Mismatch,
    /// The delivery is older, or newer, than the tolerance allows.
    #[error("delivery timestamp is {drift}s from now, outside the {tolerance}s tolerance")]
    Stale {
        /// Seconds between the delivery's timestamp and the receiver's clock.
        drift: i64,
        /// The tolerance that was applied.
        tolerance: i64,
    },
}

/// The material a signature covers: `v1.<unix seconds>.<body>`.
///
/// The version is inside the signed material as well as in the header, so a receiver that one day
/// accepts two schemes cannot be talked into verifying `v2` material as `v1`.
fn mac(key: &[u8], at: Timestamp, body: &[u8]) -> Hmac<Sha256> {
    // `new_from_slice` on HMAC accepts a key of any length — it is defined for all of them — so
    // this cannot fail and there is nothing for a caller to handle.
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(VERSION.as_bytes());
    mac.update(b".");
    mac.update(at.as_second().to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

/// Sign a payload, returning the value for [`SIGNATURE_HEADER`].
///
/// `at` must be sent alongside, in [`TIMESTAMP_HEADER`], or the receiver cannot reconstruct the
/// signed material.
pub fn sign(key: &[u8], at: Timestamp, body: &[u8]) -> String {
    let bytes = mac(key, at, body).finalize().into_bytes();
    let mut out = String::with_capacity(VERSION.len() + 1 + bytes.len() * 2);
    out.push_str(VERSION);
    out.push('=');
    for byte in &bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// A parsed [`SIGNATURE_HEADER`] value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    digest: [u8; 32],
}

impl Signature {
    /// Parse a `v1=<hex>` header value.
    pub fn parse(header: &str) -> Result<Self, SignatureError> {
        let (version, hex) = header
            .trim()
            .split_once('=')
            .ok_or(SignatureError::Malformed)?;
        if version != VERSION {
            return Err(SignatureError::UnknownVersion {
                version: version.into(),
            });
        }
        if hex.len() != 64 {
            return Err(SignatureError::Malformed);
        }
        let mut digest = [0u8; 32];
        let (pairs, _) = hex.as_bytes().as_chunks::<2>();
        for (slot, pair) in digest.iter_mut().zip(pairs) {
            let hi = (pair[0] as char)
                .to_digit(16)
                .ok_or(SignatureError::Malformed)?;
            let lo = (pair[1] as char)
                .to_digit(16)
                .ok_or(SignatureError::Malformed)?;
            *slot = (hi * 16 + lo) as u8;
        }
        Ok(Self { digest })
    }

    /// Check the signature and the delivery's age.
    ///
    /// Both, in that order: a valid signature on an hour-old delivery is a replay, and a fresh
    /// timestamp on an unsigned body is a forgery. `at` is the instant from
    /// [`TIMESTAMP_HEADER`]; `now` is the receiver's clock.
    ///
    /// The comparison is constant-time — `hmac`'s own — because a receiver that leaks how much of a
    /// digest matched leaks the digest.
    pub fn verify(
        &self,
        key: &[u8],
        body: &[u8],
        at: Timestamp,
        now: Timestamp,
        tolerance: i64,
    ) -> Result<(), SignatureError> {
        mac(key, at, body)
            .verify_slice(&self.digest)
            .map_err(|_| SignatureError::Mismatch)?;
        let drift = now.as_second() - at.as_second();
        if drift.abs() > tolerance {
            return Err(SignatureError::Stale { drift, tolerance });
        }
        Ok(())
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(VERSION)?;
        f.write_str("=")?;
        for byte in &self.digest {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Read the timestamp header a delivery carried.
pub fn parse_timestamp(header: &str) -> Option<Timestamp> {
    Timestamp::from_second(header.trim().parse().ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    const KEY: &[u8] = b"shared-secret";
    const SENT: &str = "2026-01-01T00:00:00Z";

    #[test]
    fn a_signature_round_trips() {
        let header = sign(KEY, ts(SENT), b"{}");
        assert!(header.starts_with("v1="));
        Signature::parse(&header)
            .unwrap()
            .verify(
                KEY,
                b"{}",
                ts(SENT),
                ts("2026-01-01T00:00:10Z"),
                DEFAULT_TOLERANCE,
            )
            .unwrap();
    }

    #[test]
    fn the_timestamp_is_part_of_what_is_signed() {
        // The whole point: the same body signed at a different instant is a different signature,
        // so a captured delivery cannot be re-dated to look fresh.
        assert_ne!(
            sign(KEY, ts(SENT), b"{}"),
            sign(KEY, ts("2026-01-01T00:00:01Z"), b"{}")
        );
    }

    #[test]
    fn a_replay_is_refused_even_though_the_signature_is_genuine() {
        let header = sign(KEY, ts(SENT), b"{}");
        let err = Signature::parse(&header)
            .unwrap()
            .verify(
                KEY,
                b"{}",
                ts(SENT),
                ts("2026-01-01T01:00:00Z"),
                DEFAULT_TOLERANCE,
            )
            .unwrap_err();
        assert!(matches!(err, SignatureError::Stale { .. }));
    }

    #[test]
    fn a_clock_ahead_of_the_sender_is_tolerated_symmetrically() {
        // Neither end is authoritative, so drift in either direction inside the tolerance passes.
        let header = sign(KEY, ts("2026-01-01T00:01:00Z"), b"{}");
        Signature::parse(&header)
            .unwrap()
            .verify(
                KEY,
                b"{}",
                ts("2026-01-01T00:01:00Z"),
                ts(SENT),
                DEFAULT_TOLERANCE,
            )
            .unwrap();
    }

    #[test]
    fn a_tampered_body_or_a_wrong_key_is_refused() {
        let header = sign(KEY, ts(SENT), b"{\"a\":1}");
        let signature = Signature::parse(&header).unwrap();
        let now = ts(SENT);
        assert_eq!(
            signature.verify(KEY, b"{\"a\":2}", ts(SENT), now, DEFAULT_TOLERANCE),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            signature.verify(b"other", b"{\"a\":1}", ts(SENT), now, DEFAULT_TOLERANCE),
            Err(SignatureError::Mismatch)
        );
        // And a re-dated delivery fails on the signature, not merely on the age.
        assert_eq!(
            signature.verify(
                KEY,
                b"{\"a\":1}",
                ts("2026-01-01T00:00:01Z"),
                ts("2026-01-01T00:00:01Z"),
                DEFAULT_TOLERANCE
            ),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn malformed_headers_are_refused_rather_than_guessed_at() {
        assert_eq!(Signature::parse("deadbeef"), Err(SignatureError::Malformed));
        assert_eq!(Signature::parse("v1=short"), Err(SignatureError::Malformed));
        assert_eq!(
            Signature::parse(&format!("v1={}", "z".repeat(64))),
            Err(SignatureError::Malformed)
        );
        assert!(matches!(
            Signature::parse(&format!("v2={}", "a".repeat(64))),
            Err(SignatureError::UnknownVersion { .. })
        ));
    }

    #[test]
    fn a_timestamp_header_round_trips() {
        assert_eq!(parse_timestamp("1767225600"), Some(ts(SENT)));
        assert_eq!(parse_timestamp(" 1767225600 "), Some(ts(SENT)));
        assert_eq!(parse_timestamp("not-a-number"), None);
    }
}
