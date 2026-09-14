//! The socket half: one task owns the connection, everybody else owns a channel.
//!
//! ## Why this is not a third-party client
//!
//! Two of this crate's requirements are unusual, and both were fought for against a library that
//! did not offer them:
//!
//! * **A publish completes on `PUBACK`, not on a queue insert** (D-057). The outbox's guarantee is
//!   that an entry is removed when the broker has it; a client whose `publish()` resolves on a
//!   write to an in-process channel turns that into a guarantee about a channel.
//! * **Many publishes are outstanding at once** (D-133). The dispatcher's `concurrency` exists to
//!   stop one slow endpoint serialising a batch, and a transport that correlates by "the last
//!   identifier the event loop mentioned" has to hold a lock across the whole round trip to know
//!   whose acknowledgement it saw.
//!
//! So the caller allocates the identifier *before* the packet is written and registers a
//! [`oneshot`] under it. Correlation is a map lookup: no lock across the round trip, and as many
//! publishes outstanding as the dispatcher allows.
//!
//! ## Reconnection
//!
//! The writer task reconnects with capped backoff and — the part that matters — **re-subscribes
//! whenever the broker reports `session_present = false`** `[MQTT 3.1.1 §3.2.2.2]`. A client that
//! connects with `cleanSession = 0` and assumes its subscriptions survived a broker restart stops
//! receiving anything, for ever, with no error at either end. That is the failure per-VEN topics
//! exist to prevent, arriving through the transport instead.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc, oneshot};

use super::codec::{self, ConnAck, Connect, Packet, Publish, QoS};
use super::{BrokerAddress, MqttError};

/// What the connection reports upwards.
#[derive(Debug, Clone)]
pub enum Incoming {
    /// A message arrived.
    ///
    /// The payload is here because this is an MQTT client and a client that drops payloads is not
    /// one. **Object privacy is not enforced by withholding it**: the bytes are in memory the
    /// moment the packet is decoded, so a transport that hid the field would be making a claim
    /// about a struct rather than about a process. The guarantee lives where it is actually kept —
    /// `ven::push` never reads this field and never logs it, so a notification remains a hint to
    /// re-read and a broker can cost a VEN latency but never correctness (D-083).
    Message {
        /// The topic it arrived on.
        topic: String,
        /// The message body.
        payload: Vec<u8>,
    },
    /// The connection was established. `session_present` says whether the broker restored state.
    Connected(ConnAck),
    /// The connection dropped and will be retried.
    Disconnected(String),
}

/// How a connection behaves.
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    /// Client identifier. Must be unique per connection to a broker.
    pub client_id: String,
    /// Username, when the broker authenticates.
    pub username: Option<String>,
    /// Password — where OpenADR puts the access token `[Notifiers §12.2]`.
    pub password: Option<String>,
    /// Keep-alive interval. A `PINGREQ` goes out after this long without a write.
    pub keep_alive: Duration,
    /// Longest pause between reconnection attempts.
    pub max_backoff: Duration,
    /// How long to wait for a `PUBACK` or `SUBACK`.
    pub ack_timeout: Duration,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            username: None,
            password: None,
            keep_alive: Duration::from_secs(30),
            max_backoff: Duration::from_secs(30),
            ack_timeout: Duration::from_secs(15),
        }
    }
}

/// A request from a caller to the connection task.
///
/// Both carry an identifier allocated by [`Connection::allocate_id`]. One allocator for both is
/// not tidiness: MQTT has a single identifier space per connection, and a `SUBSCRIBE` that reuses a
/// number an unacknowledged `PUBLISH` is still holding makes the next `PUBACK` ambiguous.
enum Command {
    Publish {
        publish: Publish,
        done: oneshot::Sender<Result<(), MqttError>>,
    },
    Subscribe {
        packet_id: u16,
        filters: Vec<(String, QoS)>,
        done: oneshot::Sender<Result<Vec<u8>, MqttError>>,
    },
}

