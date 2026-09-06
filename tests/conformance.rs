//! The conformance suite, run against this VTN.
//!
//! Two things are being tested here at once, and only one of them is the VTN.
//!
//! The obvious one: this implementation should pass its own suite. If it does not, either the
//! implementation is wrong or the check is, and both are worth knowing before the suite is pointed
//! at somebody else's server and used to tell them their VTN is broken.
//!
//! The less obvious one, and the reason the negative cases below exist: a suite that passes
//! everything is indistinguishable from a suite that checks nothing. So a deliberately weakened
//! VTN is run through it as well, and the checks that ought to catch each weakening are named. A
//! check that cannot fail is not a check.

// `internal-auth` as well, so the harness VTN runs a real `POST /auth/token`. The suite exercises
// the client-credentials exchange rather than a pre-shared token, and the token endpoint's own
// checks have an endpoint to inspect instead of a `501` — a skip against a VTN this file calls
// "fully configured" would be exactly the absence of evidence the suite exists to refuse.
#![cfg(all(feature = "vtn", feature = "conformance", feature = "internal-auth"))]

use std::sync::Arc;

use openadr::{
    conformance::{Credential, Outcome, Runner, Severity, Target, checks},
    vtn::{
        Vtn, VtnConfig,
        auth::{InternalAuth, Scope, Scopes},
        notify::{Notifiers, RecordingNotifier},
        store::MemoryStorage,
    },
};

const BL: &str = "bl-secret";
const VEN: &str = "ven-secret";
const BL_CLIENT: &str = "bl-client-1";
const VEN_CLIENT: &str = "ven-client-1";

/// Start a VTN and return its base URL. `configure` bends it.
///
/// The broker binding is advertised so the MQTT checks *run* rather than skipping. Nothing
/// publishes to it in these tests — the checks are about the discovery endpoints and their access
/// control, which is what a peer's client actually has to interoperate with.
async fn start(configure: impl FnOnce(VtnConfig) -> VtnConfig) -> String {
    // Bind first: the token endpoint this VTN issues from is the one it *advertises* through
    // `GET /auth/server`, and a client following that link has to reach a listener that exists. An
    // authenticator built before the port is known can only advertise a guess.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}/openadr3/3.1.0");

    let config = configure(VtnConfig {
        base_path: "/openadr3/3.1.0".into(),
        mqtt_topic_prefix: "openadr3".into(),
        ..Default::default()
    });
    let vtn = Vtn::builder()
        .storage(MemoryStorage::shared())
        .authenticator(Arc::new(
            InternalAuth::builder(format!("{base}/auth/token"))
                .client(BL_CLIENT, BL, Scopes::new(Scope::BUSINESS_LOGIC))
                .unwrap()
                .client(VEN_CLIENT, VEN, Scopes::new(Scope::VEN))
                .unwrap()
                .build(),
        ))
        // A webhook transport, so subscriptions are accepted; the suite reads them.
        .notifier(Notifiers::new().with(RecordingNotifier::shared()).shared())
        .config(config)
        .mqtt(openadr::model::MqttNotifierBinding {
            uris: vec!["mqtts://broker.test:8883".into()],
            serialization: openadr::model::Serialization::Json,
            authentication: openadr::model::MqttAuthentication::Oauth2BearerToken {
                username: "{clientID}".into(),
            },
        })
        .build();

    let router = vtn.router();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    base
}

fn bl_credential() -> Credential {
    Credential::ClientCredentials {
        id: BL_CLIENT.into(),
        secret: BL.into(),
    }
}

fn ven_credential() -> Credential {
    Credential::ClientCredentials {
        id: VEN_CLIENT.into(),
        secret: VEN.into(),
    }
}

fn full_target(base: &str) -> Target {
    Target::new(base)
        .with_business_logic(bl_credential())
        .with_ven(ven_credential(), VEN_CLIENT)
}

