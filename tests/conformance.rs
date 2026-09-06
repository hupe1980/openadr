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

use axum::http::StatusCode;
use futures_util::StreamExt as _;
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

/// How a VTN is broken for one run.
///
/// Five shapes, because that is what it takes to reach the behaviours from outside. A conformance
/// suite has no vantage point inside the server, so a defect has to be *simulated* on the wire —
/// and a simulation that reaches only the behaviours with a configuration switch would measure the
/// switches rather than the checks.
enum Break {
    /// A switch the VTN itself has.
    Config(fn(VtnConfig) -> VtnConfig),
    /// The request, on its way in: what a VTN that ignores a parameter or a header looks like.
    Request(fn(&str, &mut String, &mut axum::http::HeaderMap)),
    /// The status, on its way out.
    Status(fn(&axum::http::Method, &str, StatusCode) -> StatusCode),
    /// The response headers.
    Header(fn(&str, StatusCode, &mut axum::http::HeaderMap)),
    /// The JSON body, parsed and re-serialised. The *request* body comes too, because the
    /// interesting defects here are the ones where a VTN believes what a client told it.
    Body(fn(&axum::http::Method, &str, StatusCode, &serde_json::Value, &mut serde_json::Value)),
    /// Every credential is read as business logic.
    ///
    /// A variant of its own rather than a `Request` rewrite, because it needs something no `fn`
    /// pointer can carry: a real business-logic token, fetched from the VTN before the proxy
    /// starts. This is D-121 as an outsider sees it — the defect this project shipped, where a
    /// scope was read as an *identity* and a monitoring credential became the one role object
    /// privacy does not apply to.
    EveryCredentialIsBusinessLogic,
}

/// One way a VTN can be wrong, and the checks that must catch it.
///
/// `caught_by` is exact in both directions. A check missing from it is a check that did not notice
/// the defect it exists for; a check listed that also fires on some *other* fault is a check whose
/// failure does not mean what its title says. Both are worth failing the build over, and the second
/// is the one nobody looks for.
struct Fault {
    /// What is broken, in the words an operator would use.
    name: &'static str,
    /// How the break is made.
    how: Break,
    /// Exactly the checks that must fail.
    caught_by: &'static [&'static str],
}

