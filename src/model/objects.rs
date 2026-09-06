//! The six addressable objects, their request bodies, and the pieces they share.
//!
//! Every object follows the same shape as the specification's `allOf` composition: VTN-provisioned
//! metadata (`id`, timestamps, `objectType`) plus the client-provided request body, which is a
//! separate type so that a `POST` body cannot even name a field the client is not allowed to set.

use crate::std_shim::{String, Vec};
use core::{cmp::Ordering, fmt, str::FromStr};
use serde::{Deserialize, Serialize};

use super::{
    ids::{ClientId, ClientName, ObjectId, ProgramName, ResourceName, Target, VenName},
    time::{Duration, StartTime, Timestamp},
    values::{PayloadType, ReadingType, Unit, ValuesMap},
};

/// The kinds of object addressable through the API (`objectTypes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ObjectType {
    /// A demand-response programme or tariff.
    Program,
    /// A demand-response event.
    Event,
    /// A report submitted by a VEN.
    Report,
    /// A webhook subscription.
    Subscription,
    /// A virtual end node.
    Ven,
    /// A device or system behind a VEN.
    Resource,
}

impl ObjectType {
    /// The wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            ObjectType::Program => "PROGRAM",
            ObjectType::Event => "EVENT",
            ObjectType::Report => "REPORT",
            ObjectType::Subscription => "SUBSCRIPTION",
            ObjectType::Ven => "VEN",
            ObjectType::Resource => "RESOURCE",
        }
    }

    /// The collection path segment, e.g. `programs`.
    pub const fn collection(self) -> &'static str {
        match self {
            ObjectType::Program => "programs",
            ObjectType::Event => "events",
            ObjectType::Report => "reports",
            ObjectType::Subscription => "subscriptions",
            ObjectType::Ven => "vens",
            ObjectType::Resource => "resources",
        }
    }

    /// Every variant, for iteration.
    pub const ALL: [ObjectType; 6] = [
        ObjectType::Program,
        ObjectType::Event,
        ObjectType::Report,
        ObjectType::Subscription,
        ObjectType::Ven,
        ObjectType::Resource,
    ];

    /// Every object type a write to this one may announce, its cascade included.
    ///
    /// Deleting a programme takes its events, their reports, and the subscriptions scoped to it —
    /// and each of those removals is announced, because nothing in OpenADR can say afterwards that
    /// an object went away. So a VTN working out who to tell about a write to *this* type has to
    /// consider subscribers watching any of *these*.
    ///
    /// Always contains `self`, which a test asserts: a type missing from its own list would mean a
    /// write announcing nothing, silently, on the one path where silence is the failure.
    pub const fn announces(self) -> &'static [ObjectType] {
        match self {
            ObjectType::Program => &[
                ObjectType::Program,
                ObjectType::Event,
                ObjectType::Report,
                ObjectType::Subscription,
            ],
            ObjectType::Event => &[ObjectType::Event, ObjectType::Report],
            ObjectType::Ven => &[ObjectType::Ven, ObjectType::Resource],
            ObjectType::Report => &[ObjectType::Report],
            ObjectType::Subscription => &[ObjectType::Subscription],
            ObjectType::Resource => &[ObjectType::Resource],
        }
    }
}

impl fmt::Display for ObjectType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ObjectType {
    type Err = UnknownObjectType;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "PROGRAM" => Ok(ObjectType::Program),
            "EVENT" => Ok(ObjectType::Event),
            "REPORT" => Ok(ObjectType::Report),
            "SUBSCRIPTION" => Ok(ObjectType::Subscription),
            "VEN" => Ok(ObjectType::Ven),
            "RESOURCE" => Ok(ObjectType::Resource),
            _ => Err(UnknownObjectType),
        }
    }
}

/// The string was not one of the six object types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unknown object type")]
pub struct UnknownObjectType;

