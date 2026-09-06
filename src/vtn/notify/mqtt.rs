//! The MQTT transport.
//!
//! 3.1 added push over a broker for one reason: a VEN inside a residential appliance sits behind a
//! firewall that will never accept an inbound `POST`, so a webhook cannot reach it at all. The
//! client opens the connection instead, and the VTN publishes.
//!
//! What the VTN owes that arrangement is object privacy. The topic layout does the work — an
//! event targeted at `group1` is published to `events/vens/{venID}/create` for each VEN whose
//! grant reaches it, carrying only that VEN's own targets — and the fan-out that computes those
//! copies is [`mqtt_deliveries`](super::mqtt_deliveries), shared with the read path. This module is
//! only the wire: connect, publish, confirm.
//!
//! ## Confirming
//!
//! An outbox entry may be removed only once the notification is really gone. `AsyncClient::publish`
//! returns as soon as the packet is queued *inside the client*, which is not that: a process that
//! dies a millisecond later has deleted the row and published nothing — precisely the loss the
//! outbox exists to prevent.
//!
//! So a QoS 1 publish waits for its `PUBACK`. The event loop runs in its own task and announces
//! every packet it writes and every acknowledgement it reads; a publish holds a mutex for the round
//! trip, so exactly one is outstanding and the identifier it is told about is unambiguously its
//! own. That costs one round trip per notification and buys the guarantee the rest of the design is
//! built on. QoS 0 is available for a deployment that would rather have the throughput, and says so.
//!
//! Serialising is what makes the correlation sound, and it is worth being precise about why. A
//! publish waits for two things in order: the event loop reporting that *a* publish packet was
//! written, and the broker acknowledging *that* identifier. Because only one publish is outstanding,
//! the first is unambiguously ours — with one exception, and it is the reason the timeout is
//! generous. If a publish gave up *before* its packet was written, the packet could still be written
//! afterwards and the next publish would adopt its identifier. That needs the connection to be alive
//! (a dead one broadcasts `Dropped` instead) while a single queued packet goes unwritten for the
//! whole timeout, which is not a state a working event loop reaches. Shortening
//! [`MqttConfig::publish_timeout`] below the broker's own latency is the way to make it reachable.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, MqttOptions, Outgoing, QoS};
use tokio::sync::{Mutex, broadcast};

use super::{Channel, Delivery, DeliveryFailure, Notifier};

/// How the publisher behaves.
#[derive(Debug, Clone)]
pub struct MqttConfig {
    /// Broker URL: `mqtt://host:1883`, `mqtts://host:8883`, `ws://…` or `wss://…`.
    pub url: String,
    /// Client identifier presented to the broker. Must be unique per connection.
    pub client_id: String,
    /// Username, when the broker authenticates.
    pub username: Option<String>,
    /// Password, or an OAuth2 access token where the broker takes one there.
    pub password: Option<String>,
    /// Keep-alive interval.
    pub keep_alive: StdDuration,
    /// Publish with the retain flag.
    ///
    /// Off by default. Retained messages let a reconnecting VEN see the last notification per
    /// topic, but the specification tells clients to re-`GET` on reconnect rather than trust one
    /// `[Notifiers §16.2]`, and a retained *delete* on a per-VEN topic outlives the grant that put
    /// it there.
    pub retain: bool,
    /// Quality of service for published notifications.
    ///
    /// `AtLeastOnce` is the default and is what makes the outbox's guarantee reach the broker.
    /// `AtMostOnce` publishes without confirmation, so an entry is completed on the strength of a
    /// write to a socket.
    pub qos: QoS,
    /// How long to wait for a `PUBACK` before treating the attempt as failed.
    pub publish_timeout: StdDuration,
    /// Depth of the client's outgoing request queue.
    pub queue_capacity: usize,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            url: "mqtt://localhost:1883".into(),
            client_id: format!("openadr-vtn-{}", std::process::id()),
            username: None,
            password: None,
            keep_alive: StdDuration::from_secs(30),
            retain: false,
            qos: QoS::AtLeastOnce,
            publish_timeout: StdDuration::from_secs(15),
            queue_capacity: 128,
        }
    }
}

impl MqttConfig {
    /// A configuration pointed at a broker URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    /// Set username and password. A broker taking an OAuth2 token takes it as the password.
    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    /// Publish with the retain flag set.
    pub fn retained(mut self) -> Self {
        self.retain = true;
        self
    }

    /// Override the client identifier.
    pub fn with_client_id(mut self, id: impl Into<String>) -> Self {
        self.client_id = id.into();
        self
    }
}