/// The defects this suite has been shown to catch.
///
/// Every entry is a real implementation mistake rather than an invented one: five of them were
/// found in a *peer* VTN by running this suite against it, and the rest are shapes this project has
/// made itself and recorded in `concepts/DECISIONS.md`.
const FAULTS: &[Fault] = &[
    // -- switches the VTN has ------------------------------------------------
    Fault {
        name: "no ETag on reads",
        how: Break::Config(|c| VtnConfig {
            http_caching: false,
            ..c
        }),
        caught_by: &["etag-and-conditional-read"],
    },
    Fault {
        name: "no programName lookup",
        how: Break::Config(|c| VtnConfig {
            program_name_lookup: false,
            ..c
        }),
        caught_by: &["program-name-lookup"],
    },
    // -- a parameter that is accepted and ignored ----------------------------
    //
    // D-045's shape, and the one the suite exists to make visible: nothing errors, and the caller
    // believes it has filtered.
    Fault {
        name: "?eventID= is accepted and ignored",
        how: Break::Request(|_, target, _| strip_param(target, "eventID")),
        caught_by: &["report-filters-by-event"],
    },
    Fault {
        name: "?targets= is accepted and ignored",
        how: Break::Request(|_, target, _| strip_param(target, "targets")),
        // Not `ungranted-targets-are-invisible`: with the parameter gone the VEN names no targets
        // at all, so the targeted event is hidden and the check passes — correctly, for a reason
        // that is not the one it is about. A fault a check legitimately does not see is a fact
        // about the check worth writing down rather than an omission to paper over.
        caught_by: &[
            "granted-targets-are-visible",
            "target-hiding-on-reads",
            "targets-accept-both-forms",
        ],
    },
    Fault {
        name: "?programID= is accepted and ignored",
        how: Break::Request(|_, target, _| strip_param(target, "programID")),
        caught_by: &["filters-are-additive"],
    },
    Fault {
        name: "?skip= is accepted and ignored",
        how: Break::Request(|_, target, _| strip_param(target, "skip")),
        caught_by: &["pagination-is-complete-and-ordered"],
    },
    Fault {
        name: "?active= is accepted and ignored",
        how: Break::Request(|_, target, _| strip_param(target, "active")),
        caught_by: &["active-excludes-transpired-events"],
    },
    Fault {
        name: "?limit= beyond the maximum is clamped rather than refused",
        how: Break::Request(|_, target, _| {
            *target = target.replace("limit=500", "limit=50");
        }),
        caught_by: &["limit-is-capped-at-fifty"],
    },
    Fault {
        name: "the Content-Type of a request body is not read",
        how: Break::Request(|_, _, headers| {
            if headers.contains_key(axum::http::header::CONTENT_TYPE) {
                headers.insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/json"),
                );
            }
        }),
        caught_by: &["foreign-media-type-is-refused"],
    },
    // -- headers a VTN never set ---------------------------------------------
    Fault {
        name: "a 401 carries no WWW-Authenticate",
        how: Break::Header(|_, status, headers| {
            if status == StatusCode::UNAUTHORIZED {
                headers.remove("www-authenticate");
            }
        }),
        caught_by: &["unauthorized-names-the-scheme"],
    },
    Fault {
        name: "a token response may be stored",
        how: Break::Header(|path, _, headers| {
            if path.ends_with("/auth/token") {
                headers.remove(axum::http::header::CACHE_CONTROL);
            }
        }),
        caught_by: &["token-response-is-not-stored"],
    },
    Fault {
        name: "a read says nothing about caching",
        how: Break::Header(|path, _, headers| {
            if path.ends_with("/programs") {
                headers.remove(axum::http::header::CACHE_CONTROL);
                headers.remove(axum::http::header::VARY);
            }
        }),
        caught_by: &["reads-declare-a-caching-policy"],
    },
    Fault {
        name: "errors are plain JSON rather than problem+json",
        how: Break::Header(|_, status, headers| {
            if status.is_client_error() || status.is_server_error() {
                headers.insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/json"),
                );
            }
        }),
        caught_by: &["problem-uses-the-rfc-9457-media-type"],
    },
    // -- statuses ------------------------------------------------------------
    Fault {
        name: "a missing programme answers 403",
        how: Break::Status(|method, path, status| {
            if status == StatusCode::NOT_FOUND
                && method == axum::http::Method::GET
                && path.contains("/programs/")
            {
                return StatusCode::FORBIDDEN;
            }
            status
        }),
        // Three checks read a programme by id and expect a 404, so three notice. That is not a
        // duplication to trim: `problem-uses-the-rfc-9457-media-type` is about the media type of an
        // error and `programme-delete-leaves-no-orphan` about a cascade, and both need a 404 to
        // *get* to what they are about.
        caught_by: &[
            "missing-object-is-404-problem",
            "problem-uses-the-rfc-9457-media-type",
            "programme-delete-leaves-no-orphan",
        ],
    },
    Fault {
        name: "an object hidden by targeting answers 403, confirming it exists",
        how: Break::Status(|method, path, status| {
            if status == StatusCode::NOT_FOUND
                && method == axum::http::Method::GET
                && path.contains("/events/")
            {
                return StatusCode::FORBIDDEN;
            }
            status
        }),
        caught_by: &[
            "hidden-object-is-404-not-403",
            "programme-delete-leaves-no-orphan",
        ],
    },
    Fault {
        name: "a duplicate programName is accepted",
        how: Break::Status(|method, path, status| {
            if status == StatusCode::CONFLICT
                && method == axum::http::Method::POST
                && path.ends_with("/programs")
            {
                return StatusCode::CREATED;
            }
            status
        }),
        caught_by: &["program-name-is-unique"],
    },
    Fault {
        name: "an event naming no programme is accepted",
        how: Break::Status(|method, path, status| {
            if status == StatusCode::BAD_REQUEST
                && method == axum::http::Method::POST
                && path.ends_with("/events")
            {
                return StatusCode::CREATED;
            }
            status
        }),
        // Only this one: `malformed-body-is-400-problem` posts to `/programs`, so a break confined
        // to `/events` leaves it alone.
        caught_by: &["event-needs-a-programme"],
    },
    Fault {
        name: "a VEN's topic request for another VEN is granted",
        how: Break::Status(|method, path, status| {
            if method == axum::http::Method::GET && path.contains("/notifiers/mqtt/topics/vens/") {
                return StatusCode::OK;
            }
            status
        }),
        caught_by: &["mqtt-foreign-ven-topics-refused"],
    },
    // -- bodies --------------------------------------------------------------
    Fault {
        name: "GET /notifiers omits the WEBHOOK key",
        how: Break::Body(|_, path, _, _, body| {
            if path.ends_with("/notifiers")
                && let Some(object) = body.as_object_mut()
            {
                object.remove("WEBHOOK");
            }
        }),
        caught_by: &["notifiers-webhook-key"],
    },
    Fault {
        name: "the MQTT binding names no URIS",
        how: Break::Body(|_, path, _, _, body| {
            if path.ends_with("/notifiers")
                && let Some(mqtt) = body.get_mut("MQTT").and_then(|m| m.as_object_mut())
            {
                mqtt.remove("URIS");
            }
        }),
        caught_by: &["mqtt-binding-shape"],
    },
    Fault {
        name: "a created object carries no objectType",
        how: Break::Body(|method, path, status, _, body| {
            if method == axum::http::Method::POST
                && status == StatusCode::CREATED
                && path.ends_with("/programs")
                && let Some(object) = body.as_object_mut()
            {
                object.remove("objectType");
            }
        }),
        caught_by: &["create-stamps-metadata"],
    },
    Fault {
        name: "PUT leaves modificationDateTime where it was",
        how: Break::Body(|method, path, _, _, body| {
            if method == axum::http::Method::PUT
                && path.contains("/programs/")
                && let Some(created) = body.get("createdDateTime").cloned()
                && let Some(object) = body.as_object_mut()
            {
                object.insert("modificationDateTime".into(), created);
            }
        }),
        caught_by: &["update-moves-modification-time"],
    },
    Fault {
        name: "P9999Y is normalised into an ordinary 9999-year span",
        how: Break::Body(|_, _, _, _, body| {
            rewrite_strings(body, |s| {
                if s == "P9999Y" {
                    Some("P9999Y0M0DT0H0M0S".to_string())
                } else {
                    None
                }
            })
        }),
        caught_by: &["forever-duration-round-trips"],
    },
    Fault {
        name: "the 0001-01-01 sentinel is resolved to an ordinary date on the way out",
        how: Break::Body(|_, _, _, _, body| {
            rewrite_strings(body, |s| {
                s.starts_with("0001-01-01")
                    .then(|| "2026-01-01T00:00:00Z".to_string())
            })
        }),
        caught_by: &["now-sentinel-round-trips"],
    },
    Fault {
        name: "a problem body carries no instance",
        how: Break::Body(|_, _, status, _, body| {
            if (status.is_client_error() || status.is_server_error())
                && let Some(object) = body.as_object_mut()
            {
                object.remove("instance");
            }
        }),
        caught_by: &["problem-carries-a-traceable-instance"],
    },
    // -- refusals that were not refusals -------------------------------------
    Fault {
        name: "an unauthenticated write is accepted",
        how: Break::Status(|method, path, status| {
            if status == StatusCode::UNAUTHORIZED
                && method == axum::http::Method::POST
                && path.ends_with("/programs")
            {
                return StatusCode::CREATED;
            }
            status
        }),
        caught_by: &["unauthenticated-write-refused"],
    },
    Fault {
        name: "a body missing a required field is accepted",
        how: Break::Status(|method, path, status| {
            if status == StatusCode::BAD_REQUEST
                && method == axum::http::Method::POST
                && path.ends_with("/programs")
            {
                return StatusCode::CREATED;
            }
            status
        }),
        caught_by: &["malformed-body-is-400-problem"],
    },
    Fault {
        name: "a report naming no event is accepted",
        how: Break::Status(|method, path, status| {
            if status == StatusCode::BAD_REQUEST
                && method == axum::http::Method::POST
                && path.ends_with("/reports")
            {
                return StatusCode::CREATED;
            }
            status
        }),
        caught_by: &["report-needs-an-event"],
    },
    Fault {
        name: "a VEN obtains a collection-wide topic",
        how: Break::Status(|method, path, status| {
            if method == axum::http::Method::GET
                && path.contains("/notifiers/mqtt/topics/")
                && !path.contains("/topics/vens/")
            {
                return StatusCode::OK;
            }
            status
        }),
        caught_by: &["mqtt-collection-topics-are-business-logic-only"],
    },
    // -- a VTN that believes what a client tells it ---------------------------
    Fault {
        name: "a client-chosen id and createdDateTime are honoured",
        how: Break::Body(|method, path, status, sent, body| {
            if method == axum::http::Method::POST
                && status == StatusCode::CREATED
                && path.ends_with("/programs")
                && let Some(object) = body.as_object_mut()
            {
                for field in ["id", "createdDateTime"] {
                    if let Some(claimed) = sent.get(field) {
                        object.insert(field.into(), claimed.clone());
                    }
                }
            }
        }),
        caught_by: &["create-ignores-client-metadata"],
    },
    Fault {
        name: "a report is filed as whichever client the body claims",
        how: Break::Body(|method, path, status, sent, body| {
            if method == axum::http::Method::POST
                && status == StatusCode::CREATED
                && path.ends_with("/reports")
                && let Some(claimed) = sent.get("clientID")
                && let Some(object) = body.as_object_mut()
            {
                object.insert("clientID".into(), claimed.clone());
            }
        }),
        caught_by: &["report-is-stamped-not-claimed"],
    },
    Fault {
        name: "a VEN's own claim to a target is honoured",
        how: Break::Body(|method, path, status, sent, body| {
            // Both collections at once, because it is one mistake: believing a `targets` member on
            // a body the writer is not entitled to set. This crate makes it unrepresentable —
            // `VEN_VEN_REQUEST` has no such member — so the only way to see the check fail is to
            // put the field back on the way out.
            if method == axum::http::Method::POST
                && status == StatusCode::CREATED
                && (path.ends_with("/vens") || path.ends_with("/resources"))
                && let Some(claimed) = sent.get("targets")
                && let Some(object) = body.as_object_mut()
            {
                object.insert("targets".into(), claimed.clone());
            }
        }),
        caught_by: &[
            "ven-cannot-grant-a-resource-targets",
            "ven-cannot-grant-itself-targets",
        ],
    },
    // -- data that does not survive the round trip ---------------------------
    Fault {
        name: "prices are held as binary floats",
        how: Break::Body(|_, _, _, _, body| {
            // What an `f64` pipeline does to a tenth. The value is a settled amount.
            rewrite_numbers(body, |n| (n == "0.1").then_some(0.100_000_000_000_000_03))
        }),
        caught_by: &["decimal-prices-are-exact"],
    },
    Fault {
        name: "an event's interval ids are reassigned",
        how: Break::Body(|method, path, status, _, body| {
            if method == axum::http::Method::POST
                && status == StatusCode::CREATED
                && path.ends_with("/events")
                && let Some(intervals) = body.get_mut("intervals").and_then(|i| i.as_array_mut())
            {
                for (n, interval) in intervals.iter_mut().enumerate() {
                    if let Some(object) = interval.as_object_mut() {
                        object.insert("id".into(), serde_json::json!(100 + n));
                    }
                }
            }
        }),
        caught_by: &["interval-payloads-round-trip"],
    },
    Fault {
        name: "a report comes back without its resources",
        how: Break::Body(|method, path, _, _, body| {
            if method == axum::http::Method::GET
                && path.contains("/reports/")
                && let Some(object) = body.as_object_mut()
            {
                object.remove("resources");
            }
        }),
        caught_by: &["report-round-trips"],
    },
    Fault {
        name: "a VEN's own topic set omits DELETE",
        how: Break::Body(|_, path, _, _, body| {
            if path.contains("/notifiers/mqtt/topics/vens/")
                && let Some(topics) = body.get_mut("topics").and_then(|t| t.as_object_mut())
            {
                topics.remove("DELETE");
            }
        }),
        caught_by: &["mqtt-ven-scoped-topics"],
    },
    Fault {
        name: "an object with no targets is hidden as if it had some",
        how: Break::Body(|method, path, _, _, body| {
            if method == axum::http::Method::GET
                && path.ends_with("/programs")
                && let Some(items) = body.as_array_mut()
            {
                items.retain(|p| {
                    p["targets"]
                        .as_array()
                        .is_some_and(|targets| !targets.is_empty())
                });
            }
        }),
        // Three, because every programme the suite creates is untargeted: the two checks that walk
        // the collection lose their own objects along with everybody's.
        caught_by: &[
            "pagination-is-complete-and-ordered",
            "program-name-lookup",
            "untargeted-objects-are-visible",
        ],
    },
    Fault {
        name: "a resource reports a venID that is not its own",
        how: Break::Body(|method, path, _, _, body| {
            if method == axum::http::Method::GET
                && path.ends_with("/resources")
                && let Some(items) = body.as_array_mut()
            {
                for item in items {
                    if let Some(object) = item.as_object_mut() {
                        object.insert("venID".into(), serde_json::json!("not-this-vens"));
                    }
                }
            }
        }),
        caught_by: &["ven-reads-only-its-own-resources"],
    },
    // -- the one that is a role, not a rule ----------------------------------
    Fault {
        name: "every credential is read as business logic",
        how: Break::EveryCredentialIsBusinessLogic,
        // Thirteen checks, which is what D-121 actually cost. Not
        // `ven-cannot-grant-itself-targets` or `ven-cannot-grant-a-resource-targets`: a VEN cannot
        // grant itself a target because `VEN_VEN_REQUEST` has no `targets` member, so the
        // privilege is unreachable rather than merely unauthorised and no amount of role confusion
        // reaches it (principle 2). And not `ven-reads-only-its-own-resources`, which establishes
        // ownership by asking which VENs the caller owns — a question this fault also answers
        // wrongly, so the check compares one wrong answer with another and agrees with itself.
        caught_by: &[
            "hidden-object-is-404-not-403",
            "mqtt-collection-topics-are-business-logic-only",
            "mqtt-foreign-ven-topics-refused",
            "report-filters-by-event",
            "report-is-stamped-not-claimed",
            "report-needs-an-event",
            "report-round-trips",
            "target-hiding-on-reads",
            "ungranted-targets-are-invisible",
            "ven-cannot-write-programmes",
            "ven-reads-only-its-own-reports",
            "ven-reads-only-its-own-subscriptions",
            "ven-reads-only-its-own-vens",
        ],
    },
];

