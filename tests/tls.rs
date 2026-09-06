//! The TLS listener, over a real socket, with real certificates.
//!
//! Every assertion here needs a completed handshake, which is the point: a TLS listener is exactly
//! the kind of component whose unit tests can all pass while nothing can connect to it. The
//! certificates are generated in-process rather than checked in, so nothing here expires.

// `client` as well: the test needs an HTTP client that can present a certificate, and `reqwest`
// reaches this crate through that feature.
#![cfg(all(feature = "tls", feature = "vtn", feature = "client"))]

use std::sync::Arc;

use openadr::vtn::{
    Vtn, VtnConfig,
    auth::StaticTokenAuth,
    store::MemoryStorage,
    tls::{TlsConfig, TlsError},
};

const BL: &str = "bl-secret";

/// A self-signed certificate authority, and the certificates it issues.
struct Authority {
    pem: String,
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
}

impl Authority {
    fn new(name: &str) -> Self {
        let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = params.clone().self_signed(&key).unwrap();
        Self {
            pem: certificate.pem(),
            issuer: rcgen::Issuer::new(params, key),
        }
    }

    fn pem(&self) -> String {
        self.pem.clone()
    }

    /// A leaf certificate for `names`, signed by this authority.
    fn issue(&self, names: Vec<String>) -> (String, String) {
        let mut params = rcgen::CertificateParams::new(names).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "openadr-test");
        params.use_authority_key_identifier_extension = true;
        let key = rcgen::KeyPair::generate().unwrap();
        let leaf = params.signed_by(&key, &self.issuer).unwrap();
        (leaf.pem(), key.serialize_pem())
    }
}

/// Start a VTN over TLS and return its base URL.
async fn serve(tls: TlsConfig) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let vtn = Vtn::builder()
        .storage(MemoryStorage::shared())
        .authenticator(Arc::new(
            StaticTokenAuth::new("https://vtn.test/auth/token")
                .with_business_logic(BL, "bl".parse().unwrap()),
        ))
        .config(VtnConfig {
            base_path: "/openadr3/3.1.0".into(),
            ..Default::default()
        })
        .build();

    tokio::spawn(async move {
        let _ = vtn.serve_tls(addr, tls).await;
    });

    // The port was released a moment ago and the listener is being rebound; a handshake against a
    // closed port fails differently from one against a listener that refuses it, and confusing the
    // two is how this test would pass for the wrong reason.
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    format!("https://localhost:{}/openadr3/3.1.0", addr.port())
}

fn client(ca: &str, identity: Option<(&str, &str)>) -> reqwest::Client {
    // Explicitly, rather than relying on the VTN having built its listener first: this crate builds
    // `reqwest` without a provider of its own, and a client built here is not one this crate
    // constructs. Depending on the order would make the test pass for a reason unrelated to TLS.
    openadr::install_crypto_provider();
    let mut builder = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(ca.as_bytes()).unwrap())
        .use_rustls_tls()
        .timeout(std::time::Duration::from_secs(10));
    if let Some((certificate, key)) = identity {
        let mut pem = certificate.to_string();
        pem.push_str(key);
        builder = builder.identity(reqwest::Identity::from_pem(pem.as_bytes()).unwrap());
    }
    builder.build().unwrap()
}

#[tokio::test]
async fn the_api_is_served_over_tls() {
    let ca = Authority::new("openadr test CA");
    let (certificate, key) = ca.issue(vec!["localhost".into()]);
    let base = serve(TlsConfig::from_pem(certificate.as_bytes(), key.as_bytes()).unwrap()).await;

    let response = client(&ca.pem(), None)
        .get(format!("{base}/programs"))
        .header("authorization", format!("Bearer {BL}"))
        .send()
        .await
        .expect("the handshake and the request must both succeed");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!([])
    );
}

#[tokio::test]
async fn a_client_the_authority_did_not_issue_never_reaches_a_handler() {
    let server_ca = Authority::new("openadr server CA");
    let (certificate, key) = server_ca.issue(vec!["localhost".into()]);
    let client_ca = Authority::new("openadr client CA");
    let stranger = Authority::new("somebody else's CA");

    let base = serve(
        TlsConfig::from_pem(certificate.as_bytes(), key.as_bytes())
            .unwrap()
            .with_client_ca(client_ca.pem().as_bytes())
            .unwrap(),
    )
    .await;

    // Issued by the CA the listener was told about: served.
    let (mine, my_key) = client_ca.issue(vec!["ven-1".into()]);
    let response = client(&server_ca.pem(), Some((&mine, &my_key)))
        .get(format!("{base}/programs"))
        .header("authorization", format!("Bearer {BL}"))
        .send()
        .await
        .expect("a client the CA issued must be served");
    assert_eq!(response.status(), 200);

    // Issued by another CA: refused during the handshake, with a valid bearer token and everything.
    // The refusal is a transport error rather than a status, which is the whole point — nothing in
    // the VTN ran.
    let (theirs, their_key) = stranger.issue(vec!["ven-1".into()]);
    let refused = client(&server_ca.pem(), Some((&theirs, &their_key)))
        .get(format!("{base}/programs"))
        .header("authorization", format!("Bearer {BL}"))
        .send()
        .await;
    assert!(
        refused.is_err(),
        "a certificate from an unknown CA was served: {refused:?}"
    );

    // No certificate at all: also refused.
    let refused = client(&server_ca.pem(), None)
        .get(format!("{base}/programs"))
        .header("authorization", format!("Bearer {BL}"))
        .send()
        .await;
    assert!(
        refused.is_err(),
        "a client with no certificate was served: {refused:?}"
    );
}

#[tokio::test]
async fn an_optional_client_certificate_lets_a_bare_client_through() {
    let server_ca = Authority::new("openadr server CA");
    let (certificate, key) = server_ca.issue(vec!["localhost".into()]);
    let client_ca = Authority::new("openadr client CA");

    let base = serve(
        TlsConfig::from_pem(certificate.as_bytes(), key.as_bytes())
            .unwrap()
            .with_client_ca(client_ca.pem().as_bytes())
            .unwrap()
            .allowing_anonymous_clients(),
    )
    .await;

    let response = client(&server_ca.pem(), None)
        .get(format!("{base}/programs"))
        .header("authorization", format!("Bearer {BL}"))
        .send()
        .await
        .expect("an optional client certificate must not become a required one");
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn a_certificate_and_a_key_that_do_not_match_are_refused_before_the_port_opens() {
    let ca = Authority::new("openadr test CA");
    let (certificate, _) = ca.issue(vec!["localhost".into()]);
    let (_, other_key) = ca.issue(vec!["localhost".into()]);

    let config = TlsConfig::from_pem(certificate.as_bytes(), other_key.as_bytes()).unwrap();
    let e = config.server_config().unwrap_err();
    assert!(
        matches!(&e, TlsError::Config(m) if m.contains("refused")),
        "{e}"
    );
}
