//! JWT bearer authentication against an external authorization server.
//!
//! The common production shape, and the one Dutch grid-aware charging uses: an authorization server
//! — Keycloak, Auth0, Entra, an in-house one — issues signed JWTs, and the VTN validates them
//! against the server's JWKS. Nothing is shared between the two but a URL.
//!
//! Three things here are load-bearing, and each is a documented way of getting JWT validation
//! wrong.
//!
//! * **The algorithm comes from the key, never from the token.** A token that says `alg: HS256` and
//!   a JWKS full of RSA public keys is the classic confusion attack: the "signature" is then an
//!   HMAC over a key the attacker can read. The algorithm accepted for a `kid` is derived from the
//!   JWK, and a symmetric key in a JWKS is refused outright.
//! * **Rotation is a cache miss, not a timer.** An unknown `kid` refetches the JWKS, rate-limited so
//!   that a stream of nonsense tokens cannot turn the VTN into a load generator pointed at the
//!   authorization server. The cache also expires, so a key that was *withdrawn* stops working.
//! * **`clientID` is a configurable claim.** Keycloak puts it in `azp`, others in `client_id`, some
//!   only in `sub`, and the specification says only that the VTN discovers it "by means not
//!   specified here" `[Def §VEN created object privacy]`.
//!
//! And a fourth that is less famous: **a key published for encryption is not a key for
//! verification.** A JWKS may carry both, distinguished by `use` and `key_ops` (RFC 7517 §4.2,
//! §4.3). Verifying a signature against a key its owner published for something else is using a key
//! outside its stated purpose, which is the whole reason those members exist.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, jwk};
use serde_json::Value;

use super::{AuthError, Authenticator, Principal, Scopes};
use crate::model::ClientId;

/// How tokens from an external authorization server are validated.
#[derive(Debug, Clone)]
pub struct JwtConfig {
    /// Where the signing keys are published, e.g.
    /// `https://sso.example.com/realms/openadr/protocol/openid-connect/certs`.
    pub jwks_url: String,
    /// The token endpoint to advertise from `GET /auth/server`.
    pub token_url: String,
    /// Required `iss`. Leaving it unset accepts any issuer, which is almost never what you want.
    pub issuer: Option<String>,
    /// Accepted `aud` values. Empty disables the check.
    pub audiences: Vec<String>,
    /// Claims to read the client identity from, in order of preference.
    ///
    /// Keycloak uses `azp`; many servers use `client_id`; `sub` is the fallback every server sets.
    pub client_id_claims: Vec<String>,
    /// Claims to read scopes from, in order of preference.
    ///
    /// `scope` is a space-separated string (RFC 8693); `scp` is an array. Both occur.
    pub scope_claims: Vec<String>,
    /// Tolerance for clock skew between the VTN and the authorization server.
    pub leeway: Duration,
    /// How long a fetched key set is used before it is fetched again.
    ///
    /// This is what bounds how long a *withdrawn* key keeps working, so it is a revocation window
    /// rather than a cache tuning knob.
    pub refresh_after: Duration,
    /// Shortest interval between two fetches provoked by an unknown `kid`.
    ///
    /// Without it, a stream of tokens naming random key ids is a request amplifier aimed at the
    /// authorization server.
    pub min_refresh_interval: Duration,
    /// How long to wait for the JWKS endpoint.
    pub timeout: Duration,
}

impl JwtConfig {
    /// A configuration with the usual claim names and sensible windows.
    pub fn new(jwks_url: impl Into<String>, token_url: impl Into<String>) -> Self {
        Self {
            jwks_url: jwks_url.into(),
            token_url: token_url.into(),
            issuer: None,
            audiences: Vec::new(),
            client_id_claims: vec!["azp".into(), "client_id".into(), "sub".into()],
            scope_claims: vec!["scope".into(), "scp".into()],
            leeway: Duration::from_secs(60),
            refresh_after: Duration::from_secs(15 * 60),
            min_refresh_interval: Duration::from_secs(30),
            timeout: Duration::from_secs(10),
        }
    }

    /// Require this `iss`.
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(issuer.into());
        self
    }

    /// Require one of these `aud` values.
    pub fn with_audiences<I: Into<String>>(
        mut self,
        audiences: impl IntoIterator<Item = I>,
    ) -> Self {
        self.audiences = audiences.into_iter().map(Into::into).collect();
        self
    }

    /// Read the client identity from these claims, in order.
    pub fn with_client_id_claims<I: Into<String>>(
        mut self,
        claims: impl IntoIterator<Item = I>,
    ) -> Self {
        self.client_id_claims = claims.into_iter().map(Into::into).collect();
        self
    }
}