#[tokio::test]
async fn this_vtn_passes_its_own_suite() {
    let base = start(|c| c).await;
    let report = Runner::new(full_target(&base)).unwrap().run().await;

    assert!(
        report.is_conformant(),
        "this VTN failed its own conformance suite:\n{report}"
    );

    // Nothing may be skipped: a fully-credentialled run against a fully-featured VTN has no excuse,
    // and a suite whose checks quietly skip is a suite that measures nothing.
    let skipped: Vec<&str> = report
        .findings
        .iter()
        .filter(|f| f.outcome.is_skipped())
        .map(|f| f.check.id)
        .collect();
    assert!(
        skipped.is_empty(),
        "checks skipped against a fully-configured VTN: {skipped:?}\n{report}"
    );

    // Including the extensions, which this implementation is where they come from.
    let (passed, failed, _) = report.tally(Severity::Extension);
    assert_eq!(failed, 0, "an extension check failed:\n{report}");
    assert!(passed > 0);
}

#[tokio::test]
async fn the_suite_cleans_up_after_itself() {
    // A suite that leaves `oadr-conformance-*` objects behind is a suite nobody runs twice, and one
    // whose second run fails on its own uniqueness constraints.
    let base = start(|c| c).await;
    let runner = Runner::new(full_target(&base)).unwrap();
    runner.run().await;

    let client = openadr::client::Client::<openadr::client::BusinessLogic>::builder(&base)
        .unwrap()
        .credentials(openadr::client::Credentials::new(BL_CLIENT, BL))
        .build()
        .unwrap();

    // Reports and resources included: the suite writes both, and a report is the one object a VTN
    // never deletes on its own — nothing cascades to it once its event is gone if the event's
    // delete came first.
    for collection in ["programs", "events", "vens", "resources", "reports"] {
        let remaining: serde_json::Value = client
            .get_json(collection, &openadr::client::Query::new().limit(50))
            .await
            .unwrap();
        let litter: Vec<String> = remaining
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter(|o| o.to_string().contains("oadr-conformance-"))
                    .map(|o| o["id"].as_str().unwrap_or("?").to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            litter.is_empty(),
            "the suite left {} object(s) behind in /{collection}: {litter:?}",
            litter.len()
        );
    }

    // And a second run on the same VTN still passes, which is the property cleanup exists for.
    let second = Runner::new(full_target(&base)).unwrap().run().await;
    assert!(second.is_conformant(), "the second run failed:\n{second}");
}

#[tokio::test]
async fn a_run_without_ven_credentials_skips_rather_than_passes() {
    // The failure mode a conformance suite has to avoid: reporting "all green" for a run that never
    // attempted the half that matters.
    let base = start(|c| c).await;
    let target = Target::new(&base).with_business_logic(bl_credential());
    let report = Runner::new(target).unwrap().run().await;

    let skipped: Vec<&str> = report
        .findings
        .iter()
        .filter(|f| f.outcome.is_skipped())
        .map(|f| f.check.id)
        .collect();
    assert!(
        skipped.contains(&"target-hiding-on-reads"),
        "object privacy was not skipped without a VEN credential: {skipped:?}"
    );
    assert!(report.is_conformant(), "{report}");

    // And the report says so out loud rather than only in the total.
    let rendered = report.to_string();
    assert!(rendered.contains("skip"));
    assert!(rendered.contains("no VEN credential was supplied"));
}

