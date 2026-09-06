//! Turning every failure into the specification's `problem` object.
//!
//! `[Def §Problem]`: on 40x and 500 responses a VTN answers with a `problem` object carrying details
//! of the error. Handlers therefore never build responses by hand — they return [`ApiError`], and
//! the single [`IntoResponse`] impl below decides the status, the body, and what gets logged.
//!
//! That covers everything a *handler* produces. The errors a middleware produces — a body over the
//! limit, a request that timed out, a method the router does not have — never reach an `ApiError`,
//! and are rewritten into the same shape by `layer_problem` in
//! [`api`](super::api), outside the layers whose bare answers it has to catch (D-108).

use axum::{
    Json,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::model::Problem;
use crate::schema::PayloadViolation;

use super::auth::{AuthError, Scope};
use super::store::StorageError;

/// Everything that can go wrong while serving a request.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The request body or parameters were malformed.
    #[error("{0}")]
    BadRequest(String),
    /// The body was labelled with a media type this endpoint does not accept.
    ///
    /// Separate from [`ApiError::BadRequest`] because the two are different mistakes with
    /// different fixes: a `415` says "your bytes were never going to be read", a `400` says "they
    /// were read and they were wrong". Answering `400` to a form-encoded body sends the reader
    /// looking for a typo in JSON they did not send.
    #[error("{0}")]
    UnsupportedMediaType(String),
    /// The body exceeded [`VtnConfig::max_body_bytes`](crate::vtn::VtnConfig::max_body_bytes).
    ///
    /// Reachable two ways — the layer refuses an oversized `Content-Length` outright, and the
    /// extractor's read runs off the end of a limited stream when there is none — and both must
    /// answer `413`. They did not while the extractor mapped every read failure to `400` `[D-104]`.
    #[error("{0}")]
    PayloadTooLarge(String),
    /// A payload contradicted its enumeration under a strict policy.
    #[error("payload validation failed")]
    InvalidPayload(Vec<PayloadViolation>),
    /// No credential, or an unusable one.
    #[error(transparent)]
    Unauthorized(#[from] AuthError),
    /// Authenticated, but not permitted.
    #[error("the {0} scope is required")]
    MissingScope(Scope),
    /// Authenticated and scoped, but this object is not the caller's to touch.
    #[error("{0}")]
    Forbidden(String),
    /// The object does not exist, or the caller may not know that it does.
    ///
    /// Object privacy and genuine absence deliberately share this: a `403` on a hidden object would
    /// confirm it exists, which is what targeting conceals.
    #[error("{object_type} {id} not found")]
    NotFound {
        /// The kind of object.
        object_type: crate::model::ObjectType,
        /// The id that was looked up.
        id: crate::model::ObjectId,
    },
    /// No route matched the request path.
    #[error("no such endpoint")]
    NoSuchRoute,
    /// A storage-level failure, which carries its own status.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// Something the VTN does not implement.
    #[error("{0}")]
    NotImplemented(String),
    /// An unexpected internal failure.
    #[error("{0}")]
    Internal(String),
    /// The VTN could not attempt the request, but the request itself was fine.
    ///
    /// A `503`, so a client knows to retry. Distinct from [`ApiError::Internal`], which is a `500`
    /// and means something went wrong rather than something was busy.
    #[error("{0}")]
    Unavailable(String),
}

impl ApiError {
    /// The HTTP status this error maps to.
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) | ApiError::InvalidPayload(_) => StatusCode::BAD_REQUEST,
            ApiError::UnsupportedMediaType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ApiError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ApiError::MissingScope(_) | ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::NotFound { .. } | ApiError::NoSuchRoute => StatusCode::NOT_FOUND,
            ApiError::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Storage(e) => match e {
                StorageError::NotFound { .. } => StatusCode::NOT_FOUND,
                StorageError::Conflict { .. } => StatusCode::CONFLICT,
                // A body that references a non-existent object is a bad request, not a 404: the
                // URL resolved fine, the payload did not.
                StorageError::DanglingReference { .. } => StatusCode::BAD_REQUEST,
                StorageError::Unavailable(_) => StatusCode::INTERNAL_SERVER_ERROR,
            },
        }
    }

    /// The slug used in the problem's `type` URI.
    fn slug(&self) -> &'static str {
        match self {
            ApiError::BadRequest(_) => "bad-request",
            ApiError::UnsupportedMediaType(_) => "unsupported-media-type",
            ApiError::PayloadTooLarge(_) => "payload-too-large",
            ApiError::InvalidPayload(_) => "invalid-payload",
            ApiError::Unauthorized(_) => "unauthorized",
            ApiError::MissingScope(_) => "missing-scope",
            ApiError::Forbidden(_) => "forbidden",
            ApiError::NotFound { .. } => "not-found",
            ApiError::NoSuchRoute => "no-such-route",
            ApiError::NotImplemented(_) => "not-implemented",
            ApiError::Internal(_) => "internal-server-error",
            ApiError::Unavailable(_) => "unavailable",
            ApiError::Storage(e) => match e {
                StorageError::NotFound { .. } => "not-found",
                StorageError::Conflict { .. } => "conflict",
                StorageError::DanglingReference { .. } => "dangling-reference",
                StorageError::Unavailable(_) => "storage-unavailable",
            },
        }
    }

    /// Build the problem body.
    pub fn problem(&self) -> Problem {
        let status = self.status();
        let title = status.canonical_reason().unwrap_or("Error");
        let detail = match self {
            // Internal failures are logged in full but described in general terms: the client
            // cannot act on a database message, and it may reveal internals.
            ApiError::Internal(_) => "an unexpected internal error occurred".to_string(),
            ApiError::InvalidPayload(violations) => violations
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("; "),
            other => other.to_string(),
        };
        Problem::new(status.as_u16(), self.slug(), title).with_detail(detail)
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError::BadRequest(format!("malformed JSON: {e}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        } else {
            tracing::debug!(error = %self, status = %status, "request rejected");
        }

        let mut headers = HeaderMap::new();
        // RFC 6750: a 401 must say how to authenticate.
        if status == StatusCode::UNAUTHORIZED {
            headers.insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"openadr\""),
            );
        }
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );

        (status, headers, Json(self.problem())).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ObjectId, ObjectType};

    #[test]
    fn storage_errors_map_to_the_documented_statuses() {
        let not_found = ApiError::Storage(StorageError::NotFound {
            object_type: ObjectType::Event,
            id: ObjectId::new("e1").unwrap(),
        });
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);

        let conflict = ApiError::Storage(StorageError::Conflict {
            object_type: ObjectType::Program,
            field: "programName",
            value: "tou".into(),
        });
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        // A dangling reference is the body's fault, so 400 rather than 404.
        let dangling = ApiError::Storage(StorageError::DanglingReference {
            field: "programID",
            value: "nope".into(),
        });
        assert_eq!(dangling.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn internal_errors_do_not_leak_their_detail() {
        let e = ApiError::Internal("connection string: postgres://user:hunter2@db".into());
        let problem = e.problem();
        assert_eq!(problem.status, Some(500));
        assert!(!problem.detail.unwrap().contains("hunter2"));
    }

    #[test]
    fn problems_carry_a_dereferenceable_type() {
        let p = ApiError::MissingScope(Scope::WriteEvents).problem();
        assert_eq!(
            p.r#type.as_deref(),
            Some("https://hupe1980.github.io/openadr/problems/missing-scope")
        );
        assert!(p.detail.unwrap().contains("write_events"));
    }

    /// One of every variant, so the registry and the code cannot drift apart silently.
    ///
    /// A `type` URI is only worth minting if it resolves, and it resolves because
    /// `PROBLEM_TYPES` is published as a redirect per slug. A variant whose slug is missing here
    /// would serve clients a URI that documents nothing.
    #[test]
    fn every_variant_mints_a_published_type() {
        use crate::model::{ObjectId, ObjectType, problem::PROBLEM_TYPES};

        let id = ObjectId::new("object-0000000000").unwrap();
        let errors = [
            ApiError::BadRequest("x".into()),
            ApiError::UnsupportedMediaType("x".into()),
            ApiError::PayloadTooLarge("x".into()),
            ApiError::InvalidPayload(Vec::new()),
            ApiError::Unauthorized(AuthError::Missing),
            ApiError::MissingScope(Scope::WriteEvents),
            ApiError::Forbidden("x".into()),
            ApiError::NotFound {
                object_type: ObjectType::Event,
                id: id.clone(),
            },
            ApiError::NoSuchRoute,
            ApiError::NotImplemented("x".into()),
            ApiError::Internal("x".into()),
            ApiError::Unavailable("x".into()),
            ApiError::Storage(StorageError::NotFound {
                object_type: ObjectType::Event,
                id,
            }),
            ApiError::Storage(StorageError::Conflict {
                object_type: ObjectType::Event,
                field: "f",
                value: "v".into(),
            }),
            ApiError::Storage(StorageError::DanglingReference {
                field: "f",
                value: "v".into(),
            }),
            ApiError::Storage(StorageError::Unavailable("x".into())),
        ];

        for error in &errors {
            let slug = error.slug();
            assert!(
                PROBLEM_TYPES.contains(&slug),
                "{slug} is minted but not published; add it to PROBLEM_TYPES and the registry page"
            );
        }
    }
}
