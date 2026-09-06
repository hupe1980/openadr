//! End-to-end tests against the HTTP surface.
//!
//! These drive the real router, so they cover routing, extraction, authorization, object privacy,
//! status codes and caching together — the places where a unit-tested component can still be wired
//! up wrongly.

#![cfg(feature = "vtn")]

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use openadr::{
    core::FixedClock,
    model::{ClientId, Timestamp},
    vtn::{
        Vtn, VtnConfig,
        auth::StaticTokenAuth,
        notify::RecordingNotifier,
        store::{MemoryStorage, SharedStorage},
    },
};
use serde_json::{Value, json};
use tower::ServiceExt;

const BL: &str = "bl-secret";
const VEN_A: &str = "ven-a-secret";
const VEN_B: &str = "ven-b-secret";
/// A VEN that exists but has been granted no targets.
const VEN_C: &str = "ven-c-secret";

fn now() -> Timestamp {
    "2026-02-11T06:00:00Z".parse().unwrap()
}

struct Harness {
    router: Router,
    vtn: Vtn,
    notifier: Arc<RecordingNotifier>,
    #[allow(dead_code)]
    storage: SharedStorage,
}

impl Harness {
    fn new() -> Self {
        Self::with_policy(openadr::schema::Policy::default())
    }

    /// A VTN that refuses a payload contradicting its enumeration.
    fn strict() -> Self {
        Self::with_policy(openadr::schema::Policy::Strict)
    }

    fn with_policy(payload_policy: openadr::schema::Policy) -> Self {
        let storage = MemoryStorage::shared();
        let notifier = RecordingNotifier::shared();
        let auth = StaticTokenAuth::new("http://vtn.test/auth/token")
            .with_business_logic(BL, ClientId::new("bl").unwrap())
            .with_ven(VEN_A, ClientId::new("client-a").unwrap())
            .with_ven(VEN_B, ClientId::new("client-b").unwrap())
            .with_ven(VEN_C, ClientId::new("client-c").unwrap());

        let vtn = Vtn::builder()
            .storage(storage.clone())
            .authenticator(Arc::new(auth))
            .clock(Arc::new(FixedClock::new(now())))
            .notifier(notifier.clone())
            .config(VtnConfig {
                base_path: "/openadr3/3.1.0".into(),
                payload_policy,
                ..Default::default()
            })
            .build();

        Self {
            router: vtn.router(),
            notifier,
            storage,
            vtn,
        }
    }

