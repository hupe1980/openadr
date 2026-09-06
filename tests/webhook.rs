//! The webhook transport, end to end over real sockets.
//!
//! `GET /notifiers` reports `WEBHOOK: true` unconditionally. These tests are what make that claim
//! checkable: a real VTN, a real subscriber listening on a real port, and an assertion that the
//! notification arrived with the right body, the right token and the right signature.
//!
//! They also cover the wiring, which unit tests over the delivery computation cannot: that a write
//! to the API queues a delivery and that draining the outbox actually reaches the transport.
//!
//! Delivery is asynchronous, so every test drains explicitly rather than sleeping: `drain()` runs
//! until nothing is due, which makes retry counts exact instead of timing-dependent.

#![cfg(all(feature = "vtn", feature = "webhook"))]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Router,
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use openadr::{
    core::FixedClock,
    model::{ClientId, Timestamp, notification::AnyObject},
    vtn::{
        DispatchConfig, Vtn, VtnConfig,
        auth::StaticTokenAuth,
        notify::{Notifier, WebhookConfig, WebhookNotifier},
        store::{MemoryStorage, RetryPolicy},
    },
};
use serde_json::{Value, json};

const BL: &str = "bl-secret";
const VEN: &str = "ven-secret";
const SIGNING_KEY: &str = "a-shared-secret";

fn now() -> Timestamp {
    "2026-02-11T06:00:00Z".parse().unwrap()
}

/// What a subscriber saw.
#[derive(Debug, Clone)]
struct Received {
    body: Value,
    authorization: Option<String>,
    signature: Option<String>,
    timestamp: Option<String>,
    attempt: Option<String>,
}

#[derive(Clone, Default)]
struct Receiver {
    received: Arc<Mutex<Vec<Received>>>,
    /// Status to answer with, so a test can make the endpoint misbehave.
    status: Arc<Mutex<StatusCode>>,
    /// Whether to answer the echo challenge correctly.
    echo_correctly: Arc<Mutex<bool>>,
}

impl Receiver {
    fn new() -> Self {
        Self {
            received: Arc::new(Mutex::new(Vec::new())),
            status: Arc::new(Mutex::new(StatusCode::OK)),
            echo_correctly: Arc::new(Mutex::new(true)),
        }
    }

    fn deliveries(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    fn answer_with(&self, status: StatusCode) {
        *self.status.lock().unwrap() = status;
    }

    /// Start listening; returns the callback URL.
    async fn start(&self) -> String {
        let app = Router::new()
            .route("/hook", post(receive).get(echo))
            .with_state(self.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}/hook")
    }
}

async fn receive(State(state): State<Receiver>, headers: HeaderMap, body: String) -> StatusCode {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    state.received.lock().unwrap().push(Received {
        body: serde_json::from_str(&body).unwrap_or(Value::Null),
        authorization: header("authorization"),
        signature: header("x-openadr-signature"),
        timestamp: header("x-openadr-timestamp"),
        attempt: header("x-openadr-attempt"),
    });
    *state.status.lock().unwrap()
}

/// The echo challenge, answered the way a subscriber is meant to answer it.
///
/// Through `openadr::webhook::echo_challenge` rather than by hand, so that the crate's own
/// receiver-side helper is the thing this end-to-end test exercises. It is the only half of the
/// webhook contract a subscriber cannot skip — a subscription is not created until it passes — and
/// a helper for it that nothing calls is a helper nobody has checked against a real challenge.
async fn echo(State(state): State<Receiver>, RawQuery(query): RawQuery) -> (StatusCode, String) {
    let challenge = openadr::webhook::echo_challenge(query.as_deref().unwrap_or_default());
    match (challenge, *state.echo_correctly.lock().unwrap()) {
        (Some(challenge), true) => (StatusCode::OK, challenge),
        (Some(_), false) => (StatusCode::OK, "wrong".into()),
        (None, _) => (StatusCode::NOT_FOUND, String::new()),
    }
}

struct Harness {
    vtn: Vtn,
    router: Router,
}

impl Harness {
    fn new() -> Self {
        Self::with_breaker(openadr::vtn::store::BreakerPolicy::disabled())
    }