#[tokio::test]
async fn a_run_with_no_credentials_at_all_still_checks_what_it_can() {
    let base = start(|c| c).await;
    let report = Runner::new(Target::new(&base)).unwrap().run().await;

    let attempted: Vec<&str> = report
        .findings
        .iter()
        .filter(|f| !f.outcome.is_skipped())
        .map(|f| f.check.id)
        .collect();
    assert!(
        attempted.contains(&"auth-server-unauthenticated"),
        "the unauthenticated endpoint was not checked: {attempted:?}"
    );
    assert!(
        attempted.contains(&"unauthenticated-write-refused"),
        "{attempted:?}"
    );
    // Everything a run with no credentials can reach, and nothing else: the three unauthenticated
    // endpoints plus the challenge a refusal has to carry. Asserting the exact set is what stops a
    // check that *should* need a credential from quietly running without one.
    let mut attempted = attempted;
    attempted.sort_unstable();
    assert_eq!(
        attempted,
        [
            "auth-server-unauthenticated",
            "token-response-is-not-stored",
            "unauthenticated-write-refused",
            "unauthorized-names-the-scheme",
        ],
        "{report}"
    );
}

// ---------------------------------------------------------------------------
// A suite that cannot fail is not a suite
// ---------------------------------------------------------------------------

/// Run against a VTN with one behaviour switched off, and return which checks failed.
async fn failures_with(configure: impl FnOnce(VtnConfig) -> VtnConfig) -> Vec<String> {
    let base = start(configure).await;
    let report = Runner::new(full_target(&base)).unwrap().run().await;
    report.failures().map(|f| f.check.id.to_string()).collect()
}

#[tokio::test]
async fn switching_off_caching_is_caught() {
    let failed = failures_with(|c| VtnConfig {
        http_caching: false,
        ..c
    })
    .await;
    assert!(
        failed.contains(&"etag-and-conditional-read".to_string()),
        "a VTN with no ETag passed the caching check: {failed:?}"
    );
    // And nothing else, because caching is an extension and it is the only thing that changed.
    assert_eq!(failed.len(), 1, "{failed:?}");
}

#[tokio::test]
async fn switching_off_the_programme_name_lookup_is_caught() {
    let failed = failures_with(|c| VtnConfig {
        program_name_lookup: false,
        ..c
    })
    .await;
    assert_eq!(
        failed,
        vec!["program-name-lookup".to_string()],
        "an extension that was switched off was not detected: {failed:?}"
    );
}

/// Serve the suite a VTN whose responses have had one header stripped.
///
/// There is no configuration switch for any of these: the VTN always sets them. A layer that
/// removes the header is what an implementation that never set it looks like from the outside,
/// which is the only vantage point the suite has `[D-072]`.
async fn failures_without_headers(
    header_names: &'static [&'static str],
    only_on: Option<&'static str>,
) -> Vec<String> {
    let base = start(|c| c).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let proxy_base = format!("http://{addr}/openadr3/3.1.0");
    let upstream = base.trim_end_matches("/openadr3/3.1.0").to_string();

    let app = axum::Router::new().fallback(axum::routing::any(
        move |request: axum::extract::Request| {
            let upstream = upstream.clone();
            async move { relay(&upstream, request, header_names, only_on).await }
        },
    ));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let report = Runner::new(full_target(&proxy_base)).unwrap().run().await;
    report.failures().map(|f| f.check.id.to_string()).collect()
}

/// Forward a request upstream and hand back the answer with one header removed.
async fn relay(
    upstream: &str,
    request: axum::extract::Request,
    header_names: &[&str],
    only_on: Option<&str>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let url = format!(
        "{upstream}{}",
        parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
    );

    let client = reqwest::Client::new();
    let mut outbound = client.request(parts.method.clone(), &url).body(bytes);
    for (name, value) in parts.headers.iter() {
        if name != axum::http::header::HOST {
            outbound = outbound.header(name, value);
        }
    }
    let Ok(response) = outbound.send().await else {
        return axum::http::StatusCode::BAD_GATEWAY.into_response();
    };

    let status = response.status();
    let mut headers = response.headers().clone();
    if only_on.is_none_or(|path| parts.uri.path().ends_with(path)) {
        for name in header_names {
            headers.remove(*name);
        }
    }
    // Whatever the length was, the body is being re-sent as one chunk.
    headers.remove(axum::http::header::CONTENT_LENGTH);
    headers.remove(axum::http::header::TRANSFER_ENCODING);
    headers.remove(axum::http::header::CONTENT_ENCODING);
    let body = response.bytes().await.unwrap_or_default();
    (status, headers, body).into_response()
}

