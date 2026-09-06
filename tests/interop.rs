//! This crate's conformance suite, against somebody else's VTN.
//!
//! Everything else under `tests/` measures this implementation, and self-agreement is what people
//! mistake for interoperability. This is the other reading, made reproducible.
//!
//! ```console
//! $ cargo test --all-features --test interop -- --ignored --nocapture
//! $ OPENADR_INTEROP_JSON=matrix.json cargo test --all-features --test interop -- --ignored
//! ```
//!
//! It **does not fail on the peer's divergences** — those are findings about the peer, and the
//! report is the output. It does assert that the run *happened*: a check that could not reach the
//! peer is filed as a failure exactly like one the peer failed, so a misconfigured run reads as a
//! non-conformant peer unless something separates them.
//!
//! The credentials are written straight into the peer's database, because no VTN has an OpenADR
//! endpoint for creating its *first* client. That is the one peer-specific thing here; pointing
//! [`PEER_IMAGE_VAR`] elsewhere means replacing [`Peer::bootstrap`] too.

#![cfg(all(feature = "conformance", feature = "internal-auth", feature = "client"))]

use std::time::Duration;

use openadr::conformance::{Credential, Outcome, Runner, Severity, Target};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{ExecCommand, IntoContainerPort},
    runners::AsyncRunner,
};
use testcontainers_modules::postgres::Postgres;

/// Overrides the peer image, so the same harness measures a different VTN.
const PEER_IMAGE_VAR: &str = "OPENADR_PEER_IMAGE";

/// Written to this path, when set, as the JSON an interoperability matrix is built from.
const JSON_VAR: &str = "OPENADR_INTEROP_JSON";

const POSTGRES_TAG: &str = "17-alpine";
const NETWORK: &str = "oadr-interop";
const PG_HOST: &str = "oadr-interop-pg";
const DB: &str = "openadr";
const SECRET: &str = "interop-secret";
const BL: &str = "interop-bl";
const VEN: &str = "interop-ven";

/// A peer VTN, and what it takes to give it a first client.
struct Peer {
    image: String,
    tag: String,
}

impl Peer {
    fn from_env() -> Self {
        let image = std::env::var(PEER_IMAGE_VAR)
            .unwrap_or_else(|_| "ghcr.io/openleadr/openleadr-rs:latest".to_string());
        // `ghcr.io/owner/name:tag` — the last colon separates the tag, and a colon in the registry's
        // own host:port must not be mistaken for one.
        match image.rsplit_once(':') {
            Some((name, tag)) if !tag.contains('/') => Self {
                image: name.to_string(),
                tag: tag.to_string(),
            },
            _ => Self {
                image,
                tag: "latest".to_string(),
            },
        }
    }

    /// The SQL that gives the peer its first two clients.
    ///
    /// Peer-specific by necessity, and the only such thing here. This peer stores secrets as Argon2
    /// PHC strings — the same format this crate uses — so `InternalAuth::hash_secret` can make one,
    /// with a fresh salt every run so nothing here is a credential anybody could reuse.
    fn bootstrap(hash: &str) -> String {
        format!(
            r#"
INSERT INTO "user" (id, reference, description, created, modified, scopes) VALUES
 ('{BL}','{BL}','conformance', now(), now(),
  ARRAY['read_all','write_programs','write_events','write_subscriptions_bl','write_vens_bl','write_reports']::scope[]),
 ('{VEN}','{VEN}','conformance', now(), now(),
  ARRAY['read_targets','read_ven_objects','write_reports','write_subscriptions_ven','write_vens_ven']::scope[])
ON CONFLICT (id) DO NOTHING;
INSERT INTO user_credentials (user_id, client_id, client_secret) VALUES
 ('{BL}','{BL}','{hash}'), ('{VEN}','{VEN}','{hash}')
ON CONFLICT (client_id) DO UPDATE SET client_secret = EXCLUDED.client_secret;
"#
        )
    }
}

/// A free port on the loopback interface.
///
/// The peer advertises its token endpoint from `GET /auth/server`, and every credentialled check
/// follows that URL — so the externally reachable port has to be known *before* the container
/// starts, which rules out letting Docker choose one. Bind, read, release: a race in principle, and
/// the alternative is a hardcoded port that collides with whatever else is running.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("a free port")
}

