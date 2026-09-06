//! `check-model` — the wire model against `openadr3.yaml`.
//!
//! The payload table has been generated from the specification since the first week, so a new
//! enumeration value cannot pass unnoticed. The *objects* had no such guard: `programRequest`
//! gaining a field, or `resource` losing one, would be found by a person reading two documents side
//! by side, which is how the `clientID` question on `BL_RESOURCE_REQUEST` was found and is not a
//! process. This is that guard.
//!
//! It does not parse the Rust source. It builds a real instance of each wire type, serialises it,
//! and compares the resulting JSON object's keys with the schema's `properties` — so what is
//! checked is what actually goes on the wire, `#[serde(rename)]`, `flatten` and all. Each type is
//! built twice:
//!
//! * **full** — every optional field populated, which is what the key set is compared against.
//! * **minimal** — constructed the way the crate's own constructors do it, which is what the
//!   schema's `required` list is compared against. A required field that vanishes when nothing is
//!   set is a body the VTN would reject.
//!
//! A difference is not automatically a defect: the crate deliberately departs from 3.1.0 in a
//! handful of places, and each of those is listed in [`ACCEPTED`] with the reason. Anything not on
//! that list fails the check, so a *new* divergence has to be looked at and either fixed or
//! written down.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use openadr::model::*;
use serde_json::Value as Json;
use serde_yaml_ng::Value as Yaml;

/// A divergence that has been examined and accepted, with the reason it is not a defect.
///
/// `(schema, field, why)`. Keep it short: a long list means the model and the document have
/// drifted apart rather than that the document is wrong.
const ACCEPTED: &[(&str, &str, &str)] = &[(
    "resource",
    "clientID",
    "3.1.0 composes `resource` from `BlResourceRequest`, which required `clientID`; 3.1.1 removed \
     it as redundant with `venID`, and a resource's owner is its VEN's owner. Carrying it would \
     mean a denormalised column and a join on every read, for a field the next release deletes. \
     The request bodies still accept it — see concepts/SPEC.md and the public spec-notes page.",
)];

/// One type to check: its schema name, a full instance and a minimal one.
struct Subject {
    schema: &'static str,
    full: Json,
    minimal: Json,
}

fn subject<T: serde::Serialize>(schema: &'static str, full: &T, minimal: &T) -> Subject {
    Subject {
        schema,
        full: serde_json::to_value(full).expect("wire types serialise"),
        minimal: serde_json::to_value(minimal).expect("wire types serialise"),
    }
}

