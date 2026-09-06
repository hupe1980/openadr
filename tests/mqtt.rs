//! The MQTT publisher, over a real socket, against a real broker.
//!
//! The broker is at the bottom of this file rather than in a container, because the thing under test
//! is what the VTN puts on the wire: which topics it publishes to, and what each copy contains. A
//! mock notifier proves neither — the fan-out was computed correctly and published nowhere for as
//! long as this crate has had topic endpoints.
//!
//! Two claims carry the file, one per privacy gate.
//!
//! * **Targeting.** An event targeted at `group1` must reach `ven-a`'s private topic carrying only
//!   `group1`, and must not appear on `ven-b`'s at all. That is a competitor's dispatch schedule,
//!   and it is the claim other implementations' conformance suites skip.
//! * **Ownership.** A report, and a resource, must reach their owner's topic and no other VEN's.
//!   That half also holds the *narrowed* fan-out snapshot in place: an owned write no longer sweeps
//!   the fleet to address one topic, and a snapshot narrowed to the wrong VEN would compute the
//!   same fan-out and publish it nowhere (D-124).

#![cfg(all(feature = "vtn", feature = "mqtt"))]

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use openadr::{
    core::FixedClock,
    model::{ClientId, MqttAuthentication, MqttNotifierBinding, Serialization, Timestamp},
    vtn::{
        Vtn, VtnConfig,
        auth::StaticTokenAuth,
        notify::{MqttConfig, MqttNotifier, Notifiers},
        store::MemoryStorage,
    },
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

const BL: &str = "bl-secret";
/// A VEN credential, so a report can be filed by the client that owns it.
const VEN_A: &str = "ven-a-secret";
const PREFIX: &str = "openadr3";

fn now() -> Timestamp {
    "2026-02-11T06:00:00Z".parse().unwrap()
}

/// One message the broker received.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Published {
    topic: String,
    payload: Vec<u8>,
    qos: u8,
    retain: bool,
}

#[tokio::test]
async fn a_targeted_event_reaches_only_the_entitled_vens_private_topic() {
    let broker = Broker::start().await;
    let (vtn, router) = vtn_with(&broker);

    let program_id = post(&router, "/programs", json!({ "programName": "tou" })).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut ven_ids = Vec::new();
    for (client, name, target) in [
        ("client-a", "ven-a", "group1"),
        ("client-b", "ven-b", "group2"),
    ] {
        let ven = post(
            &router,
            "/vens",
            json!({
                "objectType": "BL_VEN_REQUEST",
                "clientID": client,
                "venName": name,
                "targets": [target],
            }),
        )
        .await;
        ven_ids.push(ven["id"].as_str().unwrap().to_string());
    }
    let (ven_a, ven_b) = (&ven_ids[0], &ven_ids[1]);

    let event = post(
        &router,
        "/events",
        json!({
            "programID": program_id,
            "eventName": "curtailment",
            "targets": ["group1"],
            "intervalPeriod": { "start": "2026-02-11T12:00:00Z", "duration": "PT15M" },
            "intervals": [
                { "id": 0, "payloads": [{ "type": "IMPORT_CAPACITY_LIMIT", "values": [60] }] }
            ]
        }),
    )
    .await;
    let event_id = event["id"].as_str().unwrap();

    // The queue is drained explicitly, so the assertions below are exact rather than timed.
    vtn.dispatcher().drain().await;

    let published = broker.published();
    let topics: Vec<&str> = published.iter().map(|p| p.topic.as_str()).collect();

    // The collection topic and the programme-scoped one, both business logic's.
    assert!(
        topics.contains(&format!("{PREFIX}/events/create").as_str()),
        "{topics:?}"
    );
    assert!(
        topics.contains(&format!("{PREFIX}/events/programs/{program_id}/create").as_str()),
        "{topics:?}"
    );

    // The privacy claim, in both directions.
    let a_topic = format!("{PREFIX}/events/vens/{ven_a}/create");
    let b_topic = format!("{PREFIX}/events/vens/{ven_b}/create");
    assert!(
        topics.contains(&a_topic.as_str()),
        "ven-a is in group1 and must be told: {topics:?}"
    );
    assert!(
        !topics.contains(&b_topic.as_str()),
        "ven-b is in group2 and must not learn that this event exists: {topics:?}"
    );

    // And the copy ven-a receives names only ven-a's own target, never the event's full set.
    let to_a = published.iter().find(|p| p.topic == a_topic).unwrap();
    let body: Value = serde_json::from_slice(&to_a.payload).unwrap();
    assert_eq!(body["operation"], "CREATE");
    assert_eq!(body["objectType"], "EVENT");
    assert_eq!(body["object"]["id"], event_id);
    assert_eq!(body["targets"], json!(["group1"]));

    // QoS 1, so the outbox entry was removed only after the broker acknowledged it; unretained,
    // because a retained delete on a per-VEN topic outlives the grant that put it there.
    assert_eq!(to_a.qos, 1);
    assert!(!to_a.retain);
}