/// A key from the authorization server, with the one algorithm it may be used for.
#[derive(Clone)]
struct VerifyingKey {
    key: DecodingKey,
    algorithm: Algorithm,
}

struct KeyCache {
    keys: HashMap<String, VerifyingKey>,
    fetched_at: Option<Instant>,
}

/// Validates JWTs issued by an external authorization server.
pub struct JwtAuthenticator {
    config: JwtConfig,
    http: reqwest::Client,
    cache: RwLock<KeyCache>,
}

impl std::fmt::Debug for JwtAuthenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtAuthenticator")
            .field("jwks_url", &self.config.jwks_url)
            .field("issuer", &self.config.issuer)
            .finish_non_exhaustive()
    }
}

impl JwtAuthenticator {
    /// Build an authenticator. Keys are fetched lazily, on the first token.
    pub fn new(config: JwtConfig) -> Result<Self, AuthError> {
        // `reqwest` is built without a crypto provider of its own, so the process default has to
        // exist before a client does. See `crate::crypto`.
        crate::crypto::install_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| AuthError::Unavailable(e.to_string()))?;
        Ok(Self {
            config,
            http,
            cache: RwLock::new(KeyCache {
                keys: HashMap::new(),
                fetched_at: None,
            }),
        })
    }

    /// The verifying key for a `kid`, refetching the key set if it is unknown or stale.
    async fn key_for(&self, kid: &str) -> Result<VerifyingKey, AuthError> {
        {
            let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());
            let held = cache.keys.get(kid).cloned();
            let fresh = cache
                .fetched_at
                .is_some_and(|at| at.elapsed() < self.config.refresh_after);
            if fresh && let Some(key) = held {
                return Ok(key);
            }
            if cache
                .fetched_at
                .is_some_and(|at| at.elapsed() < self.config.min_refresh_interval)
            {
                // Too soon to fetch again. An unknown `kid` on a set this recent is a bad token
                // rather than a rotation, and refusing it is what stops a stream of nonsense ids
                // becoming a load generator aimed at the authorization server.
                //
                // A key we *do* hold is served from the stale set instead of refused. Reaching
                // here with one at all needs `refresh_after` below `min_refresh_interval` — a
                // revocation window shorter than the anti-amplification window, which is a
                // configuration contradiction. Resolving it by rejecting valid tokens would take
                // the whole VTN down for the difference between the two.
                return held.ok_or(AuthError::Invalid);
            }
        }

        self.refresh().await?;
        self.cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys
            .get(kid)
            .cloned()
            .ok_or(AuthError::Invalid)
    }

    /// Fetch and replace the key set.
    async fn refresh(&self) -> Result<(), AuthError> {
        let response = self
            .http
            .get(&self.config.jwks_url)
            .send()
            .await
            .map_err(|e| AuthError::Unavailable(e.to_string()))?;
        if !response.status().is_success() {
            return Err(AuthError::Unavailable(format!(
                "{} returned {}",
                self.config.jwks_url,
                response.status()
            )));
        }
        let set: jwk::JwkSet = response
            .json()
            .await
            .map_err(|e| AuthError::Unavailable(format!("unreadable JWKS: {e}")))?;

        let mut keys = HashMap::new();
        for key in &set.keys {
            let Some(kid) = key.common.key_id.clone() else {
                // A key with no `kid` cannot be selected by one, and selecting by trial would mean
                // trying every key against every token.
                continue;
            };
            if !usable_for_verification(key) {
                tracing::debug!(%kid, "ignoring a JWKS key its publisher did not offer for signatures");
                continue;
            }
            let Some(algorithm) = algorithm_of(key) else {
                tracing::warn!(%kid, "ignoring a JWKS key whose algorithm is unusable here");
                continue;
            };
            match DecodingKey::from_jwk(key) {
                Ok(decoding) => {
                    keys.insert(
                        kid,
                        VerifyingKey {
                            key: decoding,
                            algorithm,
                        },
                    );
                }
                Err(e) => tracing::warn!(%kid, error = %e, "ignoring an unreadable JWKS key"),
            }
        }
        if keys.is_empty() {
            return Err(AuthError::Unavailable(format!(
                "{} published no usable keys",
                self.config.jwks_url
            )));
        }

        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        cache.keys = keys;
        cache.fetched_at = Some(Instant::now());
        Ok(())
    }

    /// Turn a validated claim set into a principal.
    fn principal(&self, claims: &Value) -> Result<Principal, AuthError> {
        let client_id = self
            .config
            .client_id_claims
            .iter()
            .find_map(|name| claims.get(name).and_then(Value::as_str))
            .and_then(|raw| ClientId::new(raw).ok());

        let scopes = self
            .config
            .scope_claims
            .iter()
            .find_map(|name| claims.get(name))
            .map(scopes_of)
            .unwrap_or_default();

        Ok(Principal { client_id, scopes })
    }
}