/// Metadata the VTN provisions on every addressable object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMetadata {
    /// VTN-assigned identifier.
    pub id: ObjectId,
    /// When the VTN created the object.
    pub created_date_time: Timestamp,
    /// When the VTN last modified the object.
    pub modification_date_time: Timestamp,
    /// Discriminator.
    pub object_type: ObjectType,
}

// ---------------------------------------------------------------------------
// Interval scaffolding
// ---------------------------------------------------------------------------

/// The temporal aspects of an interval, or the defaults for an event's intervals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IntervalPeriod {
    /// Start of the interval, or `0001-01-01` for "now".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<StartTime>,
    /// Length of the interval, or `P9999Y` for "no end".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<Duration>,
    /// Maximum offset a client may apply to `start`, in either direction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub randomize_start: Option<Duration>,
}

impl IntervalPeriod {
    /// An interval period with an absolute start and a duration.
    pub fn new(start: StartTime, duration: Duration) -> Self {
        Self {
            start: Some(start),
            duration: Some(duration),
            randomize_start: None,
        }
    }

    /// Apply a maximum start randomization.
    pub fn with_randomize_start(mut self, d: Duration) -> Self {
        self.randomize_start = Some(d);
        self
    }
}

/// A temporal window and the values that apply during it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interval {
    /// Client-assigned identifier, used to correlate report intervals with event intervals.
    ///
    /// Not a sequence number: the specification is explicit that it need not ascend.
    pub id: i32,
    /// Overrides the containing object's `intervalPeriod`, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_period: Option<IntervalPeriod>,
    /// The payloads that apply during this interval.
    pub payloads: Vec<ValuesMap>,
}

impl Interval {
    /// An interval carrying payloads and inheriting its timing from its parent.
    pub fn new(id: i32, payloads: Vec<ValuesMap>) -> Self {
        Self {
            id,
            interval_period: None,
            payloads,
        }
    }

    /// Give this interval its own timing.
    pub fn with_period(mut self, period: IntervalPeriod) -> Self {
        self.interval_period = Some(period);
        self
    }
}

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

/// Context needed to interpret event payload values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventPayloadDescriptor {
    /// Discriminator; always `EVENT_PAYLOAD_DESCRIPTOR`.
    #[serde(default = "event_payload_descriptor_tag")]
    pub object_type: String,
    /// The payload type this descriptor describes.
    pub payload_type: PayloadType,
    /// Unit of measure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<Unit>,
    /// ISO 4217 currency, for price payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
}

fn event_payload_descriptor_tag() -> String {
    "EVENT_PAYLOAD_DESCRIPTOR".into()
}

impl EventPayloadDescriptor {
    /// A descriptor for a payload type.
    pub fn new(payload_type: PayloadType) -> Self {
        Self {
            object_type: event_payload_descriptor_tag(),
            payload_type,
            units: None,
            currency: None,
        }
    }

    /// Set the unit of measure.
    pub fn with_units(mut self, units: Unit) -> Self {
        self.units = Some(units);
        self
    }

    /// Set the currency.
    pub fn with_currency(mut self, currency: impl Into<String>) -> Self {
        self.currency = Some(currency.into());
        self
    }
}

/// Context needed to interpret report payload values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportPayloadDescriptor {
    /// Discriminator; always `REPORT_PAYLOAD_DESCRIPTOR`.
    #[serde(default = "report_payload_descriptor_tag")]
    pub object_type: String,
    /// The payload type this descriptor describes.
    pub payload_type: PayloadType,
    /// How the value was obtained, e.g. [`ReadingType::DirectRead`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading_type: Option<ReadingType>,
    /// Unit of measure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<Unit>,
    /// Quantified accuracy of the values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accuracy: Option<f32>,
    /// Confidence in the values, 0..=100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<u8>,
}

fn report_payload_descriptor_tag() -> String {
    "REPORT_PAYLOAD_DESCRIPTOR".into()
}

