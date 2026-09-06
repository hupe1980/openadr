//! The checks.
//!
//! Each one is a normative sentence turned into a request and an assertion. They are listed here
//! rather than discovered, in the order they run, so that adding one is a visible edit and the
//! report is stable between runs.
//!
//! Ordering is not arbitrary: the discovery checks come first because a VTN that fails them will
//! fail everything else for the same reason, and a report that says so at the top saves reading the
//! rest.

use serde_json::{Value, json};

use super::{Check, Needs, Outcome, RUN_PREFIX, Runner, Severity};
use crate::client::{Client, ClientError, Query, RawResponse, Role};

/// How many times a request that never reached the VTN is retried.
///
/// A conformance suite runs across a network at somebody else's server, and a refused connection or
/// a reset is not evidence about the specification. Reporting one as non-conformance produces a
/// false accusation, which is worse than no measurement: the whole value of the report is that a
/// failing line can be taken to the implementer.
///
/// Only *transport* failures are retried. A `500` is an answer, and answers are never retried —
/// re-running a check that writes would leave a second object behind and could turn a real failure
/// into an intermittent one.
const TRANSPORT_ATTEMPTS: usize = 3;

/// One request, retried while it fails to arrive at all.
async fn attempt<R: Role>(
    client: &Client<R>,
    method: &str,
    path: &str,
    query: &Query,
    body: Option<&Value>,
    headers: &[(String, String)],
) -> Result<RawResponse, Outcome> {
    let mut last = None;
    for n in 0..TRANSPORT_ATTEMPTS {
        match client.exchange(method, path, query, body, headers).await {
            Ok(response) => return Ok(response),
            Err(e @ ClientError::Transport(_)) => {
                last = Some(e);
                // Short and growing: the case this exists for is a listener that is a moment from
                // ready, not a server that is down.
                tokio::time::sleep(std::time::Duration::from_millis(50 << n)).await;
            }
            Err(other) => return Err(transport(other)),
        }
    }
    Err(transport(last.expect("the loop ran at least once")))
}

