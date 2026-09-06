//! JWT authentication against a JWKS, over a real socket, with real signatures.
//!
//! The unit tests in `vtn::auth::jwt` check the pieces — which algorithm a JWK admits, how a claim
//! becomes a `clientID`. What they cannot check is whether the pieces add up to a validator, and
//! that is the whole of the value here: a token this VTN accepts is a token somebody could have
//! forged if any one of the refusals below were missing.
//!
//! The key pair is fixed and committed. It is a *test* key — an ECDSA P-256 pair generated for this
//! file and used nowhere else — and it has to be fixed, because a token has to be signed by
//! something the JWKS then publishes.

#![cfg(all(feature = "vtn", feature = "external-auth"))]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use axum::{Json, Router, extract::State, routing::get};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use openadr::vtn::auth::{Authenticator, JwtAuthenticator, JwtConfig, Scope};
use serde_json::{Value, json};

/// The private half, PKCS#8 PEM. Test-only.
const PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgxipZSH7LrfMcmPvs\n\
B2sJ55NgTbm9po886jWayY9DJ96hRANCAASeToCG8o9T4kCnaepc8YFwrgGEAf+e\n\
ZkcvsgATxwfJnhdLzlHTPbd2ts1pN5GX/KdvjDyZJfTOc9P5MXRhspzY\n\
-----END PRIVATE KEY-----\n";

const KID: &str = "test-key-1";
const ISSUER: &str = "https://sso.test/realms/openadr";
const AUDIENCE: &str = "openadr-vtn";

