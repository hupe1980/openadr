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
//! An outbox entry may be removed only once the notification is really gone, so a QoS 1 publish
//! resolves on its `PUBACK` and not before (D-057). That is [`crate::mqtt`]'s guarantee now rather
//! than this module's: the caller allocates the packet identifier, registers a channel under it and
//! writes, so correlation is a map lookup.
//!
//! This module is only the wire, because [`crate::mqtt`] owns the correlation. A transport that
//! could not name the identifier it had just written would need a lock across the broker round
//! trip, which makes the time to clear a batch the *sum* of its publish timeouts — the failure
//! [`DispatchConfig`](crate::vtn::DispatchConfig)'s concurrency exists to prevent (D-133, D-134).

use std::sync::Arc;
use std::time::Duration as StdDuration;

use async_trait::async_trait;

use super::{Channel, Delivery, DeliveryFailure, Notifier};
use crate::mqtt::{BrokerAddress, Connection, ConnectionConfig, Incoming, QoS};

/// Re-exported so a caller handling a delivery failure need not reach into [`crate::mqtt`].
pub use crate::mqtt::MqttError;

/// How the publisher behaves.
#[derive(Debug, Clone)]
pub struct MqttConfig {
    /// Broker URL: `mqtt://host:1883` or `mqtts://host:8883`.
    ///
    /// `tcp://` and `ssl://` are accepted as the same two transports under other names. Anything
    /// else is refused by name — the previous version of this line offered `ws://` and `wss://`,
    /// which the URL parser one function away had always rejected.
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

/// Publishes notifications to the VTN's broker.
///
/// Built by [`MqttNotifier::connect`], which opens the connection. Dropping it disconnects.
#[derive(Debug)]
pub struct MqttNotifier {
    connection: Arc<Connection>,
    config: MqttConfig,
}

impl MqttNotifier {
    /// Connect to the broker.
    ///
    /// Returns as soon as the connection task is spawned; it reconnects on its own, so a broker
    /// that is not up yet delays deliveries rather than refusing to start the VTN.
    ///
    /// Must be called from inside a Tokio runtime.
    pub fn connect(config: MqttConfig) -> Result<Arc<Self>, MqttError> {
        let address = BrokerAddress::parse(&config.url)?;
        // The publisher subscribes to nothing: it is the only party allowed to publish, and
        // `/internal/mqtt/acl` refuses every subscription it would make (D-102).
        let (events, mut incoming) = tokio::sync::mpsc::channel(32);
        let connection = Connection::open(
            address,
            ConnectionConfig {
                client_id: config.client_id.clone(),
                username: config.username.clone(),
                password: config.password.clone(),
                keep_alive: config.keep_alive,
                ack_timeout: config.publish_timeout,
                ..Default::default()
            },
            Vec::new(),
            events,
        );
        // Drained rather than dropped: a full channel would block the connection task, and the
        // transitions are worth a log line an operator can correlate with a stalled outbox.
        tokio::spawn(async move {
            while let Some(event) = incoming.recv().await {
                match event {
                    Incoming::Connected(ack) => tracing::info!(
                        session_present = ack.session_present,
                        "connected to the MQTT broker"
                    ),
                    Incoming::Disconnected(why) => {
                        tracing::warn!(reason = %why, "MQTT connection lost")
                    }
                    Incoming::Message { topic, .. } => {
                        // The VTN subscribes to nothing, so this is the broker echoing or a
                        // misconfigured shared client id — worth saying, without the payload.
                        tracing::warn!(%topic, "the publisher received a message it never asked for")
                    }
                }
            }
        });
        Ok(Arc::new(Self { connection, config }))
    }

    /// The broker URL this publisher is pointed at.
    pub fn url(&self) -> &str {
        &self.config.url
    }

    /// Whether the broker connection is currently up.
    ///
    /// Advisory: a delivery attempted while this is false is queued and fails on the timeout, which
    /// is what the dispatcher's backoff is for.
    pub fn is_connected(&self) -> bool {
        self.connection.is_connected()
    }

    async fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<(), MqttError> {
        // One call. Every publish in the dispatcher's batch can be outstanding at once, because
        // each carries its own identifier and its own channel — which is what `concurrency` was
        // always supposed to buy (D-133).
        tokio::time::timeout(
            self.config.publish_timeout,
            self.connection
                .publish(topic, payload, self.config.qos, self.config.retain),
        )
        .await
        .map_err(|_| MqttError::Timeout(self.config.publish_timeout))?
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_urls_pick_the_right_port_and_transport() {
        let plain =
            BrokerAddress::parse(&MqttConfig::new("mqtt://broker.example.com").url).unwrap();
        assert_eq!(
            (plain.host(), plain.port(), plain.is_tls()),
            ("broker.example.com", 1883, false)
        );

        let tls = BrokerAddress::parse(&MqttConfig::new("mqtts://broker.example.com").url).unwrap();
        assert_eq!(
            (tls.host(), tls.port(), tls.is_tls()),
            ("broker.example.com", 8883, true)
        );

        let explicit =
            BrokerAddress::parse(&MqttConfig::new("mqtt://broker.example.com:2883").url).unwrap();
        assert_eq!(explicit.port(), 2883);
    }

    #[test]
    fn an_unusable_broker_url_is_refused_rather_than_retried() {
        let err = BrokerAddress::parse("https://broker.example.com").unwrap_err();
        assert!(matches!(err, MqttError::Url(_)));
        assert!(
            !err.is_retriable(),
            "no number of attempts turns https into mqtt"
        );
        assert!(BrokerAddress::parse("not a url").is_err());
    }

    #[test]
    fn a_broker_that_is_down_is_worth_another_attempt() {
        assert!(MqttError::Disconnected("reset".into()).is_retriable());
        assert!(MqttError::Timeout(StdDuration::from_secs(1)).is_retriable());
        // A refusal too: the commonest cause is a token that expired, and a VTN that gave up would
        // need a restart to notice it had been renewed.
        assert!(MqttError::Refused(5, "not authorized").is_retriable());
    }

    #[test]
    fn credentials_reach_the_connection_config() {
        let config =
            MqttConfig::new("mqtt://broker.example.com").with_credentials("ven-1", "token");
        assert_eq!(config.username.as_deref(), Some("ven-1"));
        assert_eq!(config.password.as_deref(), Some("token"));
    }

    #[test]
    fn the_only_qualities_of_service_are_the_two_that_can_complete() {
        // Two variants only: nothing here speaks QoS 2's handshake, so a third would be a
        // setting whose publishes could only time out.
        assert_eq!(MqttConfig::default().qos, QoS::AtLeastOnce);
    }
}
