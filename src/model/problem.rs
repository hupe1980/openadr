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

/// A problem type this implementation can mint, and what it means.
///
/// The words a client reads at a terminal (`openadr explain <slug>`) and the words on the published
/// registry page are one text: `cargo xtask check-problems` fails if a row of that page differs
/// from this table, or if either has a slug the other lacks — a slug renamed in code without a
/// matching redirect leaves a `type` URI that resolves to nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProblemType {
    /// The last path segment of the `type` URI, under [`PROBLEM_TYPE_BASE`].
    pub slug: &'static str,
    /// The HTTP status it is minted with; `None` where it varies.
    pub status: Option<u16>,
    /// What it means and what a client should do, in one paragraph of Markdown.
    pub meaning: &'static str,
}

/// Every problem type this implementation can mint.
pub const PROBLEMS: &[ProblemType] = &[
    ProblemType {
        slug: "bad-request",
        status: Some(400),
        meaning: "The request is malformed in a way the more specific types below do not cover. Not retryable unchanged.",
    },
    ProblemType {
        slug: "invalid-payload",
        status: Some(400),
        meaning: "The body parsed as JSON but failed validation — a field out of range, a missing required property, a pattern that does not match. `detail` names the field.",
    },
    ProblemType {
        slug: "dangling-reference",
        status: Some(400),
        meaning: "The body refers to an object that does not exist, typically a `programID` on an event. The URL was fine; the payload was not.",
    },
    ProblemType {
        slug: "unsupported-media-type",
        status: Some(415),
        meaning: "The body was labelled something other than `application/json` or an RFC 6839 `+json` type.",
    },
    ProblemType {
        slug: "payload-too-large",
        status: Some(413),
        meaning: "The body exceeds the VTN's configured limit.",
    },
    ProblemType {
        slug: "unauthorized",
        status: Some(401),
        meaning: "No credential, or one that did not verify. The response carries `WWW-Authenticate`. Get a new token and retry once.",
    },
    ProblemType {
        slug: "missing-scope",
        status: Some(403),
        meaning: "The token verified but does not carry the scope this endpoint requires. Retrying will not help; the client's registration needs changing.",
    },
    ProblemType {
        slug: "forbidden",
        status: Some(403),
        meaning: "The credential is valid and scoped but may not act on this object — a VEN writing another VEN's report, for instance.",
    },
    ProblemType {
        slug: "not-found",
        status: Some(404),
        meaning: "No such object. Also returned for an object that exists but this client may not see, so that a `403` never confirms existence.",
    },
    ProblemType {
        slug: "no-such-route",
        status: Some(404),
        meaning: "No endpoint at that path. Usually a base-path or version mistake rather than a missing object.",
    },
    ProblemType {
        slug: "conflict",
        status: Some(409),
        meaning: "The write lost a race, or would break uniqueness. Re-read and retry.",
    },
    ProblemType {
        slug: "not-implemented",
        status: Some(501),
        meaning: "A specified feature this deployment does not provide — the MQTT topic endpoints on a VTN with no broker, for example.",
    },
    ProblemType {
        slug: "internal-server-error",
        status: Some(500),
        meaning: "A defect. `instance` is the request id; quote it.",
    },
    ProblemType {
        slug: "storage-unavailable",
        status: Some(500),
        meaning: "The database could not be reached. Transient; retry with backoff.",
    },
    ProblemType {
        slug: "unavailable",
        status: Some(503),
        meaning: "The VTN is up but declining work, typically while shutting down. Retry with backoff.",
    },
    ProblemType {
        slug: "method-not-allowed",
        status: Some(405),
        meaning: "The path exists; the method is not defined for it.",
    },
    ProblemType {
        slug: "timeout",
        status: Some(504),
        meaning: "The VTN gave up on the request. Retry with backoff.",
    },
    ProblemType {
        slug: "error",
        status: None,
        meaning: "A status produced by a middleware with no more specific mapping.",
    },
    ProblemType {
        slug: "unexpected",
        status: None,
        meaning: "Minted by **this crate's client**, not by a VTN: the peer returned an error with no problem body, so one was synthesised from the status. It says nothing about the peer beyond its status code.",
    },
];

/// The entry for `slug`, if this implementation mints it.
pub fn problem_type(slug: &str) -> Option<&'static ProblemType> {
    PROBLEMS.iter().find(|p| p.slug == slug)
}

/// The entry a `type` URI names, if it is one of this implementation's.
///
/// A peer's own problem types — any URI not under [`PROBLEM_TYPE_BASE`] — are not ours to explain.
pub fn slug_of(type_uri: &str) -> Option<&'static ProblemType> {
    type_uri
        .strip_prefix(PROBLEM_TYPE_BASE)
        .and_then(problem_type)
}

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