/// The same, decoded as JSON, for a check that only cares about the body.
async fn fetch<R: Role>(client: &Client<R>, path: &str, query: &Query) -> Result<Value, Outcome> {
    let response = attempt(client, "GET", path, query, None, &[]).await?;
    if !response.status.is_success() {
        return Err(Outcome::Failed(format!(
            "GET /{path} answered {}: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        )));
    }
    response
        .json()
        .ok_or_else(|| Outcome::Failed(format!("GET /{path} returned no JSON body")))
}

/// Every check, in the order they run.
pub fn checks() -> &'static [Check] {
    &[
        // -- discovery -----------------------------------------------------
        Check {
            id: "auth-server-unauthenticated",
            clause: "[API /auth/server]",
            title: "GET /auth/server answers without credentials and names a token URL",
            severity: Severity::Required,
            needs: Needs::Nothing,
        },
        Check {
            id: "notifiers-webhook-key",
            clause: "[Def §Notifications]",
            title: "GET /notifiers carries the WEBHOOK binding key",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "unauthenticated-write-refused",
            clause: "[API securitySchemes]",
            title: "a write with no credentials is refused",
            severity: Severity::Required,
            needs: Needs::Nothing,
        },
        // -- object lifecycle ----------------------------------------------
        Check {
            id: "create-stamps-metadata",
            clause: "[API objectMetadata]",
            title: "POST answers 201 and the VTN stamps id, timestamps and objectType",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "create-ignores-client-metadata",
            clause: "[Def §Response Codes and Errors]",
            title: "id and createdDateTime in a request body are ignored, not honoured",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "update-moves-modification-time",
            clause: "[API objectMetadata]",
            title: "PUT advances modificationDateTime and leaves createdDateTime alone",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "missing-object-is-404-problem",
            clause: "[Def §Response Codes and Errors]",
            title: "a missing object answers 404 with a problem body",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "malformed-body-is-400-problem",
            clause: "[Def §Response Codes and Errors]",
            title: "a malformed request body answers 400 with a problem body",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "program-name-is-unique",
            clause: "[Def §Object names]",
            title: "a second programme with the same programName is refused",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "event-needs-a-programme",
            clause: "[API eventRequest]",
            title: "an event naming a programme that does not exist is refused",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "programme-delete-leaves-no-orphan",
            clause: "[API eventRequest.programID]",
            title: "no event is left naming a programme that no longer exists",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        // -- collections ---------------------------------------------------
        Check {
            id: "limit-is-capped-at-fifty",
            clause: "[API skip/limit]",
            title: "limit above 50 is refused rather than silently honoured",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "pagination-is-complete-and-ordered",
            clause: "[Def §Response Filtering]",
            title: "skip and limit walk the collection once, with no gap or repeat",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "filters-are-additive",
            clause: "[Def §Response Filtering]",
            title: "two filters narrow together rather than either one winning",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "targets-accept-both-forms",
            clause: "[extension] both forms occur in the field",
            title: "?targets=a&targets=b and ?targets=a,b mean the same thing",
            severity: Severity::Extension,
            needs: Needs::BusinessLogic,
        },
        // -- time semantics ------------------------------------------------
        Check {
            id: "forever-duration-round-trips",
            clause: "[UG §7.3]",
            title: "a P9999Y duration survives a write and a read unchanged",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "now-sentinel-round-trips",
            clause: "[UG §7.3]",
            title: "a 0001-01-01 start survives a write and a read as a start, not as a date",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "interval-payloads-round-trip",
            clause: "[API eventRequest]",
            title: "an event's intervals and payloads come back exactly as written",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "decimal-prices-are-exact",
            clause: "[API values]",
            title: "a price of 0.1 comes back as 0.1",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        // -- object privacy ------------------------------------------------
        Check {
            id: "ven-cannot-write-programmes",
            clause: "[API securitySchemes]",
            title: "a VEN cannot create a programme",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "ven-cannot-grant-itself-targets",
            clause: "[Def §BL created object privacy]",
            title: "targets in a VEN-written ven body are not honoured",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "untargeted-objects-are-visible",
            clause: "[Def §Object Privacy]",
            title: "an untargeted programme is readable by a VEN",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "ungranted-targets-are-invisible",
            clause: "[Def §Object Privacy]",
            title: "a VEN cannot read an event targeted at a group it was not granted",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "granted-targets-are-visible",
            clause: "[Def §Object Privacy]",
            title: "a VEN reads an event targeted at a group it was granted, by naming it",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "target-hiding-on-reads",
            clause: "[Def §Object Privacy]",
            title: "a granted VEN sees only its own target on the object, never the full set",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "hidden-object-is-404-not-403",
            clause: "[Def §Object Privacy]",
            title: "reading a targeted object by id answers 404 rather than confirming it exists",
            severity: Severity::Recommended,
            needs: Needs::BothRoles,
        },
        Check {
            id: "ven-reads-only-its-own-vens",
            clause: "[Def §VEN created object privacy]",
            title: "GET /vens returns only the VEN objects whose clientID is the caller's",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "ven-cannot-grant-a-resource-targets",
            clause: "[API resourceRequest]",
            title: "a VEN cannot write targets onto a resource it creates",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "ven-reads-only-its-own-reports",
            clause: "[Def §Object Privacy]",
            title: "a VEN reads only the reports it filed",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "ven-reads-only-its-own-resources",
            clause: "[Def §Object Privacy]",
            title: "a VEN reads only the resources belonging to it",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "ven-reads-only-its-own-subscriptions",
            clause: "[Def §VEN created object privacy]",
            title: "GET /subscriptions returns only the caller's, tokens included",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        // -- reports -------------------------------------------------------
        Check {
            id: "report-round-trips",
            clause: "[API reportRequest]",
            title: "a report a VEN files comes back with its resources, intervals and values intact",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "report-is-stamped-not-claimed",
            clause: "[Def §VEN created object privacy]",
            title: "the VTN stamps a report's clientID from the credential, not from the body",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "report-filters-by-event",
            clause: "[API /reports ?eventID]",
            title: "GET /reports?eventID= returns that event's reports and no others",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "report-needs-an-event",
            clause: "[API reportRequest.eventID]",
            title: "a report naming an event that does not exist is refused",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        // -- the MQTT notifier binding -------------------------------------
        Check {
            id: "mqtt-binding-shape",
            clause: "[Notifiers §7.2]",
            title: "the MQTT binding names URIS, a serialization and an authentication method",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "mqtt-collection-topics-are-business-logic-only",
            clause: "[Notifiers §9.3]",
            title: "a VEN cannot obtain a collection-wide topic name",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "mqtt-ven-scoped-topics",
            clause: "[Notifiers §8.2]",
            title: "a VEN obtains its own topics, and they carry UPDATE and DELETE",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        Check {
            id: "mqtt-foreign-ven-topics-refused",
            clause: "[Notifiers §9.2]",
            title: "a VEN cannot obtain another VEN's topic names",
            severity: Severity::Required,
            needs: Needs::BothRoles,
        },
        // -- extensions ----------------------------------------------------
        Check {
            id: "active-excludes-transpired-events",
            clause: "[API /events ?active]",
            title: "?active=true drops events whose intervals have all elapsed",
            severity: Severity::Required,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "etag-and-conditional-read",
            clause: "[extension] RFC 9110 §8.8",
            title: "GET carries an ETag and a matching If-None-Match answers 304",
            severity: Severity::Extension,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "problem-uses-the-rfc-9457-media-type",
            clause: "[extension] RFC 9457 §3",
            title: "an error body is served as application/problem+json",
            severity: Severity::Extension,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "problem-carries-a-traceable-instance",
            clause: "[extension] RFC 9457 §3.1.5",
            title: "an error's problem.instance matches the response's request id header",
            severity: Severity::Extension,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "program-name-lookup",
            clause: "[extension] specification#418",
            title: "GET /programs?programName= finds one programme without paging",
            severity: Severity::Extension,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "unauthorized-names-the-scheme",
            clause: "RFC 9110 §11.6.1",
            title: "a 401 carries WWW-Authenticate, so a client knows what to present",
            severity: Severity::Required,
            needs: Needs::Nothing,
        },
        Check {
            id: "token-response-is-not-stored",
            clause: "RFC 6749 §5.1",
            title: "the token endpoint answers Cache-Control: no-store",
            severity: Severity::Required,
            needs: Needs::Nothing,
        },
        Check {
            id: "foreign-media-type-is-refused",
            clause: "[API createProgram requestBody] RFC 9110 §15.5.16",
            title: "a body labelled with a media type the endpoint does not declare answers 415",
            severity: Severity::Recommended,
            needs: Needs::BusinessLogic,
        },
        Check {
            id: "reads-declare-a-caching-policy",
            clause: "[extension] RFC 9111 §5.2.2",
            title: "a read says how it may be cached, because its body depends on who asked",
            severity: Severity::Extension,
            needs: Needs::BusinessLogic,
        },
    ]
}

/// Dispatch one check by id.
///
/// A `match` rather than a table of function pointers, so that a check listed above and never
/// implemented fails to compile — which is the same reason the storage conformance suite writes its
/// list out by hand.
pub(super) async fn run_one(run: &Runner, check: &Check) -> Outcome {
    match check.id {
        "auth-server-unauthenticated" => auth_server_unauthenticated(run).await,
        "notifiers-webhook-key" => notifiers_webhook_key(run).await,
        "unauthenticated-write-refused" => unauthenticated_write_refused(run).await,
        "create-stamps-metadata" => create_stamps_metadata(run).await,
        "create-ignores-client-metadata" => create_ignores_client_metadata(run).await,
        "update-moves-modification-time" => update_moves_modification_time(run).await,
        "missing-object-is-404-problem" => missing_object_is_404_problem(run).await,
        "malformed-body-is-400-problem" => malformed_body_is_400_problem(run).await,
        "program-name-is-unique" => program_name_is_unique(run).await,
        "event-needs-a-programme" => event_needs_a_programme(run).await,
        "programme-delete-leaves-no-orphan" => programme_delete_leaves_no_orphan(run).await,
        "limit-is-capped-at-fifty" => limit_is_capped_at_fifty(run).await,
        "pagination-is-complete-and-ordered" => pagination_is_complete_and_ordered(run).await,
        "filters-are-additive" => filters_are_additive(run).await,
        "targets-accept-both-forms" => targets_accept_both_forms(run).await,
        "forever-duration-round-trips" => forever_duration_round_trips(run).await,
        "now-sentinel-round-trips" => now_sentinel_round_trips(run).await,
        "interval-payloads-round-trip" => interval_payloads_round_trip(run).await,
        "decimal-prices-are-exact" => decimal_prices_are_exact(run).await,
        "ven-cannot-write-programmes" => ven_cannot_write_programmes(run).await,
        "ven-cannot-grant-itself-targets" => ven_cannot_grant_itself_targets(run).await,
        "untargeted-objects-are-visible" => untargeted_objects_are_visible(run).await,
        "ungranted-targets-are-invisible" => ungranted_targets_are_invisible(run).await,
        "granted-targets-are-visible" => granted_targets_are_visible(run).await,
        "target-hiding-on-reads" => target_hiding_on_reads(run).await,
        "hidden-object-is-404-not-403" => hidden_object_is_404_not_403(run).await,
        "ven-reads-only-its-own-vens" => ven_reads_only_its_own_vens(run).await,
        "ven-cannot-grant-a-resource-targets" => ven_cannot_grant_a_resource_targets(run).await,
        "ven-reads-only-its-own-reports" => ven_reads_only_its_own_reports(run).await,
        "ven-reads-only-its-own-resources" => ven_reads_only_its_own_resources(run).await,
        "ven-reads-only-its-own-subscriptions" => ven_reads_only_its_own_subscriptions(run).await,
        "report-round-trips" => report_round_trips(run).await,
        "report-is-stamped-not-claimed" => report_is_stamped_not_claimed(run).await,
        "report-filters-by-event" => report_filters_by_event(run).await,
        "report-needs-an-event" => report_needs_an_event(run).await,
        "mqtt-binding-shape" => mqtt_binding_shape(run).await,
        "mqtt-collection-topics-are-business-logic-only" => {
            mqtt_collection_topics_are_business_logic_only(run).await
        }
        "mqtt-ven-scoped-topics" => mqtt_ven_scoped_topics(run).await,
        "mqtt-foreign-ven-topics-refused" => mqtt_foreign_ven_topics_refused(run).await,
        "active-excludes-transpired-events" => active_excludes_transpired_events(run).await,
        "etag-and-conditional-read" => etag_and_conditional_read(run).await,
        "problem-uses-the-rfc-9457-media-type" => problem_uses_the_rfc_9457_media_type(run).await,
        "problem-carries-a-traceable-instance" => problem_carries_a_traceable_instance(run).await,
        "program-name-lookup" => program_name_lookup(run).await,
        "unauthorized-names-the-scheme" => unauthorized_names_the_scheme(run).await,
        "token-response-is-not-stored" => token_response_is_not_stored(run).await,
        "foreign-media-type-is-refused" => foreign_media_type_is_refused(run).await,
        "reads-declare-a-caching-policy" => reads_declare_a_caching_policy(run).await,
        other => Outcome::Skipped(format!("{other} is listed but not implemented")),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A name unique to this run, so two suites against one VTN do not collide.
fn unique(kind: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let stamp = crate::model::Timestamp::now().as_second();
    format!("{RUN_PREFIX}{kind}-{stamp}-{n}")
}

/// `Ok` if the condition holds, a failure naming what was expected otherwise.
fn expect(condition: bool, message: impl Into<String>) -> Result<(), Outcome> {
    if condition {
        Ok(())
    } else {
        Err(Outcome::Failed(message.into()))
    }
}

/// Turn a fallible async body into an `Outcome`.
///
/// The bodies use `?` to stop at the first thing that went wrong, which is what makes a check read
/// like a description of the behaviour rather than like a decision tree.
macro_rules! check {
    ($body:block) => {
        match async move { $body }.await {
            Ok(()) => Outcome::Passed,
            Err(outcome) => outcome,
        }
    };
}

/// Create a programme as business logic, tracking it for cleanup.
async fn make_program(run: &Runner, body: Value) -> Result<Value, Outcome> {
    let response = attempt(
        run.business_logic(),
        "POST",
        "programs",
        &Query::new(),
        Some(&body),
        &[],
    )
    .await?;
    if response.status.as_u16() != 201 {
        return Err(Outcome::Failed(format!(
            "POST /programs answered {} (expected 201): {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        )));
    }
    let created = response
        .json()
        .ok_or_else(|| Outcome::Failed("POST /programs returned no JSON body".into()))?;
    if let Some(id) = created["id"].as_str() {
        run.track("programs", id);
    }
    Ok(created)
}

/// Create an event as business logic, tracking it for cleanup.
async fn make_event(run: &Runner, body: Value) -> Result<Value, Outcome> {
    let response = attempt(
        run.business_logic(),
        "POST",
        "events",
        &Query::new(),
        Some(&body),
        &[],
    )
    .await?;
    if response.status.as_u16() != 201 {
        return Err(Outcome::Failed(format!(
            "POST /events answered {} (expected 201): {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        )));
    }
    let created = response
        .json()
        .ok_or_else(|| Outcome::Failed("POST /events returned no JSON body".into()))?;
    if let Some(id) = created["id"].as_str() {
        run.track("events", id);
    }
    Ok(created)
}

/// A minimal event body for a programme.
fn event_body(program_id: &str, targets: &[&str]) -> Value {
    json!({
        "programID": program_id,
        "eventName": unique("event"),
        "targets": targets,
        "intervalPeriod": { "start": "2026-02-11T12:00:00Z", "duration": "PT15M" },
        "intervals": [
            { "id": 0, "payloads": [{ "type": "IMPORT_CAPACITY_LIMIT", "values": [60] }] }
        ]
    })
}

/// File a report as the VEN, tracking it for cleanup.
///
/// The suite had no such helper, and consequently never wrote a report at all — which made
/// `ven-reads-only-its-own-reports` a check that passed against an empty collection, and left the
/// object carrying customer meter data with the least coverage of any in the API.
async fn make_report(run: &Runner, body: Value) -> Result<Value, Outcome> {
    let response = attempt(
        run.virtual_end_node(),
        "POST",
        "reports",
        &Query::new(),
        Some(&body),
        &[],
    )
    .await?;
    if response.status.as_u16() != 201 {
        return Err(Outcome::Failed(format!(
            "POST /reports as a VEN answered {} (expected 201): {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        )));
    }
    let created = response
        .json()
        .ok_or_else(|| Outcome::Failed("POST /reports returned no JSON body".into()))?;
    if let Some(id) = created["id"].as_str() {
        run.track("reports", id);
    }
    Ok(created)
}

/// A programme, an event under it, and the event's id — the scaffolding every report check needs.
async fn seed_event(run: &Runner) -> Result<String, Outcome> {
    let program = make_program(run, json!({ "programName": unique("program") })).await?;
    let program_id = program["id"]
        .as_str()
        .ok_or_else(|| Outcome::Failed("a created programme carries no id".into()))?;
    let event = make_event(run, event_body(program_id, &[])).await?;
    event["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Outcome::Failed("a created event carries no id".into()))
}

/// A report body against an event, with one resource and one interval carrying a decimal.
fn report_body(event_id: &str, resource: &str, value: f64) -> Value {
    json!({
        "eventID": event_id,
        "clientName": unique("client"),
        "reportName": unique("report"),
        "payloadDescriptors": [
            { "objectType": "REPORT_PAYLOAD_DESCRIPTOR", "payloadType": "USAGE",
              "readingType": "DIRECT_READ", "units": "KWH" }
        ],
        "resources": [{
            "resourceName": resource,
            "intervalPeriod": { "start": "2026-02-11T12:00:00Z", "duration": "PT15M" },
            "intervals": [
                { "id": 0, "payloads": [{ "type": "USAGE", "values": [value] }] }
            ]
        }]
    })
}

/// A transport failure is a failure of the check, with the reason — all of it.
///
/// `reqwest`'s own `Display` is "error sending request for url (…)" and puts the thing that
/// actually happened — connection refused, TLS handshake failed, DNS — in the source chain. A
/// conformance report that stops at the first line tells the reader nothing they can act on.
fn transport(e: crate::client::ClientError) -> Outcome {
    let mut message = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    Outcome::Failed(format!("the request could not be made: {message}"))
}

/// Grant the configured VEN a target, returning the VEN object's id.
///
/// Business logic writes the grant, because only business logic may. If no VEN object exists for
/// the configured `clientID` this creates one, which is also what a VTN's enrollment flow does.
async fn grant_target(run: &Runner, target: &str) -> Result<String, Outcome> {
    let client_id = run
        .target
        .ven_client_id
        .as_deref()
        .expect("the precondition check guarantees a clientID");
    let bl = run.business_logic();

    // An existing VEN object for this client, if there is one. `?venName=` cannot find it — the
    // name is not known — so this reads the collection business logic can see in full.
    let existing = fetch(bl, "vens", &Query::new().limit(50)).await?;
    let found = existing
        .as_array()
        .and_then(|vens| {
            vens.iter()
                .find(|v| v["clientID"].as_str() == Some(client_id))
        })
        .cloned();

    let body = json!({
        "objectType": "BL_VEN_REQUEST",
        "clientID": client_id,
        "venName": found
            .as_ref()
            .and_then(|v| v["venName"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| unique("ven")),
        "targets": [target],
    });

    let (method, path, expected) = match found.as_ref().and_then(|v| v["id"].as_str()) {
        Some(id) => ("PUT", format!("vens/{id}"), 200),
        None => ("POST", "vens".to_string(), 201),
    };
    let response = attempt(bl, method, &path, &Query::new(), Some(&body), &[]).await?;
    if response.status.as_u16() != expected {
        return Err(Outcome::Failed(format!(
            "{method} /{path} answered {} (expected {expected}) while granting a target: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        )));
    }
    let id = response
        .json()
        .and_then(|v| v["id"].as_str().map(str::to_string))
        .ok_or_else(|| Outcome::Failed("the VEN object has no id".into()))?;
    if method == "POST" {
        run.track("vens", &id);
    }
    Ok(id)
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

async fn auth_server_unauthenticated(run: &Runner) -> Outcome {
    check!({
        let client = run.anonymous().map_err(transport)?;
        let response = attempt(&client, "GET", "auth/server", &Query::new(), None, &[]).await?;
        expect(
            response.status.is_success(),
            format!(
                "GET /auth/server answered {} without credentials; the endpoint is how a client \
                 discovers where to get them, so it cannot require one",
                response.status
            ),
        )?;
        let body = response
            .json()
            .ok_or_else(|| Outcome::Failed("GET /auth/server returned no JSON".into()))?;
        expect(
            body["tokenURL"].as_str().is_some_and(|u| !u.is_empty()),
            format!("GET /auth/server returned no tokenURL: {body}"),
        )
    })
}

async fn notifiers_webhook_key(run: &Runner) -> Outcome {
    check!({
        let response = attempt(
            run.business_logic(),
            "GET",
            "notifiers",
            &Query::new(),
            None,
            &[],
        )
        .await?;
        expect(
            response.status.is_success(),
            format!("GET /notifiers answered {}", response.status),
        )?;
        let body = response
            .json()
            .ok_or_else(|| Outcome::Failed("GET /notifiers returned no JSON".into()))?;
        expect(
            body.get("WEBHOOK").is_some(),
            format!(
                "GET /notifiers has no WEBHOOK key; it must be present so a client can tell \
                 whether webhooks are available: {body}"
            ),
        )
    })
}

async fn unauthenticated_write_refused(run: &Runner) -> Outcome {
    check!({
        let client = run.anonymous().map_err(transport)?;
        let response = attempt(
            &client,
            "POST",
            "programs",
            &Query::new(),
            Some(&json!({ "programName": unique("should-not-exist") })),
            &[],
        )
        .await?;
        expect(
            matches!(response.status.as_u16(), 401 | 403),
            format!(
                "an unauthenticated POST /programs answered {} (expected 401 or 403)",
                response.status
            ),
        )
    })
}

// ---------------------------------------------------------------------------
// Object lifecycle
// ---------------------------------------------------------------------------

async fn create_stamps_metadata(run: &Runner) -> Outcome {
    check!({
        let name = unique("program");
        let created = make_program(run, json!({ "programName": name })).await?;
        for field in [
            "id",
            "createdDateTime",
            "modificationDateTime",
            "objectType",
        ] {
            expect(
                created.get(field).is_some(),
                format!("the created programme has no {field}: {created}"),
            )?;
        }
        expect(
            created["objectType"] == "PROGRAM",
            format!("objectType is {} (expected PROGRAM)", created["objectType"]),
        )?;
        expect(
            created["createdDateTime"]
                .as_str()
                .is_some_and(|t| t.parse::<crate::model::Timestamp>().is_ok()),
            format!(
                "createdDateTime is not RFC 3339: {}",
                created["createdDateTime"]
            ),
        )
    })
}

async fn create_ignores_client_metadata(run: &Runner) -> Outcome {
    check!({
        let created = make_program(
            run,
            json!({
                "programName": unique("program"),
                "id": "client-chosen-id",
                "createdDateTime": "1999-01-01T00:00:00Z",
                "objectType": "PROGRAM",
            }),
        )
        .await?;
        expect(
            created["id"] != "client-chosen-id",
            "the VTN honoured an id supplied by the client; ids are the VTN's to assign"
                .to_string(),
        )?;
        expect(
            created["createdDateTime"] != "1999-01-01T00:00:00Z",
            "the VTN honoured a createdDateTime supplied by the client".to_string(),
        )
    })
}

async fn update_moves_modification_time(run: &Runner) -> Outcome {
    check!({
        let created = make_program(run, json!({ "programName": unique("program") })).await?;
        let id = created["id"].as_str().unwrap_or_default();

        let updated = attempt(
            run.business_logic(),
            "PUT",
            &format!("programs/{id}"),
            &Query::new(),
            Some(&json!({
                "programName": created["programName"],
                "programDescriptions": [{ "URL": "https://example.com/tariff" }],
            })),
            &[],
        )
        .await?;
        expect(
            updated.status.is_success(),
            format!("PUT /programs/{id} answered {}", updated.status),
        )?;
        let updated = updated
            .json()
            .ok_or_else(|| Outcome::Failed("PUT returned no JSON".into()))?;

        expect(
            updated["createdDateTime"] == created["createdDateTime"],
            "PUT changed createdDateTime; it records when the object was created".to_string(),
        )?;
        expect(
            updated["modificationDateTime"] != created["modificationDateTime"],
            "PUT left modificationDateTime unchanged after a real change; a client polling on it \
             would never see the update"
                .to_string(),
        )
    })
}

async fn missing_object_is_404_problem(run: &Runner) -> Outcome {
    check!({
        let response = attempt(
            run.business_logic(),
            "GET",
            "programs/oadr-conformance-no-such-program",
            &Query::new(),
            None,
            &[],
        )
        .await?;
        expect(
            response.status.as_u16() == 404,
            format!(
                "a missing programme answered {} (expected 404)",
                response.status
            ),
        )?;
        let problem = response
            .problem()
            .ok_or_else(|| Outcome::Failed("the 404 carried no problem body".into()))?;
        expect(
            problem.status == Some(404),
            format!(
                "the problem body's status is {:?} (expected 404)",
                problem.status
            ),
        )?;
        expect(
            problem.title.is_some(),
            "the problem body has no title".to_string(),
        )
    })
}

async fn malformed_body_is_400_problem(run: &Runner) -> Outcome {
    check!({
        // A programme with no `programName`, which the schema requires.
        let response = attempt(
            run.business_logic(),
            "POST",
            "programs",
            &Query::new(),
            Some(&json!({ "notAField": true })),
            &[],
        )
        .await?;
        expect(
            response.status.as_u16() == 400,
            format!(
                "a body missing the required programName answered {} (expected 400)",
                response.status
            ),
        )?;
        expect(
            response.problem().is_some(),
            "the 400 carried no problem body".to_string(),
        )
    })
}

async fn program_name_is_unique(run: &Runner) -> Outcome {
    check!({
        let name = unique("program");
        make_program(run, json!({ "programName": name.clone() })).await?;

        let second = attempt(
            run.business_logic(),
            "POST",
            "programs",
            &Query::new(),
            Some(&json!({ "programName": name })),
            &[],
        )
        .await?;
        if second.status.as_u16() == 201
            && let Some(id) = second
                .json()
                .and_then(|v| v["id"].as_str().map(str::to_string))
        {
            run.track("programs", id);
        }
        expect(
            second.status.as_u16() == 409,
            format!(
                "a duplicate programName answered {} (expected 409); the name must be unique to a \
                 VTN instance",
                second.status
            ),
        )
    })
}

async fn event_needs_a_programme(run: &Runner) -> Outcome {
    check!({
        let response = attempt(
            run.business_logic(),
            "POST",
            "events",
            &Query::new(),
            Some(&event_body("oadr-conformance-no-such-program", &[])),
            &[],
        )
        .await?;
        if response.status.as_u16() == 201
            && let Some(id) = response
                .json()
                .and_then(|v| v["id"].as_str().map(str::to_string))
        {
            run.track("events", id);
        }
        expect(
            matches!(response.status.as_u16(), 400 | 404 | 409),
            format!(
                "an event naming a programme that does not exist answered {} (expected 400, 404 \
                 or 409); a dangling programID leaves an event no VEN can resolve",
                response.status
            ),
        )
    })
}

/// Deleting a programme must not leave its events pointing at nothing.
///
/// The specification says what a `DELETE /programs/{id}` returns and nothing about what becomes of
/// the events that name it, so *cascade* and *refuse while dependents exist* are both defensible
/// and implementations do both. What no reading permits is the third outcome: the delete succeeds
/// and the events survive, each naming a `programID` that now resolves to nothing.
///
/// This check used to assert the cascade — this implementation's choice — as though the
/// specification required it, and reported a peer that refuses instead as non-conformant. That is
/// the failure mode requiring a citation is supposed to prevent, and it took running the suite
/// against somebody else to notice.
async fn programme_delete_leaves_no_orphan(run: &Runner) -> Outcome {
    check!({
        let program = make_program(run, json!({ "programName": unique("program") })).await?;
        let program_id = program["id"].as_str().unwrap_or_default().to_string();
        let event = make_event(run, event_body(&program_id, &[])).await?;
        let event_id = event["id"].as_str().unwrap_or_default().to_string();

        let deleted = attempt(
            run.business_logic(),
            "DELETE",
            &format!("programs/{program_id}"),
            &Query::new(),
            None,
            &[],
        )
        .await?;

        let program_gone = attempt(
            run.business_logic(),
            "GET",
            &format!("programs/{program_id}"),
            &Query::new(),
            None,
            &[],
        )
        .await?
        .status
        .as_u16()
            == 404;
        let event_gone = attempt(
            run.business_logic(),
            "GET",
            &format!("events/{event_id}"),
            &Query::new(),
            None,
            &[],
        )
        .await?
        .status
        .as_u16()
            == 404;

        if deleted.status.is_success() {
            expect(
                program_gone,
                format!(
                    "DELETE answered {} and the programme is still there",
                    deleted.status
                ),
            )?;
            return expect(
                event_gone,
                "the programme was deleted and its event survived, naming a programID that now \
                 resolves to nothing. Either take the events with it or refuse the delete"
                    .to_string(),
            );
        }

        // Refusing is the other defensible answer — as long as nothing was half-done.
        expect(
            !program_gone && !event_gone,
            format!(
                "DELETE answered {} but something was removed anyway; a refused delete must leave \
                 the programme and its events exactly as they were",
                deleted.status
            ),
        )
    })
}

// ---------------------------------------------------------------------------
// Collections
// ---------------------------------------------------------------------------

async fn limit_is_capped_at_fifty(run: &Runner) -> Outcome {
    check!({
        let response = attempt(
            run.business_logic(),
            "GET",
            "programs",
            &Query::new().param("limit", "500"),
            None,
            &[],
        )
        .await?;
        expect(
            response.status.as_u16() == 400,
            format!(
                "?limit=500 answered {} (expected 400); the schema's maximum is 50, and silently \
                 clamping it leaves a client unable to tell a short page from the end",
                response.status
            ),
        )
    })
}

async fn pagination_is_complete_and_ordered(run: &Runner) -> Outcome {
    check!({
        // Five programmes of our own, then walked two at a time.
        let mut mine: Vec<String> = Vec::new();
        for _ in 0..5 {
            let created = make_program(run, json!({ "programName": unique("page") })).await?;
            mine.push(created["id"].as_str().unwrap_or_default().to_string());
        }

        let mut seen: Vec<String> = Vec::new();
        for skip in (0..200).step_by(2) {
            let page = fetch(
                run.business_logic(),
                "programs",
                &Query::new()
                    .param("skip", skip.to_string())
                    .param("limit", "2"),
            )
            .await?;
            let items = page.as_array().cloned().unwrap_or_default();
            let count = items.len();
            for item in items {
                if let Some(id) = item["id"].as_str() {
                    seen.push(id.to_string());
                }
            }
            if count < 2 {
                break;
            }
        }

        let mut unique_seen = seen.clone();
        unique_seen.sort();
        unique_seen.dedup();
        expect(
            unique_seen.len() == seen.len(),
            format!(
                "walking the collection returned {} records of which only {} were distinct; pages \
                 overlap, which means the order is not stable",
                seen.len(),
                unique_seen.len()
            ),
        )?;
        let missing: Vec<&String> = mine.iter().filter(|id| !seen.contains(id)).collect();
        expect(
            missing.is_empty(),
            format!(
                "{} programme(s) this run created were never returned by any page: {missing:?}",
                missing.len()
            ),
        )
    })
}

async fn filters_are_additive(run: &Runner) -> Outcome {
    check!({
        let a = make_program(run, json!({ "programName": unique("filter-a") })).await?;
        let b = make_program(run, json!({ "programName": unique("filter-b") })).await?;
        let a_id = a["id"].as_str().unwrap_or_default().to_string();
        let b_id = b["id"].as_str().unwrap_or_default().to_string();

        make_event(run, event_body(&a_id, &[])).await?;
        let wanted = make_event(run, event_body(&b_id, &[])).await?;
        let wanted_id = wanted["id"].as_str().unwrap_or_default().to_string();

        let filtered = fetch(
            run.business_logic(),
            "events",
            &Query::new().param("programID", &b_id),
        )
        .await?;
        let ids: Vec<&str> = filtered
            .as_array()
            .map(|items| items.iter().filter_map(|i| i["id"].as_str()).collect())
            .unwrap_or_default();

        expect(
            ids.contains(&wanted_id.as_str()),
            format!("?programID={b_id} did not return the event in that programme"),
        )?;
        expect(
            filtered
                .as_array()
                .is_some_and(|items| items.iter().all(|i| i["programID"] == b_id.as_str())),
            format!(
                "?programID={b_id} returned events from another programme; a filter that is \
                     ignored is worse than one that is refused"
            ),
        )
    })
}

async fn targets_accept_both_forms(run: &Runner) -> Outcome {
    check!({
        let repeated = fetch(
            run.business_logic(),
            "events",
            &Query::new()
                .param("targets", "oadr-conformance-x")
                .param("targets", "oadr-conformance-y"),
        )
        .await?;
        let comma = fetch(
            run.business_logic(),
            "events",
            &Query::new().param("targets", "oadr-conformance-x,oadr-conformance-y"),
        )
        .await?;

        expect(
            repeated == comma,
            "?targets=a&targets=b and ?targets=a,b returned different results; both forms occur \
             in the field"
                .to_string(),
        )
    })
}

// ---------------------------------------------------------------------------
// Time semantics
// ---------------------------------------------------------------------------

async fn forever_duration_round_trips(run: &Runner) -> Outcome {
    check!({
        let program = make_program(run, json!({ "programName": unique("program") })).await?;
        let program_id = program["id"].as_str().unwrap_or_default().to_string();

        let mut body = event_body(&program_id, &[]);
        body["intervalPeriod"]["duration"] = json!("P9999Y");
        let created = make_event(run, body).await?;

        expect(
            created["intervalPeriod"]["duration"] == "P9999Y",
            format!(
                "a P9999Y duration came back as {}; the value means \"no end\" and a VTN that \
                 normalises it has changed the schedule",
                created["intervalPeriod"]["duration"]
            ),
        )
    })
}

async fn now_sentinel_round_trips(run: &Runner) -> Outcome {
    check!({
        let program = make_program(run, json!({ "programName": unique("program") })).await?;
        let program_id = program["id"].as_str().unwrap_or_default().to_string();

        let mut body = event_body(&program_id, &[]);
        body["intervalPeriod"]["start"] = json!("0001-01-01T00:00:00Z");
        let created = make_event(run, body).await?;

        let start = created["intervalPeriod"]["start"].as_str().unwrap_or("");
        expect(
            start.starts_with("0001-01-01"),
            format!(
                "a 0001-01-01 start came back as {start:?}; the value means \"now\" to the \
                 reader, and resolving it at write time fixes it to the writer's clock instead"
            ),
        )
    })
}

async fn interval_payloads_round_trip(run: &Runner) -> Outcome {
    check!({
        let program = make_program(run, json!({ "programName": unique("program") })).await?;
        let program_id = program["id"].as_str().unwrap_or_default().to_string();

        let mut body = event_body(&program_id, &[]);
        body["intervals"] = json!([
            { "id": 0, "payloads": [{ "type": "IMPORT_CAPACITY_LIMIT", "values": [60] }] },
            { "id": 1,
              "intervalPeriod": { "duration": "PT30M" },
              "payloads": [{ "type": "IMPORT_CAPACITY_LIMIT", "values": [40] }] },
        ]);
        let created = make_event(run, body.clone()).await?;

        expect(
            created["intervals"] == body["intervals"],
            format!(
                "the intervals came back changed.\\n  sent: {}\\n  got:  {}",
                body["intervals"], created["intervals"]
            ),
        )
    })
}

async fn decimal_prices_are_exact(run: &Runner) -> Outcome {
    check!({
        let program = make_program(run, json!({ "programName": unique("program") })).await?;
        let program_id = program["id"].as_str().unwrap_or_default().to_string();

        let mut body = event_body(&program_id, &[]);
        body["intervals"] = json!([
            { "id": 0, "payloads": [{ "type": "PRICE", "values": [0.1, 0.2, 20.25] }] }
        ]);
        let created = make_event(run, body).await?;

        let values = &created["intervals"][0]["payloads"][0]["values"];
        let rendered = values.to_string();
        expect(
            rendered == "[0.1,0.2,20.25]",
            format!(
                "a price of 0.1 came back as {rendered}; money held as a binary float does not \
                 survive a round trip, and these are settled amounts"
            ),
        )
    })
}

// ---------------------------------------------------------------------------
// Object privacy
// ---------------------------------------------------------------------------

async fn ven_cannot_write_programmes(run: &Runner) -> Outcome {
    check!({
        let response = attempt(
            run.virtual_end_node(),
            "POST",
            "programs",
            &Query::new(),
            Some(&json!({ "programName": unique("ven-should-not-create") })),
            &[],
        )
        .await?;
        if response.status.as_u16() == 201
            && let Some(id) = response
                .json()
                .and_then(|v| v["id"].as_str().map(str::to_string))
        {
            run.track("programs", id);
        }
        expect(
            matches!(response.status.as_u16(), 401 | 403),
            format!(
                "a VEN created a programme ({}); write_programs is business logic's",
                response.status
            ),
        )
    })
}

async fn ven_cannot_grant_itself_targets(run: &Runner) -> Outcome {
    check!({
        let stolen = format!("{RUN_PREFIX}stolen-target");
        let response = attempt(
            run.virtual_end_node(),
            "POST",
            "vens",
            &Query::new(),
            Some(&json!({
                "objectType": "VEN_VEN_REQUEST",
                "venName": unique("ven"),
                "targets": [stolen],
            })),
            &[],
        )
        .await?;

        // Either refusing the body or ignoring the field is conformant; honouring it is not.
        let Some(created) = response.json().filter(|_| response.status.as_u16() == 201) else {
            return Ok(());
        };
        if let Some(id) = created["id"].as_str() {
            run.track("vens", id);
        }
        let targets = created["targets"].as_array().cloned().unwrap_or_default();
        expect(
            !targets.iter().any(|t| t == &json!(stolen)),
            format!(
                "a VEN granted itself the target {stolen:?}; only business logic may write \
                 targets, because a VEN that can grant itself one can read any event aimed at it"
            ),
        )
    })
}

async fn untargeted_objects_are_visible(run: &Runner) -> Outcome {
    check!({
        let name = unique("public");
        let created = make_program(run, json!({ "programName": name.clone() })).await?;
        let id = created["id"].as_str().unwrap_or_default().to_string();

        let visible = fetch(run.virtual_end_node(), "programs", &Query::new().limit(50)).await?;
        let found = visible
            .as_array()
            .is_some_and(|items| items.iter().any(|p| p["id"] == id.as_str()));
        expect(
            found,
            format!(
                "a VEN could not see the untargeted programme {name:?}; an object with no targets \
                 is not gated by targeting"
            ),
        )
    })
}

async fn ungranted_targets_are_invisible(run: &Runner) -> Outcome {
    check!({
        let program = make_program(run, json!({ "programName": unique("program") })).await?;
        let program_id = program["id"].as_str().unwrap_or_default().to_string();
        let secret = format!("{RUN_PREFIX}not-granted");
        let event = make_event(run, event_body(&program_id, &[&secret])).await?;
        let event_id = event["id"].as_str().unwrap_or_default().to_string();

        let listed = fetch(run.virtual_end_node(), "events", &Query::new().limit(50)).await?;
        expect(
            !listed
                .as_array()
                .is_some_and(|items| items.iter().any(|e| e["id"] == event_id.as_str())),
            "a VEN listing events with no targets saw an event targeted at a group it was not \
             granted; naming no targets must hide every targeted object"
                .to_string(),
        )?;

        let asked = fetch(
            run.virtual_end_node(),
            "events",
            &Query::new().param("targets", &secret),
        )
        .await?;
        expect(
            !asked
                .as_array()
                .is_some_and(|items| items.iter().any(|e| e["id"] == event_id.as_str())),
            format!(
                "a VEN read an event targeted at {secret:?} merely by asking for it; the target \
                 must be granted through the VEN object first, or guessing a group name is enough \
                 to read a competitor's dispatch"
            ),
        )
    })
}

async fn granted_targets_are_visible(run: &Runner) -> Outcome {
    check!({
        let target = format!("{RUN_PREFIX}granted");
        grant_target(run, &target).await?;

        let program = make_program(run, json!({ "programName": unique("program") })).await?;
        let program_id = program["id"].as_str().unwrap_or_default().to_string();
        let event = make_event(run, event_body(&program_id, &[&target])).await?;
        let event_id = event["id"].as_str().unwrap_or_default().to_string();

        let visible = fetch(
            run.virtual_end_node(),
            "events",
            &Query::new().param("targets", &target),
        )
        .await?;
        expect(
            visible
                .as_array()
                .is_some_and(|items| items.iter().any(|e| e["id"] == event_id.as_str())),
            format!(
                "a VEN granted {target:?} could not read an event targeted at it; the grant is \
                 written on the VEN object and the request named the target"
            ),
        )
    })
}

async fn target_hiding_on_reads(run: &Runner) -> Outcome {
    check!({
        let mine = format!("{RUN_PREFIX}mine");
        let theirs = format!("{RUN_PREFIX}theirs");
        grant_target(run, &mine).await?;

        let program = make_program(run, json!({ "programName": unique("program") })).await?;
        let program_id = program["id"].as_str().unwrap_or_default().to_string();
        let event = make_event(run, event_body(&program_id, &[&mine, &theirs])).await?;
        let event_id = event["id"].as_str().unwrap_or_default().to_string();

        let visible = fetch(
            run.virtual_end_node(),
            "events",
            &Query::new().param("targets", &mine),
        )
        .await?;
        let seen = visible
            .as_array()
            .and_then(|items| items.iter().find(|e| e["id"] == event_id.as_str()))
            .cloned()
            .ok_or_else(|| {
                Outcome::Failed(format!(
                    "the VEN could not read the event it was granted {mine:?} for"
                ))
            })?;

        let targets = seen["targets"].as_array().cloned().unwrap_or_default();
        expect(
            !targets.iter().any(|t| t == &json!(theirs)),
            format!(
                "the response carried {theirs:?}, a target this VEN was not granted. The full \
                 target list on an event is a competitor's dispatch schedule, and target hiding \
                 exists to conceal it: {targets:?}"
            ),
        )
    })
}

async fn hidden_object_is_404_not_403(run: &Runner) -> Outcome {
    check!({
        let program = make_program(run, json!({ "programName": unique("program") })).await?;
        let program_id = program["id"].as_str().unwrap_or_default().to_string();
        let secret = format!("{RUN_PREFIX}hidden");
        let event = make_event(run, event_body(&program_id, &[&secret])).await?;
        let event_id = event["id"].as_str().unwrap_or_default().to_string();

        let response = attempt(
            run.virtual_end_node(),
            "GET",
            &format!("events/{event_id}"),
            &Query::new(),
            None,
            &[],
        )
        .await?;
        expect(
            response.status.as_u16() != 200,
            "a VEN read an event by id that it was not granted a target for".to_string(),
        )?;
        expect(
            response.status.as_u16() == 404,
            format!(
                "reading a hidden event by id answered {} (expected 404); a 403 confirms the \
                 object exists, which is what targeting conceals",
                response.status
            ),
        )
    })
}

async fn ven_reads_only_its_own_vens(run: &Runner) -> Outcome {
    check!({
        let client_id = run
            .target
            .ven_client_id
            .as_deref()
            .expect("the precondition guarantees a clientID");

        // A VEN object belonging to somebody else.
        let other = format!("{RUN_PREFIX}other-client");
        let created = attempt(
            run.business_logic(),
            "POST",
            "vens",
            &Query::new(),
            Some(&json!({
                "objectType": "BL_VEN_REQUEST",
                "clientID": other,
                "venName": unique("other-ven"),
            })),
            &[],
        )
        .await?;
        if created.status.as_u16() != 201 {
            return Err(Outcome::Failed(format!(
                "could not create a second VEN object to test against: {}",
                created.status
            )));
        }
        if let Some(id) = created
            .json()
            .and_then(|v| v["id"].as_str().map(str::to_string))
        {
            run.track("vens", id);
        }

        let visible = fetch(run.virtual_end_node(), "vens", &Query::new().limit(50)).await?;
        let foreign: Vec<String> = visible
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter(|v| v["clientID"].as_str().is_some_and(|c| c != client_id))
                    .filter_map(|v| v["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        expect(
            foreign.is_empty(),
            format!(
                "a VEN read {} VEN object(s) belonging to another client: {foreign:?}",
                foreign.len()
            ),
        )
    })
}

async fn ven_reads_only_its_own_subscriptions(run: &Runner) -> Outcome {
    check!({
        let listed = attempt(
            run.virtual_end_node(),
            "GET",
            "subscriptions",
            &Query::new().limit(50),
            None,
            &[],
        )
        .await?;
        if !listed.status.is_success() {
            return Err(Outcome::Failed(format!(
                "GET /subscriptions as a VEN answered {}",
                listed.status
            )));
        }
        let Some(items) = listed.json().and_then(|v| v.as_array().cloned()) else {
            return Err(Outcome::Failed(
                "GET /subscriptions did not return an array".into(),
            ));
        };
        let client_id = run
            .target
            .ven_client_id
            .as_deref()
            .expect("the precondition guarantees a clientID");
        let foreign: Vec<&Value> = items
            .iter()
            .filter(|s| s["clientID"].as_str().is_some_and(|c| c != client_id))
            .collect();
        expect(
            foreign.is_empty(),
            format!(
                "a VEN read {} subscription(s) belonging to another client. A subscription carries \
                 a callbackUrl and a bearerToken, so this hands over another subscriber's \
                 credentials",
                foreign.len()
            ),
        )
    })
}

/// A VEN cannot write targets onto a resource it creates.
///
/// The other half of the discriminator claim. `VEN_VEN_REQUEST` has no `targets` field and neither
/// does `VEN_RESOURCE_REQUEST` `[API resourceRequest]`, and 3.1.0 extended target-based access
/// control to resources precisely so that a resource could carry a grant `[CL 3.1.0 issue 321]`. A
/// VEN that can write one has granted itself the right to read every event aimed at it — through
/// the collection an implementation added *after* it had got `vens` right, which is the one place
/// the rule is most likely to have been left behind.
async fn ven_cannot_grant_a_resource_targets(run: &Runner) -> Outcome {
    check!({
        // The VEN's own VEN object, which is what a resource has to hang off.
        let mine = fetch(run.virtual_end_node(), "vens", &Query::new().limit(50)).await?;
        let Some(ven_id) = mine
            .as_array()
            .and_then(|v| v.first())
            .and_then(|v| v["id"].as_str())
            .map(str::to_string)
        else {
            return Err(Outcome::Skipped(
                "this VEN credential owns no ven object, so it has nothing to hang a resource on"
                    .into(),
            ));
        };

        let stolen = format!("{RUN_PREFIX}stolen-target");
        let response = attempt(
            run.virtual_end_node(),
            "POST",
            "resources",
            &Query::new(),
            Some(&json!({
                "objectType": "VEN_RESOURCE_REQUEST",
                "resourceName": unique("res"),
                "venID": ven_id,
                "targets": [stolen],
            })),
            &[],
        )
        .await?;

        // Either refusing the body or ignoring the field is conformant; honouring it is not.
        let Some(created) = response.json().filter(|_| response.status.as_u16() == 201) else {
            return Ok(());
        };
        if let Some(id) = created["id"].as_str() {
            run.track("resources", id);
        }
        let targets = created["targets"].as_array().cloned().unwrap_or_default();
        expect(
            !targets.iter().any(|t| t == &json!(stolen)),
            format!(
                "a VEN granted its own resource the target {stolen:?}. Targets on a resource are a \
                 grant, so this is a VEN awarding itself the right to read every event aimed at \
                 that group"
            ),
        )
    })
}

/// A VEN reads only the reports it filed.
///
/// The most sensitive owned type and the one a suite is least likely to name: a report is a
/// customer's meter data. `report` carries no `targets` at all, so an implementation that reaches
/// for the targeting rule here admits *everyone* — the same shape as the subscription leak this
/// suite was extended to catch, where one backend let any VEN read every subscription in the VTN.
async fn ven_reads_only_its_own_reports(run: &Runner) -> Outcome {
    check!({
        // Seed one first. This check used to read a collection nothing in the suite ever wrote to,
        // so against a fresh VTN it asserted that an empty list contained no foreign reports —
        // which is true of every implementation, conformant or not (rule 18).
        let event_id = seed_event(run).await?;
        let filed = make_report(run, report_body(&event_id, &unique("resource"), 1.0)).await?;

        let listed = attempt(
            run.virtual_end_node(),
            "GET",
            "reports",
            &Query::new().limit(50),
            None,
            &[],
        )
        .await?;
        if !listed.status.is_success() {
            return Err(Outcome::Failed(format!(
                "GET /reports as a VEN answered {}",
                listed.status
            )));
        }
        let Some(items) = listed.json().and_then(|v| v.as_array().cloned()) else {
            return Err(Outcome::Failed(
                "GET /reports did not return an array".into(),
            ));
        };
        let client_id = run
            .target
            .ven_client_id
            .as_deref()
            .expect("the precondition guarantees a clientID");
        expect(
            items.iter().any(|r| r["id"] == filed["id"]),
            "a VEN could not read back the report it had just filed, so this check has nothing to \
             be right about"
                .to_string(),
        )?;
        let foreign: Vec<&Value> = items
            .iter()
            .filter(|r| r["clientID"].as_str().is_some_and(|c| c != client_id))
            .collect();
        expect(
            foreign.is_empty(),
            format!(
                "a VEN read {} report(s) filed by another client. A report is a customer's meter \
                 data, and it carries no targets — so target-based filtering admits everyone here",
                foreign.len()
            ),
        )
    })
}

/// A report a VEN files comes back with everything it carried.
///
/// Reports are the object with the least coverage anywhere — in this suite, and in the two
/// implementations it has been pointed at. They are also the only object carrying a customer's
/// meter data, and the only one whose contents a settlement process reads back months later. A
/// round trip that loses an interval id, rounds a decimal or drops a `payloadDescriptor` is a
/// billing dispute, and none of the three fails loudly at the time.
async fn report_round_trips(run: &Runner) -> Outcome {
    check!({
        let event_id = seed_event(run).await?;
        let resource = unique("resource");
        let filed = make_report(run, report_body(&event_id, &resource, 12.34)).await?;
        let id = filed["id"]
            .as_str()
            .ok_or_else(|| Outcome::Failed("a created report carries no id".into()))?;

        let read = fetch(
            run.business_logic(),
            &format!("reports/{id}"),
            &Query::new(),
        )
        .await?;
        expect(
            read["eventID"].as_str() == Some(event_id.as_str()),
            format!("the report came back naming eventID {:?}", read["eventID"]),
        )?;
        expect(
            read["objectType"].as_str() == Some("REPORT"),
            format!(
                "the report's objectType is {:?}, not REPORT",
                read["objectType"]
            ),
        )?;

        let resources = read["resources"].as_array().cloned().unwrap_or_default();
        expect(
            resources.len() == 1,
            format!(
                "the report came back with {} resource(s), not 1",
                resources.len()
            ),
        )?;
        expect(
            resources[0]["resourceName"].as_str() == Some(resource.as_str()),
            format!(
                "the resource came back as {:?} rather than {resource:?}",
                resources[0]["resourceName"]
            ),
        )?;

        let intervals = resources[0]["intervals"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        expect(
            intervals.len() == 1,
            format!(
                "the resource came back with {} interval(s), not 1",
                intervals.len()
            ),
        )?;
        expect(
            intervals[0]["id"].as_i64() == Some(0),
            format!(
                "the interval id came back as {:?}; a report quotes the *event's* interval ids so \
                 the VTN can correlate them [UG §7.5], and losing them makes the data unusable",
                intervals[0]["id"]
            ),
        )?;

        let values = intervals[0]["payloads"][0]["values"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        expect(
            values.len() == 1,
            format!(
                "the payload came back with {} value(s), not 1",
                values.len()
            ),
        )?;
        expect(
            values[0].as_f64() == Some(12.34),
            format!(
                "12.34 kWh came back as {:?}. A meter reading that does not survive a round trip \
                 is a settlement discrepancy nobody notices until the bill",
                values[0]
            ),
        )?;

        // The descriptor is what says what the number means. A VTN that drops it returns a series
        // of bare numbers whose unit is a guess.
        let descriptors = read["payloadDescriptors"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        expect(
            descriptors
                .iter()
                .any(|d| d["payloadType"].as_str() == Some("USAGE")),
            "the report's payloadDescriptors did not survive the round trip; without one the \
             values have no unit and no readingType"
                .to_string(),
        )
    })
}

/// The VTN stamps a report's `clientID`; a body cannot claim one.
///
/// `[Def §VEN created object privacy]` puts the identity on the credential, and every ownership
/// decision downstream reads that field. A VTN that copies it out of the body lets one VEN file a
/// report *as* another — which hides the reading from its author and shows it to a competitor, and
/// does both silently.
async fn report_is_stamped_not_claimed(run: &Runner) -> Outcome {
    check!({
        let event_id = seed_event(run).await?;
        let mine = run
            .target
            .ven_client_id
            .as_deref()
            .expect("the precondition guarantees a clientID");

        let mut body = report_body(&event_id, &unique("resource"), 1.0);
        body["clientID"] = json!(format!("{RUN_PREFIX}somebody-else"));
        let filed = make_report(run, body).await?;

        // Some VTNs omit `clientID` from the response entirely, which is a defensible reading of a
        // field the schema does not list. What none of them may do is echo the claim.
        match filed["clientID"].as_str() {
            None => Ok(()),
            Some(stamped) => expect(
                stamped == mine,
                format!(
                    "the report came back owned by {stamped:?}; the body claimed a clientID and \
                     the VTN believed it, so a VEN can file a report as another VEN"
                ),
            ),
        }
    })
}

/// `GET /reports?eventID=` narrows to that event.
///
/// The filter a settlement process actually uses: reports accumulate without bound, and reading
/// them back per event is the only query that stays bounded. A filter that is parsed and ignored
/// returns every report in the VTN and the caller cannot tell — the same shape as D-045.
async fn report_filters_by_event(run: &Runner) -> Outcome {
    check!({
        let wanted = seed_event(run).await?;
        let other = seed_event(run).await?;
        make_report(run, report_body(&wanted, &unique("resource"), 1.0)).await?;
        make_report(run, report_body(&other, &unique("resource"), 2.0)).await?;

        let listed = fetch(
            run.business_logic(),
            "reports",
            &Query::new().param("eventID", &wanted).limit(50),
        )
        .await?;
        let items = listed.as_array().cloned().unwrap_or_default();
        expect(
            !items.is_empty(),
            "?eventID= returned nothing for an event that has a report".to_string(),
        )?;
        let strays: Vec<&Value> = items
            .iter()
            .filter(|r| r["eventID"].as_str() != Some(wanted.as_str()))
            .collect();
        expect(
            strays.is_empty(),
            format!(
                "?eventID={wanted} returned {} report(s) belonging to other events, so the filter \
                 was accepted and not applied",
                strays.len()
            ),
        )
    })
}

/// A report has to name an event that exists.
///
/// `eventID` is required and is a reference `[API reportRequest]`. A report pointing at nothing
/// cannot be correlated with the intervals it reports on, which is the only thing that makes the
/// numbers mean anything. As with `event-needs-a-programme`, the specification does not pick a
/// status, so any refusal counts.
async fn report_needs_an_event(run: &Runner) -> Outcome {
    check!({
        let response = attempt(
            run.virtual_end_node(),
            "POST",
            "reports",
            &Query::new(),
            Some(&report_body(
                "oadr-conformance-no-such-event",
                &unique("resource"),
                1.0,
            )),
            &[],
        )
        .await?;
        if response.status.as_u16() == 201
            && let Some(id) = response
                .json()
                .and_then(|v| v["id"].as_str().map(str::to_string))
        {
            run.track("reports", id);
        }
        expect(
            matches!(response.status.as_u16(), 400 | 404 | 409),
            format!(
                "a report naming an event that does not exist answered {} (expected 400, 404 or \
                 409); a dangling eventID leaves meter data nothing can be correlated with",
                response.status
            ),
        )
    })
}

/// A VEN reads only the resources belonging to it.
///
/// The other half of the grant, and the half a suite is most likely to leave out. A `resource`
/// carries the targets business logic granted it `[Def §Object Privacy, CL 3.1.0 issue 321]`, so
/// reading another VEN's resources is reading which programmes a competitor was enrolled in — and
/// `/resources` only became a first-class collection in 3.1.0 `[CL 3.1.0 issue 306]`, which is
/// exactly the sort of endpoint an implementation adds without carrying the ownership rule across.
async fn ven_reads_only_its_own_resources(run: &Runner) -> Outcome {
    check!({
        let listed = attempt(
            run.virtual_end_node(),
            "GET",
            "resources",
            &Query::new().limit(50),
            None,
            &[],
        )
        .await?;
        if !listed.status.is_success() {
            return Err(Outcome::Failed(format!(
                "GET /resources as a VEN answered {}",
                listed.status
            )));
        }
        let Some(items) = listed.json().and_then(|v| v.as_array().cloned()) else {
            return Err(Outcome::Failed(
                "GET /resources did not return an array".into(),
            ));
        };

        // Which VENs the caller owns. A resource names its VEN, and 3.1.1 dropped `clientID` from
        // the resource body as redundant with it, so ownership is established through the VEN
        // rather than read off the resource.
        let mine = fetch(run.virtual_end_node(), "vens", &Query::new().limit(50)).await?;
        let mine: Vec<String> = mine
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(|ven| ven["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let foreign: Vec<&Value> = items
            .iter()
            .filter(|r| {
                r["venID"]
                    .as_str()
                    .is_some_and(|ven| !mine.iter().any(|m| m == ven))
            })
            .collect();
        expect(
            foreign.is_empty(),
            format!(
                "a VEN read {} resource(s) under a VEN it does not own. A resource carries the \
                 targets business logic granted it, so this is another operator's programme \
                 enrolment",
                foreign.len()
            ),
        )
    })
}

/// `?active=true` drops events whose intervals have all elapsed.
///
/// New in 3.1.0 `[CL 3.1.0 issue 234]`, and the parameter a poller uses on every cycle: "ignore
/// events that have transpired" `[API /events ?active]`. It is not "events happening right now" —
/// an event scheduled for next week has not transpired, so filtering it out would hide the
/// day-ahead schedule from every client that asked to skip yesterday's.
async fn active_excludes_transpired_events(run: &Runner) -> Outcome {
    check!({
        let program = make_program(run, json!({ "programName": unique("active") })).await?;
        let program_id = program["id"].as_str().unwrap_or_default().to_string();

        let mut past = event_body(&program_id, &[]);
        past["intervalPeriod"] = json!({ "start": "2020-01-01T00:00:00Z", "duration": "PT15M" });
        let past = make_event(run, past).await?;
        let past_id = past["id"].as_str().unwrap_or_default().to_string();

        let mut future = event_body(&program_id, &[]);
        future["intervalPeriod"] = json!({ "start": "2099-01-01T00:00:00Z", "duration": "PT15M" });
        let future = make_event(run, future).await?;
        let future_id = future["id"].as_str().unwrap_or_default().to_string();

        let ids = |v: &Value| -> Vec<String> {
            v.as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|e| e["id"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };

        let unfiltered = fetch(
            run.business_logic(),
            "events",
            &Query::new().param("programID", &program_id).limit(50),
        )
        .await?;
        let unfiltered = ids(&unfiltered);
        expect(
            unfiltered.contains(&past_id) && unfiltered.contains(&future_id),
            "without ?active both events should be listed; the filter cannot be measured otherwise",
        )?;

        let filtered = fetch(
            run.business_logic(),
            "events",
            &Query::new()
                .param("programID", &program_id)
                .active(true)
                .limit(50),
        )
        .await?;
        let filtered = ids(&filtered);
        expect(
            !filtered.contains(&past_id),
            "?active=true listed an event whose only interval ended in 2020",
        )?;
        expect(
            filtered.contains(&future_id),
            "?active=true dropped an event scheduled for 2099. \"Active\" is \"has not \
             transpired\", so a future event stays: dropping it hides the day-ahead schedule from \
             every client that asked to skip yesterday's",
        )
    })
}

// ---------------------------------------------------------------------------
// The MQTT notifier binding
// ---------------------------------------------------------------------------

/// The MQTT binding from `GET /notifiers`, or a skip when this VTN offers none.
///
/// A runtime skip rather than a precondition: whether a VTN has a broker is something only the VTN
/// can say, and it says it here. MQTT is **optional** — "A VTN **MAY** support notifications via
/// MQTT" `[Notifiers §7.2]` — so its absence is not a failure and must not be reported as one.
async fn mqtt_binding(run: &Runner) -> Result<Value, Outcome> {
    let notifiers = fetch(run.business_logic(), "notifiers", &Query::new()).await?;
    notifiers
        .get("MQTT")
        .filter(|v| !v.is_null())
        .cloned()
        .ok_or_else(|| {
            Outcome::Skipped("this VTN offers no MQTT notifier binding, which is optional".into())
        })
}

async fn mqtt_binding_shape(run: &Runner) -> Outcome {
    check!({
        let mqtt = mqtt_binding(run).await?;
        expect(
            mqtt["URIS"].as_array().is_some_and(|u| !u.is_empty()),
            format!("the MQTT binding names no URIS: {mqtt}"),
        )?;
        expect(
            mqtt["serialization"].as_str() == Some("JSON"),
            format!(
                "serialization is {} (expected JSON, the only format the binding defines)",
                mqtt["serialization"]
            ),
        )?;
        let method = mqtt["authentication"]["method"].as_str();
        expect(
            matches!(
                method,
                Some("ANONYMOUS" | "OAUTH2_BEARER_TOKEN" | "CERTIFICATE")
            ),
            format!(
                "authentication.method is {method:?}; the binding defines ANONYMOUS, \
                 OAUTH2_BEARER_TOKEN and CERTIFICATE, and a client cannot connect without \
                 recognising one"
            ),
        )
    })
}

async fn mqtt_collection_topics_are_business_logic_only(run: &Runner) -> Outcome {
    check!({
        mqtt_binding(run).await?;
        for path in [
            "notifiers/mqtt/topics/events",
            "notifiers/mqtt/topics/reports",
            "notifiers/mqtt/topics/vens",
        ] {
            let response = attempt(
                run.virtual_end_node(),
                "GET",
                path,
                &Query::new(),
                None,
                &[],
            )
            .await?;
            expect(
                !response.status.is_success(),
                format!(
                    "a VEN obtained {path}. Subscribing to a collection-wide topic yields every \
                     object of that type with its full target set, which is every other VEN's \
                     dispatch schedule"
                ),
            )?;
        }
        Ok(())
    })
}

async fn mqtt_ven_scoped_topics(run: &Runner) -> Outcome {
    check!({
        mqtt_binding(run).await?;
        let ven_id = grant_target(run, &format!("{RUN_PREFIX}mqtt")).await?;

        let response = attempt(
            run.virtual_end_node(),
            "GET",
            &format!("notifiers/mqtt/topics/vens/{ven_id}/events"),
            &Query::new(),
            None,
            &[],
        )
        .await?;
        expect(
            response.status.is_success(),
            format!(
                "a VEN could not obtain its own event topics ({}); the VEN-scoped endpoints are \
                 what 3.1 added for exactly this client",
                response.status
            ),
        )?;
        let body = response
            .json()
            .ok_or_else(|| Outcome::Failed("the topic response carried no JSON".into()))?;
        for operation in ["UPDATE", "DELETE"] {
            expect(
                body["topics"][operation]
                    .as_str()
                    .is_some_and(|t| !t.is_empty()),
                format!("the topic set has no {operation} entry: {body}"),
            )?;
        }
        Ok(())
    })
}

async fn mqtt_foreign_ven_topics_refused(run: &Runner) -> Outcome {
    check!({
        mqtt_binding(run).await?;

        // Somebody else's VEN object.
        let created = attempt(
            run.business_logic(),
            "POST",
            "vens",
            &Query::new(),
            Some(&json!({
                "objectType": "BL_VEN_REQUEST",
                "clientID": format!("{RUN_PREFIX}another-client"),
                "venName": unique("other-ven"),
            })),
            &[],
        )
        .await?;
        if created.status.as_u16() != 201 {
            return Err(Outcome::Failed(format!(
                "could not create a second VEN object to test against: {}",
                created.status
            )));
        }
        let other_id = created
            .json()
            .and_then(|v| v["id"].as_str().map(str::to_string))
            .ok_or_else(|| Outcome::Failed("the second VEN object has no id".into()))?;
        run.track("vens", &other_id);

        let response = attempt(
            run.virtual_end_node(),
            "GET",
            &format!("notifiers/mqtt/topics/vens/{other_id}/events"),
            &Query::new(),
            None,
            &[],
        )
        .await?;
        expect(
            !response.status.is_success(),
            format!(
                "a VEN obtained another VEN's topic names ({}); those topics carry the other \
                 VEN's dispatch",
                response.status
            ),
        )
    })
}

// ---------------------------------------------------------------------------
// Extensions
// ---------------------------------------------------------------------------

async fn etag_and_conditional_read(run: &Runner) -> Outcome {
    check!({
        let first = attempt(
            run.business_logic(),
            "GET",
            "programs",
            &Query::new().limit(1),
            None,
            &[],
        )
        .await?;
        let Some(etag) = first.header("etag").map(str::to_string) else {
            return Err(Outcome::Failed(
                "GET /programs carried no ETag; every deployment surveyed polls, so a repeat read \
                 costs the whole collection again"
                    .into(),
            ));
        };

        let second = attempt(
            run.business_logic(),
            "GET",
            "programs",
            &Query::new().limit(1),
            None,
            &[("if-none-match".to_string(), etag)],
        )
        .await?;
        expect(
            second.status.as_u16() == 304,
            format!(
                "a matching If-None-Match answered {} (expected 304)",
                second.status
            ),
        )?;
        expect(
            second.body.is_empty(),
            "the 304 carried a body; RFC 9110 says it must not".to_string(),
        )
    })
}

/// An error body is served as `application/problem+json`.
///
/// An **extension**, deliberately, and the severity is the whole point of the check. RFC 9457 §3
/// defines that media type for exactly this body, and the Zalando guidelines the specification
/// borrows the shape from say the same. `openadr3.yaml` nonetheless declares every error response as
/// `application/json`, so a VTN that sends the plain type is conformant and a report that called it
/// a failure would be filing this project's opinion as a requirement — the mistake D-075 exists to
/// remember.
///
/// It is still worth *measuring*: a client built from the document's own schema may refuse a media
/// type the document does not mention, and a matrix that says which peers send which is the only
/// place that disagreement becomes visible before it is a support ticket.
async fn problem_uses_the_rfc_9457_media_type(run: &Runner) -> Outcome {
    check!({
        let response = attempt(
            run.business_logic(),
            "GET",
            "programs/oadr-conformance-no-such-program",
            &Query::new(),
            None,
            &[],
        )
        .await?;
        expect(
            response.status.as_u16() == 404,
            format!("a missing programme answered {}", response.status),
        )?;
        expect(
            response.is_problem_json(),
            format!(
                "the error body is {:?} rather than application/problem+json. Both are defensible \
                 — the OpenAPI document says application/json and RFC 9457 says otherwise — so this \
                 is information, not non-conformance",
                response.header("content-type").unwrap_or("(absent)")
            ),
        )
    })
}

async fn problem_carries_a_traceable_instance(run: &Runner) -> Outcome {
    check!({
        let response = attempt(
            run.business_logic(),
            "GET",
            "programs/oadr-conformance-no-such-program",
            &Query::new(),
            None,
            &[],
        )
        .await?;
        let problem = response
            .problem()
            .ok_or_else(|| Outcome::Failed("the error carried no problem body".into()))?;
        let Some(instance) = problem.instance else {
            return Err(Outcome::Failed(
                "problem.instance is absent; without it an error a client quotes cannot be found \
                 in the VTN's log"
                    .into(),
            ));
        };
        let Some(header) = response.header("x-request-id") else {
            return Err(Outcome::Failed(
                "the response carried no x-request-id to correlate problem.instance with".into(),
            ));
        };
        expect(
            instance == header,
            format!(
                "problem.instance is {instance:?} and x-request-id is {header:?}; the field is \
                 only worth having if the two are the same string"
            ),
        )
    })
}

async fn program_name_lookup(run: &Runner) -> Outcome {
    check!({
        let name = unique("lookup");
        let created = make_program(run, json!({ "programName": name.clone() })).await?;
        let id = created["id"].as_str().unwrap_or_default().to_string();

        let found = fetch(
            run.business_logic(),
            "programs",
            &Query::new().param("programName", &name),
        )
        .await?;
        let items = found.as_array().cloned().unwrap_or_default();
        expect(
            items.len() == 1 && items[0]["id"] == id.as_str(),
            format!(
                "?programName={name} returned {} programme(s); finding one tariff among hundreds \
                 otherwise means paging the whole collection",
                items.len()
            ),
        )
    })
}

// ---------------------------------------------------------------------------
// HTTP contract
// ---------------------------------------------------------------------------

/// A `401` says how to authenticate.
///
/// RFC 9110 §11.6.1: a `401` **MUST** carry `WWW-Authenticate`. Without it a client that has just
/// been refused cannot tell a missing token from a wrong realm from an unsupported scheme, and the
/// OpenADR enrolment flow — discover the VTN, discover the token endpoint, present a bearer token —
/// is exactly the flow that walks into this.
async fn unauthorized_names_the_scheme(run: &Runner) -> Outcome {
    check!({
        let client = run.anonymous().map_err(transport)?;
        let response = attempt(&client, "GET", "programs", &Query::new(), None, &[]).await?;
        if response.status.as_u16() != 401 {
            // A VTN that serves anonymous readers, or that answers 403 instead, is not wrong —
            // there is simply no 401 to inspect. Reporting that as a pass would be counting an
            // absence of evidence as evidence.
            return Err(Outcome::Skipped(format!(
                "an unauthenticated GET /programs answered {} rather than 401, so there is no \
                 challenge to inspect",
                response.status
            )));
        }
        expect(
            response.header("www-authenticate").is_some(),
            "the 401 carried no WWW-Authenticate header".to_string(),
        )
    })
}

/// A response carrying an access token is never stored.
///
/// RFC 6749 §5.1, a **MUST**, and the one HTTP header on the token endpoint whose absence is
/// invisible until a proxy hands one client another client's token.
///
/// The check does not need a working credential: the header is a property of the endpoint, so a
/// deliberately wrong secret exercises it without a valid one existing. A VTN that delegates token
/// issuance has no endpoint to check and the result is a skip.
async fn token_response_is_not_stored(run: &Runner) -> Outcome {
    check!({
        let client = run.anonymous().map_err(transport)?;
        let response = attempt(
            &client,
            "POST",
            "auth/token",
            &Query::new(),
            None,
            &[
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                // reqwest sends no body without one; the credentials are deliberately absent, and
                // the endpoint's answer to that is still an answer from the endpoint.
                ("content-length".to_string(), "0".to_string()),
            ],
        )
        .await?;
        if matches!(response.status.as_u16(), 404 | 405 | 501) {
            return Err(Outcome::Skipped(format!(
                "POST /auth/token answered {}, so this VTN does not run its own token endpoint",
                response.status
            )));
        }
        let value = response
            .header("cache-control")
            .unwrap_or_default()
            .to_ascii_lowercase();
        expect(
            value.split(',').any(|d| d.trim() == "no-store"),
            format!(
                "the token endpoint answered Cache-Control: {:?}; RFC 6749 §5.1 requires no-store \
                 on any response that may carry a token",
                response.header("cache-control").unwrap_or("<absent>")
            ),
        )
    })
}

/// A body labelled with a media type the endpoint does not declare is refused.
///
/// `openadr3.yaml` declares one request media type for every write: `application/json`. A VTN that
/// parses `text/plain` as JSON has accepted something the contract does not describe, and the cost
/// falls on the client: a misconfigured serialiser is reported as a syntax error in JSON it never
/// sent. RFC 9110 §15.5.16 names the status for it.
///
/// **Recommended** rather than **Required**: the OpenADR documents say nothing about media-type
/// negotiation, so a lenient VTN is interoperable rather than wrong.
async fn foreign_media_type_is_refused(run: &Runner) -> Outcome {
    check!({
        let name = unique("media-type");
        let response = attempt(
            run.business_logic(),
            "POST",
            "programs",
            &Query::new(),
            Some(&json!({ "programName": name.clone() })),
            &[("content-type".to_string(), "text/plain".to_string())],
        )
        .await?;
        // Whatever the VTN decided, it may have created the programme; track it either way so the
        // run leaves nothing behind.
        if response.status.as_u16() == 201
            && let Some(id) = response
                .json()
                .and_then(|b| b["id"].as_str().map(str::to_string))
        {
            run.track("programs", id);
        }
        expect(
            response.status.as_u16() == 415,
            format!(
                "a text/plain body answered {} (expected 415); the document declares \
                 application/json and nothing else",
                response.status
            ),
        )
    })
}

/// A read says how it may be cached.
///
/// Object privacy makes the body a function of the *reader*: two VENs asking for the same URL are
/// entitled to different targets on the same event. A response that says nothing about caching is
/// one an intermediary is free to key by URL alone.
///
/// An **extension**: no OpenADR document requires it, and this is the report saying whether a peer
/// happens to be safe behind a shared cache.
async fn reads_declare_a_caching_policy(run: &Runner) -> Outcome {
    check!({
        let response = attempt(
            run.business_logic(),
            "GET",
            "programs",
            &Query::new().limit(1),
            None,
            &[],
        )
        .await?;
        let directives = response
            .header("cache-control")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let varies_on_credentials = response
            .headers
            .get_all("vary")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| {
                v.split(',')
                    .any(|f| f.trim().eq_ignore_ascii_case("authorization"))
            });
        expect(
            directives.contains("private")
                || directives.contains("no-store")
                || varies_on_credentials,
            format!(
                "GET /programs answered Cache-Control: {:?} and no Vary on authorization, so a \
                 shared cache may key this body by URL alone — and object privacy means the body \
                 depends on who asked",
                response.header("cache-control").unwrap_or("<absent>")
            ),
        )
    })
}