    /// The same VTN, with the subscriber circuit breaker configured.
    fn with_breaker(breaker: openadr::vtn::store::BreakerPolicy) -> Self {
        let notifier = WebhookNotifier::shared(WebhookConfig {
            signing_key: Some(SIGNING_KEY.into()),
            // The receiver is on loopback, which the transport refuses by default and rightly so.
            policy: openadr::vtn::notify::CallbackPolicy::permissive(),
            ..Default::default()
        })
        .unwrap();

        let auth = StaticTokenAuth::new("http://vtn.test/auth/token")
            .with_business_logic(BL, ClientId::new("bl").unwrap())
            .with_ven(VEN, ClientId::new("client-a").unwrap());

        let vtn = Vtn::builder()
            .storage(MemoryStorage::shared())
            .authenticator(Arc::new(auth))
            .clock(Arc::new(FixedClock::new(now())))
            .notifier(notifier.clone())
            .config(VtnConfig {
                base_path: "/openadr3/3.1.0".into(),
                // The API must accept the same URLs the transport will deliver to.
                callback_policy: openadr::vtn::notify::CallbackPolicy::permissive(),
                dispatch: DispatchConfig {
                    retry: RetryPolicy {
                        max_attempts: 3,
                        // Zero, because the clock is fixed: a retry must be due the instant the
                        // previous attempt failed, or `drain` would return with work outstanding.
                        base_delay: Duration::ZERO,
                        max_delay: Duration::ZERO,
                    },
                    breaker,
                    ..Default::default()
                },
                ..Default::default()
            })
            .build();

        Self {
            router: vtn.router(),
            vtn,
        }
    }

    /// Deliver everything queued, retries included, and return how many attempts were made.
    async fn drain(&self) -> usize {
        self.vtn.dispatcher().drain().await
    }

