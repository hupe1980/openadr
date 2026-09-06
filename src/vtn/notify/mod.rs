//! Push delivery.
//!
//! One change of state produces one notification, which then fans out to whoever is entitled to it:
//! webhook subscribers, MQTT topics, or nothing at all. The fan-out is where object privacy is
//! easiest to get wrong — publishing an event to a shared topic hands every VEN its competitors'
//! dispatch — so recipients are computed here, through the same [`Access`] the read path uses.

use async_trait::async_trait;
use std::sync::{Arc, Mutex};

mod callback;
#[cfg(feature = "mqtt")]
mod mqtt;
#[cfg(feature = "webhook")]
mod webhook;

pub use callback::{CallbackPolicy, CallbackRejected, is_private};
#[cfg(feature = "mqtt")]
#[cfg_attr(docsrs, doc(cfg(feature = "mqtt")))]
pub use mqtt::{MqttConfig, MqttError, MqttNotifier};
#[cfg(feature = "webhook")]
#[cfg_attr(docsrs, doc(cfg(feature = "webhook")))]
pub use webhook::{WebhookConfig, WebhookError, WebhookNotifier};

use crate::core::{Access, Role};
use crate::model::{
    ClientId, ObjectId, ObjectType, Operation, Subscription, Target,
    notification::{AnyObject, Notification},
};

/// Which transport a queued delivery belongs to.
///
/// A delivery is claimed by exactly one, which is what lets several transports share one outbox
/// without either of them guessing. Before this was a type, a webhook transport handed an MQTT
/// entry back as *delivered* — the queue row was removed and nothing had been published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// An HTTP callback named by a REST subscription.
    Webhook,
    /// A topic on a message broker.
    Mqtt,
}

impl core::fmt::Display for Channel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Channel::Webhook => "webhook",
            Channel::Mqtt => "mqtt",
        })
    }
}

/// Where a notification is going, and what the transport needs to send it there.
///
/// A sum rather than three `Option`s: "a callback URL and a topic" and "neither" were both
/// representable before, and the second one was silently treated as success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// POST to a subscriber's callback.
    Webhook {
        /// Where to POST.
        callback_url: String,
        /// The token the receiver expects, if it asked for one.
        bearer_token: Option<String>,
    },
    /// Publish to a broker topic.
    Topic {
        /// The topic name, as the `/notifiers/.../topics/...` endpoints report it.
        topic: String,
    },
}

impl Route {
    /// Which transport owns this route.
    pub fn channel(&self) -> Channel {
        match self {
            Route::Webhook { .. } => Channel::Webhook,
            Route::Topic { .. } => Channel::Mqtt,
        }
    }

    /// The callback URL, for a webhook route.
    pub fn callback_url(&self) -> Option<&str> {
        match self {
            Route::Webhook { callback_url, .. } => Some(callback_url),
            Route::Topic { .. } => None,
        }
    }

    /// The bearer token, for a webhook route that asked for one.
    pub fn bearer_token(&self) -> Option<&str> {
        match self {
            Route::Webhook { bearer_token, .. } => bearer_token.as_deref(),
            Route::Topic { .. } => None,
        }
    }

    /// The topic, for a broker route.
    pub fn topic(&self) -> Option<&str> {
        match self {
            Route::Topic { topic } => Some(topic),
            Route::Webhook { .. } => None,
        }
    }
}

/// A notification bound for one recipient.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    /// The subscription that asked for it, if this came from the REST subscription mechanism.
    pub subscription_id: Option<ObjectId>,
    /// Where it is going.
    pub route: Route,
    /// The body.
    pub notification: Notification,
}

impl Delivery {
    /// Which transport owns this delivery.
    pub fn channel(&self) -> Channel {
        self.route.channel()
    }
}

/// Why one delivery attempt did not succeed.
///
/// The dispatcher makes exactly one decision from this — try again, or stop — so the type carries
/// exactly that, plus a message for the operator. Anything richer would be a transport's private
/// vocabulary leaking into the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryFailure {
    /// What went wrong, for logs and for the outbox row.
    pub message: String,
    /// Whether trying again could plausibly work.
    ///
    /// A refused connection or a `503` is worth another attempt. A `400` is the receiver saying
    /// "not this, ever", and retrying it eight times only generates traffic and delays the point
    /// at which an operator sees the entry in the dead count.
    pub retriable: bool,
}

