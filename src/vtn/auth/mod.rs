//! Authentication and the OAuth2 scopes the specification defines.
//!
//! Deployments disagree about identity. Fluvius' NetFlex profile forbids OAuth2 and requires mutual
//! TLS; Californian price servers are anonymous; Dutch grid-aware charging uses Keycloak. So
//! authentication is a trait, and the rest of the VTN only ever sees the [`Principal`] that falls
//! out of it.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::model::{ClientCredentialRequest, ClientCredentialResponse, ClientId, OAuthError};

#[cfg(feature = "internal-auth")]
mod internal;
#[cfg(feature = "external-auth")]
mod jwt;

#[cfg(feature = "internal-auth")]
#[cfg_attr(docsrs, doc(cfg(feature = "internal-auth")))]
pub use internal::{InternalAuth, InternalAuthBuilder};
#[cfg(feature = "external-auth")]
#[cfg_attr(docsrs, doc(cfg(feature = "external-auth")))]
pub use jwt::{JwtAuthenticator, JwtConfig};

/// An OAuth2 scope from the specification's security scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Business logic may read everything.
    ReadAll,
    /// Business logic may read BL-scoped notifier topic metadata.
    ReadBl,
    /// A VEN may read targeted objects, by naming matching targets.
    ReadTargets,
    /// A VEN may read the objects whose `clientID` is its own.
    ReadVenObjects,
    /// Write programmes. Business logic only.
    WritePrograms,
    /// Write events. Business logic only.
    WriteEvents,
    /// Write reports.
    WriteReports,
    /// Write subscriptions.
    WriteSubscriptions,
    /// Write VENs and resources.
    WriteVens,
}

impl Scope {
    /// The wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Scope::ReadAll => "read_all",
            Scope::ReadBl => "read_bl",
            Scope::ReadTargets => "read_targets",
            Scope::ReadVenObjects => "read_ven_objects",
            Scope::WritePrograms => "write_programs",
            Scope::WriteEvents => "write_events",
            Scope::WriteReports => "write_reports",
            Scope::WriteSubscriptions => "write_subscriptions",
            Scope::WriteVens => "write_vens",
        }
    }

    /// Every scope.
    pub const ALL: [Scope; 9] = [
        Scope::ReadAll,
        Scope::ReadBl,
        Scope::ReadTargets,
        Scope::ReadVenObjects,
        Scope::WritePrograms,
        Scope::WriteEvents,
        Scope::WriteReports,
        Scope::WriteSubscriptions,
        Scope::WriteVens,
    ];

    /// The scopes a business-logic client normally holds.
    pub const BUSINESS_LOGIC: [Scope; 7] = [
        Scope::ReadAll,
        Scope::ReadBl,
        Scope::WritePrograms,
        Scope::WriteEvents,
        Scope::WriteSubscriptions,
        Scope::WriteVens,
        Scope::WriteReports,
    ];

    /// The scopes a VEN normally holds.
    pub const VEN: [Scope; 5] = [
        Scope::ReadTargets,
        Scope::ReadVenObjects,
        Scope::WriteReports,
        Scope::WriteSubscriptions,
        Scope::WriteVens,
    ];
}

impl core::str::FromStr for Scope {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Scope::ALL.into_iter().find(|c| c.as_str() == s).ok_or(())
    }
}

impl core::fmt::Display for Scope {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of scopes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scopes(Vec<Scope>);

impl Scopes {
    /// An empty set.
    pub fn none() -> Self {
        Self::default()
    }

    /// Build from scopes, de-duplicating.
    pub fn new(scopes: impl IntoIterator<Item = Scope>) -> Self {
        let mut v: Vec<Scope> = scopes.into_iter().collect();
        v.sort();
        v.dedup();
        Self(v)
    }

    /// Parse an OAuth2 space-separated scope string, ignoring unknown entries.
    ///
    /// Unknown scopes are ignored rather than rejected: an authorization server shared with other
    /// applications will hand out scopes that mean nothing here.
    pub fn parse(s: &str) -> Self {
        Self::new(s.split_whitespace().filter_map(|t| t.parse().ok()))
    }

    /// Whether a scope is present.
    pub fn contains(&self, scope: Scope) -> bool {
        self.0.binary_search(&scope).is_ok()
    }

    /// Whether any of the scopes is present.
    pub fn contains_any(&self, scopes: &[Scope]) -> bool {
        scopes.iter().any(|s| self.contains(*s))
    }