/// A live MQTT connection.
///
/// Cloning shares one connection. Dropping the last handle stops the task and disconnects.
#[derive(Debug)]
pub struct Connection {
    commands: mpsc::Sender<Command>,
    next_id: AtomicU16,
    connected: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
    address: BrokerAddress,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Connection {
    /// Open a connection and start its task.
    ///
    /// Returns as soon as the task is spawned. A broker that is not up yet delays deliveries
    /// rather than refusing to start the VTN, which is what lets a site controller boot in any
    /// order.
    ///
    /// `subscriptions` are re-sent on every connect where the broker did not restore the session.
    pub fn open(
        address: BrokerAddress,
        config: ConnectionConfig,
        subscriptions: Vec<(String, QoS)>,
        events: mpsc::Sender<Incoming>,
    ) -> Arc<Self> {
        let (commands, rx) = mpsc::channel(256);
        let connected = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run(
            address.clone(),
            config,
            subscriptions,
            rx,
            events,
            connected.clone(),
        ));
        Arc::new(Self {
            commands,
            // Identifiers start at 1: zero is not a valid MQTT packet identifier.
            next_id: AtomicU16::new(1),
            connected,
            task,
            address,
        })
    }

    /// Whether the broker connection is currently up.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// The broker this is pointed at.
    pub fn address(&self) -> &BrokerAddress {
        &self.address
    }