    /// Everything a write queued, once the outbox has been drained.
    ///
    /// Notifications are queued by the write and delivered afterwards, so a test that asserts on
    /// them has to drain first. Draining explicitly rather than sleeping keeps the assertions exact.
    async fn delivered(&self) -> Vec<openadr::vtn::notify::Delivery> {
        self.vtn.dispatcher().drain().await;
        self.notifier.delivered()
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value, axum::http::HeaderMap) {
        let mut req = Request::builder()
            .method(method)
            .uri(format!("/openadr3/3.1.0{path}"));
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&b).unwrap()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };

        let response = self.router.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value, headers)
    }

    async fn get(&self, path: &str, token: Option<&str>) -> (StatusCode, Value) {
        let (s, v, _) = self.send(Method::GET, path, token, None).await;
        (s, v)
    }

    async fn post(&self, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
        let (s, v, _) = self.send(Method::POST, path, Some(token), Some(body)).await;
        (s, v)
    }

    async fn put(&self, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
        let (s, v, _) = self.send(Method::PUT, path, Some(token), Some(body)).await;
        (s, v)
    }

    /// A programme, an event targeted at `targets`, and VEN objects for both test clients.
    async fn seed(&self) -> (String, String) {
        let (status, program) = self
            .post(
                "/programs",
                BL,
                json!({ "programName": "grid-aware-charging" }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{program}");
        let program_id = program["id"].as_str().unwrap().to_string();

        for (client, ven_name, targets) in [
            ("client-a", "ven-a", vec!["group1"]),
            ("client-b", "ven-b", vec!["group2"]),
        ] {
            let (status, body) = self
                .post(
                    "/vens",
                    BL,
                    json!({
                        "objectType": "BL_VEN_REQUEST",
                        "clientID": client,
                        "venName": ven_name,
                        "targets": targets,
                    }),
                )
                .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }

        let (status, event) = self
            .post(
                "/events",
                BL,
                json!({
                    "programID": program_id,
                    "eventName": "curtailment",
                    "targets": ["group1", "group2"],
                    "intervalPeriod": { "start": "2026-02-11T12:00:00Z", "duration": "PT15M" },
                    "intervals": [
                        { "id": 0, "payloads": [{ "type": "IMPORT_CAPACITY_LIMIT", "values": [60] }] }
                    ]
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{event}");
        (program_id, event["id"].as_str().unwrap().to_string())
    }
}

// ---------------------------------------------------------------------------
// Routing and shape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_api_is_reachable_under_the_base_path_and_at_the_root() {
    let h = Harness::new();
    let (status, _) = h.get("/programs", Some(BL)).await;
    assert_eq!(status, StatusCode::OK);

    // Also at the root, for clients configured with a bare base URL.
    let response = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/programs")
                .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn auth_server_is_reachable_without_a_token() {
    let h = Harness::new();
    let (status, body) = h.get("/auth/server", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tokenURL"], "http://vtn.test/auth/token");
}

#[tokio::test]
async fn the_optional_token_endpoint_reports_501_as_the_specification_suggests() {
    let h = Harness::new();
    let (status, body, _) = h
        .send(Method::POST, "/auth/token", None, Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(body["status"], 501);
}

#[cfg(feature = "internal-auth")]
#[tokio::test]
async fn the_token_endpoint_is_a_real_grant_when_the_vtn_runs_one() {
    use openadr::vtn::auth::{InternalAuth, Scope, Scopes};

    let auth = InternalAuth::builder("http://vtn.test/openadr3/3.1.0/auth/token")
        .client("ven-7", "hunter2", Scopes::new(Scope::VEN))
        .unwrap()
        .build();
    let vtn = Vtn::builder()
        .storage(MemoryStorage::shared())
        .authenticator(Arc::new(auth))
        .clock(Arc::new(FixedClock::new(now())))
        .config(VtnConfig {
            base_path: "/openadr3/3.1.0".into(),
            ..Default::default()
        })
        .build();
    let router = vtn.router();

    let post_form = |body: &'static str| {
        let router = router.clone();
        async move {
            let response = router
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/openadr3/3.1.0/auth/token")
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from(body))
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
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };

    // The RFC 6749 body shape, which is what the OpenAPI document specifies.
    let (status, token) =
        post_form("grant_type=client_credentials&client_id=ven-7&client_secret=hunter2").await;
    assert_eq!(status, StatusCode::OK, "{token}");
    assert_eq!(token["token_type"], "Bearer");
    let access_token = token["access_token"].as_str().unwrap().to_string();

    // A bad secret is an RFC 6749 error body, not a `problem` body: the token endpoint is OAuth2's.
    let (status, error) =
        post_form("grant_type=client_credentials&client_id=ven-7&client_secret=wrong").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["error"], "invalid_client");

    // And the token it minted actually authenticates.
    let response = router
        .oneshot(
            Request::builder()
                .uri("/openadr3/3.1.0/vens")
                .header(header::AUTHORIZATION, format!("Bearer {access_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn every_error_carries_a_problem_body_and_a_traceable_id() {
    let h = Harness::new();
    let (status, body, headers) = h.send(Method::GET, "/events/nope", Some(BL), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["status"], 404);
    assert_eq!(body["title"], "Not Found");
    // The id in the body is the id in the header, so a client quoting one is quoting the other.
    let header_id = headers
        .get("x-request-id")
        .expect("every response carries a request id")
        .to_str()
        .unwrap();
    assert_eq!(body["instance"].as_str(), Some(header_id));
    assert!(body["type"].as_str().unwrap().starts_with("https://"));
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/problem+json"
    );
}

#[tokio::test]
async fn a_missing_token_is_401_with_a_www_authenticate_header() {
    let h = Harness::new();
    let (status, _, headers) = h.send(Method::GET, "/programs", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(headers.contains_key(header::WWW_AUTHENTICATE));
}

#[tokio::test]
async fn an_unsupported_method_is_405_with_a_problem_body() {
    let h = Harness::new();
    let (status, body, _) = h
        .send(Method::PATCH, "/programs", Some(BL), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["status"], 405);
}

// ---------------------------------------------------------------------------
// Scopes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_ven_cannot_write_programmes_or_events() {
    let h = Harness::new();
    let (status, body) = h
        .post("/programs", VEN_A, json!({ "programName": "sneaky" }))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["detail"].as_str().unwrap().contains("write_programs"));

    let (status, _) = h
        .post("/events", VEN_A, json!({ "programID": "prg-00000000" }))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn business_logic_may_not_write_reports() {
    let h = Harness::new();
    let (_, event_id) = h.seed().await;
    let (status, body) = h
        .post(
            "/reports",
            BL,
            json!({
                "eventID": event_id,
                "clientName": "bl",
                "resources": [],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

// ---------------------------------------------------------------------------
// Object privacy — the part that leaks money if it is wrong
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_ven_must_name_its_targets_to_see_a_targeted_event() {
    let h = Harness::new();
    h.seed().await;

    // No targets named: the targeted event stays hidden.
    let (status, body) = h.get("/events", Some(VEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);

    // Named: visible.
    let (status, body) = h.get("/events?targets=group1", Some(VEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_ven_never_learns_the_other_groups_an_event_targets() {
    let h = Harness::new();
    h.seed().await;

    let (_, body) = h.get("/events?targets=group1", Some(VEN_A)).await;
    let targets = body[0]["targets"].as_array().unwrap();
    assert_eq!(
        targets,
        &vec![json!("group1")],
        "group2 must not appear in ven-a's view"
    );

    // Business logic sees the whole set.
    let (_, body) = h.get("/events", Some(BL)).await;
    let targets = body[0]["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 2);
}

#[tokio::test]
async fn asking_for_a_target_you_were_not_granted_reveals_nothing() {
    let h = Harness::new();
    h.seed().await;
    // ven-a is in group1 only.
    let (status, body) = h.get("/events?targets=group2", Some(VEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_hidden_object_reads_as_missing_not_as_forbidden() {
    let h = Harness::new();
    let (program_id, _) = h.seed().await;

    // An event targeted only at group2, which ven-c is not in.
    let (status, event) = h
        .post(
            "/events",
            BL,
            json!({
                "programID": program_id,
                "eventName": "group2-only",
                "targets": ["group2"],
                "intervalPeriod": { "start": "2026-02-11T14:00:00Z", "duration": "PT15M" },
                "intervals": [{ "id": 0, "payloads": [{"type": "SIMPLE", "values": [1]}] }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{event}");
    let event_id = event["id"].as_str().unwrap();

    // ven-c holds a VEN object but was granted nothing.
    let (status, body) = h
        .post(
            "/vens",
            BL,
            json!({
                "objectType": "BL_VEN_REQUEST",
                "clientID": "client-c",
                "venName": "ven-c",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // A 403 would confirm the event exists, which is exactly what targeting conceals.
    let (status, _) = h.get(&format!("/events/{event_id}"), Some(VEN_C)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // ven-b is in group2 and sees it without having to name the target.
    let (status, _) = h.get(&format!("/events/{event_id}"), Some(VEN_B)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn reading_by_id_does_not_require_naming_targets_but_still_needs_the_grant() {
    let h = Harness::new();
    let (_, event_id) = h.seed().await;

    // The id is the request; a VEN in a targeted group reads it directly.
    let (status, body) = h.get(&format!("/events/{event_id}"), Some(VEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    // Target hiding still applies.
    assert_eq!(body["targets"], json!(["group1"]));

    // A VEN with no grant still cannot, so guessing ids reveals nothing.
    let (status, _) = h.get(&format!("/events/{event_id}"), Some(VEN_C)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn untargeted_programmes_are_visible_to_everyone() {
    let h = Harness::new();
    h.seed().await;
    for token in [BL, VEN_A, VEN_B] {
        let (status, body) = h.get("/programs", Some(token)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 1, "token {token}");
    }
}

#[tokio::test]
async fn a_ven_sees_only_its_own_ven_object() {
    let h = Harness::new();
    h.seed().await;

    let (_, body) = h.get("/vens", Some(VEN_A)).await;
    let vens = body.as_array().unwrap();
    assert_eq!(vens.len(), 1);
    assert_eq!(vens[0]["venName"], "ven-a");

    let (_, body) = h.get("/vens", Some(BL)).await;
    assert_eq!(body.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_ven_cannot_grant_itself_targets() {
    let h = Harness::new();

    // The BL-flavoured body is refused outright.
    let (status, body) = h
        .post(
            "/vens",
            VEN_A,
            json!({
                "objectType": "BL_VEN_REQUEST",
                "clientID": "client-a",
                "venName": "self-promoted",
                "targets": ["group1", "group2"],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // And the VEN-flavoured one has nowhere to put targets.
    let (status, body) = h
        .post(
            "/vens",
            VEN_A,
            json!({
                "objectType": "VEN_VEN_REQUEST",
                "venName": "honest-ven",
                "targets": ["group1"],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        body["targets"].as_array().is_none_or(|t| t.is_empty()),
        "a VEN-written body must never yield targets: {body}"
    );
}

#[tokio::test]
async fn a_report_belongs_to_the_ven_that_wrote_it() {
    let h = Harness::new();
    let (_, event_id) = h.seed().await;

    let (status, report) = h
        .post(
            "/reports",
            VEN_A,
            json!({
                "eventID": event_id,
                "clientName": "ven-a",
                "resources": [{
                    "resourceName": "charger-1",
                    "intervals": [{ "id": 0, "payloads": [{"type": "USAGE", "values": [12.5]}] }]
                }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{report}");
    // The VTN stamps the identity from the token.
    assert_eq!(report["clientID"], "client-a");

    // The other VEN cannot see it.
    let (_, body) = h.get("/reports", Some(VEN_B)).await;
    assert_eq!(body.as_array().unwrap().len(), 0);

    // Its owner can.
    let (_, body) = h.get("/reports", Some(VEN_A)).await;
    assert_eq!(body.as_array().unwrap().len(), 1);

    // And so can business logic.
    let (_, body) = h.get("/reports", Some(BL)).await;
    assert_eq!(body.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn one_ven_cannot_read_anothers_report_by_id() {
    let h = Harness::new();
    let (_, event_id) = h.seed().await;
    let (_, report) = h
        .post(
            "/reports",
            VEN_A,
            json!({
                "eventID": event_id,
                "clientName": "ven-a",
                "resources": []
            }),
        )
        .await;
    let report_id = report["id"].as_str().unwrap();
    let (status, _) = h.get(&format!("/reports/{report_id}"), Some(VEN_B)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_duplicate_programme_name_is_409() {
    let h = Harness::new();
    let body = json!({ "programName": "tou" });
    assert_eq!(
        h.post("/programs", BL, body.clone()).await.0,
        StatusCode::CREATED
    );
    assert_eq!(h.post("/programs", BL, body).await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn an_event_for_a_missing_programme_is_400_not_404() {
    let h = Harness::new();
    let (status, body) = h
        .post("/events", BL, json!({ "programID": "prg-99999999" }))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["detail"].as_str().unwrap().contains("programID"));
}

#[tokio::test]
async fn an_unresolvable_event_is_rejected_at_the_boundary() {
    let h = Harness::new();
    let (program_id, _) = h.seed().await;
    // An interval with no start anywhere cannot be placed in time.
    let (status, body) = h
        .post(
            "/events",
            BL,
            json!({
                "programID": program_id,
                "intervals": [{ "id": 0, "payloads": [] }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("resolvable"));
}

#[tokio::test]
async fn a_malformed_body_names_the_problem() {
    let h = Harness::new();
    let (status, body) = h.post("/programs", BL, json!({ "wrong": "field" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["detail"].as_str().unwrap().contains("programName"));
}

#[tokio::test]
async fn limit_beyond_the_schema_maximum_is_rejected() {
    let h = Harness::new();
    let (status, _) = h.get("/programs?limit=500", Some(BL)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_webhook_callback_must_be_https_and_public() {
    let h = Harness::new();
    for url in [
        "http://example.com/hook",
        "https://127.0.0.1/hook",
        "https://192.168.1.10/hook",
        "https://localhost/hook",
    ] {
        let (status, body) = h
            .post(
                "/subscriptions",
                VEN_A,
                json!({
                    "clientName": "ven-a",
                    "objectOperations": [{
                        "objects": ["EVENT"],
                        "operations": ["CREATE"],
                        "callbackUrl": url,
                    }]
                }),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{url} should be refused: {body}"
        );
    }

    let (status, _) = h
        .post(
            "/subscriptions",
            VEN_A,
            json!({
                "clientName": "ven-a",
                "objectOperations": [{
                    "objects": ["EVENT"],
                    "operations": ["CREATE"],
                    "callbackUrl": "https://ven-a.example.com/hook",
                }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
}

// ---------------------------------------------------------------------------
// Caching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_repeated_read_is_answered_with_304() {
    let h = Harness::new();
    h.seed().await;

    let (status, _, headers) = h.send(Method::GET, "/programs", Some(BL), None).await;
    assert_eq!(status, StatusCode::OK);
    let etag = headers
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let response = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/openadr3/3.1.0/programs")
                .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(body.is_empty(), "a 304 carries no body");
}

#[tokio::test]
async fn the_tag_changes_when_the_content_does() {
    let h = Harness::new();
    h.seed().await;
    let (_, _, first) = h.send(Method::GET, "/programs", Some(BL), None).await;
    h.post("/programs", BL, json!({ "programName": "second" }))
        .await;
    let (_, _, second) = h.send(Method::GET, "/programs", Some(BL), None).await;
    assert_ne!(first.get(header::ETAG), second.get(header::ETAG));
}

#[tokio::test]
async fn two_readers_with_different_visibility_get_different_tags() {
    let h = Harness::new();
    h.seed().await;
    let (_, _, bl) = h.send(Method::GET, "/events", Some(BL), None).await;
    let (_, _, ven) = h
        .send(Method::GET, "/events?targets=group1", Some(VEN_A), None)
        .await;
    assert_ne!(
        bl.get(header::ETAG),
        ven.get(header::ETAG),
        "a tag must never let one reader validate another reader's view"
    );
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creating_an_event_notifies_only_entitled_subscribers() {
    let h = Harness::new();
    let (program_id, _) = h.seed().await;

    for token in [VEN_A, VEN_B] {
        let (status, body) = h
            .post(
                "/subscriptions",
                token,
                json!({
                    "clientName": if token == VEN_A { "ven-a" } else { "ven-b" },
                    "objectOperations": [{
                        "objects": ["EVENT"],
                        "operations": ["CREATE"],
                        "callbackUrl": format!("https://{}.example.com/hook",
                            if token == VEN_A { "a" } else { "b" }),
                    }]
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    h.notifier.clear();

    let (status, _) = h
        .post(
            "/events",
            BL,
            json!({
                "programID": program_id,
                "eventName": "group1-only",
                "targets": ["group1"],
                "intervalPeriod": { "start": "2026-02-11T13:00:00Z", "duration": "PT15M" },
                "intervals": [{ "id": 0, "payloads": [{"type": "SIMPLE", "values": [1]}] }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let delivered = h.delivered().await;
    assert_eq!(delivered.len(), 1, "only ven-a is in group1");
    assert!(
        delivered[0]
            .route
            .callback_url()
            .unwrap()
            .contains("a.example.com")
    );
    assert_eq!(
        delivered[0].notification.targets,
        vec!["group1".parse().unwrap()]
    );
}

#[tokio::test]
async fn deleting_an_event_notifies_a_delete() {
    let h = Harness::new();
    let (_, event_id) = h.seed().await;
    let (status, body) = h
        .post(
            "/subscriptions",
            VEN_A,
            json!({
                "clientName": "ven-a",
                "objectOperations": [{
                    "objects": ["EVENT"],
                    "operations": ["DELETE"],
                    "callbackUrl": "https://a.example.com/hook",
                }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    h.notifier.clear();

    let (status, _, _) = h
        .send(
            Method::DELETE,
            &format!("/events/{event_id}"),
            Some(BL),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let delivered = h.delivered().await;
    assert_eq!(delivered.len(), 1);
    assert_eq!(
        delivered[0].notification.operation,
        openadr::model::Operation::Delete
    );
}

// ---------------------------------------------------------------------------
// Notifier discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_ven_cannot_read_another_vens_subscription() {
    // A subscription carries its subscriber's callbackUrl and bearerToken, so the ownership rule
    // here is standing between one VEN and another's credentials.
    let h = Harness::new();
    h.seed().await;
    for (token, name) in [(VEN_A, "ven-a"), (VEN_B, "ven-b")] {
        let (status, body) = h
            .post(
                "/subscriptions",
                token,
                json!({
                    "clientName": name,
                    "objectOperations": [{
                        "objects": ["EVENT"],
                        "operations": ["CREATE"],
                        "callbackUrl": format!("https://{name}.example.com/hook"),
                        "bearerToken": format!("{name}-secret"),
                    }]
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    let (status, mine) = h.get("/subscriptions", Some(VEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    let mine = mine.as_array().unwrap();
    assert_eq!(mine.len(), 1, "a VEN must see only its own: {mine:?}");
    assert_eq!(mine[0]["clientID"], "client-a");
    let serialised = serde_json::to_string(&mine).unwrap();
    assert!(
        !serialised.contains("ven-b-secret"),
        "another subscriber's bearer token leaked: {serialised}"
    );

    // Business logic sees both.
    let (_, all) = h.get("/subscriptions", Some(BL)).await;
    assert_eq!(all.as_array().unwrap().len(), 2);

    // And fetching another's by id is a 404, not a 403.
    let theirs = all
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["clientID"] == "client-b")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _) = h
        .get(&format!("/subscriptions/{theirs}"), Some(VEN_A))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_target_query_returns_only_objects_carrying_that_target() {
    // `[Def §Response Filtering]`: targeting criteria "include only those objects that include
    // target terms found in the query", and filters are additive. An untargeted object is not gated
    // by targeting, but it does not carry the term either — it is reached by naming no target.
    let h = Harness::new();
    let (program_id, _) = h.seed().await;
    let (status, body) = h
        .post(
            "/events",
            BL,
            json!({
                "programID": program_id,
                "eventName": "public",
                "intervalPeriod": { "start": "2026-02-11T12:00:00Z", "duration": "PT15M" },
                "intervals": [{ "id": 0, "payloads": [{ "type": "PRICE", "values": [0.17] }] }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (_, all) = h.get("/events", Some(BL)).await;
    assert_eq!(all.as_array().unwrap().len(), 2, "one targeted, one not");

    let (_, targeted) = h.get("/events?targets=group1", Some(BL)).await;
    let targeted = targeted.as_array().unwrap();
    assert_eq!(targeted.len(), 1, "{targeted:?}");
    assert_eq!(targeted[0]["eventName"], "curtailment");

    // And the VEN in group1 sees the same one, with only its own target on it.
    let (_, mine) = h.get("/events?targets=group1", Some(VEN_A)).await;
    let mine = mine.as_array().unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["targets"], json!(["group1"]));
}

#[tokio::test]
async fn a_target_query_narrows_the_owned_collections() {
    // `?targets=` is an additive filter on /vens and /resources `[Def §Response Filtering]`.
    // Accepting it and then ignoring it is worse than refusing it: the caller believes it worked.
    let h = Harness::new();
    h.seed().await;

    let (status, all) = h.get("/vens", Some(BL)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all.as_array().unwrap().len(), 2);

    let (status, group1) = h.get("/vens?targets=group1", Some(BL)).await;
    assert_eq!(status, StatusCode::OK);
    let group1 = group1.as_array().unwrap();
    assert_eq!(group1.len(), 1, "{group1:?}");
    assert_eq!(group1[0]["venName"], "ven-a");

    let (_, none) = h.get("/vens?targets=nobody", Some(BL)).await;
    assert!(none.as_array().unwrap().is_empty());

    // A VEN still reads its own VEN object without naming a target: ownership gates these, not
    // targeting, and its own grant must not hide it from itself.
    let (status, mine) = h.get("/vens", Some(VEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    let mine = mine.as_array().unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["targets"], json!(["group1"]));
}

#[tokio::test]
async fn notifiers_always_advertise_webhooks() {
    let h = Harness::new();
    let (status, body) = h.get("/notifiers", Some(VEN_A)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["WEBHOOK"], true);
    assert!(body.get("MQTT").is_none(), "no broker is configured");
}

#[tokio::test]
async fn mqtt_topic_endpoints_are_absent_without_a_broker() {
    let h = Harness::new();
    let (status, _) = h.get("/notifiers/mqtt/topics/events", Some(BL)).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn collection_topics_are_business_logic_only() {
    let storage = MemoryStorage::shared();
    let auth = StaticTokenAuth::new("http://vtn.test/auth/token")
        .with_business_logic(BL, ClientId::new("bl").unwrap())
        .with_ven(VEN_A, ClientId::new("client-a").unwrap());
    let vtn = Vtn::builder()
        .storage(storage)
        .authenticator(Arc::new(auth))
        .clock(Arc::new(FixedClock::new(now())))
        .mqtt(openadr::model::MqttNotifierBinding {
            uris: vec!["mqtts://broker.test:8883".into()],
            serialization: openadr::model::Serialization::Json,
            authentication: openadr::model::MqttAuthentication::Anonymous,
        })
        .build();
    let router = vtn.router();

    let call = |token: &'static str, path: &'static str| {
        let router = router.clone();
        async move {
            router
                .oneshot(
                    Request::builder()
                        .uri(format!("/openadr3/3.1.0{path}"))
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        }
    };

    let call = std::sync::Arc::new(call);
    for path in [
        "/notifiers/mqtt/topics/events",
        "/notifiers/mqtt/topics/reports",
        "/notifiers/mqtt/topics/subscriptions",
        "/notifiers/mqtt/topics/vens",
        "/notifiers/mqtt/topics/resources",
        "/notifiers/mqtt/topics/programs",
    ] {
        assert_eq!(call(BL, path).await, StatusCode::OK, "{path}");
        assert_eq!(
            call(VEN_A, path).await,
            StatusCode::FORBIDDEN,
            "{path}: a VEN must not learn a collection-wide topic — subscribing to one would \
             hand it every object of that type with its full target set"
        );
    }
}

#[tokio::test]
async fn programme_scoped_topics_are_business_logic_only() {
    // `…/topics/programs/{id}` and `…/programs/{id}/events` name topics carrying every event of a
    // programme, targets and all. The OpenAPI document puts `read_all` on both; handing them to a
    // VEN would undo object privacy on the push path however the broker were configured.
    let storage = MemoryStorage::shared();
    let auth = StaticTokenAuth::new("http://vtn.test/auth/token")
        .with_business_logic(BL, ClientId::new("bl").unwrap())
        .with_ven(VEN_A, ClientId::new("client-a").unwrap());
    let vtn = Vtn::builder()
        .storage(storage)
        .authenticator(Arc::new(auth))
        .clock(Arc::new(FixedClock::new(now())))
        .mqtt(openadr::model::MqttNotifierBinding {
            uris: vec!["mqtts://broker.test:8883".into()],
            serialization: openadr::model::Serialization::Json,
            authentication: openadr::model::MqttAuthentication::Anonymous,
        })
        .build();
    let router = vtn.router();

    let create = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/openadr3/3.1.0/programs")
                .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({"programName": "private"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(create.into_body(), usize::MAX)
        .await
        .unwrap();
    let program: Value = serde_json::from_slice(&bytes).unwrap();
    let id = program["id"].as_str().unwrap();

    for path in [
        format!("/notifiers/mqtt/topics/programs/{id}"),
        format!("/notifiers/mqtt/topics/programs/{id}/events"),
    ] {
        let call = |token: &str| {
            let router = router.clone();
            let uri = format!("/openadr3/3.1.0{path}");
            let token = token.to_string();
            async move {
                router
                    .oneshot(
                        Request::builder()
                            .uri(uri)
                            .header(header::AUTHORIZATION, format!("Bearer {token}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .status()
            }
        };
        assert_eq!(call(BL).await, StatusCode::OK, "{path}");
        assert_eq!(call(VEN_A).await, StatusCode::FORBIDDEN, "{path}");
    }
}

#[tokio::test]
async fn a_ven_may_only_ask_for_its_own_scoped_topics() {
    let storage = MemoryStorage::shared();
    let auth = StaticTokenAuth::new("http://vtn.test/auth/token")
        .with_business_logic(BL, ClientId::new("bl").unwrap())
        .with_ven(VEN_A, ClientId::new("client-a").unwrap())
        .with_ven(VEN_B, ClientId::new("client-b").unwrap());
    let vtn = Vtn::builder()
        .storage(storage)
        .authenticator(Arc::new(auth))
        .clock(Arc::new(FixedClock::new(now())))
        .mqtt(openadr::model::MqttNotifierBinding {
            uris: vec!["mqtts://broker.test:8883".into()],
            serialization: openadr::model::Serialization::Json,
            authentication: openadr::model::MqttAuthentication::Anonymous,
        })
        .build();
    let h = Harness {
        router: vtn.router(),
        notifier: RecordingNotifier::shared(),
        storage: MemoryStorage::shared(),
        vtn,
    };

    for (client, name) in [("client-a", "ven-a"), ("client-b", "ven-b")] {
        h.post(
            "/vens",
            BL,
            json!({ "objectType": "BL_VEN_REQUEST", "clientID": client, "venName": name }),
        )
        .await;
    }
    let (_, vens) = h.get("/vens", Some(BL)).await;
    let ven_a_id = vens
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["venName"] == "ven-a")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = h
        .get(
            &format!("/notifiers/mqtt/topics/vens/{ven_a_id}/events"),
            Some(VEN_A),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["topics"]["CREATE"]
            .as_str()
            .unwrap()
            .contains(&format!("vens/{ven_a_id}"))
    );

    // ven-b asking for ven-a's topics gets a 404, not a 403: it must not learn the id is real.
    let (status, _) = h
        .get(
            &format!("/notifiers/mqtt/topics/vens/{ven_a_id}/events"),
            Some(VEN_B),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Query semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn active_filters_out_events_that_have_elapsed() {
    let h = Harness::new();
    let (program_id, _) = h.seed().await;

    // An event that ended before the frozen clock.
    let (status, _) = h
        .post(
            "/events",
            BL,
            json!({
                "programID": program_id,
                "eventName": "yesterday",
                "intervalPeriod": { "start": "2026-02-10T00:00:00Z", "duration": "PT1H" },
                "intervals": [{ "id": 0, "payloads": [{"type": "SIMPLE", "values": [0]}] }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, all) = h.get("/events", Some(BL)).await;
    assert_eq!(all.as_array().unwrap().len(), 2);

    let (_, active) = h.get("/events?active=true", Some(BL)).await;
    let names: Vec<&str> = active
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["eventName"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["curtailment"]);
}

#[tokio::test]
async fn programme_name_lookup_finds_one_programme_without_paging() {
    let h = Harness::new();
    h.seed().await;
    let (status, body) = h
        .get("/programs?programName=grid-aware-charging", Some(BL))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);

    let (_, body) = h.get("/programs?programName=absent", Some(BL)).await;
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn targets_accept_both_repeated_and_comma_separated_forms() {
    let h = Harness::new();
    h.seed().await;
    let (_, a) = h
        .get("/events?targets=group1&targets=group2", Some(BL))
        .await;
    let (_, b) = h.get("/events?targets=group1,group2", Some(BL)).await;
    assert_eq!(a, b);
    assert_eq!(a.as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_event_survives_a_round_trip_through_the_api() {
    let h = Harness::new();
    let (program_id, _) = h.seed().await;

    let sent = json!({
        "programID": program_id,
        "eventName": "day-ahead prices",
        "priority": 10,
        "duration": "P1D",
        "intervalPeriod": {
            "start": "2026-02-12T00:00:00Z",
            "duration": "PT1H",
            "randomizeStart": "PT5M"
        },
        "payloadDescriptors": [
            { "objectType": "EVENT_PAYLOAD_DESCRIPTOR", "payloadType": "PRICE",
              "units": "KWH", "currency": "EUR" }
        ],
        "reportDescriptors": [
            { "payloadType": "USAGE", "aggregate": false, "startInterval": -1,
              "numIntervals": -1, "historical": true, "frequency": -1, "repeat": 1,
              "reportIntervals": "INTERVALS" }
        ],
        "intervals": [
            { "id": 0, "payloads": [{ "type": "PRICE", "values": [0.17] }] },
            { "id": 1, "payloads": [{ "type": "PRICE", "values": [0.03] }] }
        ]
    });

    let (status, created) = h.post("/events", BL, sent.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let (status, fetched) = h
        .get(
            &format!("/events/{}", created["id"].as_str().unwrap()),
            Some(BL),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Every field the client sent comes back unchanged, prices included.
    for key in [
        "programID",
        "eventName",
        "priority",
        "duration",
        "intervalPeriod",
        "payloadDescriptors",
        "reportDescriptors",
        "intervals",
    ] {
        assert_eq!(fetched[key], sent[key], "field {key} did not round trip");
    }
    assert_eq!(
        fetched["intervals"][0]["payloads"][0]["values"][0],
        json!(0.17)
    );
    assert_eq!(fetched["objectType"], "EVENT");
}

#[tokio::test]
async fn the_active_filter_narrows_before_the_page_is_cut() {
    let h = Harness::new();
    let (program_id, _) = h.seed().await;

    // 60 events that have already elapsed, then 3 that are still to come.
    for i in 0..60 {
        h.post(
            "/events",
            BL,
            json!({
                "programID": program_id, "eventName": format!("past-{i}"),
                "intervalPeriod": {"start":"2026-02-10T00:00:00Z","duration":"PT1H"},
                "intervals": [{"id":0,"payloads":[{"type":"SIMPLE","values":[1]}]}]
            }),
        )
        .await;
    }
    for i in 0..3 {
        h.post(
            "/events",
            BL,
            json!({
                "programID": program_id, "eventName": format!("future-{i}"),
                "intervalPeriod": {"start":"2026-02-12T00:00:00Z","duration":"PT1H"},
                "intervals": [{"id":0,"payloads":[{"type":"SIMPLE","values":[1]}]}]
            }),
        )
        .await;
    }

    // First page of active events. There are 4 active in total (seed + 3).
    let (_, page1) = h.get("/events?active=true&skip=0&limit=50", Some(BL)).await;
    let n1 = page1.as_array().unwrap().len();
    assert_eq!(
        n1, 4,
        "all four active events belong on the first page; filtering after pagination would \
         have left it short and a client stops on a short page"
    );
}

#[tokio::test]
async fn a_partly_granted_request_still_fills_the_first_page() {
    let h = Harness::new();
    let (program_id, _) = h.seed().await;

    // 60 events for group2 only, which ven-a may not see.
    for i in 0..60 {
        h.post(
            "/events",
            BL,
            json!({
                "programID": program_id, "eventName": format!("theirs-{i}"), "targets": ["group2"],
                "intervalPeriod": {"start":"2026-02-11T12:00:00Z","duration":"PT15M"},
                "intervals": [{"id":0,"payloads":[{"type":"SIMPLE","values":[1]}]}]
            }),
        )
        .await;
    }
    for i in 0..3 {
        h.post(
            "/events",
            BL,
            json!({
                "programID": program_id, "eventName": format!("mine-{i}"), "targets": ["group1"],
                "intervalPeriod": {"start":"2026-02-11T12:00:00Z","duration":"PT15M"},
                "intervals": [{"id":0,"payloads":[{"type":"SIMPLE","values":[1]}]}]
            }),
        )
        .await;
    }

    // ven-a asks for both targets; it is only in group1.
    let (_, page1) = h
        .get("/events?targets=group1,group2&skip=0&limit=50", Some(VEN_A))
        .await;
    let n1 = page1.as_array().unwrap().len();
    assert_eq!(
        n1, 4,
        "ven-a is entitled to one of the two requested targets and has four matching events; \
         filtering after pagination returned only one of them"
    );
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// A body labelled with a media type this endpoint cannot read is refused before it is parsed.
///
/// The failure this replaces was silent in the worst direction: `curl -d '{…}'` labels its body
/// `application/x-www-form-urlencoded`, the VTN parsed it as JSON anyway, and a client whose
/// serialiser was misconfigured had no way to find out.
#[tokio::test]
async fn a_body_labelled_with_a_foreign_media_type_is_refused() {
    let h = Harness::new();
    for media_type in [
        "text/plain",
        "application/x-www-form-urlencoded",
        "application/xml",
    ] {
        let response = h
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/openadr3/3.1.0/programs")
                    .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                    .header(header::CONTENT_TYPE, media_type)
                    .body(Body::from(r#"{"programName":"p"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{media_type} must be refused"
        );
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json"),
        );
    }
}

/// JSON, however it is spelled — and a body that claims nothing is still read.
#[tokio::test]
async fn every_json_spelling_is_accepted_and_so_is_silence() {
    let h = Harness::new();
    for (n, media_type) in [
        Some("application/json"),
        Some("application/json; charset=utf-8"),
        Some("application/openadr3+json"),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/openadr3/3.1.0/programs")
            .header(header::AUTHORIZATION, format!("Bearer {BL}"));
        if let Some(media_type) = media_type {
            req = req.header(header::CONTENT_TYPE, media_type);
        }
        let response = h
            .router
            .clone()
            .oneshot(
                req.body(Body::from(format!(r#"{{"programName":"p{n}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED, "{media_type:?}");
    }
}

/// Every read says how it may be cached, because the body is a function of who asked.
#[tokio::test]
async fn reads_are_marked_private_and_revalidated() {
    let h = Harness::new();
    h.seed().await;

    let (status, _, headers) = h.send(Method::GET, "/programs", Some(BL), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CACHE_CONTROL).unwrap(),
        "private, no-cache"
    );
    // A shared cache that stored it anyway must not hand it to a different identity.
    assert!(
        headers
            .get_all(header::VARY)
            .iter()
            .any(|v| v.to_str().unwrap().eq_ignore_ascii_case("authorization"))
    );

    // The 304 carries the same instruction: a validator without a caching policy is a client
    // guessing at the policy.
    let etag = headers.get(header::ETAG).unwrap().clone();
    let response = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/openadr3/3.1.0/programs")
                .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                .header(header::IF_NONE_MATCH, etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "private, no-cache"
    );
}

/// Every path the served OpenAPI document describes is a path this VTN actually routes.
///
/// The document is a claim about the API, and a claim nothing checks is the shape half the defects
/// in this repository have had. `cargo xtask check-paths` guards the other direction — the
/// Alliance's document against the router — and needs `specs/`; this one needs nothing and runs in
/// every build, because the document a *client* fetches is the one served here.
#[tokio::test]
async fn the_openapi_document_describes_only_endpoints_that_exist() {
    let h = Harness::new();
    let (program, event) = h.seed().await;

    let document = openadr::vtn::openapi::document(&h.vtn.state().config);
    let paths = document["paths"].as_object().expect("paths is an object");
    assert!(!paths.is_empty());

    let mut checked = 0;
    for (template, operations) in paths {
        // A concrete instance of the template. Existing ids where the seed made one, so a `404`
        // in the answer means "not routed" rather than "no such object".
        let path = template
            .replace("{programID}", &program)
            .replace("{eventID}", &event)
            .replace("{reportID}", "rpt-does-not-exist")
            .replace("{subscriptionID}", "sub-does-not-exist")
            .replace("{venID}", "ven-does-not-exist")
            .replace("{resourceID}", "res-does-not-exist");
        assert!(
            !path.contains('{'),
            "{template} has a path parameter this test does not know how to fill"
        );

        for method in operations
            .as_object()
            .into_iter()
            .flat_map(|o| o.keys())
            .filter(|k| ["get", "post", "put", "delete"].contains(&k.as_str()))
        {
            let method = Method::from_bytes(method.to_uppercase().as_bytes()).unwrap();
            let body = matches!(method, Method::POST | Method::PUT).then(|| json!({}));
            let (status, _, _) = h.send(method.clone(), &path, Some(BL), body).await;
            assert!(
                status != StatusCode::METHOD_NOT_ALLOWED,
                "the document describes {method} {template}, which this VTN does not route"
            );
            // A 404 with a `no-such-route` problem is the router saying the path is unknown; a 404
            // naming an object is the handler saying the object is not there, which is routed.
            if status == StatusCode::NOT_FOUND {
                let (_, problem, _) = h.send(method.clone(), &path, Some(BL), None).await;
                assert_ne!(
                    problem["type"], "https://openadr.dev/problems/no-such-route",
                    "the document describes {method} {template}, which this VTN does not route"
                );
            }
            checked += 1;
        }
    }
    assert!(checked >= 30, "only {checked} operations were checked");
}

/// Every error is a `problem`, including the ones no handler produced.
///
/// `413` came from a `tower-http` layer outside the middleware that writes problem bodies, so it
/// went out as the eleven ASCII bytes `length limit exceeded` while three documents said the VTN
/// answered a problem on every 4xx `[D-104]`.
#[tokio::test]
async fn an_error_a_layer_produced_is_still_a_problem_body() {
    let h = Harness::new();
    let huge = "x".repeat(9 * 1024 * 1024);
    let response = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/openadr3/3.1.0/programs")
                .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(huge))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json"),
    );
    let request_id = response
        .headers()
        .get("x-request-id")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let problem: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(problem["status"], 413);
    assert_eq!(
        problem["type"],
        "https://openadr.dev/problems/payload-too-large"
    );
    // The same traceability every other error has: without it the one error an operator cannot
    // reproduce is the one they cannot look up either.
    assert_eq!(problem["instance"].as_str().map(str::to_string), request_id);
}

/// RFC 6749 §5.1: a response carrying a token says `Cache-Control: no-store`.
#[tokio::test]
async fn a_token_response_is_never_stored() {
    let h = Harness::new();
    // This harness delegates token issuance, so the endpoint answers 501 — which is still a
    // response *from the token endpoint*, and the header is a property of the endpoint rather than
    // of the happy path. `tests/jwt_auth.rs` covers a VTN that issues.
    let response = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/openadr3/3.1.0/auth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "grant_type=client_credentials&client_id=x&client_secret=y",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
}

#[tokio::test]
async fn an_oversized_body_is_refused_rather_than_buffered() {
    let h = Harness::new();
    // Well past the 8 MiB default; the point is that it is refused, not that it is parsed.
    let huge = "x".repeat(9 * 1024 * 1024);
    let response = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/openadr3/3.1.0/programs")
                .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(huge))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn responses_are_compressed_when_the_client_asks() {
    let h = Harness::new();
    h.seed().await;

    let response = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/openadr3/3.1.0/events")
                .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_ENCODING)
            .map(|v| v.to_str().unwrap()),
        Some("gzip"),
        "the specification encourages gzip for bandwidth-constrained links"
    );
}

#[tokio::test]
async fn a_client_that_does_not_ask_gets_plain_json() {
    let h = Harness::new();
    h.seed().await;
    let (_, _, headers) = h.send(Method::GET, "/events", Some(BL), None).await;
    assert!(headers.get(header::CONTENT_ENCODING).is_none());
}

// ---------------------------------------------------------------------------
// The broker's authorization callbacks
// ---------------------------------------------------------------------------

/// A VTN with a broker binding, a topic prefix, and one business-logic client named to the ACL.
async fn broker_harness() -> (Router, String) {
    let storage = MemoryStorage::shared();
    let auth = StaticTokenAuth::new("http://vtn.test/auth/token")
        .with_business_logic(BL, ClientId::new("bl").unwrap())
        .with_ven(VEN_A, ClientId::new("client-a").unwrap())
        .with_ven(VEN_B, ClientId::new("client-b").unwrap());
    let vtn = Vtn::builder()
        .storage(storage)
        .authenticator(Arc::new(auth))
        .clock(Arc::new(FixedClock::new(now())))
        .config(VtnConfig {
            base_path: "/openadr3/3.1.0".into(),
            mqtt_topic_prefix: "openadr3".into(),
            ..Default::default()
        })
        .mqtt(openadr::model::MqttNotifierBinding {
            uris: vec!["mqtts://broker.test:8883".into()],
            serialization: openadr::model::Serialization::Json,
            authentication: openadr::model::MqttAuthentication::Oauth2BearerToken {
                username: "{clientID}".into(),
            },
        })
        .mqtt_business_logic_clients([ClientId::new("bl").unwrap()])
        .mqtt_publisher_client(ClientId::new("bl").unwrap())
        .build();
    let router = vtn.router();

    // One VEN object per client, so the ACL has something to resolve.
    for (client, name) in [("client-a", "ven-a"), ("client-b", "ven-b")] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/openadr3/3.1.0/vens")
                    .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "objectType": "BL_VEN_REQUEST",
                            "clientID": client,
                            "venName": name,
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/openadr3/3.1.0/vens?venName=ven-a")
                .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let vens: Value = serde_json::from_slice(&bytes).unwrap();
    let ven_a_id = vens[0]["id"].as_str().unwrap().to_string();
    (router, ven_a_id)
}

async fn ask(router: &Router, path: &str, body: Value) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    // A refusal has to arrive as a successful "deny": EMQX treats a non-2xx as *its own* error and
    // falls back to its configured default rather than honouring the decision.
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the broker callbacks always answer 200"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn the_broker_authenticates_a_client_against_its_openadr_credential() {
    let (router, _) = broker_harness().await;
    let auth = "/internal/mqtt/auth";

    let ok = ask(
        &router,
        auth,
        json!({ "username": "client-a", "password": VEN_A, "clientid": "mqtt-1" }),
    )
    .await;
    assert_eq!(ok["result"], "allow");

    // A bad token is a bad token on both surfaces.
    let bad = ask(
        &router,
        auth,
        json!({ "username": "client-a", "password": "not-a-token" }),
    )
    .await;
    assert_eq!(bad["result"], "deny");

    // The username has to be the identity the token proved, because the ACL endpoint has nothing
    // else to go on: letting them differ lets a VEN choose which VEN it is.
    let impersonation = ask(
        &router,
        auth,
        json!({ "username": "client-b", "password": VEN_A }),
    )
    .await;
    assert_eq!(impersonation["result"], "deny");
}

#[tokio::test]
async fn a_ven_may_subscribe_only_to_its_own_topics() {
    // The specification's one MUST for messaging notifiers: a VTN must prevent a VEN subscribing to
    // topics that would expose objects it is not authorized to see. Per-VEN topics are only privacy
    // if the broker refuses the other VEN's.
    let (router, ven_a_id) = broker_harness().await;
    let acl = "/internal/mqtt/acl";

    let mine = ask(
        &router,
        acl,
        json!({
            "username": "client-a",
            "action": "subscribe",
            "topic": format!("openadr3/events/vens/{ven_a_id}/create"),
        }),
    )
    .await;
    assert_eq!(mine["result"], "allow", "{mine}");

    let theirs = ask(
        &router,
        acl,
        json!({
            "username": "client-b",
            "action": "subscribe",
            "topic": format!("openadr3/events/vens/{ven_a_id}/create"),
        }),
    )
    .await;
    assert_eq!(
        theirs["result"], "deny",
        "client-b must not read client-a's dispatch: {theirs}"
    );

    // The subscription that would defeat the design in one line.
    let wildcard = ask(
        &router,
        acl,
        json!({
            "username": "client-a",
            "action": "subscribe",
            "topic": "openadr3/events/vens/+/create",
        }),
    )
    .await;
    assert_eq!(wildcard["result"], "deny", "{wildcard}");

    // Publishing is the VTN's alone; a client that could publish could forge a dispatch.
    let publish = ask(
        &router,
        acl,
        json!({
            "username": "client-a",
            "action": "publish",
            "topic": format!("openadr3/events/vens/{ven_a_id}/create"),
        }),
    )
    .await;
    assert_eq!(publish["result"], "deny", "{publish}");
}

#[tokio::test]
async fn collection_topics_need_a_named_business_logic_client() {
    let (router, _) = broker_harness().await;
    let acl = "/internal/mqtt/acl";

    let bl = ask(
        &router,
        acl,
        json!({ "username": "bl", "action": "subscribe", "topic": "openadr3/events/create" }),
    )
    .await;
    assert_eq!(bl["result"], "allow", "{bl}");

    let ven = ask(
        &router,
        acl,
        json!({ "username": "client-a", "action": "subscribe", "topic": "openadr3/events/create" }),
    )
    .await;
    assert_eq!(
        ven["result"], "deny",
        "a collection topic carries every object's full target set: {ven}"
    );
}

#[tokio::test]
async fn a_vtn_with_no_broker_refuses_the_callbacks_outright() {
    let h = Harness::new();
    let refused = ask(
        &h.router,
        "/internal/mqtt/auth",
        json!({ "username": "client-a", "password": VEN_A }),
    )
    .await;
    assert_eq!(refused["result"], "deny");
}

#[tokio::test]
async fn a_cascade_reaches_a_subscriber_watching_only_the_cascaded_type() {
    // The fan-out snapshot is narrowed to the subscriptions that could match, which is a query
    // filter on the object type. Deleting a programme announces its events, its reports and its
    // programme-scoped subscriptions as well as itself, so narrowing to PROGRAM alone would leave
    // out precisely the subscribers those announcements exist for — and nothing would say so.
    let h = Harness::new();
    let (program_id, _) = h.seed().await;

    let (status, body) = h
        .post(
            "/subscriptions",
            VEN_A,
            json!({
                "clientName": "client-a",
                "objectOperations": [{
                    "objects": ["EVENT"],
                    "operations": ["DELETE"],
                    "callbackUrl": "https://a.example.com/hook"
                }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, _, _) = h
        .send(
            Method::DELETE,
            &format!("/programs/{program_id}"),
            Some(BL),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let delivered = h.delivered().await;
    assert!(
        delivered.iter().any(|d| {
            d.notification.object.object_type() == openadr::model::ObjectType::Event
                && d.notification.operation == openadr::model::Operation::Delete
        }),
        "the cascaded event deletion never reached the subscriber watching for it: {:?}",
        delivered
            .iter()
            .map(|d| (
                d.notification.object.object_type(),
                d.notification.operation
            ))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn abandoned_notifications_are_listable_and_revivable() {
    // The other half of the loop `GET /health` starts. A `dead` count is something to alert on;
    // acting on it needs the row, and re-arming it after the receiver is fixed needs an endpoint.
    let h = Harness::new();
    let (program_id, _) = h.seed().await;
    let program_id: openadr::model::ObjectId = program_id.parse().unwrap();

    // The router is mounted at the root, and so are the operational endpoints.
    let list = |token: &'static str| {
        let router = h.router.clone();
        async move {
            let response = router
                .oneshot(
                    Request::builder()
                        .uri("/admin/outbox")
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
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
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };

    // Nothing has been abandoned yet, and a VEN may not look either way.
    let (status, body) = list(BL).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
    let (status, _) = list(VEN_A).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a dead letter names a subscriber's callback URL"
    );

    // Abandon one by hand: the storage layer is where the dispatcher would have put it.
    let delivery = openadr::vtn::notify::Delivery {
        subscription_id: None,
        route: openadr::vtn::notify::Route::Webhook {
            callback_url: "https://gone.example.com/hook".into(),
            bearer_token: None,
        },
        notification: openadr::model::Notification::new(
            openadr::model::Operation::Create,
            openadr::model::notification::AnyObject::Program(
                h.storage.get_program(&program_id).await.unwrap(),
            ),
        ),
    };
    h.storage.enqueue(vec![delivery], now()).await.unwrap();
    let claimed = h
        .storage
        .claim_due(now(), 1, std::time::Duration::from_secs(30), "test")
        .await
        .unwrap();
    h.storage
        .record_failure(
            claimed[0].id,
            &openadr::vtn::notify::DeliveryFailure::permanent("410 Gone"),
            now(),
            &openadr::vtn::store::RetryPolicy::default(),
        )
        .await
        .unwrap();

    let (_, body) = list(BL).await;
    let entries = body.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["destination"], "https://gone.example.com/hook");
    assert_eq!(entries[0]["lastError"], "410 Gone");
    assert_eq!(entries[0]["objectType"], "PROGRAM");

    // Fixed the receiver: put it back in the queue.
    let response = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/admin/outbox/retry")
                .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let revived: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(revived["revived"], 1);

    let (_, body) = list(BL).await;
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn metrics_label_by_matched_route_not_by_request_uri() {
    // A series per event id is an unbounded label set, which turns the endpoint installed to detect
    // a memory leak into one. The matched path is the only bounded thing to key on.
    let h = Harness::new();
    let (_, event_id) = h.seed().await;
    h.get(&format!("/events/{event_id}"), Some(BL)).await;
    h.get("/events/does-not-exist", Some(BL)).await;

    let response = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();

    assert!(
        body.contains("route=\"/openadr3/3.1.0/events/{id}\""),
        "{body}"
    );
    assert!(
        !body.contains(&event_id),
        "an id reached a label, so the series count grows with the data: {body}"
    );
    assert!(body.contains("status=\"200\""));
    assert!(body.contains("status=\"404\""));
    assert!(body.contains("openadr_outbox_pending"));
}

#[tokio::test]
async fn a_vtn_with_no_webhook_transport_says_so_and_refuses_a_subscription() {
    // A subscription is a standing request for notifications, and OpenADR has no way to tell a
    // subscriber that one will never be delivered — it would simply wait. So the VTN says so twice:
    // ahead of time in `GET /notifiers`, and at the only moment the subscriber is listening.
    let storage = MemoryStorage::shared();
    let auth = StaticTokenAuth::new("http://vtn.test/auth/token")
        .with_business_logic(BL, ClientId::new("bl").unwrap())
        .with_ven(VEN_A, ClientId::new("client-a").unwrap());
    // No `.notifier(…)`: the default is an empty set of transports.
    let vtn = Vtn::builder()
        .storage(storage)
        .authenticator(Arc::new(auth))
        .clock(Arc::new(FixedClock::new(now())))
        .config(VtnConfig {
            base_path: "/openadr3/3.1.0".into(),
            ..Default::default()
        })
        .build();
    let router = vtn.router();

    let call = |method: Method, path: &'static str, body: Option<Value>| {
        let router = router.clone();
        async move {
            let mut builder = Request::builder()
                .method(method)
                .uri(format!("/openadr3/3.1.0{path}"))
                .header(header::AUTHORIZATION, format!("Bearer {VEN_A}"));
            let request = match body {
                Some(b) => {
                    builder = builder.header(header::CONTENT_TYPE, "application/json");
                    builder
                        .body(Body::from(serde_json::to_vec(&b).unwrap()))
                        .unwrap()
                }
                None => builder.body(Body::empty()).unwrap(),
            };
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };

    let (status, body) = call(Method::GET, "/notifiers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["WEBHOOK"], false,
        "reporting true here costs a subscriber real notifications: {body}"
    );

    let (status, body) = call(
        Method::POST,
        "/subscriptions",
        Some(json!({
            "clientName": "client-a",
            "objectOperations": [{
                "objects": ["EVENT"],
                "operations": ["CREATE"],
                "callbackUrl": "https://a.example.com/hook"
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
    assert!(
        body["detail"].as_str().unwrap().contains("webhook"),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// Payload and attribute typing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_strict_vtn_refuses_a_bad_payload_on_update_as_well_as_on_create() {
    // `POST /reports` checked its payloads and `PUT /reports/{id}` did not, so the policy had a way
    // round it: file an empty report, then replace it with the values the policy refuses.
    let h = Harness::strict();
    let (program, event) = h.seed().await;
    let _ = program;

    let good = json!({
        "eventID": event,
        "clientName": "ven-a",
        "resources": [{
            "resourceName": "meter",
            "intervals": [{ "id": 0, "payloads": [{ "type": "USAGE", "values": [1.5] }] }]
        }]
    });
    let (status, report) = h.post("/reports", VEN_A, good).await;
    assert_eq!(status, StatusCode::CREATED, "{report}");
    let id = report["id"].as_str().unwrap().to_string();

    // `USAGE` is a number. A string is not one, in either direction.
    let bad = json!({
        "eventID": event,
        "clientName": "ven-a",
        "resources": [{
            "resourceName": "meter",
            "intervals": [{ "id": 0, "payloads": [{ "type": "USAGE", "values": ["lots"] }] }]
        }]
    });
    let (status, body) = h.post("/reports", VEN_A, bad.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = h.put(&format!("/reports/{id}"), VEN_A, bad).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a PUT slipped past the payload policy: {body}"
    );
}

#[tokio::test]
async fn programme_and_resource_attributes_are_checked_against_their_own_enumerations() {
    // `program-attributes.schema.yaml` and `ven-resource-attributes.schema.yaml` are two of the six
    // enumeration files `spec-sync` fetches, and for a long time nothing read either — so every
    // attribute was an unvalidated free string and `RETAILER_NAME` in an event interval was as
    // acceptable as `LOCATION` carrying one coordinate.
    let h = Harness::strict();

    let (status, body) = h
        .post(
            "/programs",
            BL,
            json!({
                "programName": "tou",
                "attributes": [
                    { "type": "RETAILER_NAME", "values": ["Acme Energy"] },
                    { "type": "BINDING_EVENTS", "values": [true] }
                ]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let program = body["id"].as_str().unwrap().to_string();

    // `BINDING_EVENTS` is a boolean.
    let (status, body) = h
        .post(
            "/programs",
            BL,
            json!({
                "programName": "wrong",
                "attributes": [{ "type": "BINDING_EVENTS", "values": ["yes"] }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // And a *report* payload type is not a programme attribute, however well formed.
    let (status, body) = h
        .put(
            &format!("/programs/{program}"),
            BL,
            json!({
                "programName": "tou",
                "attributes": [{ "type": "USAGE", "values": [1] }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // A VEN's own attributes are the other enumeration: two coordinates, both in range.
    let (status, body) = h
        .post(
            "/vens",
            BL,
            json!({
                "objectType": "BL_VEN_REQUEST",
                "clientID": "client-a",
                "venName": "ven-a",
                "attributes": [{ "type": "LOCATION", "values": [4.9, 52.4] }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = h
        .post(
            "/vens",
            BL,
            json!({
                "objectType": "BL_VEN_REQUEST",
                "clientID": "client-b",
                "venName": "ven-b",
                "attributes": [{ "type": "LOCATION", "values": [999] }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_resource_body_naming_the_wrong_client_is_refused_not_ignored() {
    // 3.1.0 required `clientID` on a `BL_RESOURCE_REQUEST`; 3.1.1 removed it because `venID`
    // already determines the owner. Accepting it and never looking at it is the D-045 shape: the
    // caller believes it said something.
    let h = Harness::new();
    h.seed().await;
    let (status, vens) = h.get("/vens?venName=ven-a", Some(BL)).await;
    assert_eq!(status, StatusCode::OK);
    let ven_a = vens[0]["id"].as_str().unwrap().to_string();

    let (status, body) = h
        .post(
            "/resources",
            BL,
            json!({
                "objectType": "BL_RESOURCE_REQUEST",
                "resourceName": "meter",
                "venID": ven_a,
                "clientID": "client-a"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = h
        .post(
            "/resources",
            BL,
            json!({
                "objectType": "BL_RESOURCE_REQUEST",
                "resourceName": "other-meter",
                "venID": ven_a,
                "clientID": "client-b"
            }),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a clientID naming another client was accepted: {body}"
    );
}

#[tokio::test]
async fn a_business_logic_subscriber_hears_about_a_targeted_event() {
    // Business logic reads every object whatever its targets [Def §Object Privacy], and a
    // subscription is a standing read. A utility's own integration subscribing to its own events is
    // the ordinary webhook deployment, and a targeted event is the ordinary event.
    let h = Harness::new();
    let (program_id, _) = h.seed().await;

    let (status, body) = h
        .post(
            "/subscriptions",
            BL,
            json!({
                "clientName": "utility-backend",
                "objectOperations": [{
                    "objects": ["EVENT"],
                    "operations": ["CREATE"],
                    "callbackUrl": "https://backend.example.com/hook",
                }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    h.notifier.clear();

    let (status, _) = h
        .post(
            "/events",
            BL,
            json!({
                "programID": program_id,
                "eventName": "targeted",
                "targets": ["group1", "group2"],
                "intervalPeriod": { "start": "2026-02-11T13:00:00Z", "duration": "PT15M" },
                "intervals": [{ "id": 0, "payloads": [{"type": "SIMPLE", "values": [1]}] }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let delivered = h.delivered().await;
    assert_eq!(
        delivered.len(),
        1,
        "business logic was not told about an event it can read"
    );
    let mut targets = delivered[0].notification.targets.clone();
    targets.sort();
    assert_eq!(
        targets,
        vec!["group1".parse().unwrap(), "group2".parse().unwrap()],
        "business logic saw a narrowed target set; target hiding is a VEN's rule"
    );
}

#[tokio::test]
async fn the_subscriber_health_view_is_business_logics() {
    // A row names a subscription id and the error that subscriber's endpoint returned. Both are a
    // subscriber's business and nobody else's, so the view carries the same scope as the
    // dead-letter listing beside it.
    let h = Harness::new();

    use axum::http::{Method, Request};
    use tower::ServiceExt;
    let probe = |token: &'static str| {
        let router = h.router.clone();
        async move {
            let response = router
                .oneshot(
                    Request::builder()
                        .method(Method::GET)
                        .uri("/admin/subscribers")
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            response.status()
        }
    };
    assert_eq!(probe(BL).await, StatusCode::OK);
    assert_eq!(
        probe(VEN_A).await,
        StatusCode::FORBIDDEN,
        "a VEN could read which subscribers are failing, and what their endpoints answered"
    );
}

#[tokio::test]
async fn only_the_vtns_own_publisher_may_publish() {
    // A client that could publish could forge a dispatch instruction, which is the worst thing the
    // broker callbacks can be talked into. But a broker that authenticates through
    // `/internal/mqtt/auth` authenticates the VTN too — the fan-out is a client of its own broker —
    // so a blanket refusal locks the VTN out and it reconnects for ever against `NotAuthorized`.
    // Found by running a real EMQX against a real VTN, which is the only way it could have been.
    let (router, ven_a_id) = broker_harness().await;
    let acl = "/internal/mqtt/acl";

    // The configured publisher, under this VTN's own prefix.
    let vtn = ask(
        &router,
        acl,
        json!({
            "username": "bl",
            "action": "publish",
            "topic": format!("openadr3/events/vens/{ven_a_id}/create"),
        }),
    )
    .await;
    assert_eq!(vtn["result"], "allow", "{vtn}");

    // The same identity, on a shared broker, reaching for another deployment's topics.
    let elsewhere = ask(
        &router,
        acl,
        json!({
            "username": "bl",
            "action": "publish",
            "topic": "someone-elses-vtn/events/vens/ven-1/create",
        }),
    )
    .await;
    assert_eq!(elsewhere["result"], "deny", "{elsewhere}");

    // And a VEN, on the topic it is entitled to *read*.
    let ven = ask(
        &router,
        acl,
        json!({
            "username": "client-a",
            "action": "publish",
            "topic": format!("openadr3/events/vens/{ven_a_id}/create"),
        }),
    )
    .await;
    assert_eq!(
        ven["result"], "deny",
        "a VEN could publish a forged dispatch instruction to its own topic: {ven}"
    );
}

#[tokio::test]
async fn without_a_configured_publisher_nothing_may_publish() {
    // The default. A broker that authenticates the VTN by its own user database or by mutual TLS
    // never asks this endpoint about the VTN, and the blanket refusal is right there.
    let storage = MemoryStorage::shared();
    let vtn = Vtn::builder()
        .storage(storage)
        .authenticator(Arc::new(
            StaticTokenAuth::new("http://vtn.test/auth/token")
                .with_business_logic(BL, ClientId::new("bl").unwrap()),
        ))
        .config(VtnConfig {
            base_path: "/openadr3/3.1.0".into(),
            mqtt_topic_prefix: "openadr3".into(),
            ..Default::default()
        })
        .mqtt(openadr::model::MqttNotifierBinding {
            uris: vec!["mqtts://broker.test:8883".into()],
            serialization: openadr::model::Serialization::Json,
            authentication: openadr::model::MqttAuthentication::Anonymous,
        })
        .build();

    let refused = ask(
        &vtn.router(),
        "/internal/mqtt/acl",
        json!({ "username": "bl", "action": "publish", "topic": "openadr3/events/create" }),
    )
    .await;
    assert_eq!(refused["result"], "deny", "{refused}");
}
