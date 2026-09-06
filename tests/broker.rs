//! Object privacy on a real broker, asserted rather than described.
//!
//! `deploy/` is not documentation *about* a broker; it is a configuration a broker has to accept.
//! So this brings up `deploy/compose.yaml` — the shipped image, the shipped `emqx.conf`, EMQX — and
//! puts the four claims `deploy/README.md` makes to a real MQTT client.
//!
//! ```console
//! $ cargo test --all-features --test broker -- --ignored --nocapture
//! ```
//!
//! `#[ignore]`d: it builds a container image. `OPENADR_VTN_URL` and `OPENADR_BROKER` point it at a
//! stack you already have.
//!
//! Every wait is on an **event**, never a clock: a sleep long enough to be reliable is also long
//! enough to hide a message that arrived late.

#![cfg(all(feature = "vtn", feature = "mqtt", feature = "client"))]

use std::time::Duration;

use openadr::{
    client::{BusinessLogic, Client},
    model::{BlVenRequest, EventRequest, Interval, IntervalPeriod, ProgramRequest, VenRequest},
};
use rumqttc::{AsyncClient, ConnectionError, Event, MqttOptions, Packet, QoS};
use testcontainers::compose::DockerCompose;

const BL_TOKEN: &str = "bl-secret";
const VEN_TOKEN: &str = "ven-secret";
/// The `clientID` `--ven-token` authenticates as, and so the MQTT username a VEN connects with.
const VEN_CLIENT: &str = "ven";
const PREFIX: &str = "openadr3/3.1.0";

/// The stack under test: either one this brought up, or one already running.
struct Stack {
    /// Held so the containers outlive the test. `None` when somebody else owns them.
    _compose: Option<DockerCompose>,
    vtn: String,
    broker_host: String,
    broker_port: u16,
}