    /// The scopes, sorted.
    pub fn as_slice(&self) -> &[Scope] {
        &self.0
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Render as an OAuth2 scope string.
    pub fn to_scope_string(&self) -> String {
        self.0
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Whether these scopes make the holder business logic.
    ///
    /// The specification never labels a token "BL" or "VEN"; the distinction falls out of the
    /// scopes. Anything that can write programmes or events, or read everything, is business logic.
    pub fn is_business_logic(&self) -> bool {
        self.contains_any(&[
            Scope::ReadAll,
            Scope::ReadBl,
            Scope::WritePrograms,
            Scope::WriteEvents,
        ])
    }
}

/// Who is making a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// The client identity, absent for an unauthenticated caller.
    pub client_id: Option<ClientId>,
    /// The scopes the credential carries.
    pub scopes: Scopes,
}

impl Principal {
    /// An unauthenticated caller of a public VTN.
    pub fn anonymous() -> Self {
        Self {
            client_id: None,
            scopes: Scopes::none(),
        }
    }

    /// Whether this principal is business logic.
    pub fn is_business_logic(&self) -> bool {
        self.scopes.is_business_logic()
    }
}

/// Why authentication failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// No credential was presented where one is required.
    #[error("no bearer token was presented")]
    Missing,
    /// The credential was not valid.
    #[error("the presented credential is not valid")]
    Invalid,
    /// The credential was valid but has expired.
    #[error("the presented credential has expired")]
    Expired,
    /// The backend could not be reached.
    #[error("the authorization service is unavailable: {0}")]
    Unavailable(String),
}

/// Why a token could not be issued.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// This VTN does not issue tokens at all.
    ///
    /// `POST /auth/token` is optional `[API /auth/token]`, and a VTN that delegates to an external
    /// authorization service answers `501` and points at `GET /auth/server`.
    #[error(
        "this VTN does not issue tokens; use GET /auth/server to find the authorization service"
    )]
    NotIssuing,
    /// The request was refused, in RFC 6749 §5.2 terms.
    #[error("{}", .0.error_description.as_deref().unwrap_or("the token request was refused"))]
    Refused(OAuthError),
    /// The VTN could not attempt the request at all.
    ///
    /// Distinct from [`TokenError::Refused`] on purpose: refusing says the credential is wrong,
    /// which a client should not retry, and this says the VTN could not look, which it should.
    #[error("the token endpoint is temporarily unavailable: {0}")]
    Unavailable(String),
}

/// Turns a credential into a [`Principal`].
#[async_trait]
pub trait Authenticator: Send + Sync + 'static {
    /// Resolve a bearer token.
    async fn authenticate(&self, bearer: Option<&str>) -> Result<Principal, AuthError>;

    /// The token endpoint to advertise from `GET /auth/server`.
    ///
    /// Required even when tokens come from somewhere else entirely.
    fn token_url(&self) -> String;

    /// Whether unauthenticated requests are permitted.
    fn allows_anonymous(&self) -> bool {
        false
    }

    /// Whether this VTN runs its own `/auth/token`.
    ///
    /// Separate from [`Authenticator::issue_token`] so the endpoint can answer `501` before it
    /// looks at the body: "this VTN does not issue tokens" is a better answer to a malformed
    /// request than "your body is malformed" when both are true.
    fn issues_tokens(&self) -> bool {
        false
    }

    /// Exchange client credentials for a token, for a VTN that runs its own `/auth/token`.
    ///
    /// The default refuses, because most VTNs delegate — which is exactly why the specification
    /// marks the endpoint optional and suggests `501`.
    async fn issue_token(
        &self,
        _request: &ClientCredentialRequest,
    ) -> Result<ClientCredentialResponse, TokenError> {
        Err(TokenError::NotIssuing)
    }
}

/// A shared authenticator.
pub type SharedAuthenticator = Arc<dyn Authenticator>;

/// Accepts everything, as an anonymous principal.
///
/// The specification allows this for a VTN publishing only public information — a tariff server.
/// Writes still require scopes, which an anonymous principal does not have, so a public VTN is
/// read-only by construction.
#[derive(Debug, Clone)]
pub struct AnonymousAuth {
    token_url: String,
}

impl AnonymousAuth {
    /// A public VTN whose `/auth/server` points at `token_url`.
    pub fn new(token_url: impl Into<String>) -> Self {
        Self {
            token_url: token_url.into(),
        }
    }
}

#[async_trait]
impl Authenticator for AnonymousAuth {
    async fn authenticate(&self, _bearer: Option<&str>) -> Result<Principal, AuthError> {
        Ok(Principal::anonymous())
    }

    fn token_url(&self) -> String {
        self.token_url.clone()
    }

    fn allows_anonymous(&self) -> bool {
        true
    }
}

/// A fixed table of tokens.
///
/// For development, integration tests and single-tenant gateways where an OAuth2 server would be
/// ceremony. Tokens are compared in constant time.
#[derive(Debug, Default)]
pub struct StaticTokenAuth {
    tokens: BTreeMap<String, Principal>,
    token_url: String,
    allow_anonymous: bool,
}

impl StaticTokenAuth {
    /// An empty table.
    pub fn new(token_url: impl Into<String>) -> Self {
        Self {
            tokens: BTreeMap::new(),
            token_url: token_url.into(),
            allow_anonymous: false,
        }
    }