/// Compare every wire type against the document, and report everything at once.
///
/// Everything at once on purpose: fixing one field, rerunning, and finding the next is how a
/// mechanical check becomes a chore nobody runs.
pub fn check(spec: &Yaml) -> Result<()> {
    let schemas = spec
        .get("components")
        .and_then(|c| c.get("schemas"))
        .and_then(Yaml::as_mapping)
        .ok_or_else(|| anyhow::anyhow!("openadr3.yaml has no components.schemas"))?;

    let mut problems: Vec<String> = Vec::new();
    let mut checked = 0usize;

    // The `duration` pattern is the one *string* in the document this crate reimplements rather
    // than reads: `model::time::is_schema_duration` is that regex as a byte-level state machine,
    // because the wire model is `no_std` and a regex engine is not. A property test holds the two
    // together; this holds the constant to the document, so a pattern tightened upstream reaches
    // the test rather than slipping past a reference that has quietly gone stale.
    match schemas
        .get(Yaml::from("duration"))
        .and_then(|d| d.get("pattern"))
        .and_then(Yaml::as_str)
    {
        Some(pattern) if pattern == openadr::model::SCHEMA_DURATION_PATTERN => {}
        Some(pattern) => problems.push(format!(
            "duration: the document's pattern is {pattern:?} and \
             model::SCHEMA_DURATION_PATTERN is {:?} — the hand-written parser was written from the \
             second one",
            openadr::model::SCHEMA_DURATION_PATTERN
        )),
        None => problems.push(
            "duration: the document no longer states a `pattern`, which is the rule the \
             hand-written parser implements"
                .to_string(),
        ),
    }
    // Which accepted departures were actually needed. An entry nothing fires on is a claim with a
    // name: the departure was fixed, or the document changed, and the list now says something that
    // is no longer true.
    let mut fired = vec![false; ACCEPTED.len()];

    for subject in subjects() {
        let Some(schema) = schemas.get(Yaml::from(subject.schema)) else {
            problems.push(format!(
                "{}: the document has no such schema — has it been renamed upstream?",
                subject.schema
            ));
            continue;
        };
        checked += 1;

        let (properties, required) = resolve(schema, schemas);
        let full = keys(&subject.full);
        let minimal = keys(&subject.minimal);

        // Consulted only where there is something to excuse, so an entry that fires is an entry
        // that was needed. Asking first and reporting second would mark every accepted field as
        // used the moment it appeared in a `required` list, which is not the same thing at all.
        let mut differences: Vec<(String, String)> = Vec::new();
        for missing in properties.difference(&full) {
            differences.push((
                missing.clone(),
                format!(
                    "{}: the document defines `{missing}` and the model does not carry it",
                    subject.schema
                ),
            ));
        }
        for extra in full.difference(&properties) {
            differences.push((
                extra.clone(),
                format!(
                    "{}: the model sends `{extra}`, which the document does not define. An \
                     extension is legal `[Def §Extensibility]` — add it to ACCEPTED with the \
                     reason.",
                    subject.schema
                ),
            ));
        }
        for name in required.iter().filter(|n| !minimal.contains(*n)) {
            differences.push((
                name.clone(),
                format!(
                    "{}: `{name}` is required by the document but absent from a minimally \
                     constructed value — the VTN would reject the body this crate builds",
                    subject.schema
                ),
            ));
        }
        for (field, message) in differences {
            if !accept(subject.schema, &field, &mut fired) {
                problems.push(message);
            }
        }
    }

    for (index, used) in fired.iter().enumerate() {
        if !used {
            let (schema, field, _) = ACCEPTED[index];
            problems.push(format!(
                "{schema}: `{field}` is listed as an accepted departure but the model and the \
                 document now agree about it — delete the entry",
            ));
        }
    }

    if !problems.is_empty() {
        let mut message = format!(
            "the wire model no longer matches openadr3.yaml ({} difference(s)):\n",
            problems.len()
        );
        for problem in &problems {
            message.push_str("  - ");
            message.push_str(problem);
            message.push('\n');
        }
        message.push_str(
            "\nEach one is either a field to add, a field to remove, or a deliberate departure \
             that belongs in xtask/src/model.rs's ACCEPTED list with the reason.",
        );
        bail!(message);
    }

    println!(
        "the wire model matches openadr3.yaml across {checked} schemas ({} accepted departure(s))",
        ACCEPTED.len()
    );
    Ok(())
}

/// Whether this difference is one of the accepted departures, marking it as having fired.
fn accept(schema: &str, field: &str, fired: &mut [bool]) -> bool {
    match ACCEPTED
        .iter()
        .position(|(s, f, _)| *s == schema && *f == field)
    {
        Some(index) => {
            fired[index] = true;
            true
        }
        None => false,
    }
}

/// The property names and required list of a schema, following `allOf`.
///
/// `event`, `program`, `report`, `subscription`, `ven` and `resource` are all composed from
/// `objectMetadata` plus a request body, so a checker that stopped at the top level would compare
/// the responses against nothing.
fn resolve(
    schema: &Yaml,
    schemas: &serde_yaml_ng::Mapping,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut properties = BTreeSet::new();
    let mut required = BTreeSet::new();

    if let Some(reference) = schema.get("$ref").and_then(Yaml::as_str) {
        if let Some(name) = reference.strip_prefix("#/components/schemas/")
            && let Some(target) = schemas.get(Yaml::from(name))
        {
            return resolve(target, schemas);
        }
        return (properties, required);
    }

    if let Some(members) = schema.get("allOf").and_then(Yaml::as_sequence) {
        for member in members {
            let (p, r) = resolve(member, schemas);
            properties.extend(p);
            required.extend(r);
        }
    }
    if let Some(map) = schema.get("properties").and_then(Yaml::as_mapping) {
        properties.extend(map.keys().filter_map(Yaml::as_str).map(str::to_string));
    }
    if let Some(list) = schema.get("required").and_then(Yaml::as_sequence) {
        required.extend(list.iter().filter_map(Yaml::as_str).map(str::to_string));
    }
    (properties, required)
}

