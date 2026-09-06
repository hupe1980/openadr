//! The notification body a VTN pushes to subscribers.
//!
//! The same JSON shape is delivered by every transport: an HTTP webhook `POST`, an MQTT publish, or
//! a WebSocket frame. Only the envelope differs.

use crate::std_shim::Vec;
use serde::{Deserialize, Serialize};

use super::{
    ids::{ObjectId, Target},
    objects::{Event, ObjectType, Operation, Program, Report, Resource, Subscription, Ven},
};

/// The object a notification is about, discriminated by `objectType`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "objectType", content = "object")]
pub enum AnyObject {
    /// A programme.
    #[serde(rename = "PROGRAM")]
    Program(Program),
    /// An event.
    #[serde(rename = "EVENT")]
    Event(Event),
    /// A report.
    #[serde(rename = "REPORT")]
    Report(Report),
    /// A subscription.
    #[serde(rename = "SUBSCRIPTION")]
    Subscription(Subscription),
    /// A VEN.
    #[serde(rename = "VEN")]
    Ven(Ven),
    /// A resource.
    #[serde(rename = "RESOURCE")]
    Resource(Resource),
}

impl AnyObject {
    /// The identifier of the wrapped object.
    pub fn id(&self) -> &ObjectId {
        match self {
            AnyObject::Program(o) => &o.id,
            AnyObject::Event(o) => &o.id,
            AnyObject::Report(o) => &o.id,
            AnyObject::Subscription(o) => &o.id,
            AnyObject::Ven(o) => &o.id,
            AnyObject::Resource(o) => &o.id,
        }
    }

    /// The type of the wrapped object.
    pub fn object_type(&self) -> ObjectType {
        match self {
            AnyObject::Program(_) => ObjectType::Program,
            AnyObject::Event(_) => ObjectType::Event,
            AnyObject::Report(_) => ObjectType::Report,
            AnyObject::Subscription(_) => ObjectType::Subscription,
            AnyObject::Ven(_) => ObjectType::Ven,
            AnyObject::Resource(_) => ObjectType::Resource,
        }
    }

    /// The targets carried by the wrapped object, which drive notification filtering.
    pub fn targets(&self) -> &[Target] {
        match self {
            AnyObject::Program(o) => &o.content.targets,
            AnyObject::Event(o) => &o.content.targets,
            AnyObject::Ven(o) => &o.targets,
            AnyObject::Resource(o) => &o.targets,
            AnyObject::Subscription(o) => &o.content.targets,
            AnyObject::Report(_) => &[],
        }
    }

    /// The programme this object belongs to, where that is meaningful.
    pub fn program_id(&self) -> Option<&ObjectId> {
        match self {
            AnyObject::Program(o) => Some(&o.id),
            AnyObject::Event(o) => Some(&o.content.program_id),
            AnyObject::Subscription(o) => o.content.program_id.as_ref(),
            _ => None,
        }
    }
}

/// A change of state, as delivered to a subscriber.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    /// What happened.
    pub operation: Operation,
    /// The object it happened to, plus its `objectType` discriminator.
    #[serde(flatten)]
    pub object: AnyObject,
    /// The targets that caused this notification to be delivered.
    ///
    /// Subject to the same target hiding as a read: a subscriber only ever sees the targets it was
    /// granted, never the full set on the object.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "super::null_as_default"
    )]
    pub targets: Vec<Target>,
}

impl Notification {
    /// Build a notification.
    pub fn new(operation: Operation, object: AnyObject) -> Self {
        Self {
            operation,
            object,
            targets: Vec::new(),
        }
    }

    /// Restrict the visible targets.
    pub fn with_targets(mut self, targets: Vec<Target>) -> Self {
        self.targets = targets;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProgramName, ProgramRequest};

    fn program() -> Program {
        Program {
            id: "0".parse().unwrap(),
            created_date_time: "2023-06-15T15:51:29Z".parse().unwrap(),
            modification_date_time: "2023-06-15T15:51:29Z".parse().unwrap(),
            object_type: ObjectType::Program,
            content: ProgramRequest::new(ProgramName::new("myProgram").unwrap()),
        }
    }

    #[test]
    fn notification_matches_the_specification_example_shape() {
        let n = Notification::new(Operation::Update, AnyObject::Program(program()));
        let v: serde_json::Value = serde_json::to_value(&n).unwrap();
        assert_eq!(v["objectType"], "PROGRAM");
        assert_eq!(v["operation"], "UPDATE");
        assert_eq!(v["object"]["programName"], "myProgram");
        // The nested object keeps its own discriminator, as in the specification's example.
        assert_eq!(v["object"]["objectType"], "PROGRAM");
    }

    #[test]
    fn notification_round_trips() {
        let n = Notification::new(Operation::Create, AnyObject::Program(program()));
        let back: Notification = serde_json::from_str(&serde_json::to_string(&n).unwrap()).unwrap();
        assert_eq!(n, back);
    }
}