/// Drop every occurrence of a query parameter, keeping the rest of the target intact.
fn strip_param(target: &mut String, name: &str) {
    let Some((path, query)) = target.split_once('?') else {
        return;
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let key = pair.split('=').next().unwrap_or(pair);
            key != name
        })
        .collect();
    *target = if kept.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", kept.join("&"))
    };
}

/// Apply `f` to every number anywhere in a JSON value, replacing it where `f` answers.
///
/// The number is offered as the text `serde_json` would print, because that is what a check about
/// exactness compares: `0.1` and `0.1000000000000000055511151231257827` are the same `f64` and a
/// different answer.
fn rewrite_numbers(value: &mut serde_json::Value, f: fn(&str) -> Option<f64>) {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(replacement) = f(&n.to_string())
                && let Some(number) = serde_json::Number::from_f64(replacement)
            {
                *value = serde_json::Value::Number(number);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|i| rewrite_numbers(i, f)),
        serde_json::Value::Object(fields) => {
            fields.values_mut().for_each(|v| rewrite_numbers(v, f))
        }
        _ => {}
    }
}

/// Apply `f` to every string anywhere in a JSON value, replacing it where `f` answers.
fn rewrite_strings(value: &mut serde_json::Value, f: fn(&str) -> Option<String>) {
    match value {
        serde_json::Value::String(s) => {
            if let Some(replacement) = f(s) {
                *s = replacement;
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|i| rewrite_strings(i, f)),
        serde_json::Value::Object(fields) => {
            fields.values_mut().for_each(|v| rewrite_strings(v, f))
        }
        _ => {}
    }
}

/// Run the whole suite against a VTN broken one way, and return the ids that failed.
async fn failures_from(fault: &'static Fault) -> Vec<String> {
    let base = match fault.how {
        Break::Config(configure) => start(configure).await,
        _ => {
            let upstream = start(|c| c).await;
            behind_a_fault(&upstream, fault).await
        }
    };
    let report = Runner::new(full_target(&base)).unwrap().run().await;
    let mut failed: Vec<String> = report.failures().map(|f| f.check.id.to_string()).collect();
    failed.sort();
    failed
}

/// A proxy in front of the VTN that applies one fault to everything passing through it.
///
/// The VTN itself is untouched. That is the point: a defect a *configuration* can produce is a
/// defect this implementation happens to have a switch for, and the checks worth measuring are the
/// ones about behaviour nobody would ever offer a switch for.
async fn behind_a_fault(upstream: &str, fault: &'static Fault) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let root = upstream.trim_end_matches("/openadr3/3.1.0").to_string();

    // One fault needs something no `fn` pointer can carry: a token the proxy substitutes for
    // whatever the caller presented.
    let business_logic = match fault.how {
        Break::EveryCredentialIsBusinessLogic => Some(
            business_logic_token(upstream)
                .await
                .expect("the fixture VTN issues its own tokens"),
        ),
        _ => None,
    };

    let app = axum::Router::new().fallback(axum::routing::any(
        move |request: axum::extract::Request| {
            let root = root.clone();
            let business_logic = business_logic.clone();
            async move { relay(&root, request, fault, business_logic.as_deref()).await }
        },
    ));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/openadr3/3.1.0")
}

/// A real business-logic access token from the VTN's own grant.
async fn business_logic_token(base: &str) -> Option<String> {
    let response = reqwest::Client::new()
        .post(format!("{base}/auth/token"))
        .form(&[
            ("grant_type", "client_credentials"),
            ("client_id", BL_CLIENT),
            ("client_secret", BL),
        ])
        .send()
        .await
        .ok()?;
    let body: serde_json::Value = response.json().await.ok()?;
    body["access_token"].as_str().map(str::to_string)
}

/// Forward a request upstream, applying the fault on the way in and on the way out.
async fn relay(
    upstream: &str,
    request: axum::extract::Request,
    fault: &'static Fault,
    business_logic: Option<&str>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let sent: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);

    let mut target = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let mut headers = parts.headers.clone();
    if let Break::Request(apply) = fault.how {
        apply(parts.uri.path(), &mut target, &mut headers);
    }
    // A credential that is presented at all becomes the business-logic one. A request with none
    // stays anonymous, so "an unauthenticated write is refused" still means what it says.
    if let (Break::EveryCredentialIsBusinessLogic, Some(token)) = (&fault.how, business_logic)
        && headers.contains_key(axum::http::header::AUTHORIZATION)
        && let Ok(value) = axum::http::HeaderValue::from_str(&format!("Bearer {token}"))
    {
        headers.insert(axum::http::header::AUTHORIZATION, value);
    }

    let client = reqwest::Client::new();
    let mut outbound = client
        .request(parts.method.clone(), format!("{upstream}{target}"))
        .body(bytes);
    for (name, value) in headers.iter() {
        if name != axum::http::header::HOST {
            outbound = outbound.header(name, value);
        }
    }
    let Ok(response) = outbound.send().await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };

    let path = parts.uri.path();
    let mut status = response.status();
    let mut headers = response.headers().clone();
    let mut body = response.bytes().await.unwrap_or_default().to_vec();

    if let Break::Status(apply) = fault.how {
        status = apply(&parts.method, path, status);
    }
    if let Break::Header(apply) = fault.how {
        apply(path, status, &mut headers);
    }
    if let Break::Body(apply) = fault.how
        && let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(&body)
    {
        apply(&parts.method, path, status, &sent, &mut json);
        body = serde_json::to_vec(&json).unwrap_or(body);
    }

    // Whatever the length and encoding were, the body is being re-sent as one plain chunk.
    headers.remove(axum::http::header::CONTENT_LENGTH);
    headers.remove(axum::http::header::TRANSFER_ENCODING);
    headers.remove(axum::http::header::CONTENT_ENCODING);
    (status, headers, body).into_response()
}