impl DeliveryFailure {
    /// A failure worth another attempt.
    pub fn retriable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retriable: true,
        }
    }

    /// A failure that no number of attempts would fix.
    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retriable: false,
        }
    }
}

impl core::fmt::Display for DeliveryFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DeliveryFailure {}

/// Where notifications go.
///
/// A transport reports whether one attempt succeeded and does *not* retry: scheduling the next
/// attempt belongs to the dispatcher, which has the durable record of how many have been made
/// (see [`crate::vtn::Dispatcher`]). A transport that retried internally would hold a lease open
/// for the whole sequence and would lose its count on restart.
#[async_trait]
pub trait Notifier: Send + Sync + 'static {
    /// Which channels this transport can actually send on.
    ///
    /// The dispatcher asks before it hands anything over, so a delivery no transport claims is
    /// *recorded as undeliverable* rather than quietly completed — which is what a VTN advertising
    /// an MQTT binding with no publisher behind it would otherwise report.
    fn handles(&self, channel: Channel) -> bool;

    /// Attempt one delivery.
    ///
    /// Only ever called with a delivery whose channel [`Notifier::handles`] accepted.
    ///
    /// `attempt` is 1 for the first try. It exists so a transport can tell the receiver which try
    /// this is — the whole point of the header being there is that a receiver can recognise a
    /// repeat, and a transport that does not know cannot say.
    async fn deliver(&self, delivery: &Delivery, attempt: u32) -> Result<(), DeliveryFailure>;

    /// Prove that whoever named a callback URL controls it, before the VTN will ever post to it.
    ///
    /// `[Def §Webhooks]` requires this when a subscription is created: without it, a client could
    /// point a subscription at a third party and have the VTN deliver to it — an amplifier with the
    /// VTN's own network position behind it.
    ///
    /// The default accepts, because a transport that never posts to a callback has nothing to
    /// prove; [`WebhookNotifier`] overrides it with the echo challenge.
    async fn verify_callback(&self, _callback_url: &str) -> Result<(), DeliveryFailure> {
        Ok(())
    }

    /// A human-readable name, for logs.
    fn name(&self) -> &'static str;
}

/// Drops everything. The default, so a VTN with no push configured still runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullNotifier;

#[async_trait]
impl Notifier for NullNotifier {
    // Claims everything, because dropping everything is the whole contract. A VTN with no push
    // configured queues nothing in the first place; one that queues and installs this has said so.
    fn handles(&self, _channel: Channel) -> bool {
        true
    }
    async fn deliver(&self, _delivery: &Delivery, _attempt: u32) -> Result<(), DeliveryFailure> {
        Ok(())
    }
    fn name(&self) -> &'static str {
        "null"
    }
}

/// Records deliveries in memory. For tests and for `openadr vtn --dry-run`.
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    delivered: Mutex<Vec<Delivery>>,
}

