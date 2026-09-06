//! The VEN runtime against a real VTN, over a real socket.
//!
//! Everything here is a whole-loop assertion. The runtime's parts are unit-tested; what these cover
//! is whether they add up to a VEN — one that registers without duplicating itself, sees only what
//! it has been granted, notices a cancellation, files each report once, and does all of that
//! through the same HTTP the field would.

#![cfg(all(feature = "vtn", feature = "ven"))]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use openadr::{
    client::{Client, VirtualEndNode},
    core::{Clock, FixedClock},
    model::{
        ClientId, Interval, ObjectId, ProgramName, ProgramRequest, ReportResource, ResourceName,
        Timestamp, Value, ValuesMap,
    },
    ven::{DueReport, Meter, VenConfig, VenError, VenRuntime},
    vtn::{Vtn, VtnConfig, auth::StaticTokenAuth, store::MemoryStorage},
};
use serde_json::{Value as Json, json};

const BL: &str = "bl-secret";
const VEN: &str = "ven-secret";

fn now() -> Timestamp {
    "2026-02-11T06:00:00Z".parse().unwrap()
}

/// A clock a test can move.
///
/// [`FixedClock`] answers "what does the VEN do at this instant"; this answers "what does it do
/// *after* one" — which is the only way to see anything that ages, and the reason nothing did.
#[derive(Debug)]
struct MovableClock(Mutex<Timestamp>);

impl MovableClock {
    fn at(t: &str) -> Arc<Self> {
        Arc::new(Self(Mutex::new(t.parse().unwrap())))
    }

    fn advance(&self, span: jiff::Span) {
        let mut now = self.0.lock().unwrap();
        *now = now.checked_add(span).unwrap();
    }
}

impl Clock for MovableClock {
    fn now(&self) -> Timestamp {
        *self.0.lock().unwrap()
    }
}

struct Fixture {
    base: String,
    router: axum::Router,
}