/// A report reaches its owner's private topic, and nobody else's.
///
/// The claim is object privacy on the push path for an *owned* object, and it is asserted over the
/// wire because that is the only place it is true or false. It also guards the narrowed fan-out
/// snapshot: `POST /reports` is the highest-rate write in the system, and it no longer sweeps every
/// VEN's grant to publish to one topic (D-124). A snapshot narrowed to the wrong VEN, or to none,
/// would compute the same fan-out and publish it nowhere — which is exactly the failure D-056
/// exists for and exactly what a mock notifier cannot see.
#[tokio::test]
async fn a_report_reaches_only_its_owners_private_topic() {
    let broker = Broker::start().await;
    let (vtn, router) = vtn_with(&broker);

    let program_id = post(&router, "/programs", json!({ "programName": "tou" })).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut ven_ids = Vec::new();
    for (client, name) in [("client-a", "ven-a"), ("client-b", "ven-b")] {
        let ven = post(
            &router,
            "/vens",
            json!({
                "objectType": "BL_VEN_REQUEST",
                "clientID": client,
                "venName": name,
            }),
        )
        .await;
        ven_ids.push(ven["id"].as_str().unwrap().to_string());
    }
    let (ven_a, ven_b) = (&ven_ids[0], &ven_ids[1]);

    let event_id = post(
        &router,
        "/events",
        json!({
            "programID": program_id,
            "intervalPeriod": { "start": "2026-02-11T12:00:00Z", "duration": "PT15M" },
            "intervals": [
                { "id": 0, "payloads": [{ "type": "IMPORT_CAPACITY_LIMIT", "values": [60] }] }
            ]
        }),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Filed by the VEN that owns it, which is the only way a report acquires a `clientID`.
    post_as(
        &router,
        VEN_A,
        "/reports",
        json!({
            "programID": program_id,
            "eventID": event_id,
            "clientName": "ven-a",
            "resources": [{
                "resourceName": "meter",
                "intervals": [{ "id": 0, "payloads": [{ "type": "USAGE", "values": [1.5] }] }],
            }],
        }),
    )
    .await;
    vtn.dispatcher().drain().await;

    let topics: Vec<String> = broker.published().into_iter().map(|p| p.topic).collect();
    let a_topic = format!("{PREFIX}/reports/vens/{ven_a}/create");
    let b_topic = format!("{PREFIX}/reports/vens/{ven_b}/create");
    assert!(
        topics.contains(&a_topic),
        "the report never reached its owner's topic ({a_topic}); published: {topics:?}"
    );
    assert!(
        !topics.contains(&b_topic),
        "ven-b received ven-a's meter data: {topics:?}"
    );
    // And the topic the VTN published to is the one it tells that VEN to watch.
    let advertised = get(
        &router,
        &format!("/notifiers/mqtt/topics/vens/{ven_a}/reports"),
    )
    .await;
    assert_eq!(
        advertised["topics"]["CREATE"].as_str(),
        Some(a_topic.as_str())
    );

    // A resource is the same gate reached the long way round: it is owned through its VEN rather
    // than by naming a client, so its snapshot resolves the parent. That extra lookup is the one
    // place the owned fan-out can silently come back empty, and an empty one publishes nowhere.
    post_as(
        &router,
        VEN_A,
        "/resources",
        json!({ "objectType": "VEN_RESOURCE_REQUEST", "resourceName": "meter-1" }),
    )
    .await;
    vtn.dispatcher().drain().await;

    let topics: Vec<String> = broker.published().into_iter().map(|p| p.topic).collect();
    let resource_topic = format!("{PREFIX}/resources/vens/{ven_a}/create");
    assert!(
        topics.contains(&resource_topic),
        "the resource never reached its VEN's topic ({resource_topic}); published: {topics:?}"
    );
    assert!(
        !topics.contains(&format!("{PREFIX}/resources/vens/{ven_b}/create")),
        "ven-b was told about ven-a's resource: {topics:?}"
    );
}

#[tokio::test]
async fn the_topic_a_ven_is_told_to_watch_is_the_topic_the_vtn_publishes_to() {
    // Two computations of one name. `GET /notifiers/mqtt/topics/vens/{id}` hands out a topic and
    // the fan-out publishes to one, and for the VEN object they were built separately and came out
    // different — `vens/{venID}` against `vens/vens/{venID}`. Every VEN subscribed correctly,
    // received nothing for ever, and no error was raised anywhere.
    //
    // A report and a subscription are owned objects too, and their per-VEN topics have to be
    // discoverable for the same reason.
    let broker = Broker::start().await;
    let (vtn, router) = vtn_with(&broker);

    let ven_a = post(
        &router,
        "/vens",
        json!({ "objectType": "BL_VEN_REQUEST", "clientID": "client-a", "venName": "ven-a" }),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    post(
        &router,
        "/vens",
        json!({ "objectType": "BL_VEN_REQUEST", "clientID": "client-b", "venName": "ven-b" }),
    )
    .await;

    // Updating ven-a is the operation its own topic exists for.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/openadr3/3.1.0/vens/{ven_a}"))
                .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "objectType": "BL_VEN_REQUEST",
                        "clientID": "client-a",
                        "venName": "ven-a-renamed",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    vtn.dispatcher().drain().await;

    let advertised = get(&router, &format!("/notifiers/mqtt/topics/vens/{ven_a}")).await;
    let update_topic = advertised["topics"]["UPDATE"].as_str().unwrap().to_string();
    assert_eq!(update_topic, format!("{PREFIX}/vens/{ven_a}/update"));

    let topics: Vec<String> = broker.published().into_iter().map(|p| p.topic).collect();
    assert!(
        topics.contains(&update_topic),
        "the VTN published to {topics:?}, none of which is the topic it told ven-a to watch \
         ({update_topic})"
    );

    // The two extension endpoints, for the same reason: the fan-out publishes a VEN's own reports
    // and subscriptions to these names, so the names have to be discoverable.
    for (path, collection) in [("reports", "reports"), ("subscriptions", "subscriptions")] {
        let response = get(
            &router,
            &format!("/notifiers/mqtt/topics/vens/{ven_a}/{path}"),
        )
        .await;
        assert_eq!(
            response["topics"]["CREATE"].as_str().unwrap(),
            format!("{PREFIX}/{collection}/vens/{ven_a}/create")
        );
    }
}

/// The VEN's half: it discovers its own topics, subscribes, and wakes when the VTN publishes.
///
/// The two ends have been separately correct before and still failed to meet — a topic name is a
/// string computed twice (D-059), and no test of either half can see that. This one runs the real
/// publisher and the real subscriber against a broker that actually routes, so the assertion is
/// that a `PUT` on one side shortens a sleep on the other.
#[cfg(feature = "ven")]
#[tokio::test]
async fn a_ven_wakes_when_the_vtn_publishes_to_the_topic_it_was_told_to_watch() {
    use openadr::client::{Client, VirtualEndNode};
    use openadr::ven::{MqttPush, MqttPushConfig, VenConfig, VenRuntime};

    let broker = Broker::start().await;
    let (vtn, router) = vtn_with(&broker);

    // Serve the router on a real socket: the VEN discovers its topics over HTTP like any client.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = router.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });
    let base = format!("http://{addr}/openadr3/3.1.0");

    let ven_id = post(
        &router,
        "/vens",
        json!({ "objectType": "BL_VEN_REQUEST", "clientID": "bl", "venName": "ven-push" }),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let client = Client::<VirtualEndNode>::builder(&base)
        .unwrap()
        .bearer_token(BL)
        .build()
        .unwrap();
    let ven = VenRuntime::new(client.clone(), VenConfig::new("ven-push".parse().unwrap()))
        .with_clock(Arc::new(FixedClock::new(now())));
    let waker = ven.waker();

    let push = MqttPush::start(
        &client,
        &ven_id.parse().unwrap(),
        waker,
        MqttPushConfig::default(),
    )
    .await
    .expect("the VTN advertises a broker")
    .expect("MQTT is on");

    // Exactly the names the VTN advertised, not names the subscriber built for itself.
    let advertised = get(&router, &format!("/notifiers/mqtt/topics/vens/{ven_id}")).await;
    let all = advertised["topics"]["ALL"].as_str().unwrap();
    assert!(
        push.topics().iter().any(|t| t == all),
        "the VEN subscribed to {:?}, which does not include the topic the VTN advertised ({all})",
        push.topics()
    );

    // Let the subscription reach the broker before anything is published.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // Drain the hint the connection itself queued. A subscriber that has just connected may have
    // missed messages the broker did not hold, so it asks for one sync on `CONNACK` — and without
    // draining it here the assertion below would be satisfied by that, and would pass against a
    // VTN publishing to a topic nobody watches.
    let ven = Arc::new(ven);
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), ven.wait_for_work()).await;

    // A sleep that would otherwise last the whole poll interval.
    let waiting = tokio::spawn({
        let ven = ven.clone();
        async move { tokio::time::timeout(std::time::Duration::from_secs(5), ven.wait_for_work()).await }
    });
    // And give it a moment to actually be waiting.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/openadr3/3.1.0/vens/{ven_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {BL}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "objectType": "BL_VEN_REQUEST",
                        "clientID": "bl",
                        "venName": "ven-push-renamed",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    vtn.dispatcher().drain().await;

    waiting
        .await
        .unwrap()
        .expect("the VEN slept through a notification published to the topic it subscribed to");
}

/// The VEN reaches the broker as its own `clientID`, carrying its access token as the password.
///
/// `[Notifiers §12.2]` names both halves, and the VTN's ACL callback derives everything from the
/// *username*: `POST /internal/mqtt/acl` has a username and a topic and no token, so a VEN that
/// connects under the wrong name is a VEN the broker will refuse — silently, in a reconnect loop.
///
/// The `{clientID}` placeholder is the part with two possible implementations. It lived in
/// `MqttAuthentication::resolve_username`, which had a unit test and no caller, while the VEN's
/// subscriber wrote the rule out again inline. Nothing observed which of the two reached a socket
/// until this test did.
#[cfg(all(feature = "ven", feature = "internal-auth"))]
#[tokio::test]
async fn the_ven_connects_as_its_own_client_id_with_its_token_as_the_password() {
    use openadr::client::{Client, Credentials as ClientCredentials, VirtualEndNode};
    use openadr::vtn::auth::{InternalAuth, Scope, Scopes};
    use openadr::{
        ven::{MqttPush, MqttPushConfig},
        vtn::notify::{MqttConfig, MqttNotifier, Notifiers},
    };

    const VEN_CLIENT: &str = "ven-client-7";
    const VEN_SECRET: &str = "ven-secret";

    let broker = Broker::start().await;
    // Bind first: `/auth/server` advertises the token endpoint, and a client discovers it there —
    // so the URL has to be the one this VTN is actually reachable at.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}/openadr3/3.1.0");

    let auth = InternalAuth::builder(format!("{base}/auth/token"))
        .client("bl", "bl-secret", Scopes::new(Scope::BUSINESS_LOGIC))
        .unwrap()
        .client(VEN_CLIENT, VEN_SECRET, Scopes::new(Scope::VEN))
        .unwrap()
        .build();

    let vtn = Vtn::builder()
        .storage(MemoryStorage::shared())
        .authenticator(Arc::new(auth))
        .config(VtnConfig {
            base_path: "/openadr3/3.1.0".into(),
            mqtt_topic_prefix: PREFIX.into(),
            ..Default::default()
        })
        .mqtt(MqttNotifierBinding {
            uris: vec![broker.url()],
            serialization: Serialization::Json,
            // The placeholder, which is the whole point.
            authentication: MqttAuthentication::Oauth2BearerToken {
                username: MqttAuthentication::CLIENT_ID_PLACEHOLDER.into(),
            },
        })
        .notifier(
            Notifiers::new()
                .with(MqttNotifier::connect(MqttConfig::new(broker.url())).unwrap())
                .shared(),
        )
        .build();

    let router = vtn.router();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    // Business logic creates the VEN object the subscriber's topics are named after.
    let bl = Client::<openadr::client::BusinessLogic>::builder(&base)
        .unwrap()
        .credentials(ClientCredentials::new("bl", "bl-secret"))
        .build()
        .unwrap();
    let ven_object = bl
        .vens()
        .create(&openadr::model::VenRequest::Bl(
            openadr::model::BlVenRequest {
                client_id: VEN_CLIENT.parse().unwrap(),
                ven_name: "ven-7".parse().unwrap(),
                targets: Vec::new(),
                attributes: None,
            },
        ))
        .await
        .unwrap();

    // The VEN exchanges its own credentials, so it knows its clientID — which a pre-shared token
    // would not tell it, and which the placeholder therefore cannot be resolved without.
    let ven_client = Client::<VirtualEndNode>::builder(&base)
        .unwrap()
        .credentials(ClientCredentials::new(VEN_CLIENT, VEN_SECRET))
        .build()
        .unwrap();

    let waker = openadr::ven::VenRuntime::new(
        ven_client.clone(),
        openadr::ven::VenConfig::new("ven-7".parse().unwrap()),
    )
    .waker();

    let _push = MqttPush::start(
        &ven_client,
        &ven_object.id,
        waker,
        MqttPushConfig::default(),
    )
    .await
    .expect("the VTN advertises a broker")
    .expect("MQTT is on");

    // Give the CONNECT time to reach the broker.
    for _ in 0..40 {
        if broker.connections().iter().any(|c| c.username.is_some()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let ven_connection = broker
        .connections()
        .into_iter()
        .find(|c| c.username.is_some())
        .expect("the VEN never presented credentials to the broker");
    assert_eq!(
        ven_connection.username.as_deref(),
        Some(VEN_CLIENT),
        "the VEN connected as {:?}; the ACL callback would refuse every topic it asked for",
        ven_connection.username
    );
    let password = ven_connection.password.expect("no password was presented");
    assert_eq!(
        password.split('.').count(),
        3,
        "the password should be the OpenADR access token, and this one is not a JWT: {password:?}"
    );
    // And it is a *working* token, not merely a JWT-shaped string.
    assert_eq!(
        ven_client.access_token().await.unwrap().as_deref(),
        Some(password.as_str()),
        "the broker password is not the token this client authenticates to the VTN with"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A VTN publishing to `broker`, with the topic prefix these tests assert on.
fn vtn_with(broker: &Broker) -> (Vtn, Router) {
    let vtn = Vtn::builder()
        .storage(MemoryStorage::shared())
        .authenticator(Arc::new(
            StaticTokenAuth::new("http://vtn.test/auth/token")
                .with_business_logic(BL, ClientId::new("bl").unwrap())
                .with_ven(VEN_A, ClientId::new("client-a").unwrap()),
        ))
        .clock(Arc::new(FixedClock::new(now())))
        .config(VtnConfig {
            base_path: "/openadr3/3.1.0".into(),
            mqtt_topic_prefix: PREFIX.into(),
            ..Default::default()
        })
        .mqtt(MqttNotifierBinding {
            uris: vec![broker.url()],
            serialization: Serialization::Json,
            authentication: MqttAuthentication::Anonymous,
        })
        .notifier(
            Notifiers::new()
                .with(
                    MqttNotifier::connect(MqttConfig::new(broker.url()))
                        .expect("the broker URL is valid"),
                )
                .shared(),
        )
        .build();
    let router = vtn.router();
    (vtn, router)
}

async fn get(router: &Router, path: &str) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/openadr3/3.1.0{path}"))
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
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{path}: {value}");
    value
}

async fn post(router: &Router, path: &str, body: Value) -> Value {
    post_as(router, BL, path, body).await
}

async fn post_as(router: &Router, token: &str, path: &str, body: Value) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/openadr3/3.1.0{path}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::CREATED, "{path}: {value}");
    value
}

/// A minimal MQTT 3.1.1 broker: `CONNECT`, `SUBSCRIBE`, `PUBLISH` at QoS 1, and `PINGREQ`.
///
/// It records what it received *and routes it*, matching each publish against every subscriber's
/// filters. The routing is not a nicety: a broker that only acknowledges can say what the VTN
/// published, and cannot say whether the topic a VEN was told to watch is the topic the VTN
/// publishes to. That pair was computed in two places and came out different — `vens/{venID}`
/// against `vens/vens/{venID}` — so every VEN subscribed correctly, received nothing for ever, and
/// no error was raised anywhere (D-084).
///
/// A container would answer the same questions. This is a few hundred lines of packet framing that
/// starts in a millisecond and needs no Docker, and `tests/broker.rs` runs the same claims against a
/// real EMQX from `deploy/compose.yaml` for the ones a toy cannot settle — an ACL that refuses.
struct Broker {
    addr: std::net::SocketAddr,
    received: Arc<Mutex<Vec<Published>>>,
    /// The credentials every CONNECT carried, in arrival order.
    ///
    /// `[Notifiers §12.2]` says the access token travels as the MQTT *password* and the identity as
    /// the *username* — and the VTN's ACL callback derives what may be subscribed to from that
    /// username alone. So the username reaching the broker is not a detail: it is the whole input
    /// to the authorization decision, and nothing observed it until this recorded it.
    connections: Arc<Mutex<Vec<Credentials>>>,
}

/// What one CONNECT presented.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Credentials {
    client_id: String,
    username: Option<String>,
    password: Option<String>,
}

/// Every live subscription, so a publish can be delivered rather than merely counted.
///
/// This is the part that makes the test worth running. A broker that accepts a `PUBLISH` and a
/// `SUBSCRIBE` and never connects the two cannot tell whether the topic the VTN publishes to is the
/// topic the VEN asked for — which is the bug D-059 was, and the only way to find it is to route.
type Subscribers = Arc<Mutex<Vec<Subscriber>>>;

struct Subscriber {
    filters: Vec<String>,
    outbound: tokio::sync::mpsc::UnboundedSender<(String, Vec<u8>)>,
}

impl Broker {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let sink = received.clone();
        let seen = connections.clone();
        let subs = subscribers.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let sink = sink.clone();
                let seen = seen.clone();
                let subs = subs.clone();
                tokio::spawn(async move {
                    let _ = serve(socket, sink, seen, subs).await;
                });
            }
        });
        Self {
            addr,
            received,
            connections,
        }
    }

    fn url(&self) -> String {
        format!("mqtt://{}", self.addr)
    }

    fn published(&self) -> Vec<Published> {
        self.received.lock().unwrap().clone()
    }

    fn connections(&self) -> Vec<Credentials> {
        self.connections.lock().unwrap().clone()
    }
}