/// Whether there is a Docker daemon to talk to.
///
/// Asked separately so the two outcomes stay distinguishable: no Docker is a legitimate skip, and a
/// compose file that will not build is a failure.
fn docker_is_available() -> bool {
    std::process::Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

impl Stack {
    async fn start() -> Option<Self> {
        if let (Ok(vtn), Ok(broker)) = (
            std::env::var("OPENADR_VTN_URL"),
            std::env::var("OPENADR_BROKER"),
        ) {
            let (host, port) = broker.rsplit_once(':')?;
            let stack = Self {
                _compose: None,
                vtn,
                broker_host: host.to_string(),
                broker_port: port.parse().ok()?,
            };
            // The same readiness gate as the compose path: a stack somebody else started is not
            // necessarily one whose broker can reach its VTN yet.
            return stack.wait_until_ready().await.then_some(stack);
        }

        if !docker_is_available() {
            eprintln!("skipping: no Docker");
            return None;
        }

        // The shipped file, built and run as shipped. Re-creating the wiring here would test a
        // wiring nobody deploys.
        //
        // Absolute: the local compose client runs `docker compose` from the file's own directory
        // *and* passes the path it was given, so a relative one resolves twice. The directory has to
        // be `deploy/` either way, because the file's build context is `..`.
        const COMPOSE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/deploy/compose.yaml");
        let mut compose = DockerCompose::with_local_client(&[COMPOSE])
            .with_build(true)
            .with_wait(true);
        // Not a skip. Docker is here, so a compose file that will not come up is this repository's
        // problem — and a green test that quietly built nothing is how `deploy/` stops being tested.
        compose
            .up()
            .await
            .expect("deploy/compose.yaml must come up: it is the configuration under test");

        let vtn = compose.service("vtn")?;
        let broker = compose.service("broker")?;
        let vtn_port = vtn.get_host_port_ipv4(3000).await.ok()?;
        let broker_port = broker.get_host_port_ipv4(1883).await.ok()?;

        let stack = Self {
            _compose: Some(compose),
            vtn: format!("http://127.0.0.1:{vtn_port}"),
            broker_host: "127.0.0.1".to_string(),
            broker_port,
        };
        stack.wait_until_ready().await.then_some(stack)
    }

    fn api(&self) -> String {
        format!("{}/openadr3/3.1.0", self.vtn)
    }

    /// Wait until the broker's answers are the VTN's, in **both** directions.
    ///
    /// The broker authenticates *through* the VTN, and during start-up neither answer is
    /// trustworthy yet: before the listener is up nothing connects at all, and there is a window in
    /// which the broker has not finished wiring its authentication source to the VTN. Waiting only
    /// for a good credential to be accepted is not enough — "accepted" is also what a broker that is
    /// not yet asking anybody says.
    ///
    /// So readiness is both answers being right: a credential the VTN knows is accepted, and one it
    /// does not is refused. That is the weakest condition under which the four claims below mean
    /// anything, and it is what makes check 1 a check rather than a coin toss.
    async fn wait_until_ready(&self) -> bool {
        if !self.wait_until_serving().await {
            return false;
        }
        for _ in 0..90 {
            let good = connect_result(self, "readiness-good", VEN_CLIENT, VEN_TOKEN).await;
            let bad = connect_result(self, "readiness-bad", VEN_CLIENT, "definitely-wrong").await;
            if good.is_none() && bad.is_some() {
                return true;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        eprintln!(
            "skipping: the broker never settled into answering with the VTN's decisions — a good \
             credential and a bad one did not get different answers"
        );
        false
    }

    async fn wait_until_serving(&self) -> bool {
        openadr::install_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("an HTTP client");
        for _ in 0..120 {
            if http
                .get(format!("{}/auth/server", self.api()))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        eprintln!("skipping: the VTN never answered GET /auth/server");
        false
    }

    fn business_logic(&self) -> Client<BusinessLogic> {
        Client::<BusinessLogic>::builder(&self.api())
            .expect("the base URL parses")
            .bearer_token(BL_TOKEN)
            .build()
            .expect("a client")
    }

    /// An MQTT connection, as a VEN presents one: the `clientID` as the username, the access token
    /// as the password. `emqx.conf` sends both to `POST /internal/mqtt/auth`.
    fn mqtt(&self, id: &str, username: &str, password: &str) -> (AsyncClient, rumqttc::EventLoop) {
        let mut options = MqttOptions::new(id, &self.broker_host, self.broker_port);
        options.set_credentials(username, password);
        options.set_keep_alive(Duration::from_secs(30));
        AsyncClient::new(options, 32)
    }
}

/// Drive a fresh connection to its `CONNACK`, and say what the broker decided.
///
/// `None` means accepted. Everything is dropped on the way out, so asking the question costs nothing
/// afterwards — which matters because readiness asks it up to ninety times, and a version of this
/// that left a live client behind each time left ninety of them reconnecting in the background.
async fn connect_result(stack: &Stack, id: &str, username: &str, password: &str) -> Option<String> {
    let (_client, mut loop_) = stack.mqtt(id, username, password);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        return match tokio::time::timeout_at(deadline, loop_.poll()).await {
            Ok(Ok(Event::Incoming(Packet::ConnAck(ack)))) => {
                match ack.code == rumqttc::ConnectReturnCode::Success {
                    true => None,
                    false => Some(format!("{:?}", ack.code)),
                }
            }
            Ok(Err(ConnectionError::ConnectionRefused(code))) => Some(format!("{code:?}")),
            Ok(Err(e)) => Some(e.to_string()),
            Err(_) => Some("the broker did not answer CONNECT within 20s".to_string()),
            // Anything else on the way to the acknowledgement.
            Ok(Ok(_)) => continue,
        };
    }
}

/// Ask the broker for a subscription, and say whether it granted one.
///
/// `Err` when it refused — either a `SUBACK` carrying a failure code, or, with EMQX's
/// `deny_action = disconnect`, by closing the connection. Both are the broker enforcing the VTN's
/// answer, and both are what a cross-VEN subscription must produce.
///
/// This is stronger than waiting for silence, and faster: "nothing arrived" also describes a
/// fan-out that published nothing, and it costs a timeout to establish.
async fn subscribe_result(
    stack: &Stack,
    id: &str,
    username: &str,
    password: &str,
    filter: &str,
) -> Result<(), String> {
    let (client, mut loop_) = stack.mqtt(id, username, password);
    client
        .subscribe(filter, QoS::AtLeastOnce)
        .await
        .map_err(|e| e.to_string())?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        return match tokio::time::timeout_at(deadline, loop_.poll()).await {
            Ok(Ok(Event::Incoming(Packet::SubAck(ack)))) => {
                match ack
                    .return_codes
                    .iter()
                    .all(|c| !matches!(c, rumqttc::SubscribeReasonCode::Failure))
                {
                    true => Ok(()),
                    false => Err(format!("{:?}", ack.return_codes)),
                }
            }
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("the broker neither granted nor refused within 20s".to_string()),
            Ok(Ok(_)) => continue,
        };
    }
}

/// A live subscription, and the messages that arrive on it.
///
/// The subscription is confirmed before the caller publishes anything, so "nothing arrived" cannot
/// mean "the subscription had not landed yet". That is the difference between this and a `sleep`.
struct Subscriber {
    _client: AsyncClient,
    events: tokio::sync::mpsc::UnboundedReceiver<(String, Vec<u8>)>,
}

impl Subscriber {
    /// Connect and subscribe. The caller has already established that this credential is accepted —
    /// [`connect_result`] is the question, this is the consequence.
    async fn connect(
        stack: &Stack,
        id: &str,
        username: &str,
        password: &str,
        filter: &str,
    ) -> Self {
        let (client, mut loop_) = stack.mqtt(id, username, password);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        client
            .subscribe(filter, QoS::AtLeastOnce)
            .await
            .expect("the subscribe request is sent");
        // Wait for the SubAck, so the broker holds the filter before anything is published.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut subscribed = false;
        while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, loop_.poll()).await {
            if matches!(event, Event::Incoming(Packet::SubAck(_))) {
                subscribed = true;
                break;
            }
        }
        assert!(
            subscribed,
            "the broker never acknowledged a subscription to {filter}"
        );

        tokio::spawn(async move {
            while let Ok(event) = loop_.poll().await {
                if let Event::Incoming(Packet::Publish(p)) = event
                    && tx.send((p.topic.clone(), p.payload.to_vec())).is_err()
                {
                    return;
                }
            }
        });

        Self {
            _client: client,
            events: rx,
        }
    }

    /// The next message, or `None` if the broker sends none within `patience`.
    ///
    /// The negative checks depend on this bound, so it is generous: a false "nothing arrived" is a
    /// privacy claim that is not true.
    async fn next(&mut self, patience: Duration) -> Option<(String, serde_json::Value)> {
        let (topic, payload) = tokio::time::timeout(patience, self.events.recv())
            .await
            .ok()??;
        Some((
            topic,
            serde_json::from_slice(&payload).unwrap_or(serde_json::Value::Null),
        ))
    }
}

/// One VEN object per `clientID`, so this reuses an existing one rather than creating a second —
/// the same idempotent registration a real VEN performs.
async fn ven_for(
    bl: &Client<BusinessLogic>,
    client_id: &str,
    name: &str,
    targets: &[&str],
) -> String {
    let request = VenRequest::Bl(BlVenRequest {
        client_id: client_id.parse().expect("a client id"),
        ven_name: name.parse().expect("a ven name"),
        targets: targets
            .iter()
            .map(|t| t.parse().expect("a target"))
            .collect(),
        attributes: None,
    });
    let existing = bl
        .vens()
        .list()
        .await
        .expect("the VEN list is readable")
        .into_iter()
        .find(|v| v.client_id.as_str() == client_id);
    match existing {
        Some(v) => bl.vens().update(&v.id, &request).await.expect("update").id,
        None => bl.vens().create(&request).await.expect("create").id,
    }
    .to_string()
}

/// A targeted event on `program`, which the fan-out then publishes.
async fn targeted_event(bl: &Client<BusinessLogic>, program: &str, name: &str, target: &str) {
    let mut event = EventRequest::new(program.parse().expect("a program id"));
    event.event_name = Some(name.parse().expect("an event name"));
    event.targets = vec![target.parse().expect("a target")];
    event.interval_period = Some(IntervalPeriod::new(
        "2026-02-11T12:00:00Z".parse().expect("a start"),
        "PT15M".parse().expect("a duration"),
    ));
    event.intervals = Some(vec![Interval::new(
        0,
        vec![openadr::model::ValuesMap::single(
            "SIMPLE".parse().expect("a payload type"),
            openadr::model::Value::Integer(1),
        )],
    )]);
    bl.events()
        .create(&event)
        .await
        .expect("the event is created");
}

#[tokio::test]
#[ignore = "brings up deploy/compose.yaml: run with --ignored --nocapture"]
// `[Notifiers §9.4]` and `[Def §MQTT]`: "A VTN MUST configure, and enforce, access to the MQTT
// broker's topics in accordance with its security and access policy … by any means necessary."
// Enforced by a real broker calling the real callbacks, which is the only arrangement in which
// "enforce" means anything.
async fn object_privacy_holds_across_a_real_broker() {
    let Some(stack) = Stack::start().await else {
        return;
    };
    let bl = stack.business_logic();
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Two VENs, one of them granted `group1`.
    let ven_a = ven_for(&bl, VEN_CLIENT, &format!("probe-a-{run}"), &["group1"]).await;
    let ven_b = ven_for(&bl, "other", &format!("probe-b-{run}"), &["group2"]).await;
    assert_ne!(ven_a, ven_b);

    let program = bl
        .programs()
        .create(&ProgramRequest::new(
            format!("broker-check-{run}")
                .parse()
                .expect("a program name"),
        ))
        .await
        .expect("the programme is created")
        .id
        .to_string();

    // 1. A wrong password is refused at connect — by the VTN, through the broker.
    //
    // `Stack::start` has already proved a *good* credential is accepted, which is what stops this
    // from passing on a broker that simply cannot reach the VTN.
    assert!(
        connect_result(&stack, "bad", VEN_CLIENT, "definitely-wrong")
            .await
            .is_some(),
        "the broker accepted a credential the VTN does not know"
    );
    println!("ok   a wrong password is refused");

    // 2. A targeted event reaches the entitled VEN's private topic, carrying its own target.
    let mut entitled = Subscriber::connect(
        &stack,
        "probe-a",
        VEN_CLIENT,
        VEN_TOKEN,
        &format!("{PREFIX}/events/vens/{ven_a}/#"),
    )
    .await;
    targeted_event(&bl, &program, "curtail", "group1").await;

    let (topic, body) = entitled
        .next(Duration::from_secs(30))
        .await
        .expect("the entitled VEN's private topic received nothing");
    assert!(
        topic.starts_with(&format!("{PREFIX}/events/vens/{ven_a}/")),
        "{topic}"
    );
    assert_eq!(body["operation"], "CREATE", "{body}");
    // Parsed, not grepped: the claim is that the copy carries *this VEN's* target and no other.
    assert_eq!(
        body["targets"],
        serde_json::json!(["group1"]),
        "the notification did not carry exactly this VEN's own target: {body}"
    );
    println!("ok   a targeted event reached the entitled VEN, carrying its own target");

    // 3. A VEN cannot see another VEN's private topic.
    //
    // Asserted as a *refusal* rather than as silence. Silence is also what an event that was never
    // published looks like, so a silence-based check passes on a VTN whose fan-out has stopped —
    // which is how a privacy claim becomes untestable. A refusal needs nothing to have been
    // published at all, and check 2 above has just established that the fan-out is working.
    //
    // Only one VEN credential exists on this stack (`--ven-token` authenticates exactly one
    // `clientID`), which is why the subscriber here is that VEN reaching for somebody else's topic
    // — the case that matters — rather than the other VEN reaching for its own.
    let refused = subscribe_result(
        &stack,
        "eavesdropper",
        VEN_CLIENT,
        VEN_TOKEN,
        &format!("{PREFIX}/events/vens/{ven_b}/#"),
    )
    .await;
    assert!(
        refused.is_err(),
        "one VEN was granted a subscription to another VEN's private topic"
    );

    println!("ok   the broker kept one VEN out of another's private topic");

    // 4. A VEN's forged publish reaches nobody.
    let (forger, mut forger_loop) = stack.mqtt("forger", VEN_CLIENT, VEN_TOKEN);
    tokio::spawn(async move { while forger_loop.poll().await.is_ok() {} });
    let _ = forger
        .publish(
            format!("{PREFIX}/events/vens/{ven_a}/create"),
            QoS::AtLeastOnce,
            false,
            br#"{"forged":true}"#.to_vec(),
        )
        .await;
    let forged = entitled.next(Duration::from_secs(5)).await;
    assert!(
        forged.is_none_or(|(_, body)| body["forged"].is_null()),
        "a VEN published a forged dispatch instruction and a subscriber received it"
    );
    println!("ok   a VEN's publish reached no subscriber");
}