impl ReportPayloadDescriptor {
    /// A descriptor for a payload type.
    pub fn new(payload_type: PayloadType) -> Self {
        Self {
            object_type: report_payload_descriptor_tag(),
            payload_type,
            reading_type: None,
            units: None,
            accuracy: None,
            confidence: None,
        }
    }
}

/// A payload descriptor of either flavour, discriminated by `objectType`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "objectType")]
pub enum PayloadDescriptor {
    /// Describes event payloads.
    #[serde(rename = "EVENT_PAYLOAD_DESCRIPTOR")]
    Event(EventPayloadDescriptor),
    /// Describes report payloads.
    #[serde(rename = "REPORT_PAYLOAD_DESCRIPTOR")]
    Report(ReportPayloadDescriptor),
}

/// Which intervals a report should carry relative to the event's intervals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReportIntervals {
    /// Mirror the event's intervals exactly.
    #[default]
    Intervals,
    /// The VEN may subdivide event intervals.
    SubIntervals,
    /// The VEN chooses its own intervals.
    OpenIntervals,
}

/// A request from the VTN for a VEN to produce reports.
///
/// The `-1` sentinels are the specification's, kept verbatim so that a round trip is lossless;
/// [`crate::core::ReportSchedule`] interprets them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportDescriptor {
    /// The payload type being requested.
    pub payload_type: PayloadType,
    /// How the value should be obtained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading_type: Option<ReadingType>,
    /// Unit of measure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<Unit>,
    /// Restrict the report to matching resources.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::null_as_default"
    )]
    pub targets: Vec<Target>,
    /// Aggregate all targeted resources into one series instead of reporting each.
    #[serde(default)]
    pub aggregate: bool,
    /// Interval at which to generate a report; `-1` means "at the end of the last interval".
    #[serde(default = "minus_one")]
    pub start_interval: i32,
    /// Number of intervals per report; `-1` means "all".
    #[serde(default = "minus_one")]
    pub num_intervals: i32,
    /// `true` reports intervals preceding `startInterval`, `false` those following (a forecast).
    #[serde(default = "bool_true")]
    pub historical: bool,
    /// Intervals between reports; `-1` means "same as numIntervals", `0` means ad hoc.
    #[serde(default = "minus_one")]
    pub frequency: i32,
    /// How many reports to produce; `-1` repeats indefinitely.
    #[serde(default = "one")]
    pub repeat: i32,
    /// Whether the VEN may subdivide or replace the event's intervals.
    #[serde(default)]
    pub report_intervals: ReportIntervals,
}

const fn minus_one() -> i32 {
    -1
}
const fn one() -> i32 {
    1
}
const fn bool_true() -> bool {
    true
}

impl ReportDescriptor {
    /// A descriptor with every schema default in place.
    pub fn new(payload_type: PayloadType) -> Self {
        Self {
            payload_type,
            reading_type: None,
            units: None,
            targets: Vec::new(),
            aggregate: false,
            start_interval: -1,
            num_intervals: -1,
            historical: true,
            frequency: -1,
            repeat: 1,
            report_intervals: ReportIntervals::default(),
        }
    }

    /// Whether the VEN decides when to report (`frequency == 0`).
    pub fn is_ad_hoc(&self) -> bool {
        self.frequency == 0
    }

    /// Whether reporting repeats without end (`repeat == -1`).
    pub fn repeats_forever(&self) -> bool {
        self.repeat < 0
    }
}

// ---------------------------------------------------------------------------
// Program
// ---------------------------------------------------------------------------

/// A link to a human- or machine-readable description of a programme.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgramDescription {
    /// The URL.
    #[serde(rename = "URL")]
    pub url: String,
}