    /// A request to a root-mounted operational endpoint.
    async fn admin_call(&self, method: &str, path: &str) -> (StatusCode, Value) {
        use axum::body::Body;
        use axum::http::{Request, header};
        use tower::ServiceExt;

        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// A `GET` on a root-mounted operational endpoint.
    async fn admin(&self, path: &str) -> (StatusCode, Value) {
        use axum::body::Body;
        use axum::http::{Request, header};
        use tower::ServiceExt;

        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// `GET /health`, which reports the notification backlog.
    async fn health(&self) -> (StatusCode, Value) {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        use axum::body::Body;
        use axum::http::{Request, header};
        use tower::ServiceExt;

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

    /// A programme, a VEN in `group1`, and a subscription pointing at `callback`.
    async fn seed(&self, callback: &str) -> String {
        let (status, program) = self
            .call("POST", "/programs", BL, Some(json!({"programName": "gac"})))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{program}");

        let (status, body) = self
            .call(
                "POST",
                "/vens",
                BL,
                Some(json!({
                    "objectType": "BL_VEN_REQUEST",
                    "clientID": "client-a",
                    "venName": "ven-a",
                    "targets": ["group1"],
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        let (status, body) = self
            .call(
                "POST",
                "/subscriptions",
                VEN,
                Some(json!({
                    "clientName": "ven-a",
                    "objectOperations": [{
                        "objects": ["EVENT"],
                        "operations": ["CREATE", "UPDATE", "DELETE"],
                        "callbackUrl": callback,
                        "bearerToken": "receiver-token",
                    }]
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        program["id"].as_str().unwrap().to_string()
    }

    async fn create_event(&self, program_id: &str, name: &str, targets: Value) -> StatusCode {
        self.call(
            "POST",
            "/events",
            BL,
            Some(json!({
                "programID": program_id,
                "eventName": name,
                "targets": targets,
                "intervalPeriod": {"start": "2026-02-11T12:00:00Z", "duration": "PT15M"},
                "intervals": [{"id": 0, "payloads": [{"type": "IMPORT_CAPACITY_LIMIT", "values": [60]}]}]
            })),
        )
        .await
        .0
    }
}

// ---------------------------------------------------------------------------

#[tokio::test]
// `[Def §Subscriptions]`: "A VTN SHALL make a request to the callback URL when the conditions are
// met." Over a real socket, through the outbox, because a fan-out that computes the right recipients
// and posts to none of them looks identical from inside.
async fn a_notification_reaches_the_subscriber() {
    let receiver = Receiver::new();
    let callback = receiver.start().await;
    let h = Harness::new();
    let program = h.seed(&callback).await;

    assert_eq!(
        h.create_event(&program, "curtailment", json!(["group1"]))
            .await,
        StatusCode::CREATED
    );
    h.drain().await;

    let delivered = receiver.deliveries();
    assert_eq!(delivered.len(), 1, "expected exactly one delivery");

    let received = &delivered[0];
    assert_eq!(received.body["objectType"], "EVENT");
    assert_eq!(received.body["operation"], "CREATE");
    assert_eq!(received.body["object"]["eventName"], "curtailment");
    // Target hiding applies to notifications too.
    assert_eq!(received.body["targets"], json!(["group1"]));

    // The subscriber's own token, so it can authenticate the caller.
    assert_eq!(
        received.authorization.as_deref(),
        Some("Bearer receiver-token")
    );
    assert_eq!(received.attempt.as_deref(), Some("1"));
}

#[tokio::test]
async fn the_payload_is_signed_and_a_receiver_can_verify_it() {
    // The receiver's half, run the way a receiver would run it: the shipped verifier over the
    // bytes and headers that actually crossed the socket. Re-deriving the HMAC in the test would
    // be a second implementation of the scheme, agreeing with itself.
    use openadr::webhook::{self, Signature};

    let receiver = Receiver::new();
    let callback = receiver.start().await;
    let h = Harness::new();
    let program = h.seed(&callback).await;
    h.create_event(&program, "signed", json!(["group1"])).await;
    h.drain().await;

    let received = &receiver.deliveries()[0];
    let signature = received.signature.as_deref().expect("a signature header");
    let stamp = received.timestamp.as_deref().expect("a timestamp header");
    let sent_at = webhook::parse_timestamp(stamp).expect("a Unix-seconds timestamp");
    let body = serde_json::to_vec(&received.body).unwrap();

    Signature::parse(signature)
        .expect("a well-formed signature header")
        .verify(
            SIGNING_KEY.as_bytes(),
            &body,
            sent_at,
            openadr::model::Timestamp::now(),
            webhook::DEFAULT_TOLERANCE,
        )
        .expect("the signature the VTN sent must verify against the body it sent");

    // And a replay of the same bytes an hour later does not.
    let hour_later = sent_at
        .checked_add(jiff::Span::new().hours(1))
        .expect("in range");
    assert!(matches!(
        Signature::parse(signature).unwrap().verify(
            SIGNING_KEY.as_bytes(),
            &body,
            sent_at,
            hour_later,
            webhook::DEFAULT_TOLERANCE,
        ),
        Err(webhook::SignatureError::Stale { .. })
    ));
}

#[tokio::test]
// `[Def §program and event objects - targeting]`, the notification half: "When evaluating whether
// to send a notification of a change of state of an object with targets, the VTN SHALL use the
// clientID of …" — which is why a subscription stores what its owner *was* and not only who it was
// (D-095).
async fn a_subscriber_outside_the_target_group_is_not_told() {
    let receiver = Receiver::new();
    let callback = receiver.start().await;
    let h = Harness::new();
    let program = h.seed(&callback).await;

    // ven-a is in group1 only.
    h.create_event(&program, "someone-elses", json!(["group2"]))
        .await;
    h.drain().await;

    assert!(
        receiver.deliveries().is_empty(),
        "a VEN must not be told about another group's dispatch"
    );
}

#[tokio::test]
async fn every_subscribed_operation_is_delivered() {
    let receiver = Receiver::new();
    let callback = receiver.start().await;
    let h = Harness::new();
    let program = h.seed(&callback).await;

    h.create_event(&program, "lifecycle", json!(["group1"]))
        .await;
    h.drain().await;
    let event_id = {
        let (_, events) = h.call("GET", "/events", BL, None).await;
        events[0]["id"].as_str().unwrap().to_string()
    };

    h.call(
        "PUT",
        &format!("/events/{event_id}"),
        BL,
        Some(json!({
            "programID": program,
            "eventName": "renamed",
            "targets": ["group1"],
            "intervalPeriod": {"start": "2026-02-11T12:00:00Z", "duration": "PT15M"},
            "intervals": [{"id": 0, "payloads": [{"type": "IMPORT_CAPACITY_LIMIT", "values": [40]}]}]
        })),
    )
    .await;
    h.call("DELETE", &format!("/events/{event_id}"), BL, None)
        .await;
    h.drain().await;

    let operations: Vec<String> = receiver
        .deliveries()
        .iter()
        .map(|d| d.body["operation"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(operations, vec!["CREATE", "UPDATE", "DELETE"]);
}

#[tokio::test]
async fn a_server_error_is_retried_and_a_client_error_is_not() {
    let receiver = Receiver::new();
    let callback = receiver.start().await;
    let h = Harness::new();
    let program = h.seed(&callback).await;

    // 500: the receiver may recover, so the transport tries again.
    receiver.answer_with(StatusCode::INTERNAL_SERVER_ERROR);
    h.create_event(&program, "flaky", json!(["group1"])).await;
    h.drain().await;
    let attempts = receiver.deliveries().len();
    assert_eq!(attempts, 3, "a 5xx should exhaust max_attempts");

    // 400: the receiver is saying "not this, ever". Retrying cannot help.
    let receiver2 = Receiver::new();
    let callback2 = receiver2.start().await;
    let h2 = Harness::new();
    let program2 = h2.seed(&callback2).await;
    receiver2.answer_with(StatusCode::BAD_REQUEST);
    h2.create_event(&program2, "refused", json!(["group1"]))
        .await;
    h2.drain().await;
    assert_eq!(receiver2.deliveries().len(), 1, "a 4xx must not be retried");
    // And it is abandoned rather than left pending, so it shows up in the dead count.
    let stats = h2.vtn.dispatcher().stats().await;
    assert_eq!((stats.pending, stats.dead), (0, 1));
}

#[tokio::test]
async fn a_dead_endpoint_stops_costing_attempts_and_becomes_visible() {
    // With the breaker disabled, which is what the numbers below measure: every notification costs
    // its full `max_attempts` however many have already been abandoned for the same subscriber.
    // That is bounded per *notification* and unbounded per *subscriber*, which is what
    // `a_dead_subscriber_is_cut_off_and_can_be_restored` exists to fix.
    let receiver = Receiver::new();
    let callback = receiver.start().await;
    let h = Harness::new();
    let program = h.seed(&callback).await;
    receiver.answer_with(StatusCode::INTERNAL_SERVER_ERROR);

    for i in 0..4 {
        h.create_event(&program, &format!("attempt-{i}"), json!(["group1"]))
            .await;
        h.drain().await;
    }

    // Three attempts each, and then no more: bounded traffic per notification rather than a
    // subscription retried for ever.
    assert_eq!(receiver.deliveries().len(), 4 * 3);
    assert_eq!(
        h.drain().await,
        0,
        "an abandoned entry must not be reclaimed"
    );

    // Abandoned, not deleted. This is the number an operator alerts on — nothing else in the API
    // would say that a subscriber has stopped answering.
    let stats = h.vtn.dispatcher().stats().await;
    assert_eq!((stats.pending, stats.dead), (0, 4));

    let (status, body) = h.health().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outbox"]["dead"], 4);
}

#[tokio::test]
async fn a_dead_subscriber_is_cut_off_and_can_be_restored() {
    // The cost the retry policy cannot bound. `max_attempts` caps what *one* notification spends;
    // nothing caps what a *subscriber* spends, so an endpoint that has been gone for a week costs
    // three HTTP round trips on every write in the VTN, for ever, and the operator's only signal is
    // a dead-letter count that grows.
    //
    // The breaker counts consecutive abandonments and stops queueing for that subscription. It is
    // deliberately visible rather than silent — a cut-off subscriber queues nothing, so it would
    // otherwise be indistinguishable from a healthy one with nothing to say.
    let receiver = Receiver::new();
    let callback = receiver.start().await;
    let h = Harness::with_breaker(openadr::vtn::store::BreakerPolicy {
        threshold: 2,
        cooldown: Duration::from_secs(900),
    });
    let program = h.seed(&callback).await;
    receiver.answer_with(StatusCode::INTERNAL_SERVER_ERROR);

    for i in 0..4 {
        h.create_event(&program, &format!("attempt-{i}"), json!(["group1"]))
            .await;
        h.drain().await;
    }

    // Two notifications abandoned at three attempts each, and then nothing: the third and fourth
    // events were never queued for this subscriber at all.
    assert_eq!(
        receiver.deliveries().len(),
        2 * 3,
        "the breaker did not stop the spending"
    );
    let stats = h.vtn.dispatcher().stats().await;
    assert_eq!((stats.pending, stats.dead), (0, 2));

    // Visible, and it says which subscription and why.
    let (status, health) = h.admin("/admin/subscribers").await;
    assert_eq!(status, StatusCode::OK);
    let health = health.as_array().unwrap();
    assert_eq!(health.len(), 1, "{health:?}");
    assert_eq!(health[0]["consecutiveFailures"], 2);
    assert!(health[0]["cutOffSince"].is_string());
    assert!(health[0]["retryAt"].is_string());
    assert!(
        health[0]["lastError"]
            .as_str()
            .unwrap_or_default()
            .contains("500"),
        "the row does not say what the endpoint answered: {:?}",
        health[0]["lastError"]
    );

    // And `/health` counts it, because an empty queue means two different things without this.
    let (_, body) = h.health().await;
    assert_eq!(body["subscribersCutOff"], 1);

    // The operator fixes the endpoint and presses retry: the backlog comes back *and* the breaker
    // closes. Reviving one without the other would replay the backlog and then queue nothing more.
    receiver.answer_with(StatusCode::OK);
    let (status, retried) = h.admin_call("POST", "/admin/outbox/retry").await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["revived"], 2);
    assert_eq!(retried["subscribersRestored"], 1);

    let before = receiver.deliveries().len();
    h.drain().await;
    assert_eq!(
        receiver.deliveries().len() - before,
        2,
        "the revived backlog was not delivered"
    );

    // And new writes are queued again.
    h.create_event(&program, "after-the-fix", json!(["group1"]))
        .await;
    h.drain().await;
    assert_eq!(receiver.deliveries().len(), 2 * 3 + 2 + 1);
    let (_, body) = h.health().await;
    assert_eq!(body["subscribersCutOff"], 0);
}

#[tokio::test]
async fn a_subscription_is_refused_unless_its_endpoint_answers_the_challenge() {
    // The challenge is only a defence if it happens *before* the subscription exists: a
    // subscription pointed at a third party must never be created, not created and then found to
    // be undeliverable.
    let receiver = Receiver::new();
    let callback = receiver.start().await;
    *receiver.echo_correctly.lock().unwrap() = false;

    let h = Harness::new();
    let (status, body) = h
        .call(
            "POST",
            "/subscriptions",
            VEN,
            Some(json!({
                "clientName": "ven-a",
                "objectOperations": [{
                    "objects": ["EVENT"],
                    "operations": ["CREATE"],
                    "callbackUrl": callback,
                }]
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"].as_str().unwrap_or_default().contains("echo"),
        "{body}"
    );

    // And nothing was stored, so nothing will ever be delivered to it.
    let (status, list) = h.call("GET", "/subscriptions", VEN, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 0, "{list}");
}

#[tokio::test]
// `[Def §Webhooks]`: the VTN MUST verify the callback URL belongs to the requestor with a `GET`
// carrying an `echo` parameter, and MUST NOT create the subscription when the check fails.
async fn the_echo_challenge_proves_control_of_the_endpoint() {
    let receiver = Receiver::new();
    let callback = receiver.start().await;
    let notifier = WebhookNotifier::new(WebhookConfig {
        policy: openadr::vtn::notify::CallbackPolicy::permissive(),
        ..Default::default()
    })
    .unwrap();

    Notifier::verify_callback(&notifier, &callback)
        .await
        .expect("an endpoint that echoes correctly is accepted");

    *receiver.echo_correctly.lock().unwrap() = false;
    let err = Notifier::verify_callback(&notifier, &callback)
        .await
        .expect_err("an endpoint that does not echo must be refused");
    assert!(format!("{err}").contains("echo"), "{err}");
}

#[tokio::test]
async fn an_unreachable_endpoint_is_reported_not_ignored() {
    let notifier = WebhookNotifier::new(WebhookConfig {
        policy: openadr::vtn::notify::CallbackPolicy::permissive(),
        timeout: Duration::from_millis(200),
        ..Default::default()
    })
    .unwrap();
    // Nothing is listening on this port.
    let err = Notifier::verify_callback(&notifier, "http://127.0.0.1:1/hook")
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("unreachable"), "{err}");
}

#[tokio::test]
async fn mqtt_deliveries_are_not_the_webhook_transports_business() {
    // A broker delivery belongs to the MQTT publisher. The webhook transport must decline it —
    // *decline*, not shrug: it used to answer `Ok`, so the dispatcher recorded a notification as
    // delivered that no transport had sent, and the outbox row was deleted. `Notifiers` is what
    // keeps that from ever being asked, and this is the backstop underneath it.
    let notifier = WebhookNotifier::new(WebhookConfig::default()).unwrap();
    let delivery = openadr::vtn::notify::Delivery {
        subscription_id: None,
        route: openadr::vtn::notify::Route::Topic {
            topic: "events/create".into(),
        },
        notification: openadr::model::Notification::new(
            openadr::model::Operation::Create,
            AnyObject::Program(openadr::model::Program {
                id: "prg-1".parse().unwrap(),
                created_date_time: now(),
                modification_date_time: now(),
                object_type: openadr::model::ObjectType::Program,
                content: openadr::model::ProgramRequest::new("p".parse().unwrap()),
            }),
        ),
    };
    use openadr::vtn::notify::{Channel, Notifier as _};
    assert!(notifier.handles(Channel::Webhook));
    assert!(
        !notifier.handles(Channel::Mqtt),
        "the webhook transport must not claim broker deliveries"
    );

    let failure = notifier
        .deliver(&delivery, 1)
        .await
        .expect_err("a broker delivery is not the webhook transport's to complete");
    assert!(
        !failure.retriable,
        "routing does not improve with another attempt"
    );
}
