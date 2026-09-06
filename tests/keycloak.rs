//! The whole OAuth2 exchange, against a server that actually issues the tokens.
//!
//! `tests/jwt_auth.rs` mints tokens with a key this repository holds and serves a JWKS this
//! repository wrote. That covers key selection, claim mapping and every refusal — and it cannot
//! cover the half that actually breaks an integration, because both ends of it were written here.
//! What a real authorization server puts in `azp`, what it puts in `aud`, whether `scope` is a
//! string or an array, what its `iss` looks like down to the trailing slash: every one of those is
//! a convention rather than a rule, and getting one wrong is a `401` nobody can explain.
//!
//! So this runs a real Keycloak: creates a realm, creates a confidential client with a service
//! account and a client scope carrying OpenADR's own scope names, asks Keycloak for a token, and
//! puts that token through a real [`JwtAuthenticator`] into a real VTN.
//!
//! **It skips without Docker**, loudly. A container is the only way to have a real authorization
//! server in a test, and a machine without one is a legitimate place to build this crate — but a
//! suite that quietly passed because it did nothing is the failure this project keeps finding.

#![cfg(all(feature = "vtn", feature = "external-auth", feature = "client"))]

use std::sync::Arc;
use std::time::Duration;

use openadr::vtn::{
    Vtn, VtnConfig,
    auth::{Authenticator, JwtAuthenticator, JwtConfig},
    store::MemoryStorage,
};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

const REALM: &str = "openadr";
const BL_CLIENT: &str = "bl-1";
const BL_SECRET: &str = "bl-secret";
/// A second client, deliberately provisioned *without* an audience mapper.
const BARE_CLIENT: &str = "bl-no-audience";
const BARE_SECRET: &str = "bare-secret";
/// What this VTN calls itself in an `aud` claim.
const VTN_AUDIENCE: &str = "openadr-vtn";

/// A running Keycloak, and the base URL it answers on.
struct Keycloak {
    _container: ContainerAsync<GenericImage>,
    base: String,
    http: reqwest::Client,
}

impl Keycloak {
    /// Start one, or `None` when there is no Docker to start it with.
    async fn start() -> Option<Self> {
        // `start-dev` is the mode Keycloak documents for exactly this: an ephemeral instance with an
        // in-memory database. `--http-relative-path=/` because the issuer this test asserts on has
        // to be the one a deployment would see, not one shaped by a path prefix nobody sets.
        let container = GenericImage::new("quay.io/keycloak/keycloak", "26.0")
            .with_exposed_port(8080.tcp())
            // Keycloak prints this once the HTTP listener is serving. Waiting on the log rather
            // than on the port is the difference between "the socket is open" and "it will answer".
            .with_wait_for(WaitFor::message_on_stdout(
                "Listening on: http://0.0.0.0:8080",
            ))
            .with_env_var("KC_BOOTSTRAP_ADMIN_USERNAME", "admin")
            .with_env_var("KC_BOOTSTRAP_ADMIN_PASSWORD", "admin")
            .with_cmd(["start-dev"])
            .with_startup_timeout(Duration::from_secs(180))
            .start()
            .await
            .inspect_err(|e| eprintln!("skipping: no Docker for a Keycloak container ({e})"))
            .ok()?;

        // This crate builds `reqwest` without a crypto provider of its own and installs `ring`
        // before every client it constructs. A client built *here* is not one of those.
        openadr::install_crypto_provider();

        let port = container.get_host_port_ipv4(8080.tcp()).await.ok()?;
        Some(Self {
            _container: container,
            base: format!("http://127.0.0.1:{port}"),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .ok()?,
        })
    }

    /// An admin access token, from the master realm.
    async fn admin_token(&self) -> String {
        let body = self
            .http
            .post(format!(
                "{}/realms/master/protocol/openid-connect/token",
                self.base
            ))
            .form(&[
                ("grant_type", "password"),
                ("client_id", "admin-cli"),
                ("username", "admin"),
                ("password", "admin"),
            ])
            .send()
            .await
            .expect("Keycloak is up")
            .json::<serde_json::Value>()
            .await
            .expect("a token response");
        body["access_token"]
            .as_str()
            .unwrap_or_else(|| panic!("no admin token in {body}"))
            .to_string()
    }