/// Client-provided programme description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgramRequest {
    /// Short name, unique to the VTN.
    pub program_name: ProgramName,
    /// The temporal span of the programme, which may be years long.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_period: Option<IntervalPeriod>,
    /// Links to descriptions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_descriptions: Option<Vec<ProgramDescription>>,
    /// Descriptors giving payloads their context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_descriptors: Option<Vec<PayloadDescriptor>>,
    /// Programme attributes, e.g. `RETAILER_NAME` (3.1 replaced the flat fields with these).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Vec<ValuesMap>>,
    /// Targets that gate which VENs may read this programme.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::null_as_default"
    )]
    pub targets: Vec<Target>,
}

impl ProgramRequest {
    /// A programme with only its required field set.
    pub fn new(program_name: ProgramName) -> Self {
        Self {
            program_name,
            interval_period: None,
            program_descriptions: None,
            payload_descriptors: None,
            attributes: None,
            targets: Vec::new(),
        }
    }

    /// Set the targets.
    pub fn with_targets(mut self, targets: Vec<Target>) -> Self {
        self.targets = targets;
        self
    }
}

/// Server-provided programme.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Program {
    /// VTN-assigned identifier.
    pub id: ObjectId,
    /// Creation time.
    pub created_date_time: Timestamp,
    /// Last modification time.
    pub modification_date_time: Timestamp,
    /// Discriminator; always [`ObjectType::Program`].
    #[serde(default = "program_type")]
    pub object_type: ObjectType,
    /// The client-provided content.
    #[serde(flatten)]
    pub content: ProgramRequest,
}

fn program_type() -> ObjectType {
    ObjectType::Program
}

// ---------------------------------------------------------------------------
// Event
// ---------------------------------------------------------------------------

/// Relative priority of an event; a lower number is a higher priority.
///
/// `None` means unprioritised, which sorts *after* every explicit priority so that an event with a
/// stated priority always wins on a timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Priority(pub Option<u32>);

impl Priority {
    /// The highest priority the schema allows.
    pub const MAX: Priority = Priority(Some(0));
    /// No stated priority.
    pub const UNSPECIFIED: Priority = Priority(None);

    /// A stated priority.
    pub const fn new(value: u32) -> Self {
        Priority(Some(value))
    }

    /// The numeric value, if stated.
    pub const fn value(self) -> Option<u32> {
        self.0
    }
}

impl Ord for Priority {
    fn cmp(&self, other: &Self) -> Ordering {
        // Ordering is "most important first": Some(0) < Some(1) < None.
        match (self.0, other.0) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    }
}

impl PartialOrd for Priority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Client-provided event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRequest {
    /// The programme this event belongs to.
    #[serde(rename = "programID")]
    pub program_id: ObjectId,
    /// Free-form name for debugging and user interfaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_name: Option<String>,
    /// Overall event duration; may extend or truncate the intervals (User Guide §7.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<Duration>,
    /// Relative priority.
    #[serde(default, skip_serializing_if = "Priority::is_unspecified")]
    pub priority: Priority,
    /// Targets that gate which VENs may read this event.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::null_as_default"
    )]
    pub targets: Vec<Target>,
    /// Reports requested from VENs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_descriptors: Option<Vec<ReportDescriptor>>,
    /// Descriptors giving payloads their context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_descriptors: Option<Vec<EventPayloadDescriptor>>,
    /// Default timing for the intervals below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_period: Option<IntervalPeriod>,
    /// The intervals. Absent for a "report-only" event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intervals: Option<Vec<Interval>>,
}

impl Priority {
    fn is_unspecified(&self) -> bool {
        self.0.is_none()
    }
}

impl EventRequest {
    /// An event with only its required field set.
    pub fn new(program_id: ObjectId) -> Self {
        Self {
            program_id,
            event_name: None,
            duration: None,
            priority: Priority::UNSPECIFIED,
            targets: Vec::new(),
            report_descriptors: None,
            payload_descriptors: None,
            interval_period: None,
            intervals: None,
        }
    }

    /// Set the intervals.
    pub fn with_intervals(mut self, intervals: Vec<Interval>) -> Self {
        self.intervals = Some(intervals);
        self
    }

