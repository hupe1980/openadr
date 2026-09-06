//! The OpenADR 3.1 wire model.
//!
//! Types here mirror the Alliance's OpenAPI document, with three deliberate departures:
//!
//! 1. **Invariants are parsed, not checked later.** Identifiers, payload types and durations are
//!    newtypes that cannot hold an illegal value ([`ids`], [`time`], [`values`]).
//! 2. **Sentinels are lifted into the type system.** `P9999Y` and `0001-01-01` become
//!    [`Duration::Forever`] and [`StartTime::Now`] instead of lurking inside a timestamp.
//! 3. **Request and response bodies are distinct types.** A client cannot name `id` or
//!    `createdDateTime` in a `POST`, because those fields do not exist on the request type.
//!
//! Everything is `no_std` + `alloc` compatible.
//!
//! Where the specification is ambiguous and this model had to choose, the choice is recorded at
//! <https://hupe1980.github.io/openadr/docs/spec-notes/>.

use serde::{Deserialize, Deserializer};

pub mod adapt;
pub mod auth;
pub mod ids;
pub mod notification;
pub mod notifier;
pub mod objects;
pub mod problem;
pub mod time;
pub mod values;

pub use auth::{
    AuthServerInfo, ClientCredentialRequest, ClientCredentialResponse, OAuthError, OAuthErrorKind,
};
pub use ids::{
    ClientId, ClientName, IdentifierError, ObjectId, ProgramName, ResourceName, Target, VenName,
};
pub use notification::Notification;
pub use notifier::{
    MqttAuthentication, MqttNotifierBinding, NotifierTopics, NotifiersResponse, Serialization,
    TopicsResponse,
};
pub use objects::{
    BlResourceRequest, BlVenRequest, Event, EventPayloadDescriptor, EventRequest, Interval,
    IntervalPeriod, ObjectMetadata, ObjectOperation, ObjectType, Operation, PayloadDescriptor,
    Priority, Program, ProgramDescription, ProgramRequest, Report, ReportDescriptor,
    ReportIntervals, ReportPayloadDescriptor, ReportRequest, ReportResource, Resource,
    ResourceRequest, Subscription, SubscriptionRequest, UnknownObjectType, Ven, VenRequest,
    VenResourceRequest, VenVenRequest,
};
pub use problem::Problem;
pub use time::{Duration, SCHEMA_DURATION_PATTERN, StartTime, TimeError, Timestamp};
pub use values::{PayloadType, Point, ReadingType, Unit, Value, ValuesMap};

/// Deserialize `null` as the type's default instead of failing.
///
/// The specification marks almost every optional array `nullable: true, default: null`, so a
/// conformant peer may send `"targets": null` where we model an empty `Vec`. Accepting that is
/// required for interoperability; emitting it is not, so we skip empty collections on the way out.
pub(crate) fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}
