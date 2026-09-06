//! What the notification fan-out actually costs.
//!
//! Everything else here asserts that the VTN is *correct*. This measures how it *scales*: the
//! fan-out is O(matching subscriptions) at write time and O(entitled VENs) at broker time.
//!
//! ```console
//! $ cargo test --all-features --test load -- --ignored --nocapture
//! ```
//!
//! `#[ignore]`d, so an ordinary `cargo test` does not spend a minute on it. Set
//! `OPENADR_TEST_POSTGRES` to include that backend, which is the deployment where the answer
//! matters. Every case writes through the **real router**, so what is timed is the whole write path
//! rather than the one function somebody suspected.
//!
//! The numbers are **shapes** — how the cost moves as the input grows, on one machine, under
//! whatever else it is doing. Ratios taken back-to-back are the finding; the absolute figures are
//! not a capacity plan.

#![cfg(all(feature = "vtn", feature = "sqlite"))]

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use openadr::{
    core::FixedClock,
    model::{ClientId, Timestamp},
    vtn::{
        DispatchConfig, Vtn, VtnConfig,
        auth::StaticTokenAuth,
        notify::{Notifiers, RecordingNotifier},
        store::{MemoryStorage, SharedStorage, SqliteStorage},
    },
};
use serde_json::{Value, json};
use tower::ServiceExt;

const BL: &str = "bl-secret";

fn now() -> Timestamp {
    "2026-02-11T06:00:00Z".parse().unwrap()
}

/// One measured series.
struct Timing {
    samples: Vec<Duration>,
}

impl Timing {
    fn new() -> Self {
        Self {
            samples: Vec::new(),
        }
    }

    fn record(&mut self, d: Duration) {
        self.samples.push(d);
    }

    /// The percentile, in milliseconds. p50 and p95 rather than a mean: a fan-out's cost is a
    /// distribution with a tail, and a mean hides exactly the tail an operator meets.
    fn ms(&mut self, percentile: f64) -> f64 {
        self.samples.sort();
        if self.samples.is_empty() {
            return 0.0;
        }
        let index = ((self.samples.len() as f64 - 1.0) * percentile).round() as usize;
        self.samples[index].as_secs_f64() * 1000.0
    }
}

struct Harness {
    router: Router,
    vtn: Vtn,
    notifier: Arc<RecordingNotifier>,
}

impl Harness {
    async fn new(storage: SharedStorage, mqtt: bool) -> Self {
        let notifier = RecordingNotifier::shared();
        let mut auth = StaticTokenAuth::new("http://vtn.test/auth/token")
            .with_business_logic(BL, ClientId::new("bl").unwrap());
        // Every VEN client the broker cases need an identity for.
        for n in 0..64 {
            auth = auth.with_ven(
                format!("ven-token-{n}"),
                ClientId::new(format!("client-{n}")).unwrap(),
            );
        }

        let mut builder = Vtn::builder()
            .storage(storage.clone())
            .authenticator(Arc::new(auth))
            .clock(Arc::new(FixedClock::new(now())))
            .notifier(Notifiers::new().with(notifier.clone()).shared())
            .config(VtnConfig {
                base_path: "/openadr3/3.1.0".into(),
                mqtt_topic_prefix: "openadr3".into(),
                ..Default::default()
            });
        if mqtt {
            builder = builder.mqtt(openadr::model::MqttNotifierBinding {
                uris: vec!["mqtts://broker.test:8883".into()],
                serialization: openadr::model::Serialization::Json,
                authentication: openadr::model::MqttAuthentication::Anonymous,
            });
        }
        let vtn = builder.build();
        Self {
            router: vtn.router(),
            notifier,
            vtn,
        }
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.send_as(BL, method, path, body).await
    }