    /// Set the default interval period.
    pub fn with_interval_period(mut self, period: IntervalPeriod) -> Self {
        self.interval_period = Some(period);
        self
    }

    /// Set the targets.
    pub fn with_targets(mut self, targets: Vec<Target>) -> Self {
        self.targets = targets;
        self
    }

    /// Set the priority.
    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }
}

/// Server-provided event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    /// VTN-assigned identifier.
    pub id: ObjectId,
    /// Creation time.
    pub created_date_time: Timestamp,
    /// Last modification time.
    pub modification_date_time: Timestamp,
    /// Discriminator; always [`ObjectType::Event`].
    #[serde(default = "event_type")]
    pub object_type: ObjectType,
    /// The client-provided content.
    #[serde(flatten)]
    pub content: EventRequest,
}

fn event_type() -> ObjectType {
    ObjectType::Event
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// Report data for one resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportResource {
    /// The resource, or [`ResourceName::AGGREGATED`] for an aggregate series.
    pub resource_name: ResourceName,
    /// Default timing for the intervals below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_period: Option<IntervalPeriod>,
    /// The reported intervals.
    pub intervals: Vec<Interval>,
}

/// Client-provided report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportRequest {
    /// The event this report answers.
    #[serde(rename = "eventID")]
    pub event_id: ObjectId,
    /// Name of the reporting client.
    pub client_name: ClientName,
    /// Free-form name for debugging and user interfaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_name: Option<String>,
    /// Descriptors giving payloads their context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_descriptors: Option<Vec<ReportPayloadDescriptor>>,
    /// Per-resource data.
    pub resources: Vec<ReportResource>,
}

impl ReportRequest {
    /// A report with its required fields set.
    pub fn new(
        event_id: ObjectId,
        client_name: ClientName,
        resources: Vec<ReportResource>,
    ) -> Self {
        Self {
            event_id,
            client_name,
            report_name: None,
            payload_descriptors: None,
            resources,
        }
    }
}

/// Server-provided report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// VTN-assigned identifier.
    pub id: ObjectId,
    /// Creation time.
    pub created_date_time: Timestamp,
    /// Last modification time.
    pub modification_date_time: Timestamp,
    /// Discriminator; always [`ObjectType::Report`].
    #[serde(default = "report_type")]
    pub object_type: ObjectType,
    /// Identity of the client that created the report; stamped by the VTN.
    #[serde(rename = "clientID", default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<ClientId>,
    /// The client-provided content.
    #[serde(flatten)]
    pub content: ReportRequest,
}

fn report_type() -> ObjectType {
    ObjectType::Report
}

// ---------------------------------------------------------------------------
// Subscription
// ---------------------------------------------------------------------------

/// An operation on an object that can trigger a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Operation {
    /// The object was read.
    ///
    /// Only webhooks can carry this; the messaging-protocol bindings deliberately omit it.
    Read,
    /// The object was created.
    Create,
    /// The object was updated.
    Update,
    /// The object was deleted.
    Delete,
}

impl Operation {
    /// The lowercase spelling used in MQTT topic paths.
    pub const fn topic_segment(self) -> &'static str {
        match self {
            Operation::Read => "read",
            Operation::Create => "create",
            Operation::Update => "update",
            Operation::Delete => "delete",
        }
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Operation::Read => "READ",
            Operation::Create => "CREATE",
            Operation::Update => "UPDATE",
            Operation::Delete => "DELETE",
        })
    }
}

/// One `(objects, operations) -> callback` rule of a subscription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectOperation {
    /// Object types to watch.
    pub objects: Vec<ObjectType>,
    /// Operations to watch.
    pub operations: Vec<Operation>,
    /// Where to POST the notification. Required, and required to be HTTPS.
    pub callback_url: String,
    /// Token the VTN presents to the callback so the receiver can authenticate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
}