impl RecordingNotifier {
    /// An empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// A shared recorder.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Everything delivered so far.
    pub fn delivered(&self) -> Vec<Delivery> {
        self.delivered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Forget everything.
    pub fn clear(&self) {
        self.delivered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

#[async_trait]
impl Notifier for RecordingNotifier {
    fn handles(&self, _channel: Channel) -> bool {
        true
    }

    async fn deliver(&self, delivery: &Delivery, _attempt: u32) -> Result<(), DeliveryFailure> {
        tracing::debug!(
            object = %delivery.notification.object.object_type(),
            operation = %delivery.notification.operation,
            "notification recorded"
        );
        self.delivered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(delivery.clone());
        Ok(())
    }

    fn name(&self) -> &'static str {
        "recording"
    }
}

/// Several transports behind one [`Notifier`], routed by [`Channel`].
///
/// A VTN that offers webhooks *and* a broker has two transports and one outbox. Something has to
/// decide which entry belongs to which, and the answer has to be exhaustive: an entry nothing
/// claims must be reported as undeliverable, never returned as sent. That is the whole reason this
/// type exists rather than a `Vec` the dispatcher iterates hopefully.
#[derive(Default)]
pub struct Notifiers {
    transports: Vec<Arc<dyn Notifier>>,
}

impl std::fmt::Debug for Notifiers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifiers")
            .field(
                "transports",
                &self.transports.iter().map(|t| t.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Notifiers {
    /// No transports. Every delivery is undeliverable, which is what an empty set means.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a transport. The first one that claims a channel gets its deliveries.
    pub fn with(mut self, notifier: Arc<dyn Notifier>) -> Self {
        self.transports.push(notifier);
        self
    }

    /// Wrap in an `Arc`, ready for [`VtnBuilder::notifier`](crate::vtn::VtnBuilder::notifier).
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Whether anything has been added.
    pub fn is_empty(&self) -> bool {
        self.transports.is_empty()
    }

    fn transport_for(&self, channel: Channel) -> Option<&Arc<dyn Notifier>> {
        self.transports.iter().find(|t| t.handles(channel))
    }
}

#[async_trait]
impl Notifier for Notifiers {
    fn handles(&self, channel: Channel) -> bool {
        self.transport_for(channel).is_some()
    }

    async fn deliver(&self, delivery: &Delivery, attempt: u32) -> Result<(), DeliveryFailure> {
        let channel = delivery.channel();
        match self.transport_for(channel) {
            Some(transport) => transport.deliver(delivery, attempt).await,
            // Permanent, not retriable: no number of attempts installs a transport. The entry
            // lands in the dead count, which is where an operator sees the misconfiguration.
            None => Err(DeliveryFailure::permanent(format!(
                "no transport is configured for {channel} notifications"
            ))),
        }
    }

    async fn verify_callback(&self, callback_url: &str) -> Result<(), DeliveryFailure> {
        match self.transport_for(Channel::Webhook) {
            Some(transport) => transport.verify_callback(callback_url).await,
            // No webhook transport means no subscription will ever be delivered by one, so there is
            // nothing to prove control of.
            None => Ok(()),
        }
    }

    fn name(&self) -> &'static str {
        "notifiers"
    }
}

/// Everything needed to decide who hears about a change, captured before the change is written.
///
/// The point of the type is *where* it can be used. Working out recipients needs two async reads —
/// the subscriptions and the grant sweep — which a storage backend cannot do from inside its own
/// write transaction. Doing them first and handing over the result makes
/// [`Fanout::deliveries`] pure and synchronous, so a backend can write the object and queue its
/// notifications in **one** transaction. A crash can then no longer land between the two.
///
/// The snapshot is taken just before the write, so a subscription created in the intervening
/// microseconds is not told about this change — which is the specified behaviour anyway, since a
/// subscription's conditions are evaluated when the operation happens.
#[derive(Debug, Clone)]
pub struct Fanout {
    subscribers: Vec<(Subscription, Role)>,
    grants: Vec<(ObjectId, ClientId, crate::core::Grant)>,
    topics: Option<Topics>,
    now: crate::model::Timestamp,
}

impl Fanout {
    /// Capture a snapshot.
    pub fn new(
        subscribers: Vec<(Subscription, Role)>,
        grants: Vec<(ObjectId, ClientId, crate::core::Grant)>,
        topics: Option<Topics>,
        now: crate::model::Timestamp,
    ) -> Self {
        Self {
            subscribers,
            grants,
            topics,
            now,
        }
    }

    /// A snapshot with no recipients.
    ///
    /// For an embedder that does not want notifications, and for tests of storage itself. The
    /// timestamp is irrelevant because nothing is ever queued.
    pub fn none() -> Self {
        Self {
            subscribers: Vec::new(),
            grants: Vec::new(),
            topics: None,
            now: crate::model::Timestamp::UNIX_EPOCH,
        }
    }

    /// When the change is being made, used as the queue entry's enqueue time.
    pub fn now(&self) -> crate::model::Timestamp {
        self.now
    }

    /// Whether anything could possibly be delivered.
    ///
    /// Lets a backend skip the work entirely, which is the common case for a VTN with no
    /// subscribers and no broker.
    pub fn is_empty(&self) -> bool {
        self.subscribers.is_empty() && self.topics.is_none()
    }

    /// Who hears about this change, and what each of them is allowed to see.
    ///
    /// Pure and synchronous, so it can be called from inside a write transaction.
    pub fn deliveries(&self, object: &AnyObject, operation: Operation) -> Vec<Delivery> {
        if self.is_empty() {
            return Vec::new();
        }
        let owner = self.owner_of(object);
        let mut out = deliveries_for(object, operation, owner, &self.subscribers);
        if let Some(topics) = &self.topics {
            out.extend(mqtt_deliveries(
                object,
                operation,
                owner,
                topics,
                &self.grants,
            ));
        }
        out
    }

    /// Which client an owned object belongs to.
    ///
    /// A `resource` says which VEN it belongs to but not which client, and the grant snapshot is
    /// the `ven → client` map — which is why this lives on [`Fanout`] rather than on the object.
    /// Getting it wrong is not a small mistake: an owned object with no resolvable owner reaches
    /// either nobody or everybody, depending on which way the comparison falls.
    fn owner_of<'a>(&'a self, object: &'a AnyObject) -> Option<&'a ClientId> {
        match object {
            AnyObject::Ven(v) => Some(&v.client_id),
            AnyObject::Report(r) => r.client_id.as_ref(),
            AnyObject::Subscription(s) => Some(&s.client_id),
            AnyObject::Resource(r) => self
                .grants
                .iter()
                .find(|(ven_id, _, _)| *ven_id == r.ven_id)
                .map(|(_, client_id, _)| client_id),
            _ => None,
        }
    }
}

/// Which subscriptions want a change, and what each of them is allowed to see.
///
/// `[Def §Subscriptions]`: the VTN makes a request to the callback URL when the subscription's
/// conditions are met. This decides *which* callbacks those are; the request itself is the
/// dispatcher's, one transaction later (D-038).
///
/// Stage 3 is the notification half of `[Def §program and event objects - targeting]`. The clause
/// evaluates the rule against the `clientID` behind the subscription, which is why a subscription
/// carries its owner and its owner's *kind* (D-095) — the dispatcher has no credential to ask
/// later.
///
/// Filtering happens in three stages, all of which must pass:
///
/// 1. the subscription's `objectOperations` must name this object type and this operation;
/// 2. the subscription's `programID`, if set, must match the object's;
/// 3. object privacy must admit the subscriber, using the grant behind its `clientID`.
///
/// Stage 3 also decides which targets appear on the notification, so a subscriber never learns of a
/// target group it was not granted.
pub fn deliveries_for(
    object: &AnyObject,
    operation: Operation,
    owner: Option<&ClientId>,
    subscriptions: &[(Subscription, Role)],
) -> Vec<Delivery> {
    let object_type = object.object_type();
    let mut out = Vec::new();

    for (subscription, role) in subscriptions {
        // (2) programme scoping.
        if let Some(wanted) = &subscription.content.program_id
            && object.program_id() != Some(wanted)
        {
            continue;
        }

        let access = Access::push(role.clone(), subscription.content.targets.clone());

        // (3) object privacy. `ven`, `resource`, `report` and `subscription` are gated by
        // ownership; `program` and `event` by targeting.
        let visible = if is_owned_type(object_type) {
            if !access.owns(owner) {
                continue;
            }
            // No target hiding on owned objects `[Def §Object Privacy]`: they are readable only by
            // the one VEN that owns them, so there is nothing to conceal from it.
            object.targets().to_vec()
        } else {
            match access.visible_targets(object.targets()) {
                Some(visible) => visible,
                None => continue,
            }
        };

        // (1) object and operation.
        for op in &subscription.content.object_operations {
            if !op.matches(object_type, operation) {
                continue;
            }
            out.push(Delivery {
                subscription_id: Some(subscription.id.clone()),
                route: Route::Webhook {
                    callback_url: op.callback_url.clone(),
                    bearer_token: op.bearer_token.clone(),
                },
                notification: Notification::new(operation, object.clone())
                    .with_targets(visible.clone()),
            });
        }
    }

    out
}

/// Whether visibility of this object type is decided by ownership rather than targeting.
fn is_owned_type(object_type: ObjectType) -> bool {
    matches!(
        object_type,
        ObjectType::Ven | ObjectType::Resource | ObjectType::Report | ObjectType::Subscription
    )
}

/// MQTT topic names, as published by the `/notifiers/mqtt/topics/...` endpoints.
///
/// The VEN-scoped forms are what keep object privacy intact on a shared broker: the VTN resolves an
/// object's targets to a set of VENs and publishes a copy to each one's private topic, and the
/// broker's access control stops a VEN subscribing to anyone else's.
#[derive(Debug, Clone)]
pub struct Topics {
    prefix: String,
}

impl Topics {
    /// Topics under a prefix (which may be empty).
    pub fn new(prefix: impl Into<String>) -> Self {
        let mut prefix = prefix.into();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        Self { prefix }
    }

    fn join(&self, rest: &str) -> String {
        format!("{}{rest}", self.prefix)
    }

    /// `programs`, `events`, … for every object of a type.
    pub fn collection(&self, object_type: ObjectType) -> String {
        self.join(object_type.collection())
    }

    /// Topic root for one object.
    pub fn object(&self, object_type: ObjectType, id: &ObjectId) -> String {
        self.join(&format!("{}/{id}", object_type.collection()))
    }

    /// Topic root for the events of one programme.
    pub fn program_events(&self, program_id: &ObjectId) -> String {
        self.join(&format!("events/programs/{program_id}"))
    }

    /// Topic root for a VEN's private copy of an object type.
    ///
    /// The one function both sides use: the `/notifiers/mqtt/topics/vens/{venID}/…` endpoints
    /// render it, and the fan-out publishes to it. They were computed separately once, and the VEN
    /// object's own topic came out as `vens/{venID}` from the endpoint and `vens/vens/{venID}` from
    /// the fan-out — so every VEN subscribed to a topic the VTN never published to, and nothing
    /// anywhere reported an error.
    pub fn ven_scoped(&self, object_type: ObjectType, ven_id: &ObjectId) -> String {
        // A VEN's copy of *itself* is addressed by id, not by "the VEN objects belonging to this
        // VEN", which is why it is the one type whose path has no `vens/` segment of its own.
        if object_type == ObjectType::Ven {
            return self.object(ObjectType::Ven, ven_id);
        }
        self.join(&format!("{}/vens/{ven_id}", object_type.collection()))
    }

    /// The full topic for one operation.
    pub fn operation(&self, root: &str, operation: Operation) -> String {
        format!("{root}/{}", operation.topic_segment())
    }
}

/// Build the per-VEN topic fan-out for a targeted object.
pub fn mqtt_deliveries(
    object: &AnyObject,
    operation: Operation,
    owner: Option<&ClientId>,
    topics: &Topics,
    grants: &[(ObjectId, ClientId, crate::core::Grant)],
) -> Vec<Delivery> {
    let object_type = object.object_type();
    let object_targets: Vec<Target> = object.targets().to_vec();

    let publish = |topic: String, targets: Vec<Target>| Delivery {
        subscription_id: None,
        route: Route::Topic { topic },
        notification: Notification::new(operation, object.clone()).with_targets(targets),
    };

    // The collection-wide topic, which only business logic may subscribe to (`read_bl` on the
    // endpoint that names it, and a broker ACL that enforces the same).
    let mut out = vec![publish(
        topics.operation(&topics.collection(object_type), operation),
        object_targets.clone(),
    )];

    // The per-programme topic for events.
    if object_type == ObjectType::Event
        && let Some(program_id) = object.program_id()
    {
        out.push(publish(
            topics.operation(&topics.program_events(program_id), operation),
            object_targets.clone(),
        ));
    }

    // A private copy for every entitled VEN.
    for (ven_id, client_id, grant) in grants {
        let topic = topics.operation(&topics.ven_scoped(object_type, ven_id), operation);
        if is_owned_type(object_type) {
            // Ownership, not targeting. A `report` carries no targets at all, so evaluating the
            // targeting rule here would admit every VEN — which is how one VEN's private topic
            // would come to carry every other VEN's meter data.
            if owner.is_some() && owner == Some(client_id) {
                out.push(publish(topic, object_targets.clone()));
            }
            continue;
        }
        let access = Access::push(
            Role::Ven {
                client_id: client_id.clone(),
                grant: grant.clone(),
            },
            Vec::new(),
        );
        // Carrying only that VEN's own targets, never the object's full set.
        if let Some(visible) = access.visible_targets(&object_targets) {
            out.push(publish(topic, visible));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Grant;
    use crate::model::{
        ClientName, Event, EventRequest, ObjectOperation, Program, ProgramName, ProgramRequest,
        SubscriptionRequest, Timestamp,
    };
    use crate::std_shim::Vec;

    fn now() -> Timestamp {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }

    fn event(targets: &[&str]) -> AnyObject {
        let mut content = EventRequest::new("prg-1".parse().unwrap());
        content.targets = targets.iter().map(|t| Target::new(*t).unwrap()).collect();
        AnyObject::Event(Event {
            id: "evt-1".parse().unwrap(),
            created_date_time: now(),
            modification_date_time: now(),
            object_type: ObjectType::Event,
            content,
        })
    }

    fn subscription(client: &str, targets: &[&str], objects: &[ObjectType]) -> Subscription {
        Subscription {
            id: format!("sub-{client}").parse().unwrap(),
            created_date_time: now(),
            modification_date_time: now(),
            object_type: ObjectType::Subscription,
            client_id: ClientId::new(client).unwrap(),
            content: SubscriptionRequest {
                client_name: ClientName::new(client).unwrap(),
                program_id: None,
                object_operations: vec![ObjectOperation {
                    objects: objects.to_vec(),
                    operations: vec![Operation::Create, Operation::Update],
                    callback_url: format!("https://{client}.example.com/hook"),
                    bearer_token: Some("tok".into()),
                }],
                targets: targets.iter().map(|t| Target::new(*t).unwrap()).collect(),
            },
        }
    }

    fn ven_role(client: &str, grants: &[&str]) -> Role {
        Role::Ven {
            client_id: ClientId::new(client).unwrap(),
            grant: Grant::from_targets(grants.iter().map(|t| Target::new(*t).unwrap())),
        }
    }

    /// A transport that claims one channel and records what it was given.
    #[derive(Debug)]
    struct Claiming {
        channel: Channel,
        seen: Mutex<Vec<String>>,
    }

    impl Claiming {
        fn new(channel: Channel) -> Arc<Self> {
            Arc::new(Self {
                channel,
                seen: Mutex::new(Vec::new()),
            })
        }
        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Notifier for Claiming {
        fn handles(&self, channel: Channel) -> bool {
            channel == self.channel
        }
        async fn deliver(&self, delivery: &Delivery, _attempt: u32) -> Result<(), DeliveryFailure> {
            self.seen
                .lock()
                .unwrap()
                .push(format!("{:?}", delivery.route));
            Ok(())
        }
        fn name(&self) -> &'static str {
            "claiming"
        }
    }

    fn program_object() -> AnyObject {
        AnyObject::Program(Program {
            id: "prg-1".parse().unwrap(),
            created_date_time: now(),
            modification_date_time: now(),
            object_type: ObjectType::Program,
            content: ProgramRequest::new(ProgramName::new("tou").unwrap()),
        })
    }

    fn webhook_delivery() -> Delivery {
        Delivery {
            subscription_id: None,
            route: Route::Webhook {
                callback_url: "https://ven.example.com/hook".into(),
                bearer_token: None,
            },
            notification: Notification::new(Operation::Create, program_object()),
        }
    }

    fn topic_delivery() -> Delivery {
        Delivery {
            subscription_id: None,
            route: Route::Topic {
                topic: "programs/create".into(),
            },
            notification: Notification::new(Operation::Create, program_object()),
        }
    }

    #[tokio::test]
    async fn each_transport_gets_only_the_channel_it_claims() {
        let webhook = Claiming::new(Channel::Webhook);
        let broker = Claiming::new(Channel::Mqtt);
        let notifiers = Notifiers::new().with(webhook.clone()).with(broker.clone());

        notifiers.deliver(&webhook_delivery(), 1).await.unwrap();
        notifiers.deliver(&topic_delivery(), 1).await.unwrap();

        assert_eq!(webhook.seen().len(), 1);
        assert_eq!(broker.seen().len(), 1);
        assert!(webhook.seen()[0].contains("Webhook"));
        assert!(broker.seen()[0].contains("Topic"));
    }

    #[tokio::test]
    async fn a_delivery_no_transport_claims_is_refused_rather_than_completed() {
        // The failure this whole arrangement exists to prevent: a VTN advertising an MQTT binding
        // with no publisher behind it used to have every broker notification handed to the webhook
        // transport, answered `Ok`, and deleted from the outbox. Nothing was published and nothing
        // said so. Now the entry is abandoned, which `GET /health` counts.
        let notifiers = Notifiers::new().with(Claiming::new(Channel::Webhook));
        assert!(!notifiers.handles(Channel::Mqtt));

        let failure = notifiers
            .deliver(&topic_delivery(), 1)
            .await
            .expect_err("an unroutable delivery must not be reported as delivered");
        assert!(
            !failure.retriable,
            "no number of attempts configures a transport"
        );
        assert!(failure.message.contains("mqtt"), "{failure}");
    }

    #[test]
    fn a_targeted_event_reaches_only_entitled_subscribers() {
        let subs = vec![
            (
                subscription("ven-a", &[], &[ObjectType::Event]),
                ven_role("ven-a", &["group1"]),
            ),
            (
                subscription("ven-b", &[], &[ObjectType::Event]),
                ven_role("ven-b", &["group2"]),
            ),
        ];
        let out = deliveries_for(&event(&["group1"]), Operation::Create, None, &subs);
        assert_eq!(out.len(), 1);
        assert!(out[0].route.callback_url().unwrap().contains("ven-a"));
    }

    #[test]
    fn notifications_carry_only_the_subscribers_own_targets() {
        let subs = vec![(
            subscription("ven-a", &[], &[ObjectType::Event]),
            ven_role("ven-a", &["group1"]),
        )];
        let out = deliveries_for(
            &event(&["group1", "group2"]),
            Operation::Update,
            None,
            &subs,
        );
        assert_eq!(
            out[0].notification.targets,
            vec![Target::new("group1").unwrap()],
            "group2 must not be revealed"
        );
    }

    #[test]
    fn a_subscription_for_another_object_type_is_not_woken() {
        let subs = vec![(
            subscription("ven-a", &[], &[ObjectType::Report]),
            ven_role("ven-a", &["group1"]),
        )];
        assert!(deliveries_for(&event(&["group1"]), Operation::Create, None, &subs).is_empty());
    }

    #[test]
    fn an_operation_the_subscription_did_not_ask_for_is_not_delivered() {
        let subs = vec![(
            subscription("ven-a", &[], &[ObjectType::Event]),
            ven_role("ven-a", &["group1"]),
        )];
        assert!(deliveries_for(&event(&["group1"]), Operation::Delete, None, &subs).is_empty());
    }

    #[test]
    fn programme_scoping_is_honoured() {
        let mut sub = subscription("ven-a", &[], &[ObjectType::Event]);
        sub.content.program_id = Some("other-program".parse().unwrap());
        let subs = vec![(sub, ven_role("ven-a", &["group1"]))];
        assert!(deliveries_for(&event(&["group1"]), Operation::Create, None, &subs).is_empty());
    }

    #[test]
    fn business_logic_receives_everything() {
        let subs = vec![(
            subscription("bl", &[], &[ObjectType::Event]),
            Role::BusinessLogic,
        )];
        assert_eq!(
            deliveries_for(&event(&["group1"]), Operation::Create, None, &subs).len(),
            1
        );
    }

    #[test]
    fn untargeted_objects_reach_every_subscriber() {
        let program = AnyObject::Program(Program {
            id: "prg-1".parse().unwrap(),
            created_date_time: now(),
            modification_date_time: now(),
            object_type: ObjectType::Program,
            content: ProgramRequest::new(ProgramName::new("public-tariff").unwrap()),
        });
        let subs = vec![
            (
                subscription("ven-a", &[], &[ObjectType::Program]),
                ven_role("ven-a", &[]),
            ),
            (
                subscription("ven-b", &[], &[ObjectType::Program]),
                ven_role("ven-b", &["group2"]),
            ),
        ];
        assert_eq!(
            deliveries_for(&program, Operation::Update, None, &subs).len(),
            2
        );
    }

    fn report(client: &str) -> AnyObject {
        use crate::model::{Report, ReportRequest};
        AnyObject::Report(Report {
            id: "rpt-1".parse().unwrap(),
            created_date_time: now(),
            modification_date_time: now(),
            object_type: ObjectType::Report,
            client_id: Some(ClientId::new(client).unwrap()),
            content: ReportRequest::new(
                "evt-1".parse().unwrap(),
                ClientName::new(client).unwrap(),
                Vec::new(),
            ),
        })
    }

    fn resource(ven_id: &str) -> AnyObject {
        AnyObject::Resource(crate::model::Resource {
            id: "res-1".parse().unwrap(),
            created_date_time: now(),
            modification_date_time: now(),
            object_type: ObjectType::Resource,
            resource_name: "meter".parse().unwrap(),
            ven_id: ven_id.parse().unwrap(),
            targets: Vec::new(),
            attributes: None,
        })
    }

    fn two_vens() -> Vec<(ObjectId, ClientId, Grant)> {
        vec![
            (
                ObjectId::new("ven-a").unwrap(),
                ClientId::new("client-a").unwrap(),
                Grant::from_targets([Target::new("group1").unwrap()]),
            ),
            (
                ObjectId::new("ven-b").unwrap(),
                ClientId::new("client-b").unwrap(),
                Grant::from_targets([Target::new("group2").unwrap()]),
            ),
        ]
    }

    #[test]
    fn an_owned_object_reaches_only_its_owners_private_topic() {
        // A report carries no targets, so evaluating the *targeting* rule here admits every VEN —
        // which put one VEN's meter data on every other VEN's private topic.
        let fanout = Fanout::new(Vec::new(), two_vens(), Some(Topics::new("openadr3")), now());
        let published: Vec<String> = fanout
            .deliveries(&report("client-a"), Operation::Create)
            .into_iter()
            .filter_map(|d| d.route.topic().map(str::to_string))
            .collect();
        assert!(published.contains(&"openadr3/reports/vens/ven-a/create".to_string()));
        assert!(
            !published.contains(&"openadr3/reports/vens/ven-b/create".to_string()),
            "client-b must not receive client-a's report: {published:?}"
        );
    }

    #[test]
    fn an_owned_object_with_no_resolvable_owner_reaches_no_private_topic() {
        // Fail closed: a report whose `clientID` the VTN never stamped belongs to nobody.
        use crate::model::{Report, ReportRequest};
        let orphan = AnyObject::Report(Report {
            id: "rpt-2".parse().unwrap(),
            created_date_time: now(),
            modification_date_time: now(),
            object_type: ObjectType::Report,
            client_id: None,
            content: ReportRequest::new(
                "evt-1".parse().unwrap(),
                ClientName::new("who").unwrap(),
                Vec::new(),
            ),
        });
        let fanout = Fanout::new(Vec::new(), two_vens(), Some(Topics::new("openadr3")), now());
        let published: Vec<String> = fanout
            .deliveries(&orphan, Operation::Create)
            .into_iter()
            .filter_map(|d| d.route.topic().map(str::to_string))
            .collect();
        assert_eq!(published, vec!["openadr3/reports/create".to_string()]);
    }

    #[test]
    fn a_ven_is_told_about_its_own_resource() {
        // A resource names its VEN, not its client; the grant snapshot is the map between them.
        // Without it every resource notification was dropped, so a VEN was never told that
        // business logic had changed one of its own.
        let subs = vec![
            (
                subscription("client-a", &[], &[ObjectType::Resource]),
                ven_role("client-a", &[]),
            ),
            (
                subscription("client-b", &[], &[ObjectType::Resource]),
                ven_role("client-b", &[]),
            ),
        ];
        let fanout = Fanout::new(subs, two_vens(), None, now());
        let out = fanout.deliveries(&resource("ven-a"), Operation::Update);
        assert_eq!(out.len(), 1);
        assert!(out[0].route.callback_url().unwrap().contains("client-a"));
    }

    #[test]
    fn mqtt_fan_out_gives_each_ven_a_private_topic() {
        let topics = Topics::new("openadr3/3.1.0");
        let grants = vec![
            (
                ObjectId::new("ven-a").unwrap(),
                ClientId::new("client-a").unwrap(),
                Grant::from_targets([Target::new("group1").unwrap()]),
            ),
            (
                ObjectId::new("ven-b").unwrap(),
                ClientId::new("client-b").unwrap(),
                Grant::from_targets([Target::new("group2").unwrap()]),
            ),
        ];
        let out = mqtt_deliveries(
            &event(&["group1"]),
            Operation::Create,
            None,
            &topics,
            &grants,
        );
        let published: Vec<&str> = out.iter().filter_map(|d| d.route.topic()).collect();

        assert!(published.contains(&"openadr3/3.1.0/events/create"));
        assert!(published.contains(&"openadr3/3.1.0/events/programs/prg-1/create"));
        assert!(published.contains(&"openadr3/3.1.0/events/vens/ven-a/create"));
        assert!(
            !published.contains(&"openadr3/3.1.0/events/vens/ven-b/create"),
            "ven-b is not in group1 and must not receive a copy"
        );
    }
}
