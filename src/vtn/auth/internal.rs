//! The VTN as its own authorization server.
//!
//! `POST /auth/token` is optional `[API /auth/token]`, and a cloud VTN delegates it to Keycloak or
//! the like. The deployment this exists for is the other one: a site controller, a single-tenant
//! pilot, a home gateway — one binary and one file, where standing up an OAuth2 server beside it is
//! more infrastructure than the thing it authenticates.
//!
//! It is a real client-credentials grant, not a token table with an HTTP endpoint in front of it:
//!
//! * **Secrets are stored hashed**, with Argon2id. A configuration file or database that leaks does
//!   not hand over working credentials.
//! * **An unknown `client_id` costs the same as a known one.** The verification runs against a
//!   fixed decoy hash when no client matches, so response time does not enumerate clients.
//! * **Tokens are signed JWTs**, so validating one is arithmetic rather than a lookup — which is
//!   what lets several VTN instances share a signing key and validate each other's tokens with no
//!   shared session store.
//!
//! The signing key defaults to fresh random bytes, so a restart invalidates outstanding tokens and
//! clients simply fetch another. Pass [`InternalAuthBuilder::signing_key`] when more than one
//! instance serves the same clients.

use std::collections::BTreeMap;
use std::time::Duration;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use async_trait::async_trait;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use super::{AuthError, Authenticator, Principal, Scopes, TokenError};
use crate::model::{
    ClientCredentialRequest, ClientCredentialResponse, ClientId, OAuthError, OAuthErrorKind,
};

/// How many Argon2id verifications run at once, by default.
///
/// The work is CPU-bound, so more than the machine has cores buys nothing and costs memory — each
/// verification holds 19 MiB under the default parameters. Capped at eight so that a large host
/// does not turn `POST /auth/token` into a gigabyte of allocation on demand.
fn default_verifier_permits() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 8)
}

/// A hash no secret matches, used so that an unknown client costs the same as a known one.
///
/// Argon2id over a random secret, generated once at build time. Its value is irrelevant; its cost
/// is the point.
const DECOY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$\
                          8VYq7cA8+9zdChZk3ZKGZBTHDVKa0MEZGpTKMNL0d6E";

/// One client the VTN will issue tokens to.
#[derive(Debug, Clone)]
struct Registered {
    /// PHC-encoded Argon2 hash of the client secret.
    secret_hash: String,
    /// The most this client may ever be granted.
    scopes: Scopes,
}

/// The claims this VTN puts in a token.
#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: String,
    exp: i64,
    iat: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    scope: String,
}

/// A VTN that issues and validates its own tokens.
pub struct InternalAuth {
    clients: BTreeMap<String, Registered>,
    encoding: EncodingKey,
    decoding: DecodingKey,
    issuer: String,
    token_url: String,
    ttl: Duration,
    allow_anonymous: bool,
    clock: std::sync::Arc<dyn crate::core::Clock>,
    /// How many password verifications may run at once.
    ///
    /// Argon2id is *designed* to be expensive — the default parameters are 19 MiB and two passes
    /// per verification — and the decoy hash means an unauthenticated request costs that whether or
    /// not the client id exists. Without a bound, `POST /auth/token` is a memory and CPU amplifier
    /// anyone can point at the VTN: `spawn_blocking`'s pool would happily run five hundred of them.
    /// The permit count is the memory ceiling, and the request timeout is what a queued caller
    /// eventually hits.
    verifiers: std::sync::Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for InternalAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InternalAuth")
            .field("issuer", &self.issuer)
            .field("clients", &self.clients.len())
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

/// Builds an [`InternalAuth`].
pub struct InternalAuthBuilder {
    clients: BTreeMap<String, Registered>,
    signing_key: Option<Vec<u8>>,
    issuer: String,
    token_url: String,
    ttl: Duration,
    allow_anonymous: bool,
    clock: std::sync::Arc<dyn crate::core::Clock>,
    max_concurrent_verifications: usize,
}

impl std::fmt::Debug for InternalAuthBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InternalAuthBuilder")
            .field("issuer", &self.issuer)
            .field("clients", &self.clients.len())
            .finish_non_exhaustive()
    }
}

impl InternalAuth {
    /// Start building an issuer whose `/auth/token` lives at `token_url`.
    pub fn builder(token_url: impl Into<String>) -> InternalAuthBuilder {
        let token_url = token_url.into();
        InternalAuthBuilder {
            clients: BTreeMap::new(),
            signing_key: None,
            issuer: token_url.clone(),
            token_url,
            ttl: Duration::from_secs(3600),
            allow_anonymous: false,
            clock: std::sync::Arc::new(crate::core::SystemClock),
            max_concurrent_verifications: default_verifier_permits(),
        }
    }