impl ObjectOperation {
    /// Whether this rule matches an operation on an object type.
    pub fn matches(&self, object: ObjectType, operation: Operation) -> bool {
        self.objects.contains(&object) && self.operations.contains(&operation)
    }
}

/// Client-provided subscription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionRequest {
    /// Name of the subscribing client.
    pub client_name: ClientName,
    /// Restrict notifications to one programme.
    #[serde(rename = "programID", default, skip_serializing_if = "Option::is_none")]
    pub program_id: Option<ObjectId>,
    /// The rules.
    pub object_operations: Vec<ObjectOperation>,
    /// Restrict notifications to objects carrying these targets.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::null_as_default"
    )]
    pub targets: Vec<Target>,
}

/// Server-provided subscription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    /// VTN-assigned identifier.
    pub id: ObjectId,
    /// Creation time.
    pub created_date_time: Timestamp,
    /// Last modification time.
    pub modification_date_time: Timestamp,
    /// Discriminator; always [`ObjectType::Subscription`].
    #[serde(default = "subscription_type")]
    pub object_type: ObjectType,
    /// Identity of the client that created the subscription; stamped by the VTN.
    #[serde(rename = "clientID")]
    pub client_id: ClientId,
    /// The client-provided content.
    #[serde(flatten)]
    pub content: SubscriptionRequest,
}

fn subscription_type() -> ObjectType {
    ObjectType::Subscription
}

// ---------------------------------------------------------------------------
// VEN and resource
// ---------------------------------------------------------------------------

/// A VEN as described by business logic, which may assign identity and targets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlVenRequest {
    /// The client this VEN object belongs to.
    #[serde(rename = "clientID")]
    pub client_id: ClientId,
    /// Name, unique within the VTN.
    pub ven_name: VenName,
    /// Targets granted to this VEN. Only business logic may write these.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::null_as_default"
    )]
    pub targets: Vec<Target>,
    /// Attributes describing the VEN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Vec<ValuesMap>>,
}

/// A VEN as described by itself: no identity, no targets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VenVenRequest {
    /// Name, unique within the VTN.
    pub ven_name: VenName,
    /// Attributes describing the VEN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Vec<ValuesMap>>,
}

/// A VEN request body, discriminated by `objectType`.
///
/// The discriminator is what stops a VEN from granting itself targets: `VEN_VEN_REQUEST` has no
/// `targets` member at all, so the privilege is unreachable rather than merely unauthorised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "objectType")]
pub enum VenRequest {
    /// Written by business logic.
    #[serde(rename = "BL_VEN_REQUEST")]
    Bl(BlVenRequest),
    /// Written by the VEN itself.
    #[serde(rename = "VEN_VEN_REQUEST")]
    Ven(VenVenRequest),
}

impl VenRequest {
    /// The VEN name, whichever flavour this is.
    pub fn ven_name(&self) -> &VenName {
        match self {
            VenRequest::Bl(r) => &r.ven_name,
            VenRequest::Ven(r) => &r.ven_name,
        }
    }

    /// The attributes, whichever flavour this is.
    pub fn attributes(&self) -> Option<&Vec<ValuesMap>> {
        match self {
            VenRequest::Bl(r) => r.attributes.as_ref(),
            VenRequest::Ven(r) => r.attributes.as_ref(),
        }
    }
}

/// Server-provided VEN.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ven {
    /// VTN-assigned identifier.
    pub id: ObjectId,
    /// Creation time.
    pub created_date_time: Timestamp,
    /// Last modification time.
    pub modification_date_time: Timestamp,
    /// Discriminator.
    ///
    /// The specification composes `ven` from `objectMetadata` + `BlVenRequest`, whose `objectType`
    /// enumerations contradict each other (`VEN` vs `BL_VEN_REQUEST`). Every implementation in the
    /// field emits `VEN`, which is also what the notification discriminator requires, so that is
    /// what we emit.
    #[serde(default = "ven_type")]
    pub object_type: ObjectType,
    /// The client this VEN belongs to.
    #[serde(rename = "clientID")]
    pub client_id: ClientId,
    /// Name, unique within the VTN.
    pub ven_name: VenName,
    /// Targets granted to this VEN.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::null_as_default"
    )]
    pub targets: Vec<Target>,
    /// Attributes describing the VEN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Vec<ValuesMap>>,
}