/// The public half, as the authorization server would publish it.
fn jwk(kid: &str, extra: Value) -> Value {
    let mut key = json!({
        "kty": "EC",
        "crv": "P-256",
        "kid": kid,
        "alg": "ES256",
        "x": "nk6AhvKPU-JAp2nqXPGBcK4BhAH_nmZHL7IAE8cHyZ4",
        "y": "F0vOUdM9t3a2zWk3kZf8p2-MPJkl9M5z0_kxdGGynNg"
    });
    if let (Value::Object(base), Value::Object(more)) = (&mut key, extra) {
        base.extend(more);
    }
    key
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// PKCS#8 DER from a PEM.
///
/// `jsonwebtoken` is compiled here without `use_pem`, because the VTN only ever reads JWKs and never
/// a PEM — so the one place that needs the conversion is this file.
fn der(pem: &str) -> Vec<u8> {
    use base64::Engine as _;
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .expect("the test key is valid base64")
}

/// Mint a token, with whatever claims the test wants to bend.
fn token(claims: Value) -> String {
    let key = EncodingKey::from_ec_der(&der(PRIVATE_KEY));
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(KID.to_string());
    jsonwebtoken::encode(&header, &claims, &key).expect("signing succeeds")
}

fn valid_claims() -> Value {
    json!({
        "iss": ISSUER,
        "aud": AUDIENCE,
        "sub": "subject-uuid",
        "azp": "ven-client-7",
        "scope": "read_targets read_ven_objects write_reports",
        "iat": now() - 10,
        "exp": now() + 600,
    })
}

/// A stand-in authorization server that serves one key set and counts the fetches.
struct Jwks {
    url: String,
    fetches: Arc<AtomicUsize>,
}

impl Jwks {
    /// A key server that answers `500` to everything, and counts the attempts.
    async fn broken() -> Self {
        let fetches = Arc::new(AtomicUsize::new(0));
        let state = fetches.clone();
        let app = Router::new()
            .route(
                "/certs",
                get(|State(fetches): State<Arc<AtomicUsize>>| async move {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR
                }),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            url: format!("http://{addr}/certs"),
            fetches,
        }
    }

    async fn serve(keys: Value) -> Self {
        let fetches = Arc::new(AtomicUsize::new(0));
        let state = (keys, fetches.clone());
        let app = Router::new()
            .route(
                "/certs",
                get(
                    |State((keys, fetches)): State<(Value, Arc<AtomicUsize>)>| async move {
                        fetches.fetch_add(1, Ordering::SeqCst);
                        Json(keys)
                    },
                ),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            url: format!("http://{addr}/certs"),
            fetches,
        }
    }

    fn authenticator(&self) -> JwtAuthenticator {
        JwtAuthenticator::new(
            JwtConfig::new(&self.url, "https://sso.test/token")
                .with_issuer(ISSUER)
                .with_audiences([AUDIENCE]),
        )
        .unwrap()
    }
}

#[tokio::test]
async fn a_genuine_token_becomes_a_principal() {
    let server = Jwks::serve(json!({"keys": [jwk(KID, json!({}))]})).await;
    let auth = server.authenticator();

    let principal = auth
        .authenticate(Some(&token(valid_claims())))
        .await
        .expect("a token signed by the published key is valid");

    // `azp` before `sub`: Keycloak puts the client there, and a `sub` is the *user*, not the client.
    assert_eq!(
        principal.client_id.as_ref().map(|c| c.as_str()),
        Some("ven-client-7")
    );
    assert!(principal.scopes.contains(Scope::ReadTargets));
    assert!(principal.scopes.contains(Scope::WriteReports));
    assert!(
        !principal.scopes.contains(Scope::ReadAll),
        "a scope the token did not carry must not appear"
    );
    assert!(!principal.is_business_logic());

    // The key set is fetched once and then cached: a VTN under load must not be a load generator
    // aimed at its own authorization server.
    auth.authenticate(Some(&token(valid_claims())))
        .await
        .unwrap();
    assert_eq!(server.fetches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn every_refusal_holds() {
    let server = Jwks::serve(json!({"keys": [jwk(KID, json!({}))]})).await;
    let auth = server.authenticator();

    // Signed by a key that is not the published one. This is the whole point of the exercise.
    let other = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg6u97lUinFTbwa9dZ\n\
FEvVTK884RhSHCKwSfjR4XnbFouhRANCAAToX6i/0iQvzrfGqaDg4/nyokv0jIDY\n\
EADnz7Kjh9LBzQeoA2f35dkpLLu6mp/kNwJ8kOWfFUKqE+EQygOhYXUc\n\
-----END PRIVATE KEY-----\n";
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(KID.to_string());
    let forged = jsonwebtoken::encode(
        &header,
        &valid_claims(),
        &EncodingKey::from_ec_der(&der(other)),
    )
    .unwrap();
    assert!(
        auth.authenticate(Some(&forged)).await.is_err(),
        "a token signed by another key was accepted"
    );

    // Expired, and not yet valid.
    let mut expired = valid_claims();
    expired["exp"] = json!(now() - 3600);
    assert!(auth.authenticate(Some(&token(expired))).await.is_err());

    let mut future = valid_claims();
    future["nbf"] = json!(now() + 3600);
    assert!(
        auth.authenticate(Some(&token(future))).await.is_err(),
        "a token whose validity starts tomorrow was accepted today"
    );

    // The wrong issuer, and the wrong audience — a token minted for another service.
    let mut wrong_issuer = valid_claims();
    wrong_issuer["iss"] = json!("https://elsewhere.test/realms/openadr");
    assert!(auth.authenticate(Some(&token(wrong_issuer))).await.is_err());

    let mut wrong_audience = valid_claims();
    wrong_audience["aud"] = json!("some-other-service");
    assert!(
        auth.authenticate(Some(&token(wrong_audience)))
            .await
            .is_err(),
        "a token minted for another audience was accepted"
    );

    // And — the case that is not "wrong", but "absent". `jsonwebtoken` checks `aud` and `iss` only
    // when the token carries them, so a token naming *no* audience sailed through an audience
    // check, and one naming no issuer through an issuer check. That is the exact token a real
    // Keycloak mints for `client_credentials` when nobody has added an audience mapper, and it is
    // the replay both options are configured to stop (D-114).
    let mut no_audience = valid_claims();
    no_audience.as_object_mut().unwrap().remove("aud");
    assert!(
        auth.authenticate(Some(&token(no_audience))).await.is_err(),
        "a token naming no audience was accepted by a VTN that requires one"
    );

    let mut no_issuer = valid_claims();
    no_issuer.as_object_mut().unwrap().remove("iss");
    assert!(
        auth.authenticate(Some(&token(no_issuer))).await.is_err(),
        "a token naming no issuer was accepted by a VTN that requires one"
    );

    // No `exp` at all.
    let mut eternal = valid_claims();
    eternal.as_object_mut().unwrap().remove("exp");
    assert!(auth.authenticate(Some(&token(eternal))).await.is_err());

    // Structural nonsense, and nothing at all.
    assert!(auth.authenticate(Some("not.a.token")).await.is_err());
    assert!(auth.authenticate(None).await.is_err());
}

#[tokio::test]
async fn an_encryption_key_is_not_a_verification_key() {
    // A key set may carry both. `use` and `key_ops` are how its publisher says which is which
    // (RFC 7517 §4.2, §4.3), and verifying a signature against a key offered for encryption is
    // using it outside the purpose its owner declared.
    let server = Jwks::serve(json!({"keys": [
        jwk(KID, json!({ "use": "enc" })),
        jwk("ops-key", json!({ "key_ops": ["encrypt", "decrypt"] })),
    ]}))
    .await;
    let auth = server.authenticator();

    assert!(
        auth.authenticate(Some(&token(valid_claims())))
            .await
            .is_err(),
        "a key published for encryption was used to verify a signature"
    );
}

#[tokio::test]
async fn a_symmetric_key_in_a_key_set_is_never_used() {
    // The classic algorithm-confusion setup: publish an `oct` key, sign with HS256, and the
    // "signature" is an HMAC over a secret anyone can read from the JWKS.
    let server = Jwks::serve(json!({"keys": [
        {"kty": "oct", "kid": KID, "alg": "HS256", "k": "c2VjcmV0LXRoZS13aG9sZS13b3JsZC1jYW4tcmVhZA"}
    ]}))
    .await;
    let auth = server.authenticator();

    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some(KID.to_string());
    let forged = jsonwebtoken::encode(
        &header,
        &valid_claims(),
        &EncodingKey::from_secret(b"secret-the-whole-world-can-read"),
    )
    .unwrap();

    assert!(
        auth.authenticate(Some(&forged)).await.is_err(),
        "a token was verified against a key published in the clear"
    );
}

#[tokio::test]
async fn an_unknown_key_id_refetches_once_and_then_stops() {
    // Rotation is a cache miss, not a timer — but a stream of tokens naming random key ids must not
    // turn the VTN into a request amplifier aimed at its authorization server.
    let server = Jwks::serve(json!({"keys": [jwk(KID, json!({}))]})).await;
    let auth = server.authenticator();

    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some("a-key-that-does-not-exist".to_string());
    let unknown = jsonwebtoken::encode(
        &header,
        &valid_claims(),
        &EncodingKey::from_ec_der(&der(PRIVATE_KEY)),
    )
    .unwrap();

    for _ in 0..25 {
        assert!(auth.authenticate(Some(&unknown)).await.is_err());
    }
    assert_eq!(
        server.fetches.load(Ordering::SeqCst),
        1,
        "twenty-five bad tokens produced more than one fetch"
    );

    // And the good token still works, from the set that fetch brought back.
    assert!(
        auth.authenticate(Some(&token(valid_claims())))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn the_claim_the_client_identity_is_read_from_is_configurable() {
    // `[Def §VEN created object privacy]` says only that the VTN discovers the `clientID` "by means
    // not specified here". Keycloak puts it in `azp`, many servers in `client_id`, some only in
    // `sub` — so this is a knob, and a knob nothing exercised. Reading the wrong claim gives a
    // principal that owns nothing, which looks exactly like a VEN with no objects.
    let server = Jwks::serve(json!({"keys": [jwk(KID, json!({}))]})).await;
    let auth = JwtAuthenticator::new(
        JwtConfig::new(&server.url, "https://sso.test/token")
            .with_issuer(ISSUER)
            .with_audiences([AUDIENCE])
            .with_client_id_claims(["client_id"]),
    )
    .unwrap();

    let mut claims = valid_claims();
    claims["client_id"] = json!("from-client-id");
    // `azp` is the default's first choice and must lose to the configured claim.
    claims["azp"] = json!("from-azp");
    let principal = auth.authenticate(Some(&token(claims))).await.unwrap();
    assert_eq!(
        principal.client_id.as_ref().map(|c| c.as_str()),
        Some("from-client-id"),
        "the configured claim was ignored in favour of the default order"
    );

    // And a token carrying none of the configured claims identifies nobody rather than falling
    // back to one that was not asked for.
    let mut anonymousish = valid_claims();
    anonymousish["azp"] = json!("from-azp");
    let principal = auth.authenticate(Some(&token(anonymousish))).await.unwrap();
    assert!(
        principal.client_id.is_none(),
        "a claim the operator did not configure was read anyway"
    );
}

/// An authorization server that is down must not be asked once per incoming request.
///
/// `min_refresh_interval` exists so that a stream of tokens naming unknown key ids cannot turn the
/// VTN into a load generator aimed at the authorization server. It was anchored on the last
/// *successful* fetch — so while the JWKS was failing there was no last successful fetch to be too
/// soon after, the limit never applied, and every token opened another request to a server that was
/// already in trouble. The window that matters for amplification is the last *attempt* (D-137).
#[tokio::test]
async fn a_failing_key_server_is_asked_once_per_interval_not_once_per_request() {
    let server = Jwks::broken().await;
    let auth = JwtAuthenticator::new(
        JwtConfig::new(&server.url, "https://sso.test/token")
            .with_issuer(ISSUER)
            .with_audiences([AUDIENCE]),
    )
    .unwrap();

    for _ in 0..10 {
        let err = auth
            .authenticate(Some(&token(valid_claims())))
            .await
            .expect_err("a VTN that cannot read the key set must not accept a token");
        // And it says the VTN could not look, rather than that the credential was bad: the two send
        // an operator to completely different places.
        assert!(
            matches!(err, openadr::vtn::auth::AuthError::Unavailable(_)),
            "a JWKS that is down was reported as an invalid token: {err}"
        );
    }

    assert_eq!(
        server.fetches.load(Ordering::SeqCst),
        1,
        "ten tokens provoked that many fetches against a key server that is already failing"
    );
}

/// A revocation window shorter than the anti-amplification window must not lock everyone out.
///
/// `refresh_after` bounds how long a *withdrawn* key keeps working, so an operator who wants fast
/// revocation turns it down. `min_refresh_interval` bounds how often an unknown `kid` may provoke a
/// fetch. Set the first below the second and every request lands in the gap between them: the cache
/// is stale, so the fast path declines, and it is too recent to refetch — and the key the token was
/// actually signed with is sitting in the cache the whole time. Refusing there takes the VTN's
/// authentication down entirely, on a configuration that reads like caution (D-121's neighbour, and
/// the same shape: a guard answering a question it was not asked).
#[tokio::test]
async fn a_stale_cache_still_serves_a_key_it_holds() {
    let server = Jwks::serve(json!({"keys": [jwk(KID, json!({}))]})).await;
    let auth = JwtAuthenticator::new(
        JwtConfig::new(&server.url, "https://sso.test/token")
            .with_issuer(ISSUER)
            .with_audiences([AUDIENCE]),
    )
    .unwrap();

    // Prime the cache, then step into the gap.
    assert!(
        auth.authenticate(Some(&token(valid_claims())))
            .await
            .is_ok()
    );
    assert_eq!(server.fetches.load(Ordering::SeqCst), 1);

    let auth = {
        let mut config = JwtConfig::new(&server.url, "https://sso.test/token")
            .with_issuer(ISSUER)
            .with_audiences([AUDIENCE]);
        // Stale immediately, and rate-limited for a minute.
        config.refresh_after = Duration::from_nanos(1);
        config.min_refresh_interval = Duration::from_secs(60);
        JwtAuthenticator::new(config).unwrap()
    };
    // One fetch to fill the cache; every later token finds it stale and rate-limited.
    assert!(
        auth.authenticate(Some(&token(valid_claims())))
            .await
            .is_ok()
    );
    let after_priming = server.fetches.load(Ordering::SeqCst);
    for _ in 0..5 {
        assert!(
            auth.authenticate(Some(&token(valid_claims())))
                .await
                .is_ok(),
            "a key the cache holds was refused because the cache was stale and rate-limited"
        );
    }
    assert_eq!(
        server.fetches.load(Ordering::SeqCst),
        after_priming,
        "the rate limit still holds: a stale cache served the key rather than refetching"
    );

    // And a `kid` it does not hold is still refused rather than served from anywhere.
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some("not-in-the-set".to_string());
    let unknown = jsonwebtoken::encode(
        &header,
        &valid_claims(),
        &EncodingKey::from_ec_der(&der(PRIVATE_KEY)),
    )
    .unwrap();
    assert!(auth.authenticate(Some(&unknown)).await.is_err());
}

#[tokio::test]
async fn a_token_with_no_recognisable_client_reads_nothing_targeted() {
    // Fail closed. A principal with scopes but no `clientID` cannot be resolved to a VEN, so it has
    // no grant — and an unresolvable identity must not become an unrestricted one.
    let server = Jwks::serve(json!({"keys": [jwk(KID, json!({}))]})).await;
    let auth = server.authenticator();

    let mut anonymousish = valid_claims();
    for claim in ["azp", "client_id", "sub"] {
        anonymousish.as_object_mut().unwrap().remove(claim);
    }
    let principal = auth
        .authenticate(Some(&token(anonymousish)))
        .await
        .expect("the token is valid; it just does not say who it is");

    assert!(principal.client_id.is_none());
    assert!(
        !principal.is_business_logic(),
        "an unidentified token must not be read as business logic"
    );
}

#[tokio::test]
async fn scopes_are_read_from_either_shape() {
    // `scope` is a space-separated string (RFC 8693) and `scp` is an array. Both are in the field.
    let server = Jwks::serve(json!({"keys": [jwk(KID, json!({}))]})).await;
    let auth = server.authenticator();

    let mut array_form = valid_claims();
    array_form.as_object_mut().unwrap().remove("scope");
    array_form["scp"] = json!(["read_all", "write_events"]);

    let principal = auth.authenticate(Some(&token(array_form))).await.unwrap();
    assert!(principal.scopes.contains(Scope::ReadAll));
    assert!(principal.scopes.contains(Scope::WriteEvents));
    assert!(
        principal.is_business_logic(),
        "read_all is what makes a caller business logic"
    );
}
