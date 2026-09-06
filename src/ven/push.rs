//! The VEN's half of MQTT push.
//!
//! 3.1.0 added notifications over a broker so that a VEN behind a residential firewall — which
//! cannot receive an inbound `POST` — can be told about a change at all `[CL 3.1.0 issue 114]`. The
//! VTN side of that is complete — a QoS 1 publisher, per-VEN topics, and the broker
//! authorization callbacks — and this is the other end. Without it the crate would ship the
//! discovery endpoints, the fan-out and the topic names with nothing anywhere able to subscribe.
//!
//! ## What arrives is a hint, not an instruction
//!
//! The payload is ignored. Not skimmed, not partially trusted — ignored. A subscriber calls
//! [`Waker::wake`] and the runtime's own conditional sync re-reads from the VTN.
//!
//! That is the whole design, and it is deliberate on three counts:
//!
//! * **The broker is a third party.** Object privacy is enforced by the VTN, at the REST layer and
//!   in the broker's ACL. A VEN that acted on a payload would be taking a dispatch instruction from
//!   whoever can write to a topic; one that wakes and re-reads takes it from the VTN over an
//!   authenticated channel, and the worst a hostile broker achieves is a VEN that polls too often.
//! * **A missed message must not be a missed event.** The specification tells a reconnecting client
//!   to re-`GET` rather than trust retained state `[Notifiers §16.2]`. The poll loop still runs, so
//!   a broker that is down, wrong or silent costs latency and never correctness.
//! * **The `GET` happens anyway.** A notification carries the object, but the VEN's sync is
//!   `ETag`-conditional and reads the whole collection — so a spurious hint costs one `304`, and a
//!   real one costs the read the VEN was going to do at the next poll regardless.
//!
//! ## What it subscribes to
//!
//! Only the VEN's own topics, discovered from the VTN rather than constructed:
//! `GET /notifiers` for the binding, then `/notifiers/mqtt/topics/vens/{venID}/…` for the names.
//! Constructing them instead would be a second implementation of a naming scheme the VTN already
//! publishes — and when a topic name is computed twice, the two copies disagree silently: every VEN
//! subscribes correctly, receives nothing for ever, and no error is raised anywhere.
//!
//! ```no_run
//! # #[cfg(feature = "mqtt")]
//! # async fn run(ven: &openadr::ven::VenRuntime) -> Result<(), Box<dyn std::error::Error>> {
//! let push = openadr::ven::MqttPush::connect(ven, Default::default()).await?;
//! loop {
//!     ven.sync().await?;
//!     ven.submit_due_reports().await?;
//!     ven.wait_for_work().await; // returns early when `push` hears something
//! }
//! # }
//! ```

use std::sync::Arc;
use std::time::Duration as StdDuration;

use rumqttc::{AsyncClient, Event, Incoming, QoS};

use super::{Meter, VenError, VenRuntime, Waker};
use crate::client::{Client, VirtualEndNode};
use crate::model::{MqttAuthentication, ObjectId};

/// How the subscriber behaves.
#[derive(Debug, Clone)]
pub struct MqttPushConfig {
    /// Client identifier presented to the broker. Must be unique per connection.
    ///
    /// `None` derives one from the VEN's id, which is unique per VEN — two processes sharing a VEN
    /// object would otherwise disconnect each other, because a broker evicts the older session
    /// holding a client id.
    pub client_id: Option<String>,
    /// Keep-alive interval.
    pub keep_alive: StdDuration,
    /// Pause after a connection error before the event loop is polled again.
    ///
    /// `rumqttc` reconnects on the next poll, so without this a broker that is down is a busy loop.
    pub reconnect_delay: StdDuration,
    /// Depth of the client's outgoing request queue.
    pub queue_capacity: usize,
    /// Also subscribe to the VEN's programme topic.
    ///
    /// On by default: a programme's `intervalPeriod` and payload descriptors change how its events
    /// are read, so a VEN that hears about events and not programmes can act on a stale reading.
    pub watch_programs: bool,
    /// Also subscribe to the VEN's own `ven` and `resource` topics.
    ///
    /// On by default: a change to either is a change to what this VEN has been *granted*, and a VEN
    /// that does not re-read after a grant moves keeps acting on the old one until its next poll.
    pub watch_own_objects: bool,
}