    /// Add a token for a business-logic client.
    pub fn with_business_logic(mut self, token: impl Into<String>, client_id: ClientId) -> Self {
        self.tokens.insert(
            token.into(),
            Principal {
                client_id: Some(client_id),
                scopes: Scopes::new(Scope::BUSINESS_LOGIC),
            },
        );
        self
    }

    /// Add a token for a VEN.
    pub fn with_ven(mut self, token: impl Into<String>, client_id: ClientId) -> Self {
        self.tokens.insert(
            token.into(),
            Principal {
                client_id: Some(client_id),
                scopes: Scopes::new(Scope::VEN),
            },
        );
        self
    }

    /// Add a token with explicit scopes.
    pub fn with_token(
        mut self,
        token: impl Into<String>,
        client_id: ClientId,
        scopes: Scopes,
    ) -> Self {
        self.tokens.insert(
            token.into(),
            Principal {
                client_id: Some(client_id),
                scopes,
            },
        );
        self
    }

    /// Also serve unauthenticated readers.
    pub fn allowing_anonymous(mut self) -> Self {
        self.allow_anonymous = true;
        self
    }
}

/// Compare two secrets without leaking their common prefix through timing.
pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[async_trait]
impl Authenticator for StaticTokenAuth {
    async fn authenticate(&self, bearer: Option<&str>) -> Result<Principal, AuthError> {
        let Some(token) = bearer else {
            return if self.allow_anonymous {
                Ok(Principal::anonymous())
            } else {
                Err(AuthError::Missing)
            };
        };
        // Every entry is compared, not just up to the first match. `find` short-circuits, so the
        // number of comparisons would depend on where in the (sorted) table the token sits — which
        // leaks its position, and therefore something about its value. The table is small and the
        // comparisons are cheap; the guarantee is worth more than the branch.
        self.tokens
            .iter()
            .fold(None, |found, (known, principal)| {
                if constant_time_eq(known, token) {
                    Some(principal)
                } else {
                    found
                }
            })
            .cloned()
            .ok_or(AuthError::Invalid)
    }

    fn token_url(&self) -> String {
        self.token_url.clone()
    }

    fn allows_anonymous(&self) -> bool {
        self.allow_anonymous
    }
}

/// Extract a bearer token from an `Authorization` header value.
pub fn bearer_from_header(value: Option<&str>) -> Option<&str> {
    let value = value?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("Bearer")
        .then_some(token.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_strings_round_trip() {
        for s in Scope::ALL {
            assert_eq!(s.as_str().parse::<Scope>().unwrap(), s);
        }
    }

    #[test]
    fn unknown_scopes_are_ignored_not_fatal() {
        let s = Scopes::parse("read_all openid profile write_events");
        assert!(s.contains(Scope::ReadAll));
        assert!(s.contains(Scope::WriteEvents));
        assert_eq!(s.as_slice().len(), 2);
    }

    #[test]
    fn the_role_falls_out_of_the_scopes() {
        assert!(Scopes::new(Scope::BUSINESS_LOGIC).is_business_logic());
        assert!(!Scopes::new(Scope::VEN).is_business_logic());
        assert!(!Scopes::none().is_business_logic());
    }

    #[test]
    fn bearer_extraction_is_case_insensitive_about_the_scheme() {
        assert_eq!(bearer_from_header(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer_from_header(Some("bearer abc")), Some("abc"));
        assert_eq!(bearer_from_header(Some("Basic abc")), None);
        assert_eq!(bearer_from_header(None), None);
    }

    #[tokio::test]
    async fn static_tokens_resolve_to_principals() {
        let auth = StaticTokenAuth::new("https://vtn/auth/token")
            .with_business_logic("bl-token", ClientId::new("bl").unwrap())
            .with_ven("ven-token", ClientId::new("ven-1").unwrap());

        let bl = auth.authenticate(Some("bl-token")).await.unwrap();
        assert!(bl.is_business_logic());

        let ven = auth.authenticate(Some("ven-token")).await.unwrap();
        assert!(!ven.is_business_logic());
        assert_eq!(ven.client_id.unwrap().as_str(), "ven-1");

        assert_eq!(
            auth.authenticate(Some("nope")).await.unwrap_err(),
            AuthError::Invalid
        );
        assert_eq!(
            auth.authenticate(None).await.unwrap_err(),
            AuthError::Missing
        );
    }

    #[tokio::test]
    async fn an_anonymous_vtn_admits_everyone_without_scopes() {
        let auth = AnonymousAuth::new("https://vtn/auth/token");
        let p = auth.authenticate(None).await.unwrap();
        assert!(p.client_id.is_none());
        assert!(p.scopes.is_empty(), "an anonymous reader can never write");
    }

    #[test]
    fn token_comparison_does_not_short_circuit() {
        assert!(constant_time_eq("abcdef", "abcdef"));
        assert!(!constant_time_eq("abcdef", "abcdeg"));
        assert!(!constant_time_eq("abc", "abcdef"));
    }
}