/// MQTT topic-filter matching: `+` is one level, `#` is the rest.
fn matches(filter: &str, topic: &str) -> bool {
    let mut f = filter.split('/');
    let mut t = topic.split('/');
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => continue,
            (Some(a), Some(b)) if a == b => continue,
            (None, None) => return true,
            _ => return false,
        }
    }
}

async fn serve(
    socket: tokio::net::TcpStream,
    sink: Arc<Mutex<Vec<Published>>>,
    seen: Arc<Mutex<Vec<Credentials>>>,
    subscribers: Subscribers,
) -> std::io::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(socket);
    let (outbound, mut inbox) = tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();
    // What this connection has asked for. Shared with the read half, which appends on SUBSCRIBE.
    let mine = Arc::new(Mutex::new(Vec::<String>::new()));

    {
        let mut all = subscribers.lock().unwrap();
        all.push(Subscriber {
            filters: Vec::new(),
            outbound: outbound.clone(),
        });
    }
    let slot = subscribers.lock().unwrap().len() - 1;

    // The write half: control responses go through the same channel as delivered publishes, so
    // the two never interleave mid-packet.
    let (control, mut control_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(bytes) = control_rx.recv() => {
                    if writer.write_all(&bytes).await.is_err() {
                        return;
                    }
                }
                Some((topic, payload)) = inbox.recv() => {
                    let mut packet = vec![0x30]; // PUBLISH, QoS 0
                    let mut body = Vec::new();
                    body.extend_from_slice(&(topic.len() as u16).to_be_bytes());
                    body.extend_from_slice(topic.as_bytes());
                    body.extend_from_slice(&payload);
                    write_remaining_length(&mut packet, body.len());
                    packet.extend_from_slice(&body);
                    if writer.write_all(&packet).await.is_err() {
                        return;
                    }
                }
                else => return,
            }
            let _ = writer.flush().await;
        }
    });

    loop {
        let mut first = [0u8; 1];
        if reader.read_exact(&mut first).await.is_err() {
            return Ok(()); // client went away
        }
        let packet_type = first[0] >> 4;
        let flags = first[0] & 0x0f;
        let length = read_remaining_length(&mut reader).await?;
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).await?;

        match packet_type {
            // CONNECT → CONNACK, session-present 0, return code 0 (accepted).
            1 => {
                if let Some(credentials) = parse_connect(&body) {
                    seen.lock().unwrap().push(credentials);
                }
                let _ = control.send(vec![0x20, 0x02, 0x00, 0x00]);
            }
            // PUBLISH. Topic, then a packet identifier when QoS > 0, then the payload.
            3 => {
                let qos = (flags >> 1) & 0x03;
                let retain = flags & 0x01 == 1;
                let topic_len = u16::from_be_bytes([body[0], body[1]]) as usize;
                let topic = String::from_utf8_lossy(&body[2..2 + topic_len]).into_owned();
                let mut cursor = 2 + topic_len;
                let packet_id = if qos > 0 {
                    let id = u16::from_be_bytes([body[cursor], body[cursor + 1]]);
                    cursor += 2;
                    Some(id)
                } else {
                    None
                };
                let payload = body[cursor..].to_vec();
                sink.lock().unwrap().push(Published {
                    topic: topic.clone(),
                    payload: payload.clone(),
                    qos,
                    retain,
                });
                // Deliver it. This is the whole point of the routing broker.
                for subscriber in subscribers.lock().unwrap().iter() {
                    if subscriber.filters.iter().any(|f| matches(f, &topic)) {
                        let _ = subscriber.outbound.send((topic.clone(), payload.clone()));
                    }
                }
                if let Some(id) = packet_id {
                    let [hi, lo] = id.to_be_bytes();
                    let _ = control.send(vec![0x40, 0x02, hi, lo]);
                }
            }
            // SUBSCRIBE → SUBACK. The filters are (topic, qos) pairs after the packet identifier.
            8 => {
                let [hi, lo] = [body[0], body[1]];
                let mut cursor = 2;
                let mut granted = Vec::new();
                while cursor + 2 <= body.len() {
                    let len = u16::from_be_bytes([body[cursor], body[cursor + 1]]) as usize;
                    cursor += 2;
                    let filter = String::from_utf8_lossy(&body[cursor..cursor + len]).into_owned();
                    cursor += len + 1; // the requested QoS byte
                    mine.lock().unwrap().push(filter.clone());
                    granted.push(0x00u8);
                }
                if let Some(entry) = subscribers.lock().unwrap().get_mut(slot) {
                    entry.filters = mine.lock().unwrap().clone();
                }
                let mut suback = vec![0x90, (2 + granted.len()) as u8, hi, lo];
                suback.extend_from_slice(&granted);
                let _ = control.send(suback);
            }
            // PINGREQ → PINGRESP.
            12 => {
                let _ = control.send(vec![0xd0, 0x00]);
            }
            // DISCONNECT.
            14 => return Ok(()),
            _ => {}
        }
    }
}