#[tokio::test]
async fn a_401_with_no_challenge_is_caught() {
    let failed = failures_without_headers(&["www-authenticate"], None).await;
    assert!(
        failed.contains(&"unauthorized-names-the-scheme".to_string()),
        "a 401 with no WWW-Authenticate passed: {failed:?}"
    );
}

#[tokio::test]
async fn a_token_endpoint_that_may_be_stored_is_caught() {
    let failed = failures_without_headers(&["cache-control"], Some("/auth/token")).await;
    assert!(
        failed.contains(&"token-response-is-not-stored".to_string()),
        "a token response with no Cache-Control passed: {failed:?}"
    );
    // And only that one: the header was removed from the token endpoint alone.
    assert_eq!(failed.len(), 1, "{failed:?}");
}

#[tokio::test]
async fn a_read_with_no_caching_policy_is_caught() {
    // Both, because the check accepts either: a VTN that says nothing about caching *and* nothing
    // about what the body varies on is the one a shared cache can key by URL alone.
    let failed = failures_without_headers(&["cache-control", "vary"], Some("/programs")).await;
    assert!(
        failed.contains(&"reads-declare-a-caching-policy".to_string()),
        "a read with no Cache-Control and no Vary passed: {failed:?}"
    );
}

#[tokio::test]
async fn a_vtn_that_reads_any_media_type_is_caught() {
    // The break is at the *request* end, so the relay above cannot make it. This one rewrites the
    // inbound Content-Type to application/json, which is exactly what a VTN that ignores the header
    // does.
    let base = start(|c| c).await;
    let upstream = base.trim_end_matches("/openadr3/3.1.0").to_string();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let proxy_base = format!("http://{addr}/openadr3/3.1.0");

    let app = axum::Router::new().fallback(axum::routing::any(
        move |mut request: axum::extract::Request| {
            let upstream = upstream.clone();
            async move {
                if request
                    .headers()
                    .contains_key(axum::http::header::CONTENT_TYPE)
                {
                    request.headers_mut().insert(
                        axum::http::header::CONTENT_TYPE,
                        axum::http::HeaderValue::from_static("application/json"),
                    );
                }
                relay(&upstream, request, &[], None).await
            }
        },
    ));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let report = Runner::new(full_target(&proxy_base)).unwrap().run().await;
    let failed: Vec<&str> = report.failures().map(|f| f.check.id).collect();
    assert!(
        failed.contains(&"foreign-media-type-is-refused"),
        "a VTN that reads any media type as JSON passed: {failed:?}"
    );
}

#[tokio::test]
async fn every_check_is_reachable_and_ordered() {
    // `run_one` dispatches by id, so a check listed and never implemented would report "listed but
    // not implemented" rather than failing to compile. This is what closes that gap.
    let base = start(|c| c).await;
    let report = Runner::new(full_target(&base)).unwrap().run().await;

    assert_eq!(
        report.findings.len(),
        checks().len(),
        "the report has a different number of findings than there are checks"
    );
    for (finding, check) in report.findings.iter().zip(checks()) {
        assert_eq!(
            finding.check.id, check.id,
            "the report is not in the order the checks are listed"
        );
        if let Outcome::Skipped(reason) = &finding.outcome {
            assert!(
                !reason.contains("not implemented"),
                "{} is listed but has no implementation",
                check.id
            );
        }
    }
}

#[tokio::test]
async fn a_conformance_run_is_reportable_as_json() {
    // The output a published interoperability matrix is built from.
    let base = start(|c| c).await;
    let report = Runner::new(full_target(&base)).unwrap().run().await;
    let json = report.to_json();

    assert_eq!(json["conformant"], true);
    assert_eq!(
        json["findings"].as_array().unwrap().len(),
        checks().len(),
        "the JSON dropped findings"
    );
    for finding in json["findings"].as_array().unwrap() {
        assert!(finding["id"].is_string());
        assert!(finding["clause"].is_string());
        assert!(matches!(
            finding["outcome"].as_str(),
            Some("passed" | "failed" | "skipped")
        ));
    }
}