/// The top-level keys of a serialised object.
fn keys(value: &Json) -> BTreeSet<String> {
    value
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The instances
// ---------------------------------------------------------------------------

fn oid(s: &str) -> ObjectId {
    ObjectId::new(s).expect("literal")
}

fn now() -> Timestamp {
    "2026-01-01T00:00:00Z".parse().expect("literal")
}

fn period() -> IntervalPeriod {
    IntervalPeriod::new(StartTime::At(now()), "PT1H".parse().expect("literal"))
        .with_randomize_start("PT5M".parse().expect("literal"))
}

fn values() -> ValuesMap {
    ValuesMap::single("PRICE".parse().expect("literal"), Value::Integer(1))
}

fn metadata(object_type: ObjectType) -> ObjectMetadata {
    ObjectMetadata {
        id: oid("obj-1"),
        created_date_time: now(),
        modification_date_time: now(),
        object_type,
    }
}

fn full_program() -> ProgramRequest {
    let mut p = ProgramRequest::new("tariff".parse().expect("literal"));
    p.interval_period = Some(period());
    p.program_descriptions = Some(vec![ProgramDescription {
        url: "https://example.com/tariff".into(),
    }]);
    p.payload_descriptors = Some(vec![PayloadDescriptor::Event(EventPayloadDescriptor::new(
        "PRICE".parse().expect("literal"),
    ))]);
    p.attributes = Some(vec![values()]);
    p.targets = vec!["group1".parse().expect("literal")];
    p
}

fn full_event() -> EventRequest {
    let mut e = EventRequest::new(oid("prg-1"));
    e.event_name = Some("curtailment".into());
    e.duration = Some("PT1H".parse().expect("literal"));
    e.priority = Priority::new(1);
    e.targets = vec!["group1".parse().expect("literal")];
    e.report_descriptors = Some(vec![full_report_descriptor()]);
    e.payload_descriptors = Some(vec![
        EventPayloadDescriptor::new("PRICE".parse().expect("literal"))
            .with_units("KWH".parse().expect("literal"))
            .with_currency("EUR"),
    ]);
    e.interval_period = Some(period());
    e.intervals = Some(vec![Interval::new(0, vec![values()]).with_period(period())]);
    e
}

fn full_report_descriptor() -> ReportDescriptor {
    let mut d = ReportDescriptor::new("USAGE".parse().expect("literal"));
    d.reading_type = Some("DIRECT_READ".into());
    d.units = Some("KWH".parse().expect("literal"));
    d.targets = vec!["group1".parse().expect("literal")];
    d.aggregate = true;
    d.start_interval = 0;
    d.num_intervals = 24;
    d.historical = false;
    d.frequency = 1;
    d.repeat = -1;
    d.report_intervals = ReportIntervals::SubIntervals;
    d
}

fn full_report() -> ReportRequest {
    let mut r = ReportRequest::new(
        oid("evt-1"),
        "VEN-1".parse().expect("literal"),
        vec![ReportResource {
            resource_name: "meter".parse().expect("literal"),
            interval_period: Some(period()),
            intervals: vec![Interval::new(0, vec![values()])],
        }],
    );
    r.report_name = Some("usage".into());
    r.payload_descriptors = Some(vec![ReportPayloadDescriptor::new(
        "USAGE".parse().expect("literal"),
    )]);
    r
}

fn full_subscription() -> SubscriptionRequest {
    SubscriptionRequest {
        client_name: "VEN-1".parse().expect("literal"),
        program_id: Some(oid("prg-1")),
        object_operations: vec![ObjectOperation {
            objects: vec![ObjectType::Event],
            operations: vec![Operation::Create],
            callback_url: "https://example.com/hook".into(),
            bearer_token: Some("token".into()),
        }],
        targets: vec!["group1".parse().expect("literal")],
    }
}

fn subjects() -> Vec<Subject> {
    let minimal_program = ProgramRequest::new("tariff".parse().expect("literal"));
    let minimal_event = EventRequest::new(oid("prg-1"));
    let minimal_report = ReportRequest::new(
        oid("evt-1"),
        "VEN-1".parse().expect("literal"),
        vec![ReportResource {
            resource_name: "meter".parse().expect("literal"),
            interval_period: None,
            intervals: Vec::new(),
        }],
    );
    let minimal_subscription = SubscriptionRequest {
        client_name: "VEN-1".parse().expect("literal"),
        program_id: None,
        object_operations: vec![ObjectOperation {
            objects: vec![ObjectType::Event],
            operations: vec![Operation::Create],
            callback_url: "https://example.com/hook".into(),
            bearer_token: None,
        }],
        targets: Vec::new(),
    };

    let bl_ven = VenRequest::Bl(BlVenRequest {
        client_id: "client-1".parse().expect("literal"),
        ven_name: "ven-1".parse().expect("literal"),
        targets: vec!["group1".parse().expect("literal")],
        attributes: Some(vec![values()]),
    });
    let minimal_bl_ven = VenRequest::Bl(BlVenRequest {
        client_id: "client-1".parse().expect("literal"),
        ven_name: "ven-1".parse().expect("literal"),
        targets: Vec::new(),
        attributes: None,
    });
    let ven_ven = VenRequest::Ven(VenVenRequest {
        ven_name: "ven-1".parse().expect("literal"),
        attributes: Some(vec![values()]),
    });
    let minimal_ven_ven = VenRequest::Ven(VenVenRequest {
        ven_name: "ven-1".parse().expect("literal"),
        attributes: None,
    });

    let bl_resource = ResourceRequest::Bl(BlResourceRequest {
        resource_name: "meter".parse().expect("literal"),
        ven_id: oid("ven-1"),
        client_id: Some("client-1".parse().expect("literal")),
        targets: vec!["group1".parse().expect("literal")],
        attributes: Some(vec![values()]),
    });
    let minimal_bl_resource = ResourceRequest::Bl(BlResourceRequest {
        resource_name: "meter".parse().expect("literal"),
        ven_id: oid("ven-1"),
        client_id: Some("client-1".parse().expect("literal")),
        targets: Vec::new(),
        attributes: None,
    });
    let ven_resource = ResourceRequest::Ven(VenResourceRequest {
        resource_name: "meter".parse().expect("literal"),
        ven_id: Some(oid("ven-1")),
        attributes: Some(vec![values()]),
    });
    let minimal_ven_resource = ResourceRequest::Ven(VenResourceRequest {
        resource_name: "meter".parse().expect("literal"),
        ven_id: Some(oid("ven-1")),
        attributes: None,
    });

    let ven = Ven {
        id: oid("ven-1"),
        created_date_time: now(),
        modification_date_time: now(),
        object_type: ObjectType::Ven,
        client_id: "client-1".parse().expect("literal"),
        ven_name: "ven-1".parse().expect("literal"),
        targets: vec!["group1".parse().expect("literal")],
        attributes: Some(vec![values()]),
    };
    let resource = Resource {
        id: oid("res-1"),
        created_date_time: now(),
        modification_date_time: now(),
        object_type: ObjectType::Resource,
        resource_name: "meter".parse().expect("literal"),
        ven_id: oid("ven-1"),
        targets: vec!["group1".parse().expect("literal")],
        attributes: Some(vec![values()]),
    };

    vec![
        subject(
            "objectMetadata",
            &metadata(ObjectType::Program),
            &metadata(ObjectType::Program),
        ),
        subject("intervalPeriod", &period(), &IntervalPeriod::default()),
        subject(
            "interval",
            &Interval::new(0, vec![values()]).with_period(period()),
            &Interval::new(0, vec![values()]),
        ),
        subject("valuesMap", &values(), &values()),
        subject(
            "eventPayloadDescriptor",
            &EventPayloadDescriptor::new("PRICE".parse().expect("literal"))
                .with_units("KWH".parse().expect("literal"))
                .with_currency("EUR"),
            &EventPayloadDescriptor::new("PRICE".parse().expect("literal")),
        ),
        subject(
            "reportPayloadDescriptor",
            &{
                let mut d = ReportPayloadDescriptor::new("USAGE".parse().expect("literal"));
                d.reading_type = Some("DIRECT_READ".into());
                d.units = Some("KWH".parse().expect("literal"));
                d.accuracy = Some(1.0);
                d.confidence = Some(95);
                d
            },
            &ReportPayloadDescriptor::new("USAGE".parse().expect("literal")),
        ),
        subject(
            "reportDescriptor",
            &full_report_descriptor(),
            &ReportDescriptor::new("USAGE".parse().expect("literal")),
        ),
        subject("programRequest", &full_program(), &minimal_program),
        subject(
            "program",
            &Program {
                id: oid("prg-1"),
                created_date_time: now(),
                modification_date_time: now(),
                object_type: ObjectType::Program,
                content: full_program(),
            },
            &Program {
                id: oid("prg-1"),
                created_date_time: now(),
                modification_date_time: now(),
                object_type: ObjectType::Program,
                content: minimal_program.clone(),
            },
        ),
        subject("eventRequest", &full_event(), &minimal_event),
        subject(
            "event",
            &Event {
                id: oid("evt-1"),
                created_date_time: now(),
                modification_date_time: now(),
                object_type: ObjectType::Event,
                content: full_event(),
            },
            &Event {
                id: oid("evt-1"),
                created_date_time: now(),
                modification_date_time: now(),
                object_type: ObjectType::Event,
                content: minimal_event.clone(),
            },
        ),
        subject("reportRequest", &full_report(), &minimal_report),
        subject(
            "report",
            &Report {
                id: oid("rpt-1"),
                created_date_time: now(),
                modification_date_time: now(),
                object_type: ObjectType::Report,
                client_id: Some("client-1".parse().expect("literal")),
                content: full_report(),
            },
            &Report {
                id: oid("rpt-1"),
                created_date_time: now(),
                modification_date_time: now(),
                object_type: ObjectType::Report,
                client_id: Some("client-1".parse().expect("literal")),
                content: minimal_report.clone(),
            },
        ),
        subject(
            "subscriptionRequest",
            &full_subscription(),
            &minimal_subscription,
        ),
        subject(
            "subscription",
            &Subscription {
                id: oid("sub-1"),
                created_date_time: now(),
                modification_date_time: now(),
                object_type: ObjectType::Subscription,
                client_id: "client-1".parse().expect("literal"),
                content: full_subscription(),
            },
            &Subscription {
                id: oid("sub-1"),
                created_date_time: now(),
                modification_date_time: now(),
                object_type: ObjectType::Subscription,
                client_id: "client-1".parse().expect("literal"),
                content: minimal_subscription.clone(),
            },
        ),
        subject("BlVenRequest", &bl_ven, &minimal_bl_ven),
        subject("VenVenRequest", &ven_ven, &minimal_ven_ven),
        subject("ven", &ven, &ven),
        subject("BlResourceRequest", &bl_resource, &minimal_bl_resource),
        subject("VenResourceRequest", &ven_resource, &minimal_ven_resource),
        subject("resource", &resource, &resource),
        subject(
            "notification",
            &Notification::new(
                Operation::Create,
                notification::AnyObject::Program(Program {
                    id: oid("prg-1"),
                    created_date_time: now(),
                    modification_date_time: now(),
                    object_type: ObjectType::Program,
                    content: full_program(),
                }),
            )
            .with_targets(vec!["group1".parse().expect("literal")]),
            &Notification::new(
                Operation::Create,
                notification::AnyObject::Program(Program {
                    id: oid("prg-1"),
                    created_date_time: now(),
                    modification_date_time: now(),
                    object_type: ObjectType::Program,
                    content: minimal_program,
                }),
            ),
        ),
        subject(
            "problem",
            &Problem::new(400, "bad-request", "Bad Request")
                .with_detail("why")
                .with_instance("req-1"),
            &Problem::new(400, "bad-request", "Bad Request"),
        ),
    ]
}