/// Wait until the peer answers `GET /auth/server`, which is the endpoint every VTN serves without
/// credentials — so it is the one thing that can be polled before anything has been provisioned.
async fn wait_until_serving(base: &str) -> bool {
    openadr::install_crypto_provider();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("an HTTP client");
    for _ in 0..120 {
        if client
            .get(format!("{base}/auth/server"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// Start Postgres and the peer on one network, and hand back the peer's base URL.
///
/// `None` when there is no Docker. A machine without one is a legitimate place to build this crate;
/// a run that pretended to have measured something is not.
async fn start(
    peer: &Peer,
) -> Option<(
    ContainerAsync<Postgres>,
    ContainerAsync<GenericImage>,
    String,
)> {
    let postgres = Postgres::default()
        .with_user(DB)
        .with_password(DB)
        .with_db_name(DB)
        // Pinned: the module defaults to Postgres 11, and the peer's own queries use
        // `jsonb_path_exists`, which arrived in 12. Left alone it starts, accepts the connection,
        // and panics on the first read.
        .with_tag(POSTGRES_TAG)
        .with_network(NETWORK)
        .with_container_name(PG_HOST)
        .start()
        .await
        .inspect_err(|e| eprintln!("skipping interop: no Docker ({e})"))
        .ok()?;

    // The peer reaches Postgres by container name on the shared network, not through a mapped port.
    let port = free_port();
    let vtn = GenericImage::new(&peer.image, &peer.tag)
        .with_mapped_port(port, 3000.tcp())
        .with_network(NETWORK)
        // The peer publishes no arm64 variant; naming the platform makes the emulation deliberate
        // rather than a pull that fails differently on different machines.
        .with_platform("linux/amd64")
        .with_env_var(
            "DATABASE_URL",
            format!("postgres://{DB}:{DB}@{PG_HOST}:5432/{DB}"),
        )
        // What the peer advertises from `GET /auth/server`. It must be the address a client on
        // *this* side of the port mapping can reach, not the one inside the container.
        .with_env_var(
            "OAUTH_TOKEN_URL",
            format!("http://localhost:{port}/auth/token"),
        )
        .with_startup_timeout(Duration::from_secs(300))
        .start()
        .await
        .inspect_err(|e| eprintln!("skipping interop: the peer image would not start ({e})"))
        .ok()?;

    let base = format!("http://127.0.0.1:{port}");
    if !wait_until_serving(&base).await {
        // Its own logs, not "it never answered". A skip nobody can diagnose is a skip that becomes
        // permanent.
        eprintln!("the peer never answered GET /auth/server at {base}. Its output:");
        for (stream, bytes) in [
            ("stdout", vtn.stdout_to_vec().await),
            ("stderr", vtn.stderr_to_vec().await),
        ] {
            let text = String::from_utf8_lossy(&bytes.unwrap_or_default()).to_string();
            for line in text.lines().rev().take(20).collect::<Vec<_>>().iter().rev() {
                eprintln!("  {stream}: {line}");
            }
        }
        return None;
    }
    Some((postgres, vtn, base))
}

#[tokio::test]
#[ignore = "a measurement against somebody else's VTN: run with --ignored --nocapture"]
async fn the_suite_runs_against_a_peer() {
    let peer = Peer::from_env();
    let Some((postgres, _vtn, base)) = start(&peer).await else {
        return;
    };

    // The peer's first client, written where it can be read. See the note at the top of this file.
    let hash = openadr::vtn::auth::InternalAuth::hash_secret(SECRET).expect("a PHC string");
    let result = postgres
        .exec(ExecCommand::new([
            "psql",
            "-qtU",
            DB,
            "-d",
            DB,
            "-c",
            &Peer::bootstrap(&hash),
        ]))
        .await;
    assert!(
        result.is_ok(),
        "the peer's bootstrap failed — its schema has probably changed: {result:?}"
    );

    let credential = |id: &str| Credential::ClientCredentials {
        id: id.to_string(),
        secret: SECRET.to_string(),
    };
    let target = Target::new(&base)
        .with_business_logic(credential(BL))
        .with_ven(credential(VEN), VEN);

    let report = Runner::new(target)
        .expect("the base URL parses")
        .run()
        .await;
    println!("\n{report}");

    if let Ok(path) = std::env::var(JSON_VAR) {
        std::fs::write(&path, serde_json::to_vec_pretty(&report.to_json()).unwrap())
            .unwrap_or_else(|e| panic!("could not write {path}: {e}"));
        println!("wrote {path}");
    }

    // What is asserted, and what is not.
    //
    // Not asserted: that the peer conforms. Its divergences are findings about it, and a test that
    // went red on them would be this repository's CI reporting somebody else's bug — which is how a
    // useful measurement gets deleted.
    //
    // Asserted: that the measurement *happened*. A check that could not reach the peer is filed as
    // a failure exactly like one the peer failed, so counting attempts is not enough — a wholly
    // misconfigured run looks like a wholly non-conformant peer. Transport failures are ours.
    let (passed, failed, skipped) = report.tally(Severity::Required);
    println!("required: {passed} passed, {failed} failed, {skipped} skipped");

    let unreachable: Vec<&str> = report
        .failures()
        .filter(|f| match &f.outcome {
            Outcome::Failed(why) => why.contains("the request could not be made"),
            _ => false,
        })
        .map(|f| f.check.id)
        .collect();
    assert!(
        unreachable.is_empty(),
        "{} check(s) never reached {}:{} — that is this harness's fault, not the peer's: {unreachable:?}",
        unreachable.len(),
        peer.image,
        peer.tag
    );
    assert!(
        passed + failed >= 20,
        "only {} required checks were attempted — the suite did not run",
        passed + failed
    );
}