/// Why a publish did not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MqttError {
    /// The broker URL could not be understood.
    #[error("invalid broker URL: {0}")]
    Url(String),
    /// The client could not hand the packet to its event loop.
    #[error("could not queue the publish: {0}")]
    Queue(String),
    /// The broker did not acknowledge within the timeout.
    #[error("the broker did not acknowledge the publish within {0:?}")]
    Unacknowledged(StdDuration),
    /// The connection dropped while the publish was outstanding.
    #[error("the broker connection dropped: {0}")]
    Disconnected(String),
}

impl MqttError {
    /// Whether another attempt could plausibly succeed.
    ///
    /// Everything except a URL that does not parse: a broker that is down comes back, and a lost
    /// connection is what the dispatcher's backoff is for.
    pub fn is_retriable(&self) -> bool {
        !matches!(self, MqttError::Url(_))
    }
}

/// What the event-loop task tells the publisher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    /// A publish packet was written to the socket, with this identifier.
    Sent(u16),
    /// The broker acknowledged the identifier.
    Acked(u16),
    /// The connection dropped; anything outstanding is lost.
    Dropped,
}

/// Publishes notifications to the VTN's broker.
///
/// Built by [`MqttNotifier::connect`], which also spawns the event loop. Dropping it disconnects.
#[derive(Debug)]
pub struct MqttNotifier {
    client: AsyncClient,
    config: MqttConfig,
    wire: broadcast::Sender<Wire>,
    /// One publish at a time, so the identifier the event loop reports is unambiguously ours.
    in_flight: Mutex<()>,
    loop_task: tokio::task::JoinHandle<()>,
}

impl Drop for MqttNotifier {
    fn drop(&mut self) {
        self.loop_task.abort();
    }
}

impl MqttNotifier {
    /// Connect to the broker and start the event loop.
    ///
    /// Returns as soon as the connection is being established; `rumqttc` reconnects on its own, so
    /// a broker that is not up yet delays deliveries rather than refusing to start the VTN.
    ///
    /// Must be called from inside a Tokio runtime: the event loop is spawned, and nothing moves
    /// without it — not even a publish that has already been queued.
    pub fn connect(config: MqttConfig) -> Result<Arc<Self>, MqttError> {
        let options = build_options(&config)?;
        let (client, event_loop) = AsyncClient::new(options, config.queue_capacity);
        let (wire, _) = broadcast::channel(config.queue_capacity.max(16) * 4);
        let loop_task = tokio::spawn(run_event_loop(event_loop, wire.clone()));
        Ok(Arc::new(Self {
            client,
            config,
            wire,
            in_flight: Mutex::new(()),
            loop_task,
        }))
    }

    /// The broker URL this publisher is pointed at.
    pub fn url(&self) -> &str {
        &self.config.url
    }

    async fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<(), MqttError> {
        // Held for the whole round trip. See the module note: it is what makes the identifier the
        // event loop reports unambiguous.
        let _one_at_a_time = self.in_flight.lock().await;
        let mut events = self.wire.subscribe();

        self.client
            .publish(topic, self.config.qos, self.config.retain, payload)
            .await
            .map_err(|e| MqttError::Queue(e.to_string()))?;

        if self.config.qos == QoS::AtMostOnce {
            // Nothing to wait for: QoS 0 has no acknowledgement, which is exactly what the
            // deployment asked for when it chose it.
            return Ok(());
        }

        tokio::time::timeout(self.config.publish_timeout, async {
            let mut ours: Option<u16> = None;
            loop {
                match events.recv().await {
                    Ok(Wire::Sent(pkid)) if ours.is_none() => ours = Some(pkid),
                    Ok(Wire::Acked(pkid)) if ours == Some(pkid) => return Ok(()),
                    Ok(Wire::Dropped) => {
                        return Err(MqttError::Disconnected(
                            "connection lost mid-publish".into(),
                        ));
                    }
                    Ok(_) => continue,
                    // Falling behind the broadcast means an acknowledgement may have gone past
                    // unseen. Failing is the safe reading: a redelivery is a duplicate the receiver
                    // can recognise, a lost notification is not.
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        return Err(MqttError::Disconnected(format!(
                            "publisher fell {n} events behind its own event loop"
                        )));
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(MqttError::Disconnected("event loop stopped".into()));
                    }
                }
            }
        })
        .await
        .map_err(|_| MqttError::Unacknowledged(self.config.publish_timeout))?
    }
}

#[async_trait]
impl Notifier for MqttNotifier {
    fn handles(&self, channel: Channel) -> bool {
        channel == Channel::Mqtt
    }