fn ven_type() -> ObjectType {
    ObjectType::Ven
}

/// A resource as described by business logic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlResourceRequest {
    /// Name, unique within its VEN.
    pub resource_name: ResourceName,
    /// The VEN this resource belongs to.
    #[serde(rename = "venID")]
    pub ven_id: ObjectId,
    /// The client that owns it.
    ///
    /// Required in 3.1.0 and removed in 3.1.1, because `venID` already determines it. Optional here
    /// so that a 3.1.0 peer's body parses and a 3.1.0 peer still receives the field it expects; the
    /// VTN derives ownership from `venID` either way.
    #[serde(rename = "clientID", default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<ClientId>,
    /// Targets granted to this resource. Only business logic may write these.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::null_as_default"
    )]
    pub targets: Vec<Target>,
    /// Attributes describing the resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Vec<ValuesMap>>,
}

/// A resource as described by its VEN: no targets, and no VEN it does not already belong to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VenResourceRequest {
    /// Name, unique within its VEN.
    pub resource_name: ResourceName,
    /// The VEN this resource belongs to.
    ///
    /// Required in 3.1.0 and removed in 3.1.1: a VEN's own resources belong to the VEN the token
    /// identifies, so the field could only ever restate it or contradict it. Optional here — a
    /// 3.1.0 peer may send it, and one that names a *different* VEN is refused rather than quietly
    /// redirected.
    #[serde(rename = "venID", default, skip_serializing_if = "Option::is_none")]
    pub ven_id: Option<ObjectId>,
    /// Attributes describing the resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Vec<ValuesMap>>,
}

/// A resource request body, discriminated by `objectType`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "objectType")]
pub enum ResourceRequest {
    /// Written by business logic.
    #[serde(rename = "BL_RESOURCE_REQUEST")]
    Bl(BlResourceRequest),
    /// Written by the owning VEN.
    #[serde(rename = "VEN_RESOURCE_REQUEST")]
    Ven(VenResourceRequest),
}

impl ResourceRequest {
    /// The resource name, whichever flavour this is.
    pub fn resource_name(&self) -> &ResourceName {
        match self {
            ResourceRequest::Bl(r) => &r.resource_name,
            ResourceRequest::Ven(r) => &r.resource_name,
        }
    }

    /// The attributes, whichever flavour this is.
    pub fn attributes(&self) -> Option<&Vec<ValuesMap>> {
        match self {
            ResourceRequest::Bl(r) => r.attributes.as_ref(),
            ResourceRequest::Ven(r) => r.attributes.as_ref(),
        }
    }
}

/// Server-provided resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    /// VTN-assigned identifier.
    pub id: ObjectId,
    /// Creation time.
    pub created_date_time: Timestamp,
    /// Last modification time.
    pub modification_date_time: Timestamp,
    /// Discriminator; always [`ObjectType::Resource`] (see [`Ven::object_type`]).
    #[serde(default = "resource_type")]
    pub object_type: ObjectType,
    /// Name, unique within its VEN.
    pub resource_name: ResourceName,
    /// The VEN this resource belongs to.
    #[serde(rename = "venID")]
    pub ven_id: ObjectId,
    /// Targets granted to this resource.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::null_as_default"
    )]
    pub targets: Vec<Target>,
    /// Attributes describing the resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Vec<ValuesMap>>,
}

