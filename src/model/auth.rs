//! OAuth 2.0 client-credentials types and token-endpoint discovery.
//!
//! Field names here are `snake_case` on the wire because RFC 6749 says so — the rest of the API is
//! `camelCase`.

use crate::std_shim::String;
use serde::{Deserialize, Serialize};

/// Response of `GET /auth/server`: where to exchange credentials for a token.
///
/// This endpoint is required even when the VTN delegates to an external authorization server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthServerInfo {
    /// URL of the token endpoint.
    #[serde(rename = "tokenURL")]
    pub token_url: String,
}

/// Body of `POST /auth/token`, form-encoded per RFC 6749.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCredentialRequest {
    /// Must be `client_credentials`.
    pub grant_type: String,
    /// The client identifier.
    pub client_id: String,
    /// The client secret.
    pub client_secret: String,
    /// Space-separated scopes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Successful token response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCredentialResponse {
    /// The bearer token.
    pub access_token: String,
    /// Always `Bearer`.
    pub token_type: String,
    /// Lifetime in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    /// Refresh token, when the authorization server issues one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Granted scopes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// The RFC 6749 error codes the token endpoint may return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthErrorKind {
    /// A parameter is missing, unsupported or repeated.
    InvalidRequest,
    /// Client authentication failed.
    InvalidClient,
    /// The grant is invalid or expired.
    InvalidGrant,
    /// The requested scope is invalid.
    InvalidScope,
    /// The client may not use this grant type.
    UnauthorizedClient,
    /// The grant type is not recognised.
    UnsupportedGrantType,
}

/// Error body of the token endpoint (RFC 6749 §5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthError {
    /// The error code.
    pub error: OAuthErrorKind,
    /// A sentence or two of context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
    /// A link to more detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_uri: Option<String>,
}

impl OAuthError {
    /// An error with a description.
    pub fn new(error: OAuthErrorKind, description: impl Into<String>) -> Self {
        Self {
            error,
            error_description: Some(description.into()),
            error_uri: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_fields_stay_snake_case() {
        let r = ClientCredentialResponse {
            access_token: "t".into(),
            token_type: "Bearer".into(),
            expires_in: Some(3600),
            refresh_token: None,
            scope: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("access_token"));
        assert!(json.contains("expires_in"));
    }

    #[test]
    fn error_kinds_use_the_rfc_spelling() {
        let e = OAuthError::new(OAuthErrorKind::InvalidClient, "bad secret");
        assert!(
            serde_json::to_string(&e)
                .unwrap()
                .contains(r#""error":"invalid_client""#)
        );
    }
}