    /// Hash a client secret for storage, so an operator never has to keep the plaintext.
    ///
    /// The result is a PHC string; feed it back to [`InternalAuthBuilder::client_hashed`].
    pub fn hash_secret(secret: &str) -> Result<String, AuthError> {
        // The salt comes from the crate's own RNG rather than `password_hash`'s re-export, which
        // pins an older `rand_core` than the rest of the build uses.
        use rand::Rng as _;
        let raw: [u8; 16] = rand::rng().random();
        let salt =
            SaltString::encode_b64(&raw).map_err(|e| AuthError::Unavailable(e.to_string()))?;
        Argon2::default()
            .hash_password(secret.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| AuthError::Unavailable(e.to_string()))
    }

    fn mint(
        &self,
        client_id: &str,
        scopes: &Scopes,
    ) -> Result<ClientCredentialResponse, TokenError> {
        let now = self.clock.now().as_second();
        let claims = Claims {
            iss: self.issuer.clone(),
            sub: client_id.to_string(),
            aud: self.issuer.clone(),
            iat: now,
            exp: now + self.ttl.as_secs() as i64,
            scope: scopes.to_scope_string(),
        };
        let token = jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .map_err(|e| {
            TokenError::Refused(OAuthError::new(
                OAuthErrorKind::InvalidRequest,
                format!("could not mint a token: {e}"),
            ))
        })?;
        Ok(ClientCredentialResponse {
            access_token: token,
            token_type: "Bearer".into(),
            expires_in: Some(self.ttl.as_secs()),
            refresh_token: None,
            scope: Some(claims.scope),
        })
    }
}

impl InternalAuthBuilder {
    /// Register a client with a plaintext secret, hashed here and never kept.
    pub fn client(
        self,
        client_id: impl Into<String>,
        secret: &str,
        scopes: Scopes,
    ) -> Result<Self, AuthError> {
        let hash = InternalAuth::hash_secret(secret)?;
        Ok(self.client_hashed(client_id, hash, scopes))
    }

    /// Register a client whose secret is already hashed, as it would be in a configuration file.
    pub fn client_hashed(
        mut self,
        client_id: impl Into<String>,
        secret_hash: impl Into<String>,
        scopes: Scopes,
    ) -> Self {
        self.clients.insert(
            client_id.into(),
            Registered {
                secret_hash: secret_hash.into(),
                scopes,
            },
        );
        self
    }

    /// The key tokens are signed with.
    ///
    /// Supply one when more than one VTN instance serves the same clients; without it each process
    /// generates its own and a restart invalidates outstanding tokens.
    pub fn signing_key(mut self, key: impl Into<Vec<u8>>) -> Self {
        self.signing_key = Some(key.into());
        self
    }

    /// The `iss` claim, and the audience tokens are minted for. Defaults to the token URL.
    pub fn issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = issuer.into();
        self
    }

    /// How long an issued token is valid.
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Also serve unauthenticated readers, for a VTN that publishes public tariffs as well.
    pub fn allowing_anonymous(mut self) -> Self {
        self.allow_anonymous = true;
        self
    }

    /// Override the clock, for deterministic tests.
    pub fn clock(mut self, clock: std::sync::Arc<dyn crate::core::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// How many client secrets may be verified at once.
    ///
    /// Argon2id costs about 19 MiB and tens of milliseconds per verification by design, and the
    /// decoy hash means an unauthenticated request costs that whether the client id exists or not.
    /// This is the ceiling on both: memory is `n × 19 MiB`, and a caller beyond it queues until the
    /// request timeout. Defaults to the machine's parallelism, capped at eight.
    ///
    /// Raising it does not make the VTN faster — the work is CPU-bound — it makes `/auth/token` a
    /// larger lever for anyone who can reach it.
    pub fn max_concurrent_verifications(mut self, permits: usize) -> Self {
        self.max_concurrent_verifications = permits.max(1);
        self
    }

    /// Finish.
    pub fn build(self) -> InternalAuth {
        let key = self.signing_key.unwrap_or_else(|| {
            use rand::Rng as _;
            let bytes: [u8; 32] = rand::rng().random();
            bytes.to_vec()
        });
        InternalAuth {
            clients: self.clients,
            encoding: EncodingKey::from_secret(&key),
            decoding: DecodingKey::from_secret(&key),
            issuer: self.issuer,
            token_url: self.token_url,
            ttl: self.ttl,
            allow_anonymous: self.allow_anonymous,
            clock: self.clock,
            verifiers: std::sync::Arc::new(tokio::sync::Semaphore::new(
                self.max_concurrent_verifications,
            )),
        }
    }
}