fn resource_type() -> ObjectType {
    ObjectType::Resource
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::std_shim::vec;

    #[test]
    fn every_object_type_announces_itself() {
        // A type missing from its own list would be a write that queues nothing, on the one path
        // where nothing is exactly what the failure looks like.
        for t in ObjectType::ALL {
            assert!(t.announces().contains(&t), "{t} does not announce itself");
        }
    }

    #[test]
    fn a_cascade_is_announced_by_the_type_that_causes_it() {
        // These mirror the foreign keys the storage layer declares. If a cascade is added there and
        // not here, the subscribers watching the cascaded type are silently left out of the sweep.
        assert!(ObjectType::Program.announces().contains(&ObjectType::Event));
        assert!(
            ObjectType::Program
                .announces()
                .contains(&ObjectType::Report)
        );
        assert!(
            ObjectType::Program
                .announces()
                .contains(&ObjectType::Subscription)
        );
        assert!(ObjectType::Event.announces().contains(&ObjectType::Report));
        assert!(ObjectType::Ven.announces().contains(&ObjectType::Resource));
    }

    #[test]
    fn priority_orders_most_important_first() {
        let mut ps = vec![Priority::UNSPECIFIED, Priority::new(10), Priority::new(0)];
        ps.sort();
        assert_eq!(
            ps,
            vec![Priority::new(0), Priority::new(10), Priority::UNSPECIFIED]
        );
    }

    #[test]
    fn ven_request_discriminator_gates_target_assignment() {
        let ven_written =
            r#"{"objectType":"VEN_VEN_REQUEST","venName":"ven-1","targets":["gold"]}"#;
        let parsed: VenRequest = serde_json::from_str(ven_written).unwrap();
        // `targets` simply does not exist on the VEN-written variant, so it is dropped.
        match parsed {
            VenRequest::Ven(v) => assert_eq!(v.ven_name.as_str(), "ven-1"),
            VenRequest::Bl(_) => panic!("wrong variant"),
        }

        let bl_written = r#"{"objectType":"BL_VEN_REQUEST","clientID":"c1","venName":"ven-1","targets":["gold"]}"#;
        match serde_json::from_str::<VenRequest>(bl_written).unwrap() {
            VenRequest::Bl(v) => assert_eq!(v.targets.len(), 1),
            VenRequest::Ven(_) => panic!("wrong variant"),
        }
    }

    #[test]
    fn missing_discriminator_is_an_error() {
        assert!(serde_json::from_str::<VenRequest>(r#"{"venName":"ven-1"}"#).is_err());
    }

    #[test]
    fn null_targets_deserialize_as_empty() {
        let e: EventRequest = serde_json::from_str(r#"{"programID":"p1","targets":null}"#).unwrap();
        assert!(e.targets.is_empty());
        // And are omitted on the way out rather than emitted as null.
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"programID":"p1"}"#);
    }

    #[test]
    fn report_descriptor_defaults_match_the_schema() {
        let rd: ReportDescriptor = serde_json::from_str(r#"{"payloadType":"USAGE"}"#).unwrap();
        assert_eq!(rd.start_interval, -1);
        assert_eq!(rd.num_intervals, -1);
        assert!(rd.historical);
        assert_eq!(rd.frequency, -1);
        assert_eq!(rd.repeat, 1);
        assert!(!rd.aggregate);
        assert_eq!(rd.report_intervals, ReportIntervals::Intervals);
    }

    #[test]
    fn event_round_trips_with_metadata_flattened() {
        let json = r#"{
            "id":"e1",
            "createdDateTime":"2026-02-10T16:00:00Z",
            "modificationDateTime":"2026-02-10T16:00:00Z",
            "objectType":"EVENT",
            "programID":"p1",
            "eventName":"test",
            "intervals":[{"id":0,"payloads":[{"type":"PRICE","values":[0.17]}]}]
        }"#;
        let e: Event = serde_json::from_str(json).unwrap();
        assert_eq!(e.object_type, ObjectType::Event);
        assert_eq!(e.content.program_id.as_str(), "p1");
        let back: Event = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(e, back);
    }
}