/// How many checks must be demonstrably able to fail.
///
/// A ratchet, like `trace`'s. It is not 48 and will not be: a few checks describe behaviour that
/// cannot be simulated from outside the VTN — a VEN reading another VEN's reports needs the *server*
/// to be wrong, and a proxy cannot invent data it was never sent. Those are named in
/// `UNFALSIFIABLE_FROM_OUTSIDE` with the reason, so the gap is a list somebody decided rather than
/// a number nobody noticed.
const MIN_FALSIFIABLE: usize = 47;

/// Checks no proxy in front of the VTN can make fail, and why.
///
/// Written out rather than left as the difference between two numbers: a gap somebody decided is a
/// gap somebody can argue with, and a gap nobody wrote down is one nobody revisits.
const UNFALSIFIABLE_FROM_OUTSIDE: &[(&str, &str)] = &[(
    "auth-server-unauthenticated",
    "the suite discovers the token endpoint through GET /auth/server, so a fault that breaks it \
     breaks the run rather than the check. Reaching it needs a VTN wrong in that one way, not a \
     proxy in front of a right one",
)];

#[tokio::test]
async fn every_break_is_caught_by_exactly_the_checks_that_name_it() {
    // A suite that passes everything is indistinguishable from a suite that checks nothing, so the
    // suite is run against VTNs that are deliberately wrong and told which checks must notice
    // `[D-072]`. Exactly which: a check that fires on a defect it is not about is a check whose
    // failure does not mean what its title says, and that is the half nobody looks for.
    // The faults are independent — each gets its own VTN on its own port — so they run together.
    // Walking them one at a time is twenty-odd full suite runs and a minute and a half, which is
    // the sort of number that ends with somebody adding `#[ignore]`.
    // Bounded rather than all at once: every fault is a VTN, a proxy and a full suite walk, and
    // two dozen of those competing for one runtime is slower than a queue of them.
    let runs: Vec<(&Fault, Vec<String>)> = futures_util::stream::iter(
        FAULTS
            .iter()
            .map(|fault| async move { (fault, failures_from(fault).await) }),
    )
    .buffer_unordered(6)
    .collect()
    .await;

    let mut observed: std::collections::BTreeSet<String> = Default::default();
    // Everything is asserted at the end. One run then reports every correction the table needs,
    // rather than one per edit against a suite that takes a while to walk.
    let mut wrong: Vec<String> = Vec::new();

    for (fault, failed) in runs {
        let mut expected: Vec<String> = fault.caught_by.iter().map(|s| s.to_string()).collect();
        expected.sort();
        if failed != expected {
            wrong.push(format!(
                "\n  break: {}\n    caught by: {failed:?}\n    expected:  {expected:?}",
                fault.name
            ));
        }
        observed.extend(failed);
    }
    assert!(
        wrong.is_empty(),
        "{} break(s) are not caught by exactly the checks that name them:{}",
        wrong.len(),
        wrong.join("")
    );

    let unproven: Vec<&str> = checks()
        .iter()
        .map(|c| c.id)
        .filter(|id| !observed.contains(*id))
        .collect();
    println!(
        "{} of {} checks have been observed failing against a deliberately broken VTN.\n\
         Never seen failing: {unproven:?}",
        observed.len(),
        checks().len()
    );
    for (id, _) in UNFALSIFIABLE_FROM_OUTSIDE {
        assert!(
            !observed.contains(*id),
            "{id} is listed as unfalsifiable from outside and a fault caught it; drop the entry"
        );
        assert!(
            checks().iter().any(|c| c.id == *id),
            "{id} is listed as unfalsifiable and is not a check"
        );
    }
    assert!(
        observed.len() >= MIN_FALSIFIABLE,
        "{} checks are demonstrably able to fail, and the floor is {MIN_FALSIFIABLE}. A check \
         that has never been seen failing is a check nobody has shown to be a check: add a fault \
         to FAULTS rather than lowering the floor.",
        observed.len()
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