impl Fixture {
    async fn start() -> Self {
        let vtn = Vtn::builder()
            .storage(MemoryStorage::shared())
            .authenticator(Arc::new(
                StaticTokenAuth::new("http://unused/auth/token")
                    .with_business_logic(BL, ClientId::new("bl").unwrap())
                    .with_ven(VEN, ClientId::new("ven-client").unwrap()),
            ))
            .clock(Arc::new(FixedClock::new(now())))
            .config(VtnConfig {
                base_path: "/openadr3/3.1.0".into(),
                ..Default::default()
            })
            .build();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = vtn.router();
        let served = router.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, served).await;
        });
        Self {
            base: format!("http://{addr}/openadr3/3.1.0"),
            router,
        }
    }

    /// A runtime on the fixture's frozen clock.
    ///
    /// The skew tolerance is opened right up, and only here: these runtimes are deliberately
    /// frozen at a date the host clock is nowhere near, and `Date` comes from the host. The guard
    /// itself is exercised by `the_clock_is_checked_against_the_vtns_and_a_wrong_one_is_fatal`,
    /// which uses the real default.
    fn ven(&self, config: VenConfig) -> VenRuntime {
        VenRuntime::new(
            self.client(),
            config.with_max_clock_skew(std::time::Duration::from_secs(86_400 * 3650)),
        )
        .with_clock(Arc::new(FixedClock::new(now())))
    }

    /// The same, with a meter and a clock the caller chooses.
    fn metered<M: Meter>(&self, config: VenConfig, meter: M, at: &str) -> VenRuntime<M> {
        VenRuntime::with_meter(
            self.client(),
            config.with_max_clock_skew(std::time::Duration::from_secs(86_400 * 3650)),
            meter,
        )
        .with_clock(Arc::new(FixedClock::new(at.parse().unwrap())))
    }

    /// A runtime on a clock the test drives.
    fn moving(&self, config: VenConfig, clock: Arc<MovableClock>) -> VenRuntime {
        VenRuntime::new(
            self.client(),
            config.with_max_clock_skew(std::time::Duration::from_secs(86_400 * 3650)),
        )
        .with_clock(clock)
    }

    fn client(&self) -> Client<VirtualEndNode> {
        Client::<VirtualEndNode>::builder(&self.base)
            .unwrap()
            .bearer_token(VEN)
            .build()
            .unwrap()
    }

    /// A business-logic write, straight through the router.
    async fn bl(&self, method: &str, path: &str, body: Option<Json>) -> Json {
        use axum::http::{Method, Request, header};
        let mut builder = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(format!("/openadr3/3.1.0{path}"))
            .header(header::AUTHORIZATION, format!("Bearer {BL}"));
        let request = match body {
            Some(b) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                builder
                    .body(axum::body::Body::from(serde_json::to_vec(&b).unwrap()))
                    .unwrap()
            }
            None => builder.body(axum::body::Body::empty()).unwrap(),
        };
        use tower::ServiceExt;
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Json = serde_json::from_slice(&bytes).unwrap_or(Json::Null);
        assert!(status.is_success(), "{method} {path} → {status}: {value}");
        value
    }

    async fn program(&self, name: &str) -> ObjectId {
        self.bl("POST", "/programs", Some(json!({ "programName": name })))
            .await["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Grant a client a target, which is the only way a VEN comes to see a targeted object.
    async fn grant(&self, ven_id: &ObjectId, client_id: &str, ven_name: &str, targets: &[&str]) {
        self.bl(
            "PUT",
            &format!("/vens/{ven_id}"),
            Some(json!({
                "objectType": "BL_VEN_REQUEST",
                "clientID": client_id,
                "venName": ven_name,
                "targets": targets,
            })),
        )
        .await;
    }

    async fn event(&self, program: &ObjectId, name: &str, targets: &[&str]) -> ObjectId {
        self.bl(
            "POST",
            "/events",
            Some(json!({
                "programID": program,
                "eventName": name,
                "targets": targets,
                "intervalPeriod": { "start": "2026-02-11T07:00:00Z", "duration": "PT1H" },
                "intervals": [
                    { "id": 0, "payloads": [{ "type": "IMPORT_CAPACITY_LIMIT", "values": [60] }] },
                    { "id": 1, "payloads": [{ "type": "IMPORT_CAPACITY_LIMIT", "values": [40] }] }
                ],
                "reportDescriptors": [
                    { "payloadType": "USAGE", "startInterval": 0, "numIntervals": 1,
                      "frequency": 1, "repeat": 2 }
                ]
            })),
        )
        .await["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }
}

/// A VTN whose *default* page size is smaller than the schema's maximum.
///
/// `limit` has a `maximum` of 50 in `openadr3.yaml` and no `default`, so what a VTN returns when it
/// is sent no `limit` is entirely its own business. This layer makes that concrete: an explicit
/// `limit` is honoured, and a request without one is answered twenty at a time.
fn pages_twenty_by_default(router: axum::Router) -> axum::Router {
    use axum::http::{Request, Uri};
    router.layer(axum::middleware::from_fn(
        async |mut request: Request<axum::body::Body>, next: axum::middleware::Next| {
            let uri = request.uri().clone();
            let query = uri.query().unwrap_or_default();
            if !query.split('&').any(|p| p.starts_with("limit=")) {
                let separator = if query.is_empty() { "" } else { "&" };
                let rebuilt = format!("{}?{query}{separator}limit=20", uri.path());
                if let Ok(uri) = rebuilt.parse::<Uri>() {
                    *request.uri_mut() = uri;
                }
            }
            next.run(request).await
        },
    ))
}

#[tokio::test]
async fn a_ven_follows_every_event_against_a_vtn_that_pages_small() {
    // The VEN decides "was that the whole collection?" from the length of one page. That question
    // only has an answer if the VEN named the page size: against a VTN that pages at twenty, a
    // reader comparing against its own default of fifty concludes it has everything after twenty
    // events and follows a truncated schedule — silently, with a `200` at both ends.
    let vtn = Vtn::builder()
        .storage(MemoryStorage::shared())
        .authenticator(Arc::new(
            StaticTokenAuth::new("http://unused/auth/token")
                .with_business_logic(BL, ClientId::new("bl").unwrap())
                .with_ven(VEN, ClientId::new("ven-client").unwrap()),
        ))
        .clock(Arc::new(FixedClock::new(now())))
        .config(VtnConfig {
            base_path: "/openadr3/3.1.0".into(),
            ..Default::default()
        })
        .build();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = vtn.router();
    let served = pages_twenty_by_default(router.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });

    let fixture = Fixture {
        base: format!("http://{addr}/openadr3/3.1.0"),
        router,
    };

    // Untargeted, so a VEN that names no targets may read them all.
    let program = fixture.program("wide-programme").await;
    const EVENTS: usize = 60;
    for i in 0..EVENTS {
        fixture.event(&program, &format!("event-{i}"), &[]).await;
    }

    let ven = fixture.ven(VenConfig::new("wide-ven".parse().unwrap()));
    ven.sync().await.unwrap();
    assert_eq!(
        ven.events().len(),
        EVENTS,
        "the VEN stopped at the peer's page boundary and followed a truncated schedule"
    );
}

#[tokio::test]
async fn registering_twice_produces_one_ven_and_one_set_of_resources() {
    // A VEN restarts. Re-registering must find itself rather than create a second object or fail
    // because the first one exists — `venName` is unique per VTN, so both would be errors a VEN
    // cannot recover from on its own.
    let fixture = Fixture::start().await;
    let config = VenConfig::new("charge-point-42".parse().unwrap())
        .with_resources([ResourceName::new("connector-1").unwrap()]);

    let first = fixture.ven(config.clone());
    let ven = first.register().await.unwrap();
    assert_eq!(first.ven_id().as_ref(), Some(&ven.id));
    assert_eq!(first.resources().len(), 1);

    // A second process, same configuration, no shared state.
    let second = fixture.ven(config);
    let again = second.register().await.unwrap();
    assert_eq!(again.id, ven.id, "registration created a second VEN object");
    assert_eq!(second.resources().len(), 1, "and a second resource with it");

    let vens = fixture.bl("GET", "/vens", None).await;
    assert_eq!(vens.as_array().unwrap().len(), 1);
    let resources = fixture.bl("GET", "/resources", None).await;
    assert_eq!(resources.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_ven_sees_only_what_it_was_granted_and_notices_a_cancellation() {
    let fixture = Fixture::start().await;
    let program = fixture.program("grid-aware-charging").await;

    let ven = fixture.ven(
        VenConfig::new("charge-point-42".parse().unwrap())
            .with_targets(["group1".parse().unwrap()]),
    );
    let registered = ven.register().await.unwrap();

    // Targeted, and this VEN has been granted nothing yet.
    let mine = fixture.event(&program, "for-group1", &["group1"]).await;
    let theirs = fixture.event(&program, "for-group2", &["group2"]).await;

    let outcome = ven.sync().await.unwrap();
    assert!(
        outcome.added.is_empty(),
        "a VEN with no grant must see no targeted event: {outcome:?}"
    );

    // Business logic grants the target. Only then does the event exist as far as this VEN is
    // concerned — which is the whole of object privacy, seen from the other end.
    fixture
        .grant(&registered.id, "ven-client", "charge-point-42", &["group1"])
        .await;

    let outcome = ven.sync().await.unwrap();
    assert_eq!(outcome.added, vec![mine.clone()], "{outcome:?}");
    assert!(
        !outcome.added.contains(&theirs),
        "group2's event reached a group1 VEN"
    );

    // A quiet cycle is a 304 and no work.
    let quiet = ven.sync().await.unwrap();
    assert!(
        quiet.is_quiet(),
        "nothing changed, so nothing should be: {quiet:?}"
    );

    // The timeline is built and answers the question a VEN actually asks.
    let during = "2026-02-11T07:30:00Z".parse::<Timestamp>().unwrap();
    let segment = ven
        .active_at(during)
        .expect("the event is in force at 07:30");
    assert_eq!(segment.event_id, mine);
    assert_eq!(segment.interval_id, 0);
    assert_eq!(
        ven.next_change(during),
        Some("2026-02-11T08:00:00Z".parse().unwrap())
    );

    // Deleting an event is how OpenADR cancels one, and a VEN that missed that would keep
    // curtailing for an instruction that has been withdrawn.
    fixture.bl("DELETE", &format!("/events/{mine}"), None).await;
    let outcome = ven.sync().await.unwrap();
    assert_eq!(outcome.removed, vec![mine], "{outcome:?}");
    assert!(ven.active_at(during).is_none());
}

/// A meter that answers every request with one reading per interval, and counts the asking.
#[derive(Debug, Default)]
struct CountingMeter {
    reads: std::sync::Mutex<Vec<(ObjectId, u64)>>,
}

#[async_trait]
impl Meter for CountingMeter {
    async fn read(&self, due: &DueReport) -> Result<Vec<ReportResource>, VenError> {
        self.reads
            .lock()
            .unwrap()
            .push((due.event_id.clone(), due.sequence));
        Ok(vec![ReportResource {
            resource_name: ResourceName::new(ResourceName::AGGREGATED).unwrap(),
            interval_period: None,
            intervals: due
                .interval_ids
                .iter()
                .map(|id| {
                    Interval::new(
                        *id,
                        vec![ValuesMap::new(
                            due.payload_type.clone(),
                            vec![Value::Integer(42)],
                        )],
                    )
                })
                .collect(),
        }])
    }
}

/// A meter with two resources, each reporting its own number.
#[derive(Default)]
struct TwoMeters;

#[async_trait]
impl Meter for TwoMeters {
    async fn read(&self, due: &DueReport) -> Result<Vec<ReportResource>, VenError> {
        let series = |name: &str, value: &str| ReportResource {
            resource_name: ResourceName::new(name).unwrap(),
            interval_period: None,
            intervals: due
                .interval_ids
                .iter()
                .map(|id| {
                    Interval::new(
                        *id,
                        vec![ValuesMap::new(
                            due.payload_type.clone(),
                            vec![Value::Number(value.parse().unwrap())],
                        )],
                    )
                })
                .collect(),
        };
        Ok(vec![
            series("connector-1", "1.5"),
            series("connector-2", "0.25"),
        ])
    }
}

#[tokio::test]
async fn a_descriptor_asking_to_aggregate_gets_one_summed_series() {
    // `[UG §7.7]`: a descriptor with `aggregate: true` asks for a single resource entry named
    // `AGGREGATED_REPORT`, and "aggregation means the data from a set of resources are summed".
    // The meter here does neither — it reports per connector, as a meter does — so if the runtime
    // does not sum and rename, the VTN receives two series under their own names and has no way to
    // tell it did not get what it asked for.
    let fixture = Fixture::start().await;
    let program = fixture.program("aggregated").await;

    fixture
        .bl(
            "POST",
            "/events",
            Some(json!({
                "programID": program,
                "eventName": "aggregate-me",
                "intervalPeriod": { "start": "2026-02-11T07:00:00Z", "duration": "PT1H" },
                "intervals": [
                    { "id": 0, "payloads": [{ "type": "IMPORT_CAPACITY_LIMIT", "values": [60] }] }
                ],
                "reportDescriptors": [
                    { "payloadType": "USAGE", "aggregate": true, "startInterval": 0,
                      "numIntervals": 1, "frequency": 1, "repeat": 1,
                      "readingType": "DIRECT_READ", "units": "KWH" }
                ]
            })),
        )
        .await;

    let runtime = fixture.metered(
        VenConfig::new("charger-7".parse().unwrap()),
        TwoMeters,
        "2026-02-11T09:00:00Z",
    );
    runtime.register().await.unwrap();
    runtime.sync().await.unwrap();

    let due = runtime.due_reports("2026-02-11T09:00:00Z".parse().unwrap());
    assert_eq!(due.len(), 1, "{due:?}");
    assert!(due[0].aggregate, "the descriptor asked for an aggregate");

    assert_eq!(runtime.submit_due_reports().await.unwrap().len(), 1);

    let reports = fixture.bl("GET", "/reports", None).await;

    // `[UG §7.6]`: "reports contain payloadDescriptors", and the descriptor is what turns a series
    // of bare numbers into a quantity. Everything it needs came from the `reportDescriptor` that
    // asked for the report.
    let descriptors = reports[0]["payloadDescriptors"].as_array().unwrap();
    assert_eq!(
        descriptors.len(),
        1,
        "{:?}",
        reports[0]["payloadDescriptors"]
    );
    assert_eq!(descriptors[0]["objectType"], "REPORT_PAYLOAD_DESCRIPTOR");
    assert_eq!(descriptors[0]["payloadType"], "USAGE");
    // The unit and the reading type the VTN asked for come back with the numbers, rather than
    // leaving a consumer to assume kilowatt-hours.
    assert_eq!(descriptors[0]["units"], "KWH");
    assert_eq!(descriptors[0]["readingType"], "DIRECT_READ");

    let resources = reports[0]["resources"].as_array().unwrap();
    assert_eq!(
        resources.len(),
        1,
        "an aggregate report carries one series, and the VTN was sent {}",
        resources.len()
    );
    assert_eq!(resources[0]["resourceName"], "AGGREGATED_REPORT");
    assert_eq!(
        resources[0]["intervals"][0]["payloads"][0]["values"][0],
        json!(1.75),
        "1.5 + 0.25 did not arrive as 1.75"
    );
}

#[tokio::test]
async fn a_due_report_is_filed_once_and_not_again() {
    // Filing the same report twice is a duplicate in somebody's settlement data, and the VTN will
    // accept both — nothing on that side deduplicates. Remembering what has been sent is the VEN's
    // job, and it is the one piece of its state that a restart must not lose.
    let fixture = Fixture::start().await;
    let program = fixture.program("compliance").await;

    let meter = Arc::new(CountingMeter::default());
    // Both report windows have closed by 09:00: the descriptor asks for one report per interval,
    // and the event's two intervals run 07:00–09:00.
    let runtime = fixture.metered(
        VenConfig::new("meter-1".parse().unwrap()),
        MeterHandle(meter.clone()),
        "2026-02-11T09:00:00Z",
    );

    runtime.register().await.unwrap();
    let event = fixture.event(&program, "compliance", &[]).await;
    runtime.sync().await.unwrap();

    let due = runtime.due_reports("2026-02-11T09:00:00Z".parse().unwrap());
    assert_eq!(due.len(), 2, "one report per interval: {due:?}");
    assert_eq!(due[0].event_id, event);
    assert_eq!(due[0].interval_ids, vec![0]);
    assert_eq!(due[1].interval_ids, vec![1]);

    let filed = runtime.submit_due_reports().await.unwrap();
    assert_eq!(filed.len(), 2);
    assert_eq!(meter.reads.lock().unwrap().len(), 2);

    // Asked again on the next cycle: nothing more is due, and the meter is not disturbed.
    let filed_again = runtime.submit_due_reports().await.unwrap();
    assert!(filed_again.is_empty(), "the same report was filed twice");
    assert_eq!(meter.reads.lock().unwrap().len(), 2);

    // The VTN holds exactly two, and they quote the event's interval ids so it can correlate them.
    let reports = fixture.bl("GET", "/reports", None).await;
    let reports = reports.as_array().unwrap();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0]["resources"][0]["intervals"][0]["id"], 0);

    // And the state a restart would need is exactly what was sent.
    let state = runtime.exported_state();
    assert_eq!(state.reported.len(), 2);
    assert!(state.ven_id.is_some());

    // A fresh runtime that restores it does not re-file.
    let restarted = fixture.metered(
        VenConfig::new("meter-1".parse().unwrap()),
        MeterHandle(meter.clone()),
        "2026-02-11T09:00:00Z",
    );
    restarted.restore(state);
    restarted.sync().await.unwrap();
    assert!(
        restarted.submit_due_reports().await.unwrap().is_empty(),
        "a restart re-filed reports the previous process had already sent"
    );
}

#[tokio::test]
async fn a_looping_tariff_keeps_owing_reports_after_the_first_day() {
    // The canonical §7.3 tariff: twenty-four hourly intervals, `duration: P9999Y`, one report per
    // repetition, for ever. Expanding one pass produces day one's reports and then nothing, and
    // the failure is silent at both ends — the VEN believes it is up to date and the VTN has no
    // way to say a report never arrived.
    let fixture = Fixture::start().await;
    let program = fixture.program("tariff").await;
    let intervals: Vec<Json> = (0..24)
        .map(|i| json!({ "id": i, "payloads": [{ "type": "PRICE", "values": [0.21] }] }))
        .collect();
    fixture
        .bl(
            "POST",
            "/events",
            Some(json!({
                "programID": program,
                "eventName": "day-ahead",
                "duration": "P9999Y",
                "intervalPeriod": { "start": "2026-02-01T00:00:00Z", "duration": "PT1H" },
                "intervals": intervals,
                "reportDescriptors": [{ "payloadType": "USAGE", "repeat": -1 }]
            })),
        )
        .await;

    let meter = Arc::new(CountingMeter::default());
    // The eleventh day of a tariff that started on the first.
    let runtime = fixture.metered(
        VenConfig::new("meter-loop".parse().unwrap()),
        MeterHandle(meter.clone()),
        "2026-02-11T06:00:00Z",
    );
    runtime.register().await.unwrap();
    runtime.sync().await.unwrap();

    // One per day within the 24-hour catch-up window, not zero and not ten days' worth.
    let due = runtime.due_reports("2026-02-11T06:00:00Z".parse().unwrap());
    assert_eq!(
        due.len(),
        1,
        "a repeating tariff stopped owing reports: {due:?}"
    );
    // Report 9 covers 10 February and fell due at midnight on the eleventh. The number counts from
    // the event's first interval, which is what makes the memory of what was filed durable.
    assert_eq!(due[0].sequence, 9);
    assert_eq!(
        due[0].covers_from,
        "2026-02-10T00:00:00Z".parse::<Timestamp>().unwrap()
    );
    assert_eq!(due[0].interval_ids.len(), 24);

    assert_eq!(runtime.submit_due_reports().await.unwrap().len(), 1);
    assert!(
        runtime.submit_due_reports().await.unwrap().is_empty(),
        "the same day's report was filed twice"
    );

    // The next day owes the next one, with the next sequence number.
    let tomorrow = fixture.metered(
        VenConfig::new("meter-loop".parse().unwrap()),
        MeterHandle(meter.clone()),
        "2026-02-12T06:00:00Z",
    );
    tomorrow.restore(runtime.exported_state());
    tomorrow.sync().await.unwrap();
    let due = tomorrow.due_reports("2026-02-12T06:00:00Z".parse().unwrap());
    assert_eq!(due.len(), 1, "the tariff stopped again on day two: {due:?}");
    assert_eq!(due[0].sequence, 10);
}

#[tokio::test]
async fn a_capability_forecast_is_owed_against_intervals_the_event_never_lists() {
    // User Guide §8.7, verbatim: an `intervalPeriod` and no `intervals` at all. The interval
    // structure is implied by the period and the count comes from the descriptor, so an event of
    // eleven lines asks for a rolling forty-eight-hour forecast every hour, for ever.
    let fixture = Fixture::start().await;
    let program = fixture.program("capability").await;
    fixture
        .bl(
            "POST",
            "/events",
            Some(json!({
                "programID": program,
                "eventName": "capability_report_Event",
                "intervalPeriod": { "start": "2026-02-11T00:00:00Z", "duration": "PT1H" },
                "reportDescriptors": [{
                    "payloadType": "LOAD_SHED_DELTA_AVAILABLE",
                    "startInterval": 0,
                    "numIntervals": 48,
                    "historical": false,
                    "frequency": 1,
                    "repeat": -1
                }]
            })),
        )
        .await;

    let meter = Arc::new(CountingMeter::default());
    let runtime = fixture.metered(
        VenConfig::new("meter-forecast".parse().unwrap()),
        MeterHandle(meter.clone()),
        "2026-02-11T06:00:00Z",
    );
    runtime.register().await.unwrap();
    runtime.sync().await.unwrap();

    // Hourly since the event began at midnight: seven forecasts have fallen due by 06:00.
    let due = runtime.due_reports("2026-02-11T06:00:00Z".parse().unwrap());
    assert_eq!(
        due.len(),
        7,
        "the implied interval structure was not resolved: {due:?}"
    );
    // Each is a forecast, so it is due when the window it covers *opens*, and it looks forward.
    assert_eq!(
        due[0].covers_from,
        "2026-02-11T00:00:00Z".parse::<Timestamp>().unwrap()
    );
    assert_eq!(
        due[0].covers_to,
        Some("2026-02-13T00:00:00Z".parse::<Timestamp>().unwrap())
    );
    // The specification leaves the ids of implied intervals to the VEN.
    assert!(due[0].interval_ids.is_empty());

    assert_eq!(runtime.submit_due_reports().await.unwrap().len(), 7);
    assert!(runtime.submit_due_reports().await.unwrap().is_empty());

    // And the event places nothing on the timeline: it carries no payloads, so there is no
    // instruction to follow and it must not mask one.
    assert!(
        runtime
            .active_at("2026-02-11T06:00:00Z".parse().unwrap())
            .is_none(),
        "a report-only event claimed the schedule"
    );
}

/// `Meter` is implemented for the handle so the test can keep its own reference to the counter.
struct MeterHandle(Arc<CountingMeter>);

#[async_trait]
impl Meter for MeterHandle {
    async fn read(&self, due: &DueReport) -> Result<Vec<ReportResource>, VenError> {
        self.0.read(due).await
    }
}

#[tokio::test]
async fn a_meter_that_cannot_answer_leaves_the_report_due() {
    // A meter that is briefly unavailable must not cost the window. Skipping without marking sent
    // is the difference between a late report and a missing one.
    struct Silent;
    #[async_trait]
    impl Meter for Silent {
        async fn read(&self, _due: &DueReport) -> Result<Vec<ReportResource>, VenError> {
            Ok(Vec::new())
        }
    }

    let fixture = Fixture::start().await;
    let program = fixture.program("compliance").await;
    let runtime = fixture.metered(
        VenConfig::new("meter-2".parse().unwrap()),
        Silent,
        "2026-02-11T09:00:00Z",
    );
    runtime.register().await.unwrap();
    fixture.event(&program, "compliance", &[]).await;
    runtime.sync().await.unwrap();

    assert!(runtime.submit_due_reports().await.unwrap().is_empty());
    assert_eq!(
        runtime
            .due_reports("2026-02-11T09:00:00Z".parse().unwrap())
            .len(),
        2,
        "a report the meter could not answer must stay due"
    );
    assert!(runtime.exported_state().reported.is_empty());
}

#[tokio::test]
async fn the_clock_is_checked_against_the_vtns_and_a_wrong_one_is_fatal() {
    // Every interval in OpenADR is an absolute instant, so a VEN with a wrong clock curtails at the
    // wrong time and reports compliance it did not achieve. Refusing is the only safe answer.
    let fixture = Fixture::start().await;

    // `Date` comes from the host clock, so a runtime reading the same host clock agrees with it.
    let honest = VenRuntime::new(fixture.client(), VenConfig::new("in-time".parse().unwrap()));
    assert!(honest.check_clock().await.unwrap().abs() < 5);

    let mut config = VenConfig::new("adrift".parse().unwrap());
    config.max_clock_skew = std::time::Duration::from_secs(30);
    let adrift = VenRuntime::new(fixture.client(), config).with_clock(Arc::new(FixedClock::new(
        "2020-01-01T00:00:00Z".parse().unwrap(),
    )));

    match adrift.check_clock().await {
        Err(VenError::ClockSkew { tolerance, .. }) => assert_eq!(tolerance, 30),
        other => panic!("a clock six years out was accepted: {other:?}"),
    }
    // And registration refuses rather than acting on it.
    assert!(matches!(
        adrift.register().await,
        Err(VenError::ClockSkew { .. })
    ));
}

#[tokio::test]
async fn a_program_filter_narrows_what_the_ven_follows() {
    let fixture = Fixture::start().await;
    let followed = fixture.program("followed").await;
    let ignored = fixture.program("ignored").await;

    let ven = fixture.ven(
        VenConfig::new("selective".parse().unwrap())
            .with_programs([ProgramName::new("followed").unwrap()]),
    );
    ven.register().await.unwrap();
    ven.sync_programs().await.unwrap();

    let wanted = fixture.event(&followed, "wanted", &[]).await;
    fixture.event(&ignored, "unwanted", &[]).await;

    let outcome = ven.sync().await.unwrap();
    assert_eq!(outcome.added, vec![wanted], "{outcome:?}");
}

#[tokio::test]
async fn a_program_the_ven_follows_is_readable_by_name() {
    let fixture = Fixture::start().await;
    fixture.program("tou").await;
    fixture.program("critical-peak").await;

    let ven = fixture.ven(
        VenConfig::new("reader".parse().unwrap()).with_programs([ProgramName::new("tou").unwrap()]),
    );
    let programs = ven.sync_programs().await.unwrap();
    assert_eq!(programs.len(), 1);
    assert_eq!(
        programs[0].content.program_name,
        ProgramRequest::new(ProgramName::new("tou").unwrap()).program_name
    );
}

#[tokio::test]
async fn a_schedule_that_never_changes_is_still_in_force_two_days_later() {
    // The flagship case, and the one nothing caught: a tariff that loops for ever. Its events never
    // change, so every poll after the first is a `304`, so nothing ever asked the runtime to look
    // at its timelines again — and a timeline reaches a fixed distance ahead of the moment it was
    // built. Two days on, `active_at` returned `None` and the VEN quietly stopped following a
    // signal the VTN was still publishing. Nothing errors; nothing logs; the load just drifts.
    let fixture = Fixture::start().await;
    let program = fixture.program("perpetual-tariff").await;
    let intervals: Vec<Json> = (0..24)
        .map(|i| json!({ "id": i, "payloads": [{ "type": "PRICE", "values": [0.21] }] }))
        .collect();
    fixture
        .bl(
            "POST",
            "/events",
            Some(json!({
                "programID": program,
                "eventName": "day-ahead",
                "duration": "P9999Y",
                "intervalPeriod": { "start": "2026-02-01T00:00:00Z", "duration": "PT1H" },
                "intervals": intervals,
            })),
        )
        .await;

    let clock = MovableClock::at("2026-02-11T06:00:00Z");
    let runtime = fixture.moving(VenConfig::new("drifter".parse().unwrap()), clock.clone());
    runtime.register().await.unwrap();
    runtime.sync().await.unwrap();
    assert!(
        runtime.active_at(clock.now()).is_some(),
        "the tariff was not in force on the day it was read"
    );

    // Three days of quiet polling. Every one of them is a 304.
    for _ in 0..3 {
        clock.advance(jiff::Span::new().hours(24));
        let outcome = runtime.sync().await.unwrap();
        assert!(
            outcome.is_quiet(),
            "nothing changed, so nothing should have"
        );
        assert!(
            runtime.active_at(clock.now()).is_some(),
            "the tariff stopped being in force at {} without anything changing",
            clock.now()
        );
    }
    assert!(
        runtime.next_change(clock.now()).is_some(),
        "and there is still a transition to sleep until"
    );
}

#[tokio::test]
async fn a_do_it_now_event_does_not_slide_forward_on_every_rebuild() {
    // `0001-01-01` means "now from the reader's point of view" [UG §7.3]. The reader's point of
    // view is when it *read* the event. Re-resolving the sentinel on each rebuild moved the start
    // to the rebuild instant, so a one-hour "do it now" curtailment ended one hour after the last
    // rebuild rather than one hour after it arrived — which, with a rebuild every cycle, is never.
    let fixture = Fixture::start().await;
    let program = fixture.program("dispatch").await;
    fixture
        .bl(
            "POST",
            "/events",
            Some(json!({
                "programID": program,
                "eventName": "curtail-now",
                "intervalPeriod": { "start": "0001-01-01T00:00:00Z", "duration": "PT1H" },
                "intervals": [
                    { "id": 0, "payloads": [{ "type": "SIMPLE", "values": [1] }] }
                ],
            })),
        )
        .await;

    let clock = MovableClock::at("2026-02-11T06:00:00Z");
    let runtime = fixture.moving(VenConfig::new("prompt".parse().unwrap()), clock.clone());
    runtime.register().await.unwrap();
    runtime.sync().await.unwrap();

    let segment = runtime
        .active_at(clock.now())
        .expect("in force immediately");
    let ends = segment.end.expect("a one-hour event ends");
    assert_eq!(ends, "2026-02-11T07:00:00Z".parse::<Timestamp>().unwrap());

    // Half an hour later something *else* changes — a second event, in a second programme, which
    // has nothing to do with this one. That is what rebuilds the timelines, and it is why the slide
    // never needed a long-running VEN to bite: one unrelated write is enough.
    clock.advance(jiff::Span::new().minutes(30));
    let other = fixture.program("unrelated").await;
    fixture
        .bl(
            "POST",
            "/events",
            Some(json!({
                "programID": other,
                "eventName": "somebody-elses-business",
                "intervalPeriod": { "start": "2026-02-12T00:00:00Z", "duration": "PT1H" },
                "intervals": [
                    { "id": 0, "payloads": [{ "type": "SIMPLE", "values": [0] }] }
                ],
            })),
        )
        .await;
    let outcome = runtime.sync().await.unwrap();
    assert_eq!(outcome.added.len(), 1, "the unrelated event should arrive");

    let segment = runtime.active_at(clock.now()).expect("still in force");
    assert_eq!(
        segment.end.unwrap(),
        ends,
        "the event's end moved when an unrelated event was written"
    );

    // And an hour and a bit after it arrived, it is over.
    clock.advance(jiff::Span::new().minutes(45));
    runtime.sync().await.unwrap();
    assert!(
        runtime.active_at(clock.now()).is_none(),
        "a one-hour 'do it now' event was still in force 75 minutes later"
    );
}

#[tokio::test]
async fn the_interval_helper_quotes_the_events_own_ids() {
    // `VenRuntime::intervals_for` is documented as the convenience most meters want, and until this
    // test it had no caller anywhere — including its own tests. The thing it exists to get right is
    // the part a hand-written meter gets wrong: a report interval quotes the *event's* interval id
    // so the VTN can correlate the reading with the price it answers [UG §7.5]. An id the VEN
    // invented correlates with nothing, and nothing anywhere says so.
    let fixture = Fixture::start().await;
    let program = fixture.program("helper").await;
    fixture.event(&program, "compliance", &[]).await;

    let runtime = fixture.metered(
        VenConfig::new("helper-ven".parse().unwrap()),
        MeterHandle(Arc::new(CountingMeter::default())),
        "2026-02-11T09:00:00Z",
    );
    runtime.register().await.unwrap();
    runtime.sync().await.unwrap();

    let due = runtime.due_reports("2026-02-11T09:00:00Z".parse().unwrap());
    assert_eq!(due.len(), 2, "{due:?}");

    let intervals = runtime.intervals_for(&due[1], |id| vec![Value::Integer(i64::from(id) * 10)]);
    let ids: Vec<i32> = intervals.iter().map(|i| i.id).collect();
    assert_eq!(
        ids, due[1].interval_ids,
        "the helper renumbered the intervals"
    );
    assert_eq!(
        intervals[0].payloads[0].value_type.as_str(),
        due[1].payload_type.as_str()
    );
    assert_eq!(intervals[0].payloads[0].values, vec![Value::Integer(10)]);
}