/// Whether the key's publisher offered it for verifying signatures.
///
/// `use` and `key_ops` are how a key set says what each key is *for* (RFC 7517 §4.2, §4.3). Both are
/// optional, and a key that states neither is accepted — most authorization servers publish a
/// signing-only set and say nothing. A key that states `use: "enc"`, or lists `key_ops` without
/// `verify`, has been offered for something else, and using it anyway is using a key outside the
/// purpose its owner declared.
fn usable_for_verification(key: &jwk::Jwk) -> bool {
    use jwk::PublicKeyUse;

    if let Some(declared) = &key.common.public_key_use
        && !matches!(declared, PublicKeyUse::Signature)
    {
        return false;
    }
    match &key.common.key_operations {
        Some(ops) if !ops.is_empty() => ops
            .iter()
            .any(|op| matches!(op, jwk::KeyOperations::Verify | jwk::KeyOperations::Sign)),
        _ => true,
    }
}

/// The one algorithm a JWK may be used with.
///
/// Taken from the key, because taking it from the token is how algorithm-confusion attacks work.
fn algorithm_of(key: &jwk::Jwk) -> Option<Algorithm> {
    use jwk::{AlgorithmParameters, EllipticCurve, KeyAlgorithm};

    if let Some(declared) = key.common.key_algorithm {
        return match declared {
            KeyAlgorithm::RS256 => Some(Algorithm::RS256),
            KeyAlgorithm::RS384 => Some(Algorithm::RS384),
            KeyAlgorithm::RS512 => Some(Algorithm::RS512),
            KeyAlgorithm::PS256 => Some(Algorithm::PS256),
            KeyAlgorithm::PS384 => Some(Algorithm::PS384),
            KeyAlgorithm::PS512 => Some(Algorithm::PS512),
            KeyAlgorithm::ES256 => Some(Algorithm::ES256),
            KeyAlgorithm::ES384 => Some(Algorithm::ES384),
            KeyAlgorithm::EdDSA => Some(Algorithm::EdDSA),
            // An HMAC key published in a JWKS is a secret published in public. Refuse it rather
            // than validate tokens against a key anyone can read.
            _ => None,
        };
    }

    // No `alg`: infer the family from the key material, which is unambiguous for the ones we accept.
    match &key.algorithm {
        AlgorithmParameters::RSA(_) => Some(Algorithm::RS256),
        AlgorithmParameters::EllipticCurve(ec) => match ec.curve {
            EllipticCurve::P256 => Some(Algorithm::ES256),
            EllipticCurve::P384 => Some(Algorithm::ES384),
            _ => None,
        },
        AlgorithmParameters::OctetKeyPair(_) => Some(Algorithm::EdDSA),
        AlgorithmParameters::OctetKey(_) => None,
    }
}

/// Read scopes from either shape an authorization server uses.
fn scopes_of(value: &Value) -> Scopes {
    match value {
        Value::String(s) => Scopes::parse(s),
        Value::Array(items) => Scopes::new(
            items
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|s| s.parse().ok()),
        ),
        _ => Scopes::none(),
    }
}