#[tokio::test]
async fn a_vtn_with_no_broker_skips_the_mqtt_checks_rather_than_failing_them() {
    // MQTT is optional: "A VTN **MAY** support notifications via MQTT". Reporting its absence as
    // non-conformance would make the suite unusable against most of the field, and would be wrong.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}/openadr3/3.1.0");
    let vtn = Vtn::builder()
        .storage(MemoryStorage::shared())
        .authenticator(Arc::new(
            InternalAuth::builder(format!("{base}/auth/token"))
                .client(BL_CLIENT, BL, Scopes::new(Scope::BUSINESS_LOGIC))
                .unwrap()
                .client(VEN_CLIENT, VEN, Scopes::new(Scope::VEN))
                .unwrap()
                .build(),
        ))
        .notifier(Notifiers::new().with(RecordingNotifier::shared()).shared())
        .config(VtnConfig {
            base_path: "/openadr3/3.1.0".into(),
            ..Default::default()
        })
        .build();
    let router = vtn.router();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let report = Runner::new(full_target(&base)).unwrap().run().await;
    assert!(report.is_conformant(), "{report}");

    let mqtt: Vec<&openadr::conformance::Finding> = report
        .findings
        .iter()
        .filter(|f| f.check.id.starts_with("mqtt-"))
        .collect();
    assert!(!mqtt.is_empty());
    for finding in mqtt {
        assert!(
            finding.outcome.is_skipped(),
            "{} was not skipped on a VTN with no broker: {:?}",
            finding.check.id,
            finding.outcome
        );
    }
    assert!(
        report
            .to_string()
            .contains("offers no MQTT notifier binding")
    );
}

#[tokio::test]
async fn a_report_filter_that_is_accepted_and_ignored_is_caught() {
    // The D-045 shape, in someone else's VTN: `?eventID=` is parsed, carried into the query and
    // never read, so a settlement process asking for one event's reports quietly receives every
    // report in the system. Nothing errors at either end.
    //
    // There is no configuration switch for this, so the VTN is broken with a layer that strips the
    // parameter before it reaches the router — which is exactly what an implementation that forgot
    // to apply it does.
    use axum::http::Uri;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}/openadr3/3.1.0");
    let vtn = Vtn::builder()
        .storage(MemoryStorage::shared())
        .authenticator(Arc::new(
            InternalAuth::builder(format!("{base}/auth/token"))
                .client(BL_CLIENT, BL, Scopes::new(Scope::BUSINESS_LOGIC))
                .unwrap()
                .client(VEN_CLIENT, VEN, Scopes::new(Scope::VEN))
                .unwrap()
                .build(),
        ))
        .notifier(Notifiers::new().with(RecordingNotifier::shared()).shared())
        .config(VtnConfig {
            base_path: "/openadr3/3.1.0".into(),
            ..Default::default()
        })
        .build();

    let router = vtn.router().layer(axum::middleware::from_fn(
        |mut request: axum::extract::Request, next: axum::middleware::Next| async move {
            if request.uri().path().ends_with("/reports") {
                let mut parts = request.uri().clone().into_parts();
                parts.path_and_query = request.uri().path().parse().ok();
                if let Ok(stripped) = Uri::from_parts(parts) {
                    *request.uri_mut() = stripped;
                }
            }
            next.run(request).await
        },
    ));

    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let report = Runner::new(full_target(&base)).unwrap().run().await;
    let failed: Vec<&str> = report.failures().map(|f| f.check.id).collect();
    assert!(
        failed.contains(&"report-filters-by-event"),
        "a VTN that ignores ?eventID= passed the filter check: {report}"
    );
}