    async fn admin_post(&self, token: &str, path: &str, body: serde_json::Value) {
        let response = self
            .http
            .post(format!("{}/admin/realms{path}", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .expect("the admin API answers");
        assert!(
            response.status().is_success(),
            "POST /admin/realms{path} answered {}: {}",
            response.status(),
            response.text().await.unwrap_or_default()
        );
    }

    /// Create the realm, a confidential client with a service account, and OpenADR's scopes on it.
    ///
    /// Keycloak does not hand out arbitrary scopes: a scope has to exist as a *client scope* and be
    /// attached to the client before it appears in a token. That is exactly the sort of convention
    /// this test exists to pin down — a VTN that assumed otherwise would receive tokens with an
    /// empty `scope` and refuse every write for a reason nothing in either system explains.
    async fn provision(&self) {
        let admin = self.admin_token().await;
        self.admin_post(
            &admin,
            "",
            serde_json::json!({ "realm": REALM, "enabled": true }),
        )
        .await;

        let bl_scopes = [
            "read_all",
            "read_bl",
            "write_programs",
            "write_events",
            "write_reports",
            "write_subscriptions",
            "write_vens",
        ];
        for scope in bl_scopes {
            self.admin_post(
                &admin,
                &format!("/{REALM}/client-scopes"),
                serde_json::json!({
                    "name": scope,
                    "protocol": "openid-connect",
                    "attributes": { "include.in.token.scope": "true" },
                }),
            )
            .await;
        }

        // The client a deployment would actually configure: service account, OpenADR's scopes, and
        // an **audience mapper** naming the VTN. Keycloak puts no `aud` on a `client_credentials`
        // token without one — which is how D-114 was found, and why the bare client below exists.
        self.admin_post(
            &admin,
            &format!("/{REALM}/clients"),
            serde_json::json!({
                "clientId": BL_CLIENT,
                "secret": BL_SECRET,
                "enabled": true,
                "protocol": "openid-connect",
                "publicClient": false,
                "serviceAccountsEnabled": true,
                "standardFlowEnabled": false,
                "defaultClientScopes": bl_scopes,
                "protocolMappers": [{
                    "name": "vtn-audience",
                    "protocol": "openid-connect",
                    "protocolMapper": "oidc-audience-mapper",
                    "config": {
                        "included.custom.audience": VTN_AUDIENCE,
                        "access.token.claim": "true",
                    },
                }],
            }),
        )
        .await;

        // The same client with no audience mapper, which is Keycloak's default and the token shape
        // that used to be accepted by a VTN configured to require an audience.
        self.admin_post(
            &admin,
            &format!("/{REALM}/clients"),
            serde_json::json!({
                "clientId": BARE_CLIENT,
                "secret": BARE_SECRET,
                "enabled": true,
                "protocol": "openid-connect",
                "publicClient": false,
                "serviceAccountsEnabled": true,
                "standardFlowEnabled": false,
                "defaultClientScopes": bl_scopes,
            }),
        )
        .await;
    }

    fn issuer(&self) -> String {
        format!("{}/realms/{REALM}", self.base)
    }

    fn jwks_url(&self) -> String {
        format!("{}/protocol/openid-connect/certs", self.issuer())
    }

    fn token_url(&self) -> String {
        format!("{}/protocol/openid-connect/token", self.issuer())
    }

    /// Exchange client credentials for an access token, the way a business-logic client would.
    async fn token(&self, client_id: &str, secret: &str) -> String {
        let body = self
            .http
            .post(self.token_url())
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", client_id),
                ("client_secret", secret),
            ])
            .send()
            .await
            .expect("the token endpoint answers")
            .json::<serde_json::Value>()
            .await
            .expect("a token response");
        body["access_token"]
            .as_str()
            .unwrap_or_else(|| panic!("no access token in {body}"))
            .to_string()
    }
}

/// A `JwtAuthenticator` pointed at this Keycloak, requiring `audience`.
fn vtn_auth(keycloak: &Keycloak, audience: &str) -> JwtAuthenticator {
    JwtAuthenticator::new(
        JwtConfig::new(keycloak.jwks_url(), keycloak.token_url())
            .with_issuer(keycloak.issuer())
            .with_audiences([audience]),
    )
    .expect("the configuration is valid")
}