    async fn deliver(&self, delivery: &Delivery, _attempt: u32) -> Result<(), DeliveryFailure> {
        let Some(topic) = delivery.route.topic() else {
            // Unreachable through `Notifiers`, which routes by channel. Reported rather than
            // ignored so that a hand-wired dispatcher cannot lose an entry silently.
            return Err(DeliveryFailure::permanent(
                "the MQTT transport was handed a delivery with no topic",
            ));
        };
        let payload = serde_json::to_vec(&delivery.notification).map_err(|e| {
            DeliveryFailure::permanent(format!("notification is not encodable: {e}"))
        })?;

        match self.publish(topic, payload).await {
            Ok(()) => {
                tracing::debug!(
                    topic,
                    object = %delivery.notification.object.object_type(),
                    operation = %delivery.notification.operation,
                    "notification published"
                );
                Ok(())
            }
            Err(e) => {
                tracing::debug!(topic, error = %e, retriable = e.is_retriable(), "publish failed");
                Err(if e.is_retriable() {
                    DeliveryFailure::retriable(e.to_string())
                } else {
                    DeliveryFailure::permanent(e.to_string())
                })
            }
        }
    }

    fn name(&self) -> &'static str {
        "mqtt"
    }
}

/// Translate the configuration into `rumqttc`'s options.
///
/// The URL, the port defaults and the TLS provider are [`crate::mqtt`]'s, because the VEN's
/// subscriber has to reach the same broker and a second parser is a second set of defaults.
fn build_options(config: &MqttConfig) -> Result<MqttOptions, MqttError> {
    let mut options =
        crate::mqtt::broker_options(&config.url, &config.client_id, config.keep_alive)
            .map_err(MqttError::Url)?;
    if let Some(username) = &config.username {
        options.set_credentials(
            username.clone(),
            config.password.clone().unwrap_or_default(),
        );
    }
    Ok(options)
}

/// Drive the connection and announce what happens on it.
///
/// `rumqttc` requires the event loop to be polled for anything at all to move, including the
/// packets a publish queues. Everything the publisher needs to know — which packet went out, which
/// came back acknowledged, and when the connection died — is only visible here.
async fn run_event_loop(mut event_loop: EventLoop, wire: broadcast::Sender<Wire>) {
    loop {
        match event_loop.poll().await {
            Ok(Event::Outgoing(Outgoing::Publish(pkid))) => {
                let _ = wire.send(Wire::Sent(pkid));
            }
            Ok(Event::Incoming(Incoming::PubAck(ack))) => {
                let _ = wire.send(Wire::Acked(ack.pkid));
            }
            Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                tracing::info!("connected to the MQTT broker");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "MQTT connection error; reconnecting");
                let _ = wire.send(Wire::Dropped);
                // `rumqttc` reconnects on the next poll; pausing keeps a broker that is down from
                // becoming a busy loop.
                tokio::time::sleep(StdDuration::from_secs(1)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_urls_pick_the_right_port_and_transport() {
        let plain = build_options(&MqttConfig::new("mqtt://broker.example.com")).unwrap();
        assert_eq!(plain.broker_address(), ("broker.example.com".into(), 1883));

        let tls = build_options(&MqttConfig::new("mqtts://broker.example.com")).unwrap();
        assert_eq!(tls.broker_address(), ("broker.example.com".into(), 8883));

        let explicit = build_options(&MqttConfig::new("mqtt://broker.example.com:2883")).unwrap();
        assert_eq!(
            explicit.broker_address(),
            ("broker.example.com".into(), 2883)
        );
    }

    #[test]
    fn an_unusable_broker_url_is_refused_rather_than_retried() {
        let err = build_options(&MqttConfig::new("https://broker.example.com")).unwrap_err();
        assert!(matches!(err, MqttError::Url(_)));
        assert!(
            !err.is_retriable(),
            "no number of attempts turns https into mqtt"
        );
        assert!(build_options(&MqttConfig::new("not a url")).is_err());
    }

    #[test]
    fn a_broker_that_is_down_is_worth_another_attempt() {
        assert!(MqttError::Disconnected("reset".into()).is_retriable());
        assert!(MqttError::Unacknowledged(StdDuration::from_secs(1)).is_retriable());
        assert!(MqttError::Queue("full".into()).is_retriable());
    }

    #[test]
    fn credentials_reach_the_options() {
        let config =
            MqttConfig::new("mqtt://broker.example.com").with_credentials("ven-1", "token");
        let options = build_options(&config).unwrap();
        let login = options.credentials().expect("credentials were set");
        assert_eq!(login.username, "ven-1");
        assert_eq!(login.password, "token");
    }
}