impl Default for MqttPushConfig {
    fn default() -> Self {
        Self {
            client_id: None,
            keep_alive: StdDuration::from_secs(30),
            reconnect_delay: StdDuration::from_secs(1),
            queue_capacity: 32,
            watch_programs: true,
            watch_own_objects: true,
        }
    }
}

/// A live subscription to this VEN's notification topics.
///
/// Dropping it stops the subscriber; the VEN keeps polling, which is why dropping it is safe.
#[derive(Debug)]
pub struct MqttPush {
    topics: Vec<String>,
    broker: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MqttPush {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MqttPush {
    /// Discover the broker and this VEN's topics, connect, and wake the runtime on every message.
    ///
    /// Returns [`VenError::NotRegistered`] before [`VenRuntime::register`], because the topics are
    /// named after the VEN object's id and there is no id until then.
    ///
    /// A VTN with no broker is not an error — MQTT is optional `[Notifiers §7.2]` — so this returns
    /// `Ok(None)`, and a caller that treats it as a failure would refuse to run against a
    /// conformant VTN.
    pub async fn connect<M: Meter>(
        ven: &VenRuntime<M>,
        config: MqttPushConfig,
    ) -> Result<Option<Self>, VenError> {
        let ven_id = ven.ven_id().ok_or(VenError::NotRegistered)?;
        Self::start(ven.client(), &ven_id, ven.waker(), config).await
    }

    /// The same, for a caller wiring the parts together itself.
    pub async fn start(
        client: &Client<VirtualEndNode>,
        ven_id: &ObjectId,
        waker: Waker,
        config: MqttPushConfig,
    ) -> Result<Option<Self>, VenError> {
        let notifiers = client.notifiers().await?;
        let Some(binding) = notifiers.mqtt else {
            tracing::info!("this VTN offers no MQTT notifier; the VEN will poll");
            return Ok(None);
        };
        let broker = binding
            .uris
            .first()
            .cloned()
            .ok_or_else(|| VenError::Protocol("the MQTT binding names no broker URI".into()))?;

        let topics = Self::discover(client, ven_id, &config).await?;
        if topics.is_empty() {
            return Err(VenError::Protocol(
                "the VTN named no topics this VEN may subscribe to".into(),
            ));
        }

        let client_id = config
            .client_id
            .clone()
            .unwrap_or_else(|| format!("openadr-ven-{ven_id}"));
        let mut options = crate::mqtt::broker_options(&broker, &client_id, config.keep_alive)
            .map_err(VenError::Protocol)?;

        // `[Notifiers §12.2]`: the access token is the MQTT password. The VTN's authorization
        // callback then derives what may be subscribed to from the *username*, which is why it has
        // to be the client id the token proves rather than anything of our choosing — and why an
        // absent one is refused here rather than sent empty. A broker that rejects an empty
        // username produces a VEN that reconnects for ever with nothing anywhere saying why.
        //
        // The `{clientID}` placeholder is resolved by the binding itself
        // ([`MqttAuthentication::resolve_username`]), not restated here: it was written out twice
        // once, and a rule with two implementations has two chances to be wrong.
        if matches!(
            binding.authentication,
            MqttAuthentication::Oauth2BearerToken { .. }
        ) {
            let token = client.access_token().await?.ok_or_else(|| {
                VenError::Protocol(
                    "the broker wants the access token as its password, and this client has \
                     neither credentials nor a pre-shared token"
                        .into(),
                )
            })?;
            let username = binding
                .authentication
                .resolve_username(client.client_id())
                .ok_or_else(|| {
                    VenError::Protocol(
                        "the broker wants this VEN's clientID as its MQTT username, and a \
                         pre-shared bearer token does not say what that is — configure client \
                         credentials, or a fixed username in the VTN's binding"
                            .into(),
                    )
                })?
                .to_string();
            options.set_credentials(username, token);
        }

        let (mqtt, event_loop) = AsyncClient::new(options, config.queue_capacity);
        for topic in &topics {
            // QoS 1: the broker holds a notification that arrives while the VEN is disconnected,
            // and a duplicate costs one conditional `GET`. A hint is idempotent, so the weaker
            // guarantee buys nothing and the stronger one costs nothing.
            mqtt.subscribe(topic.clone(), QoS::AtLeastOnce)
                .await
                .map_err(|e| VenError::Protocol(format!("could not subscribe: {e}")))?;
        }

        let task = tokio::spawn(run(
            event_loop,
            waker,
            config.reconnect_delay,
            Arc::new(mqtt),
        ));
        tracing::info!(broker = %broker, topics = topics.len(), "subscribed to VTN notifications");
        Ok(Some(Self {
            topics,
            broker,
            task,
        }))
    }

    /// The topics being watched, in subscription order.
    pub fn topics(&self) -> &[String] {
        &self.topics
    }

    /// The broker URI in use.
    pub fn broker(&self) -> &str {
        &self.broker
    }

    /// Ask the VTN which topics this VEN may subscribe to.
    ///
    /// One wildcard subscription per endpoint where the VTN offers `ALL`, and the individual
    /// operations otherwise — a binding that cannot express `+` is legal, and three subscriptions
    /// are the same set of messages.
    async fn discover(
        client: &Client<VirtualEndNode>,
        ven_id: &ObjectId,
        config: &MqttPushConfig,
    ) -> Result<Vec<String>, VenError> {
        let mut paths = vec![format!("vens/{ven_id}/events")];
        if config.watch_programs {
            paths.push(format!("vens/{ven_id}/programs"));
        }
        if config.watch_own_objects {
            paths.push(format!("vens/{ven_id}"));
            paths.push(format!("vens/{ven_id}/resources"));
        }

        let mut topics = Vec::new();
        for path in paths {
            // A VTN may serve some of these and not others — the two beyond the twelve the document
            // lists are extensions this implementation adds — so one that is absent is not a
            // failure.
            let response = match client.mqtt_topics(&path).await {
                Ok(response) => response,
                Err(e) if e.status() == Some(404) => {
                    tracing::debug!(path, "this VTN does not offer that topic endpoint");
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let t = response.topics;
            match t.all {
                Some(all) => topics.push(all),
                None => {
                    topics.extend(t.create);
                    topics.push(t.update);
                    topics.push(t.delete);
                }
            }
        }
        topics.sort();
        topics.dedup();
        Ok(topics)
    }
}

/// Poll the connection and wake the runtime on every message that arrives.
///
/// The payload never leaves this function. It is not deserialised, not inspected and not logged at
/// any level that would put a competitor's dispatch schedule in a log file.
async fn run(
    mut event_loop: rumqttc::EventLoop,
    waker: Waker,
    reconnect_delay: StdDuration,
    _client: Arc<AsyncClient>,
) {
    loop {
        match event_loop.poll().await {
            Ok(Event::Incoming(Incoming::Publish(publish))) => {
                tracing::debug!(topic = %publish.topic, "notification received; syncing early");
                waker.wake();
            }
            Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                tracing::info!("connected to the MQTT broker");
                // A reconnect may have missed messages the broker did not hold, so re-read once
                // rather than wait for the poll interval `[Notifiers §16.2]`.
                waker.wake();
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "MQTT connection error; reconnecting");
                tokio::time::sleep(reconnect_delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_replaces_three_subscriptions_and_a_binding_without_one_does_not() {
        use crate::model::{NotifierTopics, TopicsResponse};

        let wildcard = TopicsResponse::under("events/vens/ven-1").topics;
        assert_eq!(wildcard.all.as_deref(), Some("events/vens/ven-1/+"));

        let plain = NotifierTopics {
            create: Some("a/create".into()),
            update: "a/update".into(),
            delete: "a/delete".into(),
            all: None,
        };
        // Without `ALL` the subscriber takes the three named ones, which is the same set.
        let mut expanded: Vec<String> = Vec::new();
        expanded.extend(plain.create.clone());
        expanded.push(plain.update.clone());
        expanded.push(plain.delete.clone());
        assert_eq!(expanded, ["a/create", "a/update", "a/delete"]);
    }

    #[test]
    fn the_default_client_id_is_per_ven_not_per_process() {
        // Two processes sharing a VEN object with one client id evict each other from the broker,
        // and the symptom is a VEN that receives nothing and reports no error.
        let config = MqttPushConfig::default();
        assert!(config.client_id.is_none());
        assert!(config.watch_programs && config.watch_own_objects);
    }
}