/// The whole exchange: Keycloak issues, the VTN validates, and a write goes through.
///
/// One test rather than several, because starting Keycloak costs half a minute and every assertion
/// below is about the same realm.
#[tokio::test]
async fn a_token_a_real_authorization_server_issued_is_accepted_end_to_end() {
    let Some(keycloak) = Keycloak::start().await else {
        return;
    };
    keycloak.provision().await;
    let token = keycloak.token(BL_CLIENT, BL_SECRET).await;
    let authenticator = vtn_auth(&keycloak, VTN_AUDIENCE);

    // 1. The token maps to the identity and the scopes Keycloak was told to put in it.
    let principal = authenticator
        .authenticate(Some(&token))
        .await
        .expect("a token this server issued must be accepted");
    assert_eq!(
        principal.client_id.as_ref().map(|c| c.as_str()),
        Some(BL_CLIENT),
        "the clientID must come from `azp`, which is where Keycloak puts it"
    );
    assert!(
        principal.is_business_logic(),
        "a client granted the business-logic scopes must read as business logic: {:?}",
        principal.scopes
    );

    // 2. And it works through a real VTN, on a real write.
    let vtn = Vtn::builder()
        .storage(MemoryStorage::shared())
        .authenticator(Arc::new(authenticator))
        .config(VtnConfig {
            base_path: "/openadr3/3.1.0".into(),
            ..Default::default()
        })
        .build();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = vtn.router();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let client = openadr::client::Client::<openadr::client::BusinessLogic>::builder(&format!(
        "http://{addr}/openadr3/3.1.0"
    ))
    .unwrap()
    .bearer_token(token)
    .build()
    .unwrap();

    let program = client
        .programs()
        .create(&openadr::model::ProgramRequest::new(
            "keycloak-issued".parse().unwrap(),
        ))
        .await
        .expect("a write authorized by a Keycloak token must succeed");
    assert_eq!(program.content.program_name.as_str(), "keycloak-issued");

    // 3. `GET /auth/server` points at the server that issued it, so a client holding only the VTN's
    //    URL can find where to get a credential.
    let advertised: serde_json::Value =
        reqwest::get(format!("http://{addr}/openadr3/3.1.0/auth/server"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(advertised["tokenURL"], keycloak.token_url());

    // 4. A token from the same server for a *different* audience is refused, which is the check
    //    that stops one deployment's tokens being replayed at another's VTN.
    let elsewhere = vtn_auth(&keycloak, "some-other-vtn");
    let token = keycloak.token(BL_CLIENT, BL_SECRET).await;
    assert!(
        elsewhere.authenticate(Some(&token)).await.is_err(),
        "a token whose audience is somebody else's was accepted"
    );

    // 5. And a token naming *no* audience is refused too, which is the one this test found
    //    (D-114). Keycloak issues exactly this for `client_credentials` unless somebody adds an
    //    audience mapper, and `jsonwebtoken` checks `aud` only when the token carries one — so a
    //    VTN configured to require an audience accepted every token that named none.
    let bare = keycloak.token(BARE_CLIENT, BARE_SECRET).await;
    assert!(
        !bare_token_has_an_audience(&bare),
        "this client is provisioned without an audience mapper; if Keycloak started adding one, \
         the assertion below would pass for the wrong reason"
    );
    let requires_audience = vtn_auth(&keycloak, VTN_AUDIENCE);
    assert!(
        requires_audience.authenticate(Some(&bare)).await.is_err(),
        "a token naming no audience was accepted by a VTN that requires one"
    );

    // And with no audience configured, that same token is fine: requiring `aud` is a consequence of
    // asking for one, not a new rule of its own.
    let any_audience = JwtAuthenticator::new(
        JwtConfig::new(keycloak.jwks_url(), keycloak.token_url()).with_issuer(keycloak.issuer()),
    )
    .unwrap();
    assert!(
        any_audience.authenticate(Some(&bare)).await.is_ok(),
        "a VTN that asks for no audience must still accept a token that names none"
    );
}

/// Whether a JWT carries an `aud` claim. Payload only — the signature is somebody else's business.
fn bare_token_has_an_audience(token: &str) -> bool {
    use base64::Engine as _;
    let Some(payload) = token.split('.').nth(1) else {
        return false;
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|claims| claims.get("aud").cloned())
        .is_some()
}