#[async_trait]
impl Authenticator for InternalAuth {
    async fn authenticate(&self, bearer: Option<&str>) -> Result<Principal, AuthError> {
        let Some(token) = bearer else {
            return if self.allow_anonymous {
                Ok(Principal::anonymous())
            } else {
                Err(AuthError::Missing)
            };
        };

        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.issuer]);
        // `jsonwebtoken` checks `iss` and `aud` only when the token carries them, so a token that
        // named neither would pass both checks `[D-114]`. `Claims` requires both fields, so such a
        // token already failed to deserialise — this makes the *refusal* the rule rather than a
        // consequence of a struct definition, and keeps this path saying the same thing as
        // `JwtAuthenticator`, which is where it matters.
        validation
            .required_spec_claims
            .extend(["iss".to_string(), "aud".to_string()]);
        let data = jsonwebtoken::decode::<Claims>(token, &self.decoding, &validation).map_err(
            |e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::Expired,
                _ => AuthError::Invalid,
            },
        )?;

        Ok(Principal {
            client_id: ClientId::new(data.claims.sub).ok(),
            scopes: Scopes::parse(&data.claims.scope),
        })
    }

    fn token_url(&self) -> String {
        self.token_url.clone()
    }

    fn allows_anonymous(&self) -> bool {
        self.allow_anonymous
    }

    fn issues_tokens(&self) -> bool {
        true
    }

    async fn issue_token(
        &self,
        request: &ClientCredentialRequest,
    ) -> Result<ClientCredentialResponse, TokenError> {
        if request.grant_type != "client_credentials" {
            return Err(TokenError::Refused(OAuthError::new(
                OAuthErrorKind::UnsupportedGrantType,
                "only the client_credentials grant is supported",
            )));
        }

        // Look the client up, then verify *either way*. Returning early on an unknown client id
        // would make the response time say which ids exist.
        let found = self.clients.get(&request.client_id);
        let hash = found
            .map_or(DECOY_HASH, |c| c.secret_hash.as_str())
            .to_string();

        // Off the runtime, and one of a bounded number at a time. Argon2id spends tens of
        // milliseconds and megabytes on purpose; run inline it blocks a worker thread, and enough
        // concurrent token requests stall every other thing the VTN is doing — including delivering
        // the dispatch instructions it exists to deliver.
        let permit = self
            .verifiers
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| TokenError::Unavailable("the VTN is shutting down".into()))?;
        let secret = request.client_secret.clone();
        let verified = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            PasswordHash::new(&hash)
                .map(|parsed| {
                    Argon2::default()
                        .verify_password(secret.as_bytes(), &parsed)
                        .is_ok()
                })
                .unwrap_or(false)
        })
        .await
        // A panic in the hasher is not an authentication. Failing closed is the only safe reading.
        .unwrap_or(false);

        let Some(client) = found.filter(|_| verified) else {
            return Err(TokenError::Refused(OAuthError::new(
                OAuthErrorKind::InvalidClient,
                "unknown client, or the secret does not match",
            )));
        };

        // A client may ask for less than it holds, never for more.
        let granted = match &request.scope {
            None => client.scopes.clone(),
            Some(asked) => {
                let asked = Scopes::parse(asked);
                let granted = Scopes::new(
                    asked
                        .as_slice()
                        .iter()
                        .copied()
                        .filter(|s| client.scopes.contains(*s)),
                );
                if granted.is_empty() {
                    return Err(TokenError::Refused(OAuthError::new(
                        OAuthErrorKind::InvalidScope,
                        "none of the requested scopes are granted to this client",
                    )));
                }
                granted
            }
        };

        self.mint(&request.client_id, &granted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vtn::auth::Scope;

    fn request(client_id: &str, secret: &str, scope: Option<&str>) -> ClientCredentialRequest {
        ClientCredentialRequest {
            grant_type: "client_credentials".into(),
            client_id: client_id.into(),
            client_secret: secret.into(),
            scope: scope.map(str::to_string),
        }
    }

    fn auth() -> InternalAuth {
        InternalAuth::builder("https://vtn.test/auth/token")
            .client("ven-7", "hunter2", Scopes::new(Scope::VEN))
            .unwrap()
            .build()
    }

    #[tokio::test]
    async fn a_minted_token_authenticates_back() {
        let auth = auth();
        let issued = auth
            .issue_token(&request("ven-7", "hunter2", None))
            .await
            .unwrap();
        assert_eq!(issued.token_type, "Bearer");
        assert_eq!(issued.expires_in, Some(3600));

        let principal = auth.authenticate(Some(&issued.access_token)).await.unwrap();
        assert_eq!(principal.client_id.as_ref().unwrap().as_str(), "ven-7");
        assert!(principal.scopes.contains(Scope::ReadTargets));
        assert!(!principal.is_business_logic());
    }

    #[tokio::test]
    async fn a_wrong_secret_and_an_unknown_client_are_the_same_answer() {
        let auth = auth();
        for req in [
            request("ven-7", "wrong", None),
            request("nobody", "hunter2", None),
        ] {
            match auth.issue_token(&req).await {
                Err(TokenError::Refused(e)) => assert_eq!(e.error, OAuthErrorKind::InvalidClient),
                other => panic!("expected invalid_client, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_client_cannot_ask_for_more_than_it_holds() {
        let auth = auth();
        // `write_events` is business logic's; a VEN asking for it gets its own scopes minus that.
        let issued = auth
            .issue_token(&request(
                "ven-7",
                "hunter2",
                Some("read_targets write_events"),
            ))
            .await
            .unwrap();
        let principal = auth.authenticate(Some(&issued.access_token)).await.unwrap();
        assert!(principal.scopes.contains(Scope::ReadTargets));
        assert!(
            !principal.scopes.contains(Scope::WriteEvents),
            "asking for a scope must not grant it"
        );

        match auth
            .issue_token(&request("ven-7", "hunter2", Some("write_events")))
            .await
        {
            Err(TokenError::Refused(e)) => assert_eq!(e.error, OAuthErrorKind::InvalidScope),
            other => panic!("expected invalid_scope, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn only_the_client_credentials_grant_is_offered() {
        let mut req = request("ven-7", "hunter2", None);
        req.grant_type = "password".into();
        match auth().issue_token(&req).await {
            Err(TokenError::Refused(e)) => {
                assert_eq!(e.error, OAuthErrorKind::UnsupportedGrantType)
            }
            other => panic!("expected unsupported_grant_type, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_token_request_yields_the_runtime_rather_than_blocking_it() {
        // Argon2id is expensive on purpose — 19 MiB and tens of milliseconds under the default
        // parameters — and the decoy hash means *every* request pays it, including an
        // unauthenticated one naming a client that does not exist. Run inline, it blocks a worker
        // thread; enough concurrent token requests then stall everything else the VTN is doing,
        // which on a VTN is the dispatcher delivering curtailment instructions.
        //
        // Asserted without a clock or a sleep. On a single-threaded runtime a queued task can only
        // run if the current one suspends, so the flag is set exactly when the verification hands
        // the runtime back — and stays unset if the hashing happens inline.
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        let queued = tokio::spawn(async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let auth = auth();
        assert!(
            auth.issue_token(&request("ghost", "wrong", None))
                .await
                .is_err()
        );
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            "the verification never yielded: it hashed on the runtime thread, so a handful of \
             token requests would stall every other task the VTN has"
        );
        queued.await.unwrap();
    }

    #[tokio::test]
    async fn verification_concurrency_is_bounded() {
        // The permit count is the memory ceiling: Argon2id's default parameters are 19 MiB each,
        // and `spawn_blocking`'s pool would otherwise run five hundred at once.
        let auth = InternalAuth::builder("https://vtn.test/auth/token")
            .max_concurrent_verifications(0) // clamped up to one
            .build();
        assert_eq!(auth.verifiers.available_permits(), 1);
    }

    #[tokio::test]
    async fn a_token_from_another_issuer_is_not_accepted() {
        let issued = auth()
            .issue_token(&request("ven-7", "hunter2", None))
            .await
            .unwrap();
        // A second VTN with its own random signing key must not accept it.
        let other = InternalAuth::builder("https://other.test/auth/token").build();
        assert_eq!(
            other
                .authenticate(Some(&issued.access_token))
                .await
                .unwrap_err(),
            AuthError::Invalid
        );
    }

    #[tokio::test]
    async fn an_expired_token_says_so_rather_than_merely_failing() {
        use crate::core::FixedClock;
        let past: crate::model::Timestamp = "2020-01-01T00:00:00Z".parse().unwrap();
        let auth = InternalAuth::builder("https://vtn.test/auth/token")
            .client("ven-7", "hunter2", Scopes::new(Scope::VEN))
            .unwrap()
            .clock(std::sync::Arc::new(FixedClock::new(past)))
            .build();
        let issued = auth
            .issue_token(&request("ven-7", "hunter2", None))
            .await
            .unwrap();
        // The token was minted in 2020 with a one-hour life; the validator uses the real clock.
        assert_eq!(
            auth.authenticate(Some(&issued.access_token))
                .await
                .unwrap_err(),
            AuthError::Expired
        );
    }

    #[test]
    fn a_stored_hash_is_not_the_secret() {
        let hash = InternalAuth::hash_secret("hunter2").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(!hash.contains("hunter2"));
        assert_ne!(
            hash,
            InternalAuth::hash_secret("hunter2").unwrap(),
            "salted"
        );
    }
}
