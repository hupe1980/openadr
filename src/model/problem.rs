//! RFC 9457 / Zalando-style problem objects.
//!
//! The specification requires a `problem` body on every 4xx and 5xx response.

use crate::std_shim::{String, ToString, format};
use serde::{Deserialize, Serialize};

/// Base URI for problem types this implementation defines.
///
/// A domain this project publishes, because RFC 9457 §3.1.1 asks that dereferencing a `type` URI
/// produce documentation of that type — and a URI minted under somebody else's domain documents
/// nothing and is theirs to redirect. Every slug below resolves; `cargo xtask check-problems`
/// fails if one stops.
pub const PROBLEM_TYPE_BASE: &str = "https://hupe1980.github.io/openadr/problems/";

/// Every problem type slug this implementation can mint.
///
/// The registry `PROBLEM_TYPE_BASE` points at documents exactly these, and `cargo xtask
/// check-problems` fails if the two lists diverge — a slug renamed in code without a matching
/// redirect leaves a `type` URI that resolves to nothing, which is the whole reason to publish them.
pub const PROBLEM_TYPES: &[&str] = &[
    // `ApiError::slug`.
    "bad-request",
    "invalid-payload",
    "dangling-reference",
    "unsupported-media-type",
    "payload-too-large",
    "unauthorized",
    "missing-scope",
    "forbidden",
    "not-found",
    "no-such-route",
    "conflict",
    "not-implemented",
    "internal-server-error",
    "storage-unavailable",
    "unavailable",
    // Produced by a middleware rather than a handler; see `layer_problem`.
    "method-not-allowed",
    "timeout",
    "error",
    // Synthesised by the client for a peer that sent an error with no problem body.
    "unexpected",
];

/// A machine-readable error body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Problem {
    /// Absolute URI identifying the problem type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// Short, human-readable summary; stable for a given type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The HTTP status code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Human-readable explanation specific to this occurrence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// URI or token identifying this specific occurrence.
    ///
    /// A VTN built on this crate fills it with the request id, which is also the `x-request-id`
    /// header on the same response — the field is only worth having if the two match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

impl Problem {
    /// A problem with a status, a title and a dereferenceable type URI built from `slug`.
    pub fn new(status: u16, slug: &str, title: impl Into<String>) -> Self {
        Self {
            r#type: Some(format!("{PROBLEM_TYPE_BASE}{slug}")),
            title: Some(title.into()),
            status: Some(status),
            detail: None,
            instance: None,
        }
    }

    /// Attach an occurrence-specific explanation.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Attach the request id.
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// The status, defaulting to 500 when absent.
    pub fn status_or_500(&self) -> u16 {
        self.status.unwrap_or(500)
    }
}

impl core::fmt::Display for Problem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let title = self.title.clone().unwrap_or_else(|| "Problem".to_string());
        match &self.detail {
            Some(d) => write!(f, "{} ({}): {d}", title, self.status_or_500()),
            None => write!(f, "{} ({})", title, self.status_or_500()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problem_serialises_only_what_is_set() {
        let p = Problem::new(404, "not-found", "Not Found").with_detail("no such event");
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains(r#""status":404"#));
        assert!(json.contains(r#""title":"Not Found""#));
        assert!(!json.contains("instance"));
    }

    #[test]
    fn problem_type_is_dereferenceable() {
        let p = Problem::new(400, "bad-request", "Bad Request");
        assert_eq!(
            p.r#type.as_deref(),
            Some("https://hupe1980.github.io/openadr/problems/bad-request")
        );
    }
}