    /// Allocate the next packet identifier, skipping zero on wrap.
    fn allocate_id(&self) -> u16 {
        loop {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    /// Publish, resolving when the broker has acknowledged it.
    ///
    /// For [`QoS::AtMostOnce`] there is nothing to acknowledge and this resolves when the bytes
    /// reach the socket — which is what that quality of service means and what a deployment
    /// choosing it has decided.
    pub async fn publish(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: QoS,
        retain: bool,
    ) -> Result<(), MqttError> {
        let packet_id = match qos {
            QoS::AtMostOnce => None,
            QoS::AtLeastOnce => Some(self.allocate_id()),
        };
        let publish = Publish {
            topic: topic.to_string(),
            qos,
            retain,
            dup: false,
            packet_id,
            payload,
        };

        let (done, wait) = oneshot::channel();
        self.commands
            .send(Command::Publish { publish, done })
            .await
            .map_err(|_| MqttError::Disconnected("the connection task has stopped".into()))?;
        wait.await.map_err(|_| {
            MqttError::Disconnected("the connection task dropped the publish".into())
        })?
    }

    /// Subscribe, resolving when the broker has answered.
    ///
    /// The granted codes are returned so a caller can tell a refusal (`0x80`) from a grant — a
    /// broker that refuses a topic and a broker that grants it both leave the connection up, and
    /// only one of them will ever deliver anything.
    pub async fn subscribe(&self, filters: Vec<(String, QoS)>) -> Result<Vec<u8>, MqttError> {
        let (done, wait) = oneshot::channel();
        self.commands
            .send(Command::Subscribe {
                packet_id: self.allocate_id(),
                filters,
                done,
            })
            .await
            .map_err(|_| MqttError::Disconnected("the connection task has stopped".into()))?;
        wait.await.map_err(|_| {
            MqttError::Disconnected("the connection task dropped the subscribe".into())
        })?
    }
}

/// Either half of a connected socket, boxed so TCP and TLS share one code path.
pub(crate) type Stream = Box<dyn Duplex>;

/// What the read and write halves need. A trait rather than an enum so that adding WebSocket
/// transport later is a new implementor rather than a new arm in six matches.
pub trait Duplex: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Duplex for T {}

/// State shared between one connection attempt's reader and writer.
#[derive(Default)]
struct Pending {
    publishes: BTreeMap<u16, oneshot::Sender<Result<(), MqttError>>>,
    subscribes: BTreeMap<u16, oneshot::Sender<Result<Vec<u8>, MqttError>>>,
}

impl Pending {
    /// Fail everything outstanding. Called on every disconnect, so a caller waiting on an
    /// acknowledgement that can no longer arrive is told rather than left to its timeout — which
    /// is the difference between a dispatcher retrying in a second and one retrying in fifteen.
    fn fail_all(&mut self, why: &str) {
        for (_, tx) in core::mem::take(&mut self.publishes) {
            let _ = tx.send(Err(MqttError::Disconnected(why.to_string())));
        }
        for (_, tx) in core::mem::take(&mut self.subscribes) {
            let _ = tx.send(Err(MqttError::Disconnected(why.to_string())));
        }
    }
}

async fn run(
    address: BrokerAddress,
    config: ConnectionConfig,
    subscriptions: Vec<(String, QoS)>,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Incoming>,
    connected: Arc<AtomicBool>,
) {
    let mut backoff = Duration::from_millis(250);
    let pending: Arc<Mutex<Pending>> = Arc::default();

    loop {
        match session(
            &address,
            &config,
            &subscriptions,
            &mut commands,
            &events,
            &pending,
            &connected,
        )
        .await
        {
            Ok(()) => {
                // The command channel closed: every handle is gone and there is nothing to serve.
                return;
            }
            Err(e) => {
                connected.store(false, Ordering::Relaxed);
                pending.lock().await.fail_all(&e.to_string());
                let _ = events.send(Incoming::Disconnected(e.to_string())).await;
                tracing::warn!(error = %e, broker = %address, "MQTT connection lost; reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(config.max_backoff);
            }
        }
    }
}

/// One connection attempt, from `CONNECT` to the socket dying.
async fn session(
    address: &BrokerAddress,
    config: &ConnectionConfig,
    subscriptions: &[(String, QoS)],
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<Incoming>,
    pending: &Arc<Mutex<Pending>>,
    connected: &Arc<AtomicBool>,
) -> Result<(), MqttError> {
    let mut stream = address.connect().await?;

    stream
        .write_all(&codec::encode_connect(&Connect {
            client_id: config.client_id.clone(),
            keep_alive_secs: config.keep_alive.as_secs().clamp(1, u64::from(u16::MAX)) as u16,
            // Never clean. The broker holding QoS 1 state across a blip is what the publisher's
            // acknowledgements and the subscriber's missed messages both depend on — and where it
            // does not hold it, `session_present` says so and the subscriptions are re-sent.
            clean_session: false,
            username: config.username.clone(),
            password: config.password.clone(),
        }))
        .await
        .map_err(|e| MqttError::Disconnected(e.to_string()))?;

    let mut buf = Vec::with_capacity(4096);
    let ack = match read_packet(&mut stream, &mut buf, config.ack_timeout).await? {
        Some(Packet::ConnAck(ack)) => ack,
        _ => {
            return Err(MqttError::Disconnected(
                "the broker did not answer CONNECT with a CONNACK".into(),
            ));
        }
    };
    if !ack.is_accepted() {
        // A refusal is not a transient failure, but returning early still retries — deliberately:
        // a token that expired is refused now and accepted after the operator renews it, and a VTN
        // that gave up would need a restart to notice. The log names the reason so the operator is
        // not reading `NotAuthorized` with nothing to attach it to (D-102).
        return Err(MqttError::Refused(ack.code, ack.reason()));
    }
    connected.store(true, Ordering::Relaxed);
    let _ = events.send(Incoming::Connected(ack)).await;

    // The whole reason `session_present` is decoded. See the module note.
    if !subscriptions.is_empty() && !ack.session_present {
        // `u16::MAX` is reserved for this one packet: it is sent before any caller can reach the
        // command channel for this session, and nothing waits on its `SUBACK` — a refusal shows up
        // as a topic that never delivers, which `subscribe()`'s returned codes are for.
        let packet = codec::encode_subscribe(u16::MAX, subscriptions);
        stream
            .write_all(&packet)
            .await
            .map_err(|e| MqttError::Disconnected(e.to_string()))?;
        tracing::info!(
            filters = subscriptions.len(),
            "the broker restored no session; re-subscribing"
        );
    }

    let mut ping = tokio::time::interval(config.keep_alive);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // the first tick is immediate

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    // Every handle is gone. Say goodbye rather than dropping the socket: a broker
                    // that sees a TCP reset treats the session as having died and may publish the
                    // client's will, while `DISCONNECT` says this was deliberate
                    // `[MQTT 3.1.1 §3.14]`. Best-effort — the socket may already be gone.
                    let _ = write(&mut stream, &codec::encode_disconnect()).await;
                    let _ = stream.shutdown().await;
                    return Ok(());
                };
                dispatch(command, &mut stream, pending).await?;
            }
            packet = read_packet(&mut stream, &mut buf, config.ack_timeout * 100) => {
                match packet? {
                    Some(Packet::PubAck(id)) => {
                        if let Some(tx) = pending.lock().await.publishes.remove(&id) {
                            let _ = tx.send(Ok(()));
                        }
                    }
                    Some(Packet::SubAck { packet_id, codes }) => {
                        if let Some(tx) = pending.lock().await.subscribes.remove(&packet_id) {
                            let _ = tx.send(Ok(codes));
                        }
                    }
                    Some(Packet::Publish(publish)) => {
                        if let Some(id) = publish.packet_id {
                            stream.write_all(&codec::encode_puback(id)).await
                                .map_err(|e| MqttError::Disconnected(e.to_string()))?;
                        }
                        let _ = events
                            .send(Incoming::Message {
                                topic: publish.topic,
                                payload: publish.payload,
                            })
                            .await;
                    }
                    Some(Packet::ConnAck(_)) => {
                        return Err(MqttError::Disconnected(
                            "the broker sent a second CONNACK".into(),
                        ));
                    }
                    Some(Packet::PingResp) | None => {}
                }
            }
            _ = ping.tick() => {
                stream.write_all(&codec::encode_pingreq()).await
                    .map_err(|e| MqttError::Disconnected(e.to_string()))?;
            }
        }
    }
}