/// Read the client id, username and password out of a CONNECT payload (MQTT 3.1.1 §3.1).
///
/// Variable header: protocol name, level, connect flags, keep-alive. Then the payload, in flag
/// order: client id, will topic and message, username, password — each a length-prefixed string.
fn parse_connect(body: &[u8]) -> Option<Credentials> {
    let name_len = u16::from_be_bytes([*body.first()?, *body.get(1)?]) as usize;
    // protocol name, protocol level, connect flags, keep-alive
    let mut cursor = 2 + name_len + 1;
    let flags = *body.get(cursor)?;
    cursor += 1 + 2;

    let field = |cursor: &mut usize| -> Option<String> {
        let len = u16::from_be_bytes([*body.get(*cursor)?, *body.get(*cursor + 1)?]) as usize;
        *cursor += 2;
        let value = String::from_utf8_lossy(body.get(*cursor..*cursor + len)?).into_owned();
        *cursor += len;
        Some(value)
    };

    let client_id = field(&mut cursor)?;
    if flags & 0x04 != 0 {
        field(&mut cursor)?; // will topic
        field(&mut cursor)?; // will message
    }
    let username = (flags & 0x80 != 0).then(|| field(&mut cursor)).flatten();
    let password = (flags & 0x40 != 0).then(|| field(&mut cursor)).flatten();
    Some(Credentials {
        client_id,
        username,
        password,
    })
}

/// MQTT's variable-length integer, on the way out.
fn write_remaining_length(out: &mut Vec<u8>, mut value: usize) {
    loop {
        let mut byte = (value % 128) as u8;
        value /= 128;
        if value > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return;
        }
    }
}

/// MQTT's variable-length integer: seven bits per byte, high bit continues.
async fn read_remaining_length<R: tokio::io::AsyncRead + Unpin>(
    socket: &mut R,
) -> std::io::Result<usize> {
    let mut value = 0usize;
    let mut multiplier = 1usize;
    loop {
        let mut byte = [0u8; 1];
        socket.read_exact(&mut byte).await?;
        value += usize::from(byte[0] & 0x7f) * multiplier;
        if byte[0] & 0x80 == 0 {
            return Ok(value);
        }
        multiplier *= 128;
        if multiplier > 128 * 128 * 128 {
            return Err(std::io::Error::other("malformed remaining length"));
        }
    }
}