#[async_trait]
impl Authenticator for JwtAuthenticator {
    async fn authenticate(&self, bearer: Option<&str>) -> Result<Principal, AuthError> {
        let token = bearer.ok_or(AuthError::Missing)?;
        let header = jsonwebtoken::decode_header(token).map_err(|_| AuthError::Invalid)?;
        // Without a `kid` there is no way to say which key signed this, and trying them all turns
        // one bad token into N signature verifications.
        let kid = header.kid.ok_or(AuthError::Invalid)?;
        let key = self.key_for(&kid).await?;

        let mut validation = Validation::new(key.algorithm);
        validation.leeway = self.config.leeway.as_secs();
        // Off by default in `jsonwebtoken`, which means a token whose validity *starts* tomorrow is
        // accepted today. Leeway still applies, so a small clock difference is not a refusal.
        validation.validate_nbf = true;
        // `set_issuer` and `set_audience` check a claim the token *carries*: `jsonwebtoken`'s
        // validator matches `(present, configured)` and lets `(absent, configured)` fall through to
        // `Ok`. So a token with no `aud` at all passes an audience check, and one with no `iss`
        // passes an issuer check — which is precisely the replay these two options are set to stop,
        // and precisely the token shape a real Keycloak issues for `client_credentials` unless
        // somebody adds an audience mapper. Requiring the claim turns "absent" into a refusal
        // `[D-114]`. Same defect as `validate_nbf` in D-073, one claim over.
        if let Some(issuer) = &self.config.issuer {
            validation.set_issuer(&[issuer]);
            validation.required_spec_claims.insert("iss".to_string());
        }
        if self.config.audiences.is_empty() {
            validation.validate_aud = false;
        } else {
            validation.set_audience(&self.config.audiences);
            validation.required_spec_claims.insert("aud".to_string());
        }

        match jsonwebtoken::decode::<Value>(token, &key.key, &validation) {
            Ok(data) => self.principal(&data.claims),
            Err(e) => Err(match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::Expired,
                _ => AuthError::Invalid,
            }),
        }
    }

    fn token_url(&self) -> String {
        self.config.token_url.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwk_from(value: serde_json::Value) -> jwk::Jwk {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_symmetric_key_in_a_jwks_is_refused() {
        // An `oct` key in a published key set is a shared secret that is not secret. Accepting it
        // would let anyone who can read the JWKS mint tokens.
        let key = jwk_from(json!({
            "kty": "oct",
            "kid": "shared",
            "alg": "HS256",
            "k": "c2VjcmV0"
        }));
        assert_eq!(algorithm_of(&key), None);
    }

    #[test]
    fn the_algorithm_comes_from_the_key() {
        let rsa = jwk_from(json!({
            "kty": "RSA", "kid": "a", "alg": "RS256",
            "n": "xGOr-H7A-PWG8kUJmXH-1ZDdYLXY0KsCcHfe1UwqhBrbjXPaMYcnbYbXOVEfHZ1Zsxq",
            "e": "AQAB"
        }));
        assert_eq!(algorithm_of(&rsa), Some(Algorithm::RS256));

        // No `alg`: inferred from the key material rather than from whatever the token claims.
        let bare = jwk_from(json!({
            "kty": "EC", "kid": "b", "crv": "P-256",
            "x": "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU",
            "y": "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0"
        }));
        assert_eq!(algorithm_of(&bare), Some(Algorithm::ES256));
    }

    #[test]
    fn scopes_are_read_from_either_shape() {
        let from_string = scopes_of(&json!("read_all write_events openid"));
        assert!(from_string.contains(super::super::Scope::ReadAll));
        assert!(from_string.contains(super::super::Scope::WriteEvents));
        assert_eq!(
            from_string.as_slice().len(),
            2,
            "unknown scopes are ignored"
        );

        let from_array = scopes_of(&json!(["read_targets", "read_ven_objects"]));
        assert_eq!(from_array.as_slice().len(), 2);

        assert!(scopes_of(&json!(42)).is_empty());
    }

    #[test]
    fn the_client_id_claim_is_configurable() {
        let auth = JwtAuthenticator::new(
            JwtConfig::new("https://sso.example/jwks", "https://sso.example/token")
                .with_client_id_claims(["azp", "sub"]),
        )
        .unwrap();

        // Keycloak's shape: `azp` wins over `sub`.
        let keycloak = auth
            .principal(&json!({"sub": "uuid-1", "azp": "ven-7", "scope": "read_targets"}))
            .unwrap();
        assert_eq!(keycloak.client_id.unwrap().as_str(), "ven-7");

        // A server that only sets `sub` still works.
        let plain = auth.principal(&json!({"sub": "ven-9"})).unwrap();
        assert_eq!(plain.client_id.unwrap().as_str(), "ven-9");
        assert!(plain.scopes.is_empty());
    }

    #[tokio::test]
    async fn a_token_without_a_kid_is_refused_without_a_network_call() {
        // The JWKS URL points nowhere; reaching it would be the test failing, not passing.
        let auth = JwtAuthenticator::new(JwtConfig::new(
            "http://127.0.0.1:1/jwks",
            "http://127.0.0.1:1/token",
        ))
        .unwrap();
        // `{"alg":"none"}` with no `kid`, an empty payload and an empty signature.
        let token = "eyJhbGciOiJub25lIn0.e30.";
        assert_eq!(
            auth.authenticate(Some(token)).await.unwrap_err(),
            AuthError::Invalid
        );
        assert_eq!(
            auth.authenticate(None).await.unwrap_err(),
            AuthError::Missing
        );
    }
}