async fn dispatch(
    command: Command,
    stream: &mut Stream,
    pending: &Arc<Mutex<Pending>>,
) -> Result<(), MqttError> {
    match command {
        // QoS 0 has nothing to acknowledge: the publish is complete when the bytes reach the
        // socket, which is what that quality of service means.
        Command::Publish { publish, done } if publish.qos == QoS::AtMostOnce => {
            let result = write(stream, &codec::encode_publish(&publish)).await;
            let _ = done.send(result.clone());
            result
        }
        Command::Publish { publish, done } => {
            let id = publish
                .packet_id
                .expect("a QoS 1 publish carries an identifier");
            let bytes = codec::encode_publish(&publish);
            // Registered *before* the write, so a broker fast enough to acknowledge while the
            // write is still in the kernel has somewhere to land its answer. The previous
            // transport's whole two-phase protocol existed because it could not do this.
            pending.lock().await.publishes.insert(id, done);
            if let Err(e) = write(stream, &bytes).await {
                if let Some(tx) = pending.lock().await.publishes.remove(&id) {
                    let _ = tx.send(Err(e.clone()));
                }
                return Err(e);
            }
            Ok(())
        }
        Command::Subscribe {
            packet_id,
            filters,
            done,
        } => {
            let bytes = codec::encode_subscribe(packet_id, &filters);
            pending.lock().await.subscribes.insert(packet_id, done);
            if let Err(e) = write(stream, &bytes).await {
                if let Some(tx) = pending.lock().await.subscribes.remove(&packet_id) {
                    let _ = tx.send(Err(e.clone()));
                }
                return Err(e);
            }
            Ok(())
        }
    }
}

async fn write(stream: &mut Stream, bytes: &[u8]) -> Result<(), MqttError> {
    stream
        .write_all(bytes)
        .await
        .map_err(|e| MqttError::Disconnected(e.to_string()))
}

/// Read until one packet is complete, or the deadline passes.
///
/// `Ok(None)` is a well-framed packet this client does not model — consumed and ignored.
async fn read_packet(
    stream: &mut Stream,
    buf: &mut Vec<u8>,
    timeout: Duration,
) -> Result<Option<Packet>, MqttError> {
    loop {
        match codec::decode(buf) {
            Ok(Some((packet, used))) => {
                buf.drain(..used);
                return Ok(packet);
            }
            Ok(None) => {}
            Err(e) => return Err(MqttError::Protocol(e)),
        }
        let mut chunk = [0u8; 4096];
        let n = tokio::time::timeout(timeout, stream.read(&mut chunk))
            .await
            .map_err(|_| MqttError::Disconnected("the broker went quiet".into()))?
            .map_err(|e| MqttError::Disconnected(e.to_string()))?;
        if n == 0 {
            return Err(MqttError::Disconnected(
                "the broker closed the connection".into(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}
