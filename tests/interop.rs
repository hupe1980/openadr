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
//! peer is a skip, so a misconfigured run would read as a peer that was barely measured unless
//! something counts the skips.
//!
//! The credentials are written straight into the peer's database, because no VTN has an OpenADR
//! endpoint for creating its *first* client. That is the one peer-specific thing here; pointing
//! [`PEER_IMAGE_VAR`] elsewhere means replacing [`Peer::bootstrap`] too.

#![cfg(all(feature = "conformance", feature = "internal-auth", feature = "client"))]

use std::{path::PathBuf, time::Duration};

use openadr::conformance::{Credential, Outcome, RunMeta, Runner, Severity, Target};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{CmdWaitFor, ExecCommand, IntoContainerPort},
    runners::AsyncRunner,
};
use testcontainers_modules::postgres::Postgres;

/// Overrides the peer image, so the same harness measures a different VTN.
const PEER_IMAGE_VAR: &str = "OPENADR_PEER_IMAGE";

/// Written to this path, when set, as the JSON an interoperability matrix is built from.
const JSON_VAR: &str = "OPENADR_INTEROP_JSON";

const POSTGRES_TAG: &str = "17-alpine";
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
  ARRAY['read_all','write_programs','write_events','write_subscriptions_bl','write_vens_bl','write_reports_bl']::scope[]),
 ('{VEN}','{VEN}','conformance', now(), now(),
  ARRAY['read_targets','read_ven_objects','write_reports_ven','write_subscriptions_ven','write_vens_ven']::scope[])
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
    openadr::install_crypto_provider().unwrap();
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
async fn start(
    peer: &Peer,
) -> (
    ContainerAsync<Postgres>,
    ContainerAsync<GenericImage>,
    String,
) {
    let network = format!("oadr-interop-{}", uuid::Uuid::now_v7());
    let pg_host = format!("{network}-pg");
    let postgres = Postgres::default()
        .with_user(DB)
        .with_password(DB)
        .with_db_name(DB)
        // Pinned: the module defaults to Postgres 11, and the peer's own queries use
        // `jsonb_path_exists`, which arrived in 12. Left alone it starts, accepts the connection,
        // and panics on the first read.
        .with_tag(POSTGRES_TAG)
        .with_network(&network)
        .with_container_name(&pg_host)
        .start()
        .await
        .expect("interop measurement requires a running PostgreSQL container");

    // The peer reaches Postgres by container name on the shared network, not through a mapped port.
    let port = free_port();
    let vtn = GenericImage::new(&peer.image, &peer.tag)
        .with_mapped_port(port, 3000.tcp())
        .with_network(&network)
        // The peer publishes no arm64 variant; naming the platform makes the emulation deliberate
        // rather than a pull that fails differently on different machines.
        .with_platform("linux/amd64")
        .with_env_var(
            "DATABASE_URL",
            format!("postgres://{DB}:{DB}@{pg_host}:5432/{DB}"),
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
        .expect("interop measurement requires the peer VTN container to start");

    let base = format!("http://127.0.0.1:{port}");
    assert!(
        wait_until_serving(&base).await,
        "peer never became ready at {base}/auth/server"
    );
    (postgres, vtn, base)
}

#[tokio::test]
#[ignore = "a measurement against somebody else's VTN: run with --ignored --nocapture"]
async fn the_suite_runs_against_a_peer() {
    let peer = Peer::from_env();
    let (postgres, _vtn, base) = start(&peer).await;

    // The peer's first client, written where it can be read. See the note at the top of this file.
    let hash = openadr::vtn::auth::InternalAuth::hash_secret(SECRET).expect("a PHC string");
    postgres
        .exec(
            ExecCommand::new([
                "psql",
                "-v",
                "ON_ERROR_STOP=1",
                "-qtU",
                DB,
                "-d",
                DB,
                "-c",
                &Peer::bootstrap(&hash),
            ])
            .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )
        .await
        .expect("peer database bootstrap must succeed before measuring");

    let credential = |id: &str| Credential::ClientCredentials {
        id: id.to_string(),
        secret: SECRET.to_string(),
    };
    let target = Target::new(&base)
        .with_business_logic(credential(BL))
        .with_ven(credential(VEN), VEN);

    let mut report = Runner::new(target)
        .expect("the base URL parses")
        .run()
        .await;
    // What is asserted, and what is not.
    //
    // Not asserted: that the peer conforms. Its divergences are findings about it, and a test that
    // went red on them would be this repository's CI reporting somebody else's bug — which is how a
    // useful measurement gets deleted.
    //
    // Asserted: that the measurement *happened*. A check that could not reach the peer is a skip,
    // so a wholly misconfigured run looks like a barely measured peer. Transport failures are ours.
    let (passed, failed, skipped) = report.tally(Severity::Required);
    println!("required: {passed} passed, {failed} failed, {skipped} skipped");

    let unreachable: Vec<&str> = report
        .findings
        .iter()
        .filter(|f| match &f.outcome {
            Outcome::Skipped(why) => why.contains("could not be reached"),
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
    report.target = format!("{}:{}", peer.image, peer.tag);
    if let Some(path) = std::env::var_os(JSON_VAR).map(PathBuf::from) {
        assert_eq!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("json"),
            "{JSON_VAR} must name a .json file"
        );
        let meta = RunMeta {
            generated: openadr::model::Timestamp::now().to_string(),
            suite_version: env!("CARGO_PKG_VERSION").into(),
            spec_version: openadr::SPEC_VERSION.into(),
        };
        let mut json = report.to_json();
        json["generated"] = meta.generated.clone().into();
        json["suiteVersion"] = meta.suite_version.clone().into();
        json["specVersion"] = meta.spec_version.clone().into();
        let html = path.with_extension("html");
        std::fs::write(&html, report.to_html(&meta)).expect("write HTML interop report");
        std::fs::write(&path, serde_json::to_vec_pretty(&json).unwrap())
            .expect("write JSON interop report");
        println!("reports: {} and {}", path.display(), html.display());
    } else {
        println!("{report}");
    }
}