    /// The same, as a named credential. Reports are written by VENs, never by business logic: the
    /// VTN refuses a `POST /reports` from a BL token, and a load harness has to respect that like
    /// anybody else — measuring a path nobody can take would measure nothing.
    async fn send_as(
        &self,
        token: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(format!("/openadr3/3.1.0{path}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&b).unwrap()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn program(&self) -> String {
        let (status, body) = self
            .send(
                Method::POST,
                "/programs",
                Some(json!({ "programName": format!("load-{}", uid()) })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_string()
    }

    /// `count` webhook subscriptions watching events, all of them matching every write.
    async fn subscriptions(&self, count: usize) {
        for n in 0..count {
            let (status, body) = self
                .send(
                    Method::POST,
                    "/subscriptions",
                    Some(json!({
                        "clientName": format!("sub-{}-{n}", uid()),
                        "objectOperations": [{
                            "objects": ["EVENT"],
                            "operations": ["CREATE"],
                            "callbackUrl": format!("https://subscriber-{n}.example.com/hook"),
                        }],
                    })),
                )
                .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
    }

    /// `count` VEN objects, each granted a target, so the broker fan-out has somebody to reach.
    async fn vens(&self, count: usize) {
        for n in 0..count {
            let (status, body) = self
                .send(
                    Method::POST,
                    "/vens",
                    Some(json!({
                        "objectType": "BL_VEN_REQUEST",
                        "clientID": format!("client-{}-{n}", uid()),
                        "venName": format!("ven-{}-{n}", uid()),
                        "targets": ["fleet"],
                    })),
                )
                .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
    }
}

/// One case at a time.
///
/// The four cases are separate tests, so the runner would otherwise measure them in parallel — and
/// four fan-outs competing for one machine's cores and one disk is a measurement of contention
/// rather than of the fan-out. It also let them share a SQLite file and, with `OPENADR_TEST_POSTGRES`
/// set, truncate one database underneath each other.
async fn measuring() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// A counter so names are unique across cases sharing one database.
fn uid() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

fn event_body(program: &str) -> Value {
    json!({
        "programID": program,
        "intervalPeriod": { "start": "2026-02-11T07:00:00Z", "duration": "PT1H" },
        "intervals": [{ "id": 0, "payloads": [{ "type": "PRICE", "values": [0.11] }] }],
    })
}

/// A backend a case can ask for a **clean** store from.
///
/// Per row rather than per run. Sharing one store across the rows of a case makes every row after
/// the first measure a VTN carrying the previous rows' objects — so "0 subscribers" means zero only
/// while it happens to run first, and the moment a row is added or reordered the labels stop
/// describing what was measured. Which is the one thing a measurement has to get right.
enum Backend {
    Memory,
    Sqlite(std::path::PathBuf),
    #[cfg(feature = "postgres")]
    Postgres(String),
}

impl Backend {
    async fn fresh(&self) -> SharedStorage {
        match self {
            Backend::Memory => MemoryStorage::shared(),
            Backend::Sqlite(dir) => {
                // Its own file, never a re-used one: the cases hold a store for as long as they
                // measure, and deleting a file another row is still reading is not a clean start.
                let path = dir.join(format!("load-{}.sqlite", uid()));
                SqliteStorage::shared(path.to_str().unwrap())
                    .await
                    .expect("a fresh SQLite file")
            }
            #[cfg(feature = "postgres")]
            Backend::Postgres(url) => {
                let store = openadr::vtn::store::PostgresStorage::open(url)
                    .await
                    .expect("the server answered a moment ago");
                store.truncate_all().await.expect("a clean database");
                Arc::new(store) as SharedStorage
            }
        }
    }
}

/// Every backend this run can reach.
async fn backends() -> Vec<(&'static str, Backend)> {
    let mut out: Vec<(&'static str, Backend)> = vec![("memory", Backend::Memory)];

    // Files rather than `:memory:`: an in-memory SQLite measures SQLite's planner and none of the
    // I/O a deployment has.
    let dir = std::env::temp_dir().join(format!("openadr-load-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_ok() {
        out.push(("sqlite", Backend::Sqlite(dir)));
    }

    #[cfg(feature = "postgres")]
    match std::env::var("OPENADR_TEST_POSTGRES") {
        Ok(url) => match openadr::vtn::store::PostgresStorage::open(&url).await {
            Ok(_) => out.push(("postgres", Backend::Postgres(url))),
            Err(e) => eprintln!("postgres unavailable ({e}); skipping that backend"),
        },
        // Said out loud rather than left to the absence of a row. A measurement is a claim about
        // the configuration it was taken in, and the published figures for the cloud deployment
        // come from this backend — a reader comparing their own run against them needs to know
        // which one they did not take.
        Err(_) => eprintln!(
            "note: OPENADR_TEST_POSTGRES is unset, so no PostgreSQL row appears below. The \
             published figures for the cloud deployment are that backend's; these are not \
             comparable with them."
        ),
    }
    out
}

/// **The headline number.** What one `POST /events` costs as the subscriber count grows.
///
/// The fan-out is computed on the write path — deliberately, because recipients must be decided
/// against the subscriptions that existed when the change happened, and a delivery computed after
/// the transaction cannot be inside it. This is the price of that decision.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement, not an assertion: run with --ignored --nocapture"]
async fn write_latency_against_subscriber_count() {
    let _measuring = measuring().await;
    println!("\n== POST /events, by matching subscriptions ==");
    println!(
        "{:<10} {:>12} {:>10} {:>10} {:>12}",
        "backend", "subscribers", "p50 ms", "p95 ms", "queued/write"
    );

    for (name, backend) in backends().await {
        for subscribers in [0usize, 100, 1_000] {
            let h = Harness::new(backend.fresh().await, false).await;
            let program = h.program().await;
            h.subscriptions(subscribers).await;

            let mut timing = Timing::new();
            for _ in 0..20 {
                let started = Instant::now();
                let (status, body) = h
                    .send(Method::POST, "/events", Some(event_body(&program)))
                    .await;
                timing.record(started.elapsed());
                assert_eq!(status, StatusCode::CREATED, "{body}");
            }

            let queued = h.vtn.dispatcher().stats().await.pending;
            println!(
                "{name:<10} {subscribers:>12} {:>10.2} {:>10.2} {:>12}",
                timing.ms(0.50),
                timing.ms(0.95),
                queued / 20,
            );
            // Drain, so the next case starts from an empty queue.
            h.vtn.dispatcher().drain().await;
            let _ = h.notifier.delivered();
        }
    }
}

/// The broker fan-out, which is O(VENs) by construction: one private copy per entitled VEN.
///
/// An *untargeted* event on a VTN with ten thousand VENs is ten thousand rows, and no cleverness
/// removes that — object privacy is what makes the per-VEN copy necessary. The number worth knowing
/// is what each row costs.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement, not an assertion: run with --ignored --nocapture"]
async fn broker_fanout_against_ven_count() {
    let _measuring = measuring().await;
    println!("\n== POST /events with a broker, by VEN count ==");
    println!(
        "{:<10} {:>8} {:>10} {:>10} {:>12}",
        "backend", "vens", "p50 ms", "p95 ms", "queued/write"
    );

    for (name, backend) in backends().await {
        for vens in [0usize, 100, 1_000] {
            let h = Harness::new(backend.fresh().await, true).await;
            let program = h.program().await;
            h.vens(vens).await;

            let mut timing = Timing::new();
            for _ in 0..10 {
                let started = Instant::now();
                let (status, body) = h
                    .send(Method::POST, "/events", Some(event_body(&program)))
                    .await;
                timing.record(started.elapsed());
                assert_eq!(status, StatusCode::CREATED, "{body}");
            }

            let queued = h.vtn.dispatcher().stats().await.pending;
            println!(
                "{name:<10} {vens:>8} {:>10.2} {:>10.2} {:>12}",
                timing.ms(0.50),
                timing.ms(0.95),
                queued / 10,
            );
            h.vtn.dispatcher().drain().await;
            let _ = h.notifier.delivered();
        }
    }
}

/// How fast the queue drains, and whether concurrency helps.
///
/// `DispatchConfig::concurrency` exists because deliveries are independent — different subscribers,
/// different sockets — and doing them one after another makes a batch cost the *sum* of its
/// timeouts. Against a notifier that returns immediately this measures the floor: the bookkeeping,
/// not the network.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement, not an assertion: run with --ignored --nocapture"]
async fn drain_rate_by_concurrency() {
    let _measuring = measuring().await;
    println!("\n== outbox drain, 2 000 entries ==");
    println!(
        "{:<10} {:>12} {:>12} {:>14}",
        "backend", "concurrency", "seconds", "entries/sec"
    );

    for (name, backend) in backends().await {
        for concurrency in [1usize, 8, 64] {
            // Held, because the dispatcher under test has to drain the store the harness filled.
            let storage = backend.fresh().await;
            let h = Harness::new(storage.clone(), false).await;
            let program = h.program().await;
            h.subscriptions(100).await;
            for _ in 0..20 {
                h.send(Method::POST, "/events", Some(event_body(&program)))
                    .await;
            }

            let dispatcher = openadr::vtn::Dispatcher::new(
                storage.clone(),
                h.notifier.clone(),
                Arc::new(FixedClock::new(now())),
                DispatchConfig {
                    concurrency,
                    batch: 128,
                    ..Default::default()
                },
            );
            let started = Instant::now();
            let drained = dispatcher.drain().await;
            let elapsed = started.elapsed().as_secs_f64();
            println!(
                "{name:<10} {concurrency:>12} {elapsed:>12.3} {:>14.0}",
                drained as f64 / elapsed.max(f64::EPSILON)
            );
            let _ = h.notifier.delivered();
        }
    }
}

/// Report ingest: the write path a real deployment saturates first, and the one with no fan-out.
///
/// Every VEN in a fleet files on a schedule, so this is the highest-rate write in the system. It is
/// also the one the per-request grant lookup was removed from (D-053), and this is where that shows
/// up or does not.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement, not an assertion: run with --ignored --nocapture"]
async fn report_ingest_rate() {
    let _measuring = measuring().await;
    println!("\n== POST /reports, 200 reports ==");
    println!(
        "{:<10} {:>12} {:>6} {:>7} {:>10} {:>10} {:>14}",
        "backend", "subscribers", "vens", "broker", "p50 ms", "p95 ms", "reports/sec"
    );

    for (name, backend) in backends().await {
        // Three shapes, because report ingest is the highest-rate write in the system and has no
        // fan-out at all — so its cost must not depend on anything it does not reach.
        //
        // * unrelated *subscriptions*: the fan-out query is narrowed by object type, so an event
        //   subscription costs a report write nothing (D-116);
        // * unrelated *VENs*, with a broker configured: the snapshot is narrowed by owner, so the
        //   fleet costs a report write nothing either. That row is the one this harness could not
        //   see for as long as it measured report ingest only on a VTN with no broker — where the
        //   grant sweep it would have caught is skipped entirely (D-124).
        // The last two rows are the comparison that matters: the same broker, with and without a
        // fleet behind it. They must read alike.
        for (subscribers, vens, mqtt) in [
            (0usize, 0usize, false),
            (1_000, 0, false),
            (0, 0, true),
            (0, 1_000, true),
        ] {
            let h = Harness::new(backend.fresh().await, mqtt).await;
            let program = h.program().await;
            h.subscriptions(subscribers).await;
            h.vens(vens).await;
            if mqtt {
                // The report's own owner, so the write publishes to a real VEN topic rather than
                // measuring the lookup that finds nothing.
                let (status, body) = h
                    .send(
                        Method::POST,
                        "/vens",
                        Some(json!({
                            "objectType": "BL_VEN_REQUEST",
                            "clientID": "client-0",
                            "venName": format!("owner-{}", uid()),
                        })),
                    )
                    .await;
                assert_eq!(status, StatusCode::CREATED, "{body}");
            }
            let (status, event) = h
                .send(Method::POST, "/events", Some(event_body(&program)))
                .await;
            assert_eq!(status, StatusCode::CREATED, "{event}");
            let event_id = event["id"].as_str().unwrap().to_string();
            h.vtn.dispatcher().drain().await;

            let mut timing = Timing::new();
            let started = Instant::now();
            for n in 0..200 {
                let body = json!({
                    "programID": program,
                    "eventID": event_id,
                    "clientName": format!("ven-{n}"),
                    "resources": [{
                        "resourceName": format!("meter-{n}"),
                        "intervals": [{
                            "id": 0,
                            "payloads": [{ "type": "USAGE", "values": [1.5] }],
                        }],
                    }],
                });
                let at = Instant::now();
                let (status, body) = h
                    .send_as("ven-token-0", Method::POST, "/reports", Some(body))
                    .await;
                timing.record(at.elapsed());
                assert_eq!(status, StatusCode::CREATED, "{body}");
            }
            let elapsed = started.elapsed().as_secs_f64();
            println!(
                "{name:<10} {subscribers:>12} {vens:>6} {:>7} {:>10.2} {:>10.2} {:>14.0}",
                if mqtt { "yes" } else { "no" },
                timing.ms(0.50),
                timing.ms(0.95),
                200.0 / elapsed.max(f64::EPSILON)
            );
            h.vtn.dispatcher().drain().await;
            let _ = h.notifier.delivered();
        }
    }
}
