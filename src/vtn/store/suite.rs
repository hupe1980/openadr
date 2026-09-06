//! The conformance suite every storage backend must pass.
//!
//! A second backend is where a rule quietly diverges: the in-memory one filters with an iterator,
//! SQLite filters with a `WHERE` clause, and nothing forces the two to mean the same thing. So the
//! behaviour is stated once, here, and both backends run it.
//!
//! Each function takes `&dyn Storage` and asserts one behaviour. The backends' own test modules
//! call every one.

use crate::core::{Access, Grant, Role};
use crate::model::{
    ClientId, ClientName, EventRequest, Interval, IntervalPeriod, ObjectId, ObjectOperation,
    ObjectType, ProgramName, ProgramRequest, ReportRequest, ResourceName, StartTime,
    SubscriptionRequest, Target, Timestamp, Value, ValuesMap, Ven, VenName,
};

use super::{
    BreakerPolicy, EventQuery, OwnerKind, Page, ProgramQuery, ReportQuery, ResourceQuery,
    RetryPolicy, Storage, StorageError, SubscriptionQuery, VenQuery,
};
use crate::model::{
    Operation,
    notification::{AnyObject, Notification},
};
use crate::vtn::notify::{Delivery, DeliveryFailure, Fanout, Route};

pub fn now() -> Timestamp {
    "2026-01-01T00:00:00Z".parse().unwrap()
}

fn later() -> Timestamp {
    "2026-06-01T00:00:00Z".parse().unwrap()
}

fn oid(s: &str) -> ObjectId {
    ObjectId::new(s).unwrap()
}

fn target(s: &str) -> Target {
    Target::new(s).unwrap()
}

fn program(name: &str) -> ProgramRequest {
    ProgramRequest::new(ProgramName::new(name).unwrap())
}

fn ven(client: &str, name: &str, targets: &[&str]) -> Ven {
    Ven {
        id: oid("pending"),
        created_date_time: now(),
        modification_date_time: now(),
        object_type: ObjectType::Ven,
        client_id: ClientId::new(client).unwrap(),
        ven_name: VenName::new(name).unwrap(),
        targets: targets.iter().map(|t| target(t)).collect(),
        attributes: None,
    }
}

fn resource(ven_id: &ObjectId, name: &str, targets: &[&str]) -> crate::model::Resource {
    crate::model::Resource {
        id: oid("pending"),
        created_date_time: now(),
        modification_date_time: now(),
        object_type: ObjectType::Resource,
        resource_name: ResourceName::new(name).unwrap(),
        ven_id: ven_id.clone(),
        targets: targets.iter().map(|t| target(t)).collect(),
        attributes: None,
    }
}

fn watching(client: &str, object_type: ObjectType, callback_url: &str) -> SubscriptionRequest {
    SubscriptionRequest {
        client_name: ClientName::new(client).unwrap(),
        program_id: None,
        object_operations: vec![ObjectOperation {
            objects: vec![object_type],
            operations: vec![Operation::Create],
            callback_url: callback_url.into(),
            bearer_token: None,
        }],
        targets: Vec::new(),
    }
}

fn ven_role(client: &str, grants: &[&str]) -> Role {
    Role::Ven {
        client_id: ClientId::new(client).unwrap(),
        grant: Grant::from_targets(grants.iter().map(|t| target(t))),
    }
}

fn bl() -> Access {
    Access::unrestricted()
}

fn page() -> Page {
    Page::default()
}

fn programs(access: Access) -> ProgramQuery {
    ProgramQuery {
        program_name: None,
        access,
        page: page(),
    }
}

fn events(access: Access) -> EventQuery {
    EventQuery {
        program_id: None,
        active_at: None,
        access,
        page: page(),
    }
}

/// An event with `count` hourly intervals starting at `start`.
fn hourly(program_id: &ObjectId, start: Timestamp, count: i32) -> EventRequest {
    EventRequest::new(program_id.clone())
        .with_interval_period(IntervalPeriod::new(
            StartTime::At(start),
            "PT1H".parse().unwrap(),
        ))
        .with_intervals(
            (0..count)
                .map(|i| {
                    Interval::new(
                        i,
                        vec![ValuesMap::single(
                            "PRICE".parse().unwrap(),
                            Value::Integer(i as i64),
                        )],
                    )
                })
                .collect(),
        )
}

// ---------------------------------------------------------------------------
// Integrity
// ---------------------------------------------------------------------------

pub async fn programme_names_are_unique(s: &dyn Storage) {
    s.create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    let err = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            StorageError::Conflict {
                field: "programName",
                ..
            }
        ),
        "{err:?}"
    );
    // And the conflict names the *value*, not the database's own message. Both SQL backends put
    // the raw constraint text here — `programName "unique constraint failed: program.program_name"
    // already exists` — which reaches the client in a `problem` body: useless to it, and a
    // disclosure of the schema on a path anyone who can write can reach.
    let StorageError::Conflict { value, .. } = &err else {
        unreachable!()
    };
    assert_eq!(
        value, "tou",
        "the conflict quotes the database rather than the value the client sent: {value:?}"
    );
}

pub async fn an_event_needs_an_existing_programme(s: &dyn Storage) {
    let err = s
        .create_event(EventRequest::new(oid("nope")), now(), &Fanout::none())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            StorageError::DanglingReference {
                field: "programID",
                ..
            }
        ),
        "{err:?}"
    );
}

pub async fn deleting_a_programme_takes_its_events_and_reports(s: &dyn Storage) {
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    let e = s
        .create_event(EventRequest::new(p.id.clone()), now(), &Fanout::none())
        .await
        .unwrap();
    let r = s
        .create_report(
            ReportRequest::new(e.id.clone(), ClientName::new("ven-1").unwrap(), Vec::new()),
            Some(ClientId::new("c1").unwrap()),
            now(),
            &Fanout::none(),
        )
        .await
        .unwrap();

    s.delete_program(&p.id, &Fanout::none()).await.unwrap();
    assert!(
        s.get_event(&e.id).await.is_err(),
        "event outlived programme"
    );
    assert!(s.get_report(&r.id).await.is_err(), "report outlived event");
}

pub async fn deleting_a_ven_takes_its_resources(s: &dyn Storage) {
    let v = s
        .create_ven(ven("c1", "ven-1", &[]), &Fanout::none())
        .await
        .unwrap();
    let r = s
        .create_resource(
            crate::model::Resource {
                id: oid("pending"),
                created_date_time: now(),
                modification_date_time: now(),
                object_type: ObjectType::Resource,
                resource_name: ResourceName::new("battery").unwrap(),
                ven_id: v.id.clone(),
                targets: Vec::new(),
                attributes: None,
            },
            &Fanout::none(),
        )
        .await
        .unwrap();

    s.delete_ven(&v.id, &Fanout::none()).await.unwrap();
    assert!(s.get_resource(&r.id).await.is_err());
}

pub async fn one_ven_object_per_client(s: &dyn Storage) {
    s.create_ven(ven("client-a", "ven-1", &[]), &Fanout::none())
        .await
        .unwrap();
    let err = s
        .create_ven(ven("client-a", "ven-2", &[]), &Fanout::none())
        .await
        .unwrap_err();
    if let StorageError::Conflict { value, .. } = &err {
        // The clientID, not the database's message, and not the venName either: two unique columns
        // on one table, and the field the backend reports has to be the one the value belongs to.
        assert_eq!(value, "client-a", "{err:?}");
    }
    assert!(
        matches!(
            err,
            StorageError::Conflict {
                field: "clientID",
                ..
            }
        ),
        "{err:?}"
    );
}

pub async fn ven_names_are_unique(s: &dyn Storage) {
    s.create_ven(ven("client-a", "shared", &[]), &Fanout::none())
        .await
        .unwrap();
    let err = s
        .create_ven(ven("client-b", "shared", &[]), &Fanout::none())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            StorageError::Conflict {
                field: "venName",
                ..
            }
        ),
        "{err:?}"
    );
}

pub async fn resource_names_are_unique_within_a_ven(s: &dyn Storage) {
    let a = s
        .create_ven(ven("c1", "ven-a", &[]), &Fanout::none())
        .await
        .unwrap();
    let b = s
        .create_ven(ven("c2", "ven-b", &[]), &Fanout::none())
        .await
        .unwrap();
    let make = |ven_id: &ObjectId| crate::model::Resource {
        id: oid("pending"),
        created_date_time: now(),
        modification_date_time: now(),
        object_type: ObjectType::Resource,
        resource_name: ResourceName::new("meter").unwrap(),
        ven_id: ven_id.clone(),
        targets: Vec::new(),
        attributes: None,
    };
    s.create_resource(make(&a.id), &Fanout::none())
        .await
        .unwrap();
    // The same name under a different VEN is fine.
    s.create_resource(make(&b.id), &Fanout::none())
        .await
        .unwrap();
    // Under the same VEN it is not.
    let err = s
        .create_resource(make(&a.id), &Fanout::none())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            StorageError::Conflict {
                field: "resourceName",
                ..
            }
        ),
        "{err:?}"
    );
}

pub async fn a_no_op_update_does_not_bump_the_modification_time(s: &dyn Storage) {
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();

    let same = s
        .update_program(&p.id, program("tou"), later(), &Fanout::none())
        .await
        .unwrap();
    assert_eq!(
        same.modification_date_time, p.modification_date_time,
        "an unchanged PUT must not wake every subscriber"
    );

    let changed = s
        .update_program(&p.id, program("tou-2"), later(), &Fanout::none())
        .await
        .unwrap();
    assert_eq!(changed.modification_date_time, later());
    assert_eq!(
        changed.created_date_time, p.created_date_time,
        "an update must not rewrite the creation time"
    );
}

pub async fn identifiers_are_sequenced_per_kind(s: &dyn Storage) {
    // Identifiers are opaque to the protocol, so this is not a conformance rule — it is a
    // *divergence* rule. The in-memory backend had its own copy of the prefix table and one counter
    // shared across every object type, so the same three writes produced different ids there than
    // on SQL and a fixture written against one did not read against the other. One implementation,
    // and a behaviour that says so.
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    assert_eq!(p.id.as_str(), "prg-00000001");

    let second = s
        .create_program(program("critical-peak"), now(), &Fanout::none())
        .await
        .unwrap();
    assert_eq!(second.id.as_str(), "prg-00000002");

    // A different kind counts separately, and starts at one.
    let e = s
        .create_event(hourly(&p.id, now(), 1), now(), &Fanout::none())
        .await
        .unwrap();
    assert_eq!(e.id.as_str(), "evt-00000001");
}

pub async fn a_missing_object_is_reported_not_invented(s: &dyn Storage) {
    assert!(matches!(
        s.get_program(&oid("absent")).await,
        Err(StorageError::NotFound { .. })
    ));
    assert!(matches!(
        s.delete_event(&oid("absent"), &Fanout::none()).await,
        Err(StorageError::NotFound { .. })
    ));
    assert!(matches!(
        s.update_program(&oid("absent"), program("x"), now(), &Fanout::none())
            .await,
        Err(StorageError::NotFound { .. })
    ));
}

// ---------------------------------------------------------------------------
// Filtering, ordering, pagination
// ---------------------------------------------------------------------------

pub async fn pagination_is_ordered_and_complete(s: &dyn Storage) {
    for i in 0..60 {
        s.create_program(program(&format!("p{i:03}")), now(), &Fanout::none())
            .await
            .unwrap();
    }
    let query = |skip| ProgramQuery {
        page: Page { skip, limit: 50 },
        ..programs(bl())
    };

    let first = s.list_programs(&query(0)).await.unwrap();
    let second = s.list_programs(&query(50)).await.unwrap();
    assert_eq!(first.len(), 50);
    assert_eq!(second.len(), 10);

    // Every record appears exactly once across the pages.
    let mut ids: Vec<String> = first
        .iter()
        .chain(second.iter())
        .map(|p| p.id.to_string())
        .collect();
    assert_eq!(ids.len(), 60);
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 60, "pagination returned duplicates");

    // And the order is stable across identical requests.
    let again = s.list_programs(&query(0)).await.unwrap();
    assert_eq!(
        first.iter().map(|p| &p.id).collect::<Vec<_>>(),
        again.iter().map(|p| &p.id).collect::<Vec<_>>(),
    );
}

/// Filtering must narrow the set *before* the page is cut, or a page comes back short.
pub async fn creation_order_survives_sub_second_timestamps(s: &dyn Storage) {
    // A timestamp prints with the fewest fractional digits it needs, so `…00.5Z` is shorter than
    // `…00.55Z` — and as text the longer one sorts *first*. A backend that stores timestamps as
    // variable-width text therefore returns pages in the wrong order, which is invisible until a
    // client walking a collection sees an object move between pages.
    let stamps = [
        "2026-01-01T00:00:00Z",
        "2026-01-01T00:00:00.5Z",
        "2026-01-01T00:00:00.55Z",
        "2026-01-01T00:00:00.123456789Z",
        "2026-01-01T00:00:01Z",
    ];
    let mut ordered: Vec<(Timestamp, String)> = Vec::new();
    for (i, at) in stamps.iter().enumerate() {
        let at: Timestamp = at.parse().unwrap();
        let name = format!("p{i}");
        s.create_program(program(&name), at, &Fanout::none())
            .await
            .unwrap();
        ordered.push((at, name));
    }
    ordered.sort_by_key(|(at, _)| *at);

    let listed = s.list_programs(&programs(bl())).await.unwrap();
    let got: Vec<String> = listed
        .iter()
        .map(|p| p.content.program_name.to_string())
        .collect();
    let want: Vec<String> = ordered.into_iter().map(|(_, name)| name).collect();
    assert_eq!(got, want, "collections are not in creation order");
}

pub async fn privacy_filtering_happens_before_pagination(s: &dyn Storage) {
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    s.create_ven(ven("client-a", "ven-a", &["group1"]), &Fanout::none())
        .await
        .unwrap();

    // Sixty events this VEN may not see, then four it may.
    for _ in 0..60 {
        let mut e = hourly(&p.id, now(), 1);
        e.targets = vec![target("group2")];
        s.create_event(e, now(), &Fanout::none()).await.unwrap();
    }
    for _ in 0..4 {
        let mut e = hourly(&p.id, now(), 1);
        e.targets = vec![target("group1")];
        s.create_event(e, now(), &Fanout::none()).await.unwrap();
    }

    // It asks for both targets; it is entitled to one.
    let access = Access::list(
        ven_role("client-a", &["group1"]),
        vec![target("group1"), target("group2")],
    );
    let page = s.list_events(&events(access)).await.unwrap();
    assert_eq!(
        page.len(),
        4,
        "all four visible events belong on the first page"
    );
}

pub async fn the_active_filter_narrows_before_pagination(s: &dyn Storage) {
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    let past: Timestamp = "2025-01-01T00:00:00Z".parse().unwrap();

    for _ in 0..60 {
        s.create_event(hourly(&p.id, past, 1), now(), &Fanout::none())
            .await
            .unwrap();
    }
    for _ in 0..3 {
        s.create_event(hourly(&p.id, later(), 1), now(), &Fanout::none())
            .await
            .unwrap();
    }

    let query = EventQuery {
        active_at: Some(now()),
        ..events(bl())
    };
    assert_eq!(s.list_events(&query).await.unwrap().len(), 3);
}

pub async fn an_event_with_no_intervals_is_always_active(s: &dyn Storage) {
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    // A report-only event has nothing to elapse.
    s.create_event(EventRequest::new(p.id.clone()), now(), &Fanout::none())
        .await
        .unwrap();
    let query = EventQuery {
        active_at: Some("2099-01-01T00:00:00Z".parse().unwrap()),
        ..events(bl())
    };
    assert_eq!(s.list_events(&query).await.unwrap().len(), 1);
}

pub async fn an_updated_event_gets_a_fresh_active_window(s: &dyn Storage) {
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    let past: Timestamp = "2025-01-01T00:00:00Z".parse().unwrap();
    let e = s
        .create_event(hourly(&p.id, past, 1), now(), &Fanout::none())
        .await
        .unwrap();

    let active = EventQuery {
        active_at: Some(now()),
        ..events(bl())
    };
    assert_eq!(s.list_events(&active).await.unwrap().len(), 0);

    // Move it into the future; the stored window must follow.
    s.update_event(&e.id, hourly(&p.id, later(), 1), now(), &Fanout::none())
        .await
        .unwrap();
    assert_eq!(s.list_events(&active).await.unwrap().len(), 1);
}

pub async fn untargeted_objects_are_visible_to_everyone(s: &dyn Storage) {
    s.create_program(program("public"), now(), &Fanout::none())
        .await
        .unwrap();
    for access in [
        bl(),
        Access::list(ven_role("c", &[]), vec![]),
        Access::list(Role::Anonymous, vec![]),
    ] {
        assert_eq!(s.list_programs(&programs(access)).await.unwrap().len(), 1);
    }
}

pub async fn a_ven_listing_without_targets_sees_no_targeted_object(s: &dyn Storage) {
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    let mut e = hourly(&p.id, now(), 1);
    e.targets = vec![target("group1")];
    s.create_event(e, now(), &Fanout::none()).await.unwrap();

    let access = Access::list(ven_role("c", &["group1"]), vec![]);
    assert!(s.list_events(&events(access)).await.unwrap().is_empty());

    let access = Access::list(ven_role("c", &["group1"]), vec![target("group1")]);
    assert_eq!(s.list_events(&events(access)).await.unwrap().len(), 1);
}

pub async fn ownership_filters_reports(s: &dyn Storage) {
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    let e = s
        .create_event(EventRequest::new(p.id.clone()), now(), &Fanout::none())
        .await
        .unwrap();
    for client in ["c1", "c2"] {
        s.create_report(
            ReportRequest::new(e.id.clone(), ClientName::new(client).unwrap(), Vec::new()),
            Some(ClientId::new(client).unwrap()),
            now(),
            &Fanout::none(),
        )
        .await
        .unwrap();
    }

    let mine = ReportQuery {
        program_id: None,
        event_id: None,
        client_name: None,
        access: Access::list(ven_role("c1", &[]), vec![]),
        page: page(),
    };
    let all = ReportQuery {
        access: bl(),
        ..mine.clone()
    };
    assert_eq!(s.list_reports(&mine).await.unwrap().len(), 1);
    assert_eq!(s.list_reports(&all).await.unwrap().len(), 2);
}

pub async fn a_resource_is_owned_through_its_ven(s: &dyn Storage) {
    let a = s
        .create_ven(ven("c1", "ven-a", &[]), &Fanout::none())
        .await
        .unwrap();
    let b = s
        .create_ven(ven("c2", "ven-b", &[]), &Fanout::none())
        .await
        .unwrap();
    for (v, name) in [(&a, "meter-a"), (&b, "meter-b")] {
        s.create_resource(
            crate::model::Resource {
                id: oid("pending"),
                created_date_time: now(),
                modification_date_time: now(),
                object_type: ObjectType::Resource,
                resource_name: ResourceName::new(name).unwrap(),
                ven_id: v.id.clone(),
                targets: Vec::new(),
                attributes: None,
            },
            &Fanout::none(),
        )
        .await
        .unwrap();
    }

    let query = |access| ResourceQuery {
        ven_id: None,
        resource_name: None,
        access,
        page: page(),
    };
    let mine = s
        .list_resources(&query(Access::list(ven_role("c1", &[]), vec![])))
        .await
        .unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].resource_name.as_str(), "meter-a");
    assert_eq!(s.list_resources(&query(bl())).await.unwrap().len(), 2);
}

/// A VEN is findable by the client that owns it, and an unknown client owns none.
///
/// The one query the notification fan-out narrows itself with: an owned object reaches its owner's
/// VEN topic and no other, so this lookup decides which topic that is (D-124). It had no behaviour
/// in this suite at all, which meant the three backends could have disagreed about it — including
/// about the *absent* case, where the choice is between `None` and an error and only one of them is
/// "this client has no VEN yet", which is an ordinary state during enrolment.
pub async fn a_ven_is_found_by_the_client_that_owns_it(s: &dyn Storage) {
    let created = s
        .create_ven(ven("c1", "ven-a", &["group1"]), &Fanout::none())
        .await
        .unwrap();
    s.create_ven(ven("c2", "ven-b", &[]), &Fanout::none())
        .await
        .unwrap();

    let found = s
        .get_ven_by_client(&ClientId::new("c1").unwrap())
        .await
        .unwrap()
        .expect("c1 owns a VEN");
    assert_eq!(found.id, created.id);
    assert_eq!(found.ven_name.as_str(), "ven-a");
    assert_eq!(found.targets, vec![target("group1")], "the whole object");

    // Absent is `Ok(None)`, not an error: a client that has not enrolled yet is a state, not a
    // failure, and the fan-out reads it on every owned write.
    assert!(
        s.get_ven_by_client(&ClientId::new("nobody").unwrap())
            .await
            .unwrap()
            .is_none()
    );
}

/// Every collection's `update` keeps the object's identity and replaces its content.
///
/// Four of the six object types had no update behaviour here at all, so the SQL backends' update
/// paths for `ven`, `resource`, `report` and `subscription` were exercised only through the HTTP
/// tests — which run against the in-memory backend (D-129). The rule is the same for all of them:
/// the id and `createdDateTime` survive, `modificationDateTime` moves, and the content is the new
/// content rather than a merge of the two.
pub async fn an_update_keeps_the_identity_and_replaces_the_content(s: &dyn Storage) {
    // -- ven
    let v = s
        .create_ven(ven("c1", "ven-a", &["group1"]), &Fanout::none())
        .await
        .unwrap();
    let mut renamed = ven("c1", "ven-renamed", &["group2"]);
    renamed.id = v.id.clone();
    renamed.created_date_time = v.created_date_time;
    renamed.modification_date_time = later();
    let updated = s.update_ven(&v.id, renamed, &Fanout::none()).await.unwrap();
    assert_eq!(updated.id, v.id);
    assert_eq!(updated.created_date_time, v.created_date_time);
    assert_eq!(updated.modification_date_time, later());
    assert_eq!(updated.ven_name.as_str(), "ven-renamed");
    assert_eq!(updated.targets, vec![target("group2")], "targets replaced");
    // And the store agrees on a re-read, which is what distinguishes a write from a return value.
    let read = s.get_ven(&v.id).await.unwrap();
    assert_eq!(read.ven_name.as_str(), "ven-renamed");
    assert_eq!(read.targets, vec![target("group2")]);
    // The new name is the one the index answers to, and the old one is gone.
    assert!(
        s.get_ven_by_client(&ClientId::new("c1").unwrap())
            .await
            .unwrap()
            .is_some_and(|found| found.ven_name.as_str() == "ven-renamed")
    );

    // -- resource
    let r = s
        .create_resource(resource(&v.id, "meter", &["group1"]), &Fanout::none())
        .await
        .unwrap();
    let mut moved = resource(&v.id, "meter-2", &[]);
    moved.id = r.id.clone();
    moved.created_date_time = r.created_date_time;
    moved.modification_date_time = later();
    let updated = s
        .update_resource(&r.id, moved, &Fanout::none())
        .await
        .unwrap();
    assert_eq!(updated.id, r.id);
    assert_eq!(updated.created_date_time, r.created_date_time);
    assert_eq!(updated.resource_name.as_str(), "meter-2");
    assert!(updated.targets.is_empty(), "targets replaced, not merged");
    assert_eq!(
        s.get_resource(&r.id).await.unwrap().resource_name.as_str(),
        "meter-2"
    );
    // The grant is the union of the VEN's targets and its resources', so dropping a resource's
    // targets must move it. This is the read the whole privacy model hangs off.
    assert_eq!(
        s.grant_for(&ClientId::new("c1").unwrap())
            .await
            .unwrap()
            .targets(),
        [target("group2")],
        "the grant still carries a target no object has any more"
    );

    // -- report
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    let e = s
        .create_event(hourly(&p.id, now(), 1), now(), &Fanout::none())
        .await
        .unwrap();
    let filed = s
        .create_report(
            ReportRequest::new(e.id.clone(), ClientName::new("ven-a").unwrap(), Vec::new()),
            Some(ClientId::new("c1").unwrap()),
            now(),
            &Fanout::none(),
        )
        .await
        .unwrap();
    let mut revised =
        ReportRequest::new(e.id.clone(), ClientName::new("ven-a").unwrap(), Vec::new());
    revised.report_name = Some("revised".into());
    let updated = s
        .update_report(&filed.id, revised, later(), &Fanout::none())
        .await
        .unwrap();
    assert_eq!(updated.id, filed.id);
    assert_eq!(updated.created_date_time, filed.created_date_time);
    assert_eq!(updated.modification_date_time, later());
    assert_eq!(updated.content.report_name.as_deref(), Some("revised"));
    assert_eq!(
        updated.client_id, filed.client_id,
        "an update must not re-stamp the owner: the body carries no clientID to take it from"
    );

    // -- subscription
    let sub = s
        .create_subscription(
            watching("c1", ObjectType::Event, "https://example.com/a"),
            ClientId::new("c1").unwrap(),
            OwnerKind::Ven,
            now(),
            &Fanout::none(),
        )
        .await
        .unwrap();
    let updated = s
        .update_subscription(
            &sub.id,
            watching("c1", ObjectType::Report, "https://example.com/b"),
            later(),
            &Fanout::none(),
        )
        .await
        .unwrap();
    assert_eq!(updated.id, sub.id);
    assert_eq!(updated.created_date_time, sub.created_date_time);
    assert_eq!(updated.modification_date_time, later());
    assert_eq!(
        updated.client_id, sub.client_id,
        "an update must not re-stamp the owner"
    );
    let read = s.get_subscription(&sub.id).await.unwrap();
    assert_eq!(
        read.content.object_operations[0].objects,
        [ObjectType::Report]
    );
    assert_eq!(
        read.content.object_operations[0].callback_url,
        "https://example.com/b"
    );
    // The watched-type index has to move with the document, or the fan-out keeps answering from the
    // old one — the index is what `subscribers` reads, and it is a separate table on both SQL
    // backends.
    let by_type = |t| SubscriptionQuery {
        program_id: None,
        client_name: None,
        objects: vec![t],
        access: bl(),
        page: page(),
    };
    assert_eq!(
        s.list_subscriptions(&by_type(ObjectType::Report))
            .await
            .unwrap()
            .len(),
        1,
        "the subscription did not move to the type it now watches"
    );
    assert!(
        s.list_subscriptions(&by_type(ObjectType::Event))
            .await
            .unwrap()
            .is_empty(),
        "the subscription still answers for the type it stopped watching"
    );
    assert_eq!(
        s.subscribers(&[ObjectType::Report]).await.unwrap().len(),
        1,
        "the fan-out query disagrees with the listing"
    );
}

/// Every collection's `delete` returns the object it removed, and it is then gone.
///
/// `report`, `resource` and `subscription` had no delete behaviour in this suite (D-129). The
/// return value is not a nicety: the API answers `200` with the deleted object, and the cascade
/// announcement is built from it.
pub async fn a_delete_returns_the_object_and_then_it_is_gone(s: &dyn Storage) {
    let v = s
        .create_ven(ven("c1", "ven-a", &[]), &Fanout::none())
        .await
        .unwrap();
    let r = s
        .create_resource(resource(&v.id, "meter", &[]), &Fanout::none())
        .await
        .unwrap();
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    let e = s
        .create_event(hourly(&p.id, now(), 1), now(), &Fanout::none())
        .await
        .unwrap();
    let filed = s
        .create_report(
            ReportRequest::new(e.id.clone(), ClientName::new("ven-a").unwrap(), Vec::new()),
            Some(ClientId::new("c1").unwrap()),
            now(),
            &Fanout::none(),
        )
        .await
        .unwrap();
    let sub = s
        .create_subscription(
            watching("c1", ObjectType::Event, "https://example.com/a"),
            ClientId::new("c1").unwrap(),
            OwnerKind::Ven,
            now(),
            &Fanout::none(),
        )
        .await
        .unwrap();

    let removed = s.delete_report(&filed.id, &Fanout::none()).await.unwrap();
    assert_eq!(removed.id, filed.id);
    assert_eq!(
        removed.client_id, filed.client_id,
        "the owner comes back too"
    );
    assert!(matches!(
        s.get_report(&filed.id).await,
        Err(StorageError::NotFound { .. })
    ));

    let removed = s
        .delete_subscription(&sub.id, &Fanout::none())
        .await
        .unwrap();
    assert_eq!(removed.id, sub.id);
    assert!(matches!(
        s.get_subscription(&sub.id).await,
        Err(StorageError::NotFound { .. })
    ));
    // And the watched-type index went with it, or the fan-out keeps queueing for a subscriber that
    // no longer exists.
    assert!(
        s.subscribers(&[ObjectType::Event])
            .await
            .unwrap()
            .is_empty()
    );

    let removed = s.delete_resource(&r.id, &Fanout::none()).await.unwrap();
    assert_eq!(removed.id, r.id);
    assert!(matches!(
        s.get_resource(&r.id).await,
        Err(StorageError::NotFound { .. })
    ));

    // Deleting all of it twice is a `NotFound`, not a silent success.
    for outcome in [
        s.delete_report(&filed.id, &Fanout::none()).await.err(),
        s.delete_subscription(&sub.id, &Fanout::none()).await.err(),
        s.delete_resource(&r.id, &Fanout::none()).await.err(),
    ] {
        assert!(
            matches!(outcome, Some(StorageError::NotFound { .. })),
            "deleting a gone object should say so: {outcome:?}"
        );
    }
}

/// Waiting for work returns, and returns within the budget it was given.
///
/// The last method on the trait with no behaviour here. Its *contract* is weak on purpose — the
/// default is a sleep, and PostgreSQL replaces it with `LISTEN`/`NOTIFY` so a write wakes a
/// dispatcher on commit rather than on a timer — but "returns at all, and no later than the timeout"
/// is shared, and a backend that broke it would stall the outbox for ever with nothing to see: the
/// queue would simply stop draining (D-129).
///
/// That Postgres returns *early* is asserted where it belongs, in that backend's own tests: a
/// shared behaviour asserting it would fail on the two backends that legitimately sleep.
pub async fn waiting_for_work_returns_within_its_timeout(s: &dyn Storage) {
    let budget = core::time::Duration::from_millis(50);
    let started = std::time::Instant::now();
    s.await_outbox(budget).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < core::time::Duration::from_secs(5),
        "await_outbox({budget:?}) took {elapsed:?}; a dispatcher waiting on this never drains"
    );
}

/// A live backend says it is live.
///
/// One line, and it is what `GET /health` answers from — the endpoint an orchestrator restarts a
/// container on.
pub async fn a_reachable_backend_reports_itself_healthy(s: &dyn Storage) {
    assert!(s.healthy().await);
}

pub async fn ownership_filters_subscriptions(s: &dyn Storage) {
    // A subscription carries its subscriber's callbackUrl and bearerToken. This is the behaviour
    // the in-memory backend was missing while both SQL backends had it — the exact divergence a
    // shared suite exists to catch, and a credential leak while it lasted.
    let rule = |client: &str| SubscriptionRequest {
        client_name: ClientName::new(client).unwrap(),
        program_id: None,
        object_operations: vec![ObjectOperation {
            objects: vec![ObjectType::Event],
            operations: vec![Operation::Create],
            callback_url: "https://example.com/hook".into(),
            bearer_token: Some(format!("{client}-secret")),
        }],
        targets: Vec::new(),
    };
    for client in ["c1", "c2"] {
        s.create_subscription(
            rule(client),
            ClientId::new(client).unwrap(),
            OwnerKind::Ven,
            now(),
            &Fanout::none(),
        )
        .await
        .unwrap();
    }

    let query = |access| SubscriptionQuery {
        program_id: None,
        client_name: None,
        objects: Vec::new(),
        access,
        page: page(),
    };
    let mine = s
        .list_subscriptions(&query(Access::list(ven_role("c1", &[]), vec![])))
        .await
        .unwrap();
    assert_eq!(mine.len(), 1, "a VEN must not see another's subscription");
    assert_eq!(mine[0].client_id.as_str(), "c1");
    assert_eq!(s.list_subscriptions(&query(bl())).await.unwrap().len(), 2);
}

pub async fn subscriptions_come_back_in_creation_order(s: &dyn Storage) {
    // Offset pagination over an unordered set returns non-repeatable pages, and this is
    // the one collection whose in-memory implementation forgot to order at all.
    for client in ["c3", "c1", "c2"] {
        s.create_subscription(
            SubscriptionRequest {
                client_name: ClientName::new(client).unwrap(),
                program_id: None,
                object_operations: vec![ObjectOperation {
                    objects: vec![ObjectType::Event],
                    operations: vec![Operation::Create],
                    callback_url: "https://example.com/hook".into(),
                    bearer_token: None,
                }],
                targets: Vec::new(),
            },
            ClientId::new(client).unwrap(),
            OwnerKind::Ven,
            now(),
            &Fanout::none(),
        )
        .await
        .unwrap();
    }
    let all = s
        .list_subscriptions(&SubscriptionQuery {
            program_id: None,
            client_name: None,
            objects: Vec::new(),
            access: bl(),
            page: page(),
        })
        .await
        .unwrap();
    let ids: Vec<&str> = all.iter().map(|s| s.id.as_str()).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(
        ids, sorted,
        "subscriptions must come back in creation order"
    );
}

pub async fn a_target_query_filters_owned_collections(s: &dyn Storage) {
    // `?targets=` on /vens and /resources is an ordinary additive filter — it used to be accepted
    // and then silently ignored, which is worse than refusing it.
    s.create_ven(ven("c1", "ven-a", &["north"]), &Fanout::none())
        .await
        .unwrap();
    s.create_ven(ven("c2", "ven-b", &["south"]), &Fanout::none())
        .await
        .unwrap();
    s.create_ven(ven("c3", "ven-c", &[]), &Fanout::none())
        .await
        .unwrap();

    let query = |requested: Vec<Target>| VenQuery {
        ven_name: None,
        access: Access::list(Role::BusinessLogic, requested),
        page: page(),
    };
    assert_eq!(s.list_vens(&query(vec![])).await.unwrap().len(), 3);
    let north = s.list_vens(&query(vec![target("north")])).await.unwrap();
    assert_eq!(north.len(), 1);
    assert_eq!(north[0].ven_name.as_str(), "ven-a");
    assert!(
        s.list_vens(&query(vec![target("east")]))
            .await
            .unwrap()
            .is_empty()
    );
}

pub async fn a_ven_sees_its_own_targeted_object_without_naming_targets(s: &dyn Storage) {
    // Ownership gates ven objects, not targeting: running the privacy rule over them would hide a
    // VEN's own object from it the moment business logic granted it a target.
    s.create_ven(ven("c1", "ven-a", &["north"]), &Fanout::none())
        .await
        .unwrap();
    let mine = s
        .list_vens(&VenQuery {
            ven_name: None,
            access: Access::list(ven_role("c1", &["north"]), vec![]),
            page: page(),
        })
        .await
        .unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].targets, vec![target("north")]);
}

pub async fn subscriptions_filter_by_watched_object_type(s: &dyn Storage) {
    use crate::model::{ObjectOperation, Operation};
    let rule = |objects: Vec<ObjectType>| SubscriptionRequest {
        client_name: ClientName::new("c1").unwrap(),
        program_id: None,
        object_operations: vec![ObjectOperation {
            objects,
            operations: vec![Operation::Create],
            callback_url: "https://example.com/hook".into(),
            bearer_token: None,
        }],
        targets: Vec::new(),
    };
    let owner = ClientId::new("c1").unwrap();
    s.create_subscription(
        rule(vec![ObjectType::Event]),
        owner.clone(),
        OwnerKind::Ven,
        now(),
        &Fanout::none(),
    )
    .await
    .unwrap();
    s.create_subscription(
        rule(vec![ObjectType::Report]),
        owner.clone(),
        OwnerKind::Ven,
        now(),
        &Fanout::none(),
    )
    .await
    .unwrap();

    let query = |objects| SubscriptionQuery {
        program_id: None,
        client_name: None,
        objects,
        access: bl(),
        page: page(),
    };
    assert_eq!(
        s.list_subscriptions(&query(vec![ObjectType::Event]))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        s.list_subscriptions(&query(Vec::new()))
            .await
            .unwrap()
            .len(),
        2
    );
}

pub async fn name_lookups_are_exact(s: &dyn Storage) {
    s.create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    s.create_program(program("tou-extended"), now(), &Fanout::none())
        .await
        .unwrap();

    let query = ProgramQuery {
        program_name: Some(ProgramName::new("tou").unwrap()),
        ..programs(bl())
    };
    let found = s.list_programs(&query).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].content.program_name.as_str(), "tou");

    let vens = VenQuery {
        ven_name: Some(VenName::new("ven-a").unwrap()),
        access: bl(),
        page: page(),
    };
    s.create_ven(ven("c1", "ven-a", &[]), &Fanout::none())
        .await
        .unwrap();
    s.create_ven(ven("c2", "ven-ab", &[]), &Fanout::none())
        .await
        .unwrap();
    assert_eq!(s.list_vens(&vens).await.unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Grants
// ---------------------------------------------------------------------------

pub async fn a_grant_unions_ven_and_resource_targets(s: &dyn Storage) {
    let client = ClientId::new("c1").unwrap();
    let v = s
        .create_ven(ven("c1", "ven-1", &["group1"]), &Fanout::none())
        .await
        .unwrap();
    s.create_resource(
        crate::model::Resource {
            id: oid("pending"),
            created_date_time: now(),
            modification_date_time: now(),
            object_type: ObjectType::Resource,
            resource_name: ResourceName::new("battery").unwrap(),
            ven_id: v.id.clone(),
            targets: vec![target("areaX")],
            attributes: None,
        },
        &Fanout::none(),
    )
    .await
    .unwrap();

    let grant = s.grant_for(&client).await.unwrap();
    assert_eq!(grant.targets(), &[target("areaX"), target("group1")]);

    let all = s.all_grants().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].1, client);
    assert_eq!(all[0].2.targets(), &[target("areaX"), target("group1")]);
}

pub async fn an_unknown_client_has_an_empty_grant(s: &dyn Storage) {
    let grant = s
        .grant_for(&ClientId::new("nobody").unwrap())
        .await
        .unwrap();
    assert!(grant.is_empty());
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

pub async fn an_event_survives_storage_unchanged(s: &dyn Storage) {
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    let mut sent = hourly(&p.id, now(), 3);
    sent.event_name = Some("day-ahead".into());
    sent.priority = crate::model::Priority::new(7);
    sent.targets = vec![target("group1"), target("group2")];
    sent.duration = Some("P1D".parse().unwrap());

    let created = s
        .create_event(sent.clone(), now(), &Fanout::none())
        .await
        .unwrap();
    assert_eq!(created.content, sent);

    let fetched = s.get_event(&created.id).await.unwrap();
    assert_eq!(fetched, created, "a round trip through storage changed it");
}

pub async fn a_ven_survives_storage_unchanged(s: &dyn Storage) {
    let sent = ven("c1", "ven-1", &["group1", "areaX"]);
    let created = s.create_ven(sent.clone(), &Fanout::none()).await.unwrap();
    assert_eq!(created.ven_name, sent.ven_name);
    assert_eq!(created.targets, sent.targets);
    assert_eq!(s.get_ven(&created.id).await.unwrap(), created);
}

// ---------------------------------------------------------------------------
// The outbox
// ---------------------------------------------------------------------------

/// A fan-out with one subscriber that wants every event, for the atomicity behaviours.
async fn one_subscriber(s: &dyn Storage) -> Fanout {
    let subscription = s
        .create_subscription(
            SubscriptionRequest {
                client_name: ClientName::new("ven-1").unwrap(),
                program_id: None,
                object_operations: vec![ObjectOperation {
                    objects: vec![ObjectType::Event],
                    operations: vec![Operation::Create, Operation::Update, Operation::Delete],
                    callback_url: "https://example.com/hook".into(),
                    bearer_token: None,
                }],
                targets: Vec::new(),
            },
            ClientId::new("c1").unwrap(),
            OwnerKind::Ven,
            now(),
            &Fanout::none(),
        )
        .await
        .unwrap();
    Fanout::new(
        vec![(subscription, ven_role("c1", &["group1"]))],
        Vec::new(),
        None,
        now(),
    )
}

pub async fn a_write_queues_its_own_notifications(s: &dyn Storage) {
    let fanout = one_subscriber(s).await;
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();

    // No separate enqueue call: the store did it, inside the same transaction as the event.
    let event = s
        .create_event(hourly(&p.id, now(), 1), now(), &fanout)
        .await
        .unwrap();

    let queued = s.claim_due(now(), 10, lease(), "d1").await.unwrap();
    assert_eq!(queued.len(), 1, "the write did not queue its notification");
    assert_eq!(
        queued[0].delivery.notification.object.id(),
        &event.id,
        "the queued notification names a different object"
    );
    assert_eq!(queued[0].delivery.notification.operation, Operation::Create);
}

/// A subscriber that wants deletions of `objects`, owned by `client`.
async fn deletions_subscriber(
    s: &dyn Storage,
    client: &str,
    objects: Vec<ObjectType>,
    grants: Vec<(ObjectId, ClientId, Grant)>,
) -> Fanout {
    let subscription = s
        .create_subscription(
            SubscriptionRequest {
                client_name: ClientName::new(client).unwrap(),
                program_id: None,
                object_operations: vec![ObjectOperation {
                    objects,
                    operations: vec![Operation::Delete],
                    callback_url: "https://example.com/hook".into(),
                    bearer_token: None,
                }],
                targets: Vec::new(),
            },
            ClientId::new(client).unwrap(),
            OwnerKind::Ven,
            now(),
            &Fanout::none(),
        )
        .await
        .unwrap();
    Fanout::new(
        vec![(subscription, ven_role(client, &[]))],
        grants,
        None,
        now(),
    )
}

pub async fn a_cascade_announces_what_it_deleted(s: &dyn Storage) {
    // Deleting a programme takes its events with it. The database does that silently, so a VEN
    // subscribed to event deletions would otherwise hold a dispatch instruction for an event that
    // no longer exists — and OpenADR has no way to tell it afterwards.
    let fanout = deletions_subscriber(
        s,
        "c1",
        vec![
            ObjectType::Program,
            ObjectType::Event,
            ObjectType::Subscription,
        ],
        Vec::new(),
    )
    .await;
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    let event = s
        .create_event(hourly(&p.id, now(), 1), now(), &Fanout::none())
        .await
        .unwrap();

    s.delete_program(&p.id, &fanout).await.unwrap();

    let queued = s.claim_due(now(), 10, lease(), "d1").await.unwrap();
    let announced: Vec<(ObjectType, &ObjectId)> = queued
        .iter()
        .map(|q| {
            (
                q.delivery.notification.object.object_type(),
                q.delivery.notification.object.id(),
            )
        })
        .collect();
    assert!(
        announced.contains(&(ObjectType::Event, &event.id)),
        "the cascaded event was deleted without saying so: {announced:?}"
    );
    assert!(
        announced.contains(&(ObjectType::Program, &p.id)),
        "{announced:?}"
    );
    assert!(
        queued
            .iter()
            .all(|q| q.delivery.notification.operation == Operation::Delete)
    );
}

pub async fn a_programme_scoped_subscription_goes_with_its_programme(s: &dyn Storage) {
    // `subscriptionRequest.programID` is a foreign key. Leaving one dangling gives a VEN a
    // subscription that can never match anything; deleting it silently gives it one that vanished.
    // So it goes, and it is announced — and every backend has to agree, because two of them used to
    // delete it and one did not.
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    let scoped = s
        .create_subscription(
            SubscriptionRequest {
                client_name: ClientName::new("c1").unwrap(),
                program_id: Some(p.id.clone()),
                object_operations: vec![ObjectOperation {
                    objects: vec![ObjectType::Event],
                    operations: vec![Operation::Create],
                    callback_url: "https://example.com/hook".into(),
                    bearer_token: None,
                }],
                targets: Vec::new(),
            },
            ClientId::new("c1").unwrap(),
            OwnerKind::Ven,
            now(),
            &Fanout::none(),
        )
        .await
        .unwrap();

    s.delete_program(&p.id, &Fanout::none()).await.unwrap();

    let query = SubscriptionQuery {
        program_id: None,
        client_name: None,
        objects: Vec::new(),
        access: bl(),
        page: page(),
    };
    let left = s.list_subscriptions(&query).await.unwrap();
    assert!(
        !left.iter().any(|s| s.id == scoped.id),
        "a subscription scoped to a deleted programme must not survive it: {left:?}"
    );
}

/// Create a programme whose cascade removes one of every kind of row that carries targets.
///
/// Not a behaviour — a *fixture*, for the two backends that keep targets in a side table. The
/// suite cannot state this one: `object_target` is a schema detail no trait method exposes, and the
/// in-memory backend has no such table to leak. See [`ORPHANED_TARGETS`] for what the SQL backends
/// then assert, and D-123 for what they were leaking.
///
/// Returns the programme's id.
pub async fn seed_a_cascade_of_targeted_children(s: &dyn Storage) -> ObjectId {
    // Named uniquely, because this fixture runs against a database the other behaviours are using
    // at the same time and `programName` is unique per VTN.
    static NEXT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let name = crate::std_shim::format!("cascade-{n}");

    let p = s
        .create_program(program(&name), now(), &Fanout::none())
        .await
        .unwrap();
    // A programme with targets of its own.
    let mut request = program(&name);
    request.targets = vec![target("group1")];
    s.update_program(&p.id, request, now(), &Fanout::none())
        .await
        .unwrap();
    // A targeted event under it.
    let mut event = hourly(&p.id, now(), 1);
    event.targets = vec![target("group1")];
    s.create_event(event, now(), &Fanout::none()).await.unwrap();
    // And a targeted subscription scoped to it, which is the one the cascade forgot.
    s.create_subscription(
        SubscriptionRequest {
            client_name: ClientName::new("c1").unwrap(),
            program_id: Some(p.id.clone()),
            object_operations: vec![ObjectOperation {
                objects: vec![ObjectType::Event],
                operations: vec![Operation::Create],
                callback_url: "https://example.com/hook".into(),
                bearer_token: None,
            }],
            targets: vec![target("group1")],
        },
        ClientId::new("c1").unwrap(),
        OwnerKind::Ven,
        now(),
        &Fanout::none(),
    )
    .await
    .unwrap();
    p.id
}

/// Target rows whose object is gone: what a SQL backend must find none of, ever.
///
/// One statement, shared, so the two backends cannot check different things. It states the
/// invariant rather than counting rows — `SELECT COUNT(*) FROM object_target` after a truncate
/// would also do, but only on a database nothing else is using, and both backends run their
/// behaviours concurrently against one.
pub const ORPHANED_TARGETS: &str = "\
SELECT COUNT(*) FROM object_target t \
WHERE NOT EXISTS (SELECT 1 FROM program p WHERE p.id = t.object_id) \
  AND NOT EXISTS (SELECT 1 FROM event e WHERE e.id = t.object_id) \
  AND NOT EXISTS (SELECT 1 FROM subscription s WHERE s.id = t.object_id) \
  AND NOT EXISTS (SELECT 1 FROM ven v WHERE v.id = t.object_id) \
  AND NOT EXISTS (SELECT 1 FROM resource r WHERE r.id = t.object_id)";

pub async fn an_event_deletion_announces_its_reports(s: &dyn Storage) {
    // `report.eventID` cascades, so deleting an event takes the reports filed against it. The
    // database does that silently. All three backends did too — the suite named the programme
    // cascade and the VEN cascade and not this one, so all three were consistently wrong, which is
    // exactly what a shared suite is supposed to make impossible.
    let fanout = deletions_subscriber(
        s,
        "c1",
        vec![ObjectType::Event, ObjectType::Report],
        Vec::new(),
    )
    .await;
    let p = s
        .create_program(program("tou"), now(), &Fanout::none())
        .await
        .unwrap();
    let event = s
        .create_event(hourly(&p.id, now(), 1), now(), &Fanout::none())
        .await
        .unwrap();
    let report = s
        .create_report(
            ReportRequest::new(event.id.clone(), ClientName::new("c1").unwrap(), Vec::new()),
            Some(ClientId::new("c1").unwrap()),
            now(),
            &Fanout::none(),
        )
        .await
        .unwrap();

    s.delete_event(&event.id, &fanout).await.unwrap();

    let announced: Vec<(ObjectType, ObjectId)> = s
        .claim_due(now(), 10, lease(), "d1")
        .await
        .unwrap()
        .iter()
        .map(|q| {
            (
                q.delivery.notification.object.object_type(),
                q.delivery.notification.object.id().clone(),
            )
        })
        .collect();
    assert!(
        announced.contains(&(ObjectType::Report, report.id.clone())),
        "the cascaded report was deleted without saying so: {announced:?}"
    );
    assert!(announced.contains(&(ObjectType::Event, event.id.clone())));
    // And it really is gone, not merely announced.
    assert!(matches!(
        s.get_report(&report.id).await,
        Err(StorageError::NotFound { .. })
    ));
}

pub async fn a_ven_deletion_announces_its_resources(s: &dyn Storage) {
    let v = s
        .create_ven(ven("c1", "ven-x", &[]), &Fanout::none())
        .await
        .unwrap();
    let resource = s
        .create_resource(
            crate::model::Resource {
                id: oid("pending"),
                created_date_time: now(),
                modification_date_time: now(),
                object_type: ObjectType::Resource,
                resource_name: ResourceName::new("battery").unwrap(),
                ven_id: v.id.clone(),
                targets: Vec::new(),
                attributes: None,
            },
            &Fanout::none(),
        )
        .await
        .unwrap();
    // The grant snapshot is what maps a resource's `venID` back to the client that owns it.
    let fanout = deletions_subscriber(
        s,
        "c1",
        vec![ObjectType::Resource],
        vec![(v.id.clone(), ClientId::new("c1").unwrap(), Grant::default())],
    )
    .await;

    s.delete_ven(&v.id, &fanout).await.unwrap();

    let queued = s.claim_due(now(), 10, lease(), "d1").await.unwrap();
    assert!(
        queued
            .iter()
            .any(|q| q.delivery.notification.object.id() == &resource.id),
        "the cascaded resource was deleted without saying so"
    );
}

pub async fn a_write_that_fails_queues_nothing(s: &dyn Storage) {
    let fanout = one_subscriber(s).await;

    // A dangling programID: the event is refused, so nobody may be told about it.
    let mut request = hourly(&oid("prg-missing"), now(), 1);
    request.program_id = oid("prg-missing");
    assert!(s.create_event(request, now(), &fanout).await.is_err());

    assert!(
        s.outbox_stats(now()).await.unwrap().is_idle(),
        "a change that did not happen was announced anyway"
    );
}

pub async fn an_abandoned_entry_can_be_seen_and_revived(s: &dyn Storage) {
    // `GET /health` says `dead: 1`. That is a number to alert on and not one to act on: which
    // subscriber, which object, and what went wrong are all in the row, and reading them out of the
    // database by hand is not an operational procedure. Nor is fixing the receiver and having no
    // way to make the entry due again.
    let d = delivery(s, "tou").await;
    s.enqueue(vec![d.clone()], now()).await.unwrap();
    let claimed = s.claim_due(now(), 1, lease(), "d1").await.unwrap();
    s.record_failure(
        claimed[0].id,
        &DeliveryFailure::permanent("410 Gone"),
        now(),
        &eager(),
    )
    .await
    .unwrap();
    assert_eq!(s.outbox_stats(now()).await.unwrap().dead, 1);

    let dead = s.dead_letters(10).await.unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].object_type, ObjectType::Program);
    assert_eq!(dead[0].destination, "https://example.com/tou");
    assert_eq!(dead[0].last_error.as_deref(), Some("410 Gone"));
    assert_eq!(dead[0].subscription_id.as_ref(), Some(&oid("sub-1")));

    assert_eq!(s.revive_dead(now()).await.unwrap(), 1);
    let stats = s.outbox_stats(now()).await.unwrap();
    assert_eq!((stats.pending, stats.dead), (1, 0));

    // And the attempt counter went back to zero with it: charging a fixed receiver for the
    // attempts a broken one cost would abandon the entry again on its first try.
    let again = s.claim_due(now(), 1, lease(), "d2").await.unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].attempts, 0);
    assert_eq!(again[0].delivery, d);
}

/// A queued delivery for a freshly created programme.
async fn delivery(s: &dyn Storage, name: &str) -> Delivery {
    let p = s
        .create_program(program(name), now(), &Fanout::none())
        .await
        .unwrap();
    Delivery {
        subscription_id: Some(oid("sub-1")),
        route: Route::Webhook {
            callback_url: format!("https://example.com/{name}"),
            bearer_token: None,
        },
        notification: Notification::new(Operation::Create, AnyObject::Program(p)),
    }
}

fn eager() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        // Zero, so a retry is due immediately and the test does not sleep.
        base_delay: core::time::Duration::ZERO,
        max_delay: core::time::Duration::ZERO,
    }
}

fn lease() -> core::time::Duration {
    core::time::Duration::from_secs(60)
}

pub async fn a_queued_delivery_comes_back_intact(s: &dyn Storage) {
    let sent = delivery(s, "tou").await;
    s.enqueue(vec![sent.clone()], now()).await.unwrap();

    let claimed = s.claim_due(now(), 10, lease(), "d1").await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].attempts, 0);
    assert_eq!(
        claimed[0].delivery, sent,
        "a round trip through the queue changed the delivery"
    );
}

pub async fn a_claimed_entry_is_not_handed_to_a_second_dispatcher(s: &dyn Storage) {
    let d = delivery(s, "tou").await;
    s.enqueue(vec![d], now()).await.unwrap();

    assert_eq!(
        s.claim_due(now(), 10, lease(), "d1").await.unwrap().len(),
        1
    );
    // This is the whole point of the lease: two dispatchers must never deliver the same entry.
    assert!(
        s.claim_due(now(), 10, lease(), "d2")
            .await
            .unwrap()
            .is_empty()
    );
}

pub async fn an_expired_lease_releases_the_entry(s: &dyn Storage) {
    let d = delivery(s, "tou").await;
    s.enqueue(vec![d], now()).await.unwrap();
    let claimed = s.claim_due(now(), 10, lease(), "d1").await.unwrap();
    assert_eq!(claimed.len(), 1);

    // A dispatcher that dies mid-delivery costs one lease of delay, not the notification.
    let after = now().checked_add(jiff::Span::new().hours(1)).unwrap();
    let retaken = s.claim_due(after, 10, lease(), "d2").await.unwrap();
    assert_eq!(retaken.len(), 1);
    assert_eq!(retaken[0].id, claimed[0].id);
}

pub async fn completing_an_entry_removes_it(s: &dyn Storage) {
    let d = delivery(s, "tou").await;
    s.enqueue(vec![d], now()).await.unwrap();
    let claimed = s.claim_due(now(), 10, lease(), "d1").await.unwrap();
    s.complete(claimed[0].id).await.unwrap();

    assert!(s.outbox_stats(now()).await.unwrap().is_idle());
    let after = now().checked_add(jiff::Span::new().hours(1)).unwrap();
    assert!(
        s.claim_due(after, 10, lease(), "d1")
            .await
            .unwrap()
            .is_empty()
    );
}

pub async fn a_failure_counts_and_is_retried_until_it_is_abandoned(s: &dyn Storage) {
    let d = delivery(s, "tou").await;
    s.enqueue(vec![d], now()).await.unwrap();
    let policy = eager();

    for attempt in 1..=policy.max_attempts {
        let claimed = s.claim_due(now(), 10, lease(), "d1").await.unwrap();
        assert_eq!(claimed.len(), 1, "attempt {attempt} found nothing to do");
        assert_eq!(claimed[0].attempts, attempt - 1, "the count is not durable");
        let abandoned = s
            .record_failure(
                claimed[0].id,
                &DeliveryFailure::retriable("connection refused"),
                now(),
                &policy,
            )
            .await
            .unwrap();
        assert_eq!(abandoned, attempt == policy.max_attempts);
    }

    // Abandoned, not deleted: an operator has to be able to see what was given up on.
    assert!(
        s.claim_due(now(), 10, lease(), "d1")
            .await
            .unwrap()
            .is_empty()
    );
    let stats = s.outbox_stats(now()).await.unwrap();
    assert_eq!(stats.pending, 0);
    assert_eq!(stats.dead, 1);
}

pub async fn a_retry_waits_for_its_backoff(s: &dyn Storage) {
    let d = delivery(s, "tou").await;
    s.enqueue(vec![d], now()).await.unwrap();
    let policy = RetryPolicy {
        max_attempts: 5,
        base_delay: core::time::Duration::from_secs(60),
        max_delay: core::time::Duration::from_secs(60),
    };
    let claimed = s.claim_due(now(), 10, lease(), "d1").await.unwrap();
    s.record_failure(
        claimed[0].id,
        &DeliveryFailure::retriable("boom"),
        now(),
        &policy,
    )
    .await
    .unwrap();

    // Released by the failure, but not yet due.
    assert!(
        s.claim_due(now(), 10, lease(), "d1")
            .await
            .unwrap()
            .is_empty(),
        "a failed entry was retried before its backoff elapsed"
    );
    let later = now().checked_add(jiff::Span::new().seconds(61)).unwrap();
    assert_eq!(
        s.claim_due(later, 10, lease(), "d1").await.unwrap().len(),
        1
    );
}

pub async fn a_permanent_failure_is_abandoned_on_the_first_attempt(s: &dyn Storage) {
    let d = delivery(s, "tou").await;
    s.enqueue(vec![d], now()).await.unwrap();
    let claimed = s.claim_due(now(), 10, lease(), "d1").await.unwrap();

    // A 400 is the receiver saying "not this, ever". Seven more attempts would only be traffic.
    let abandoned = s
        .record_failure(
            claimed[0].id,
            &DeliveryFailure::permanent("callback returned 400"),
            now(),
            &RetryPolicy::default(),
        )
        .await
        .unwrap();
    assert!(abandoned, "a permanent failure must not be retried");

    let later = now().checked_add(jiff::Span::new().hours(24)).unwrap();
    assert!(
        s.claim_due(later, 10, lease(), "d1")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(s.outbox_stats(now()).await.unwrap().dead, 1);
}

pub async fn the_queue_drains_in_the_order_it_was_filled(s: &dyn Storage) {
    let a = delivery(s, "a").await;
    let b = delivery(s, "b").await;
    let c = delivery(s, "c").await;
    s.enqueue(vec![a.clone(), b.clone()], now()).await.unwrap();
    s.enqueue(vec![c.clone()], now()).await.unwrap();

    let claimed = s.claim_due(now(), 2, lease(), "d1").await.unwrap();
    let urls: Vec<_> = claimed
        .iter()
        .map(|q| q.delivery.route.callback_url().unwrap().to_string())
        .collect();
    assert_eq!(
        urls,
        vec![
            a.route.callback_url().unwrap().to_string(),
            b.route.callback_url().unwrap().to_string()
        ]
    );
    assert!(claimed[0].id < claimed[1].id, "ids are not monotonic");
}

pub async fn the_stats_say_how_far_behind_the_queue_is(s: &dyn Storage) {
    assert!(s.outbox_stats(now()).await.unwrap().is_idle());

    let d = delivery(s, "tou").await;
    s.enqueue(vec![d], now()).await.unwrap();
    let later = now().checked_add(jiff::Span::new().seconds(90)).unwrap();
    let stats = s.outbox_stats(later).await.unwrap();
    assert_eq!(stats.pending, 1);
    assert_eq!(stats.dead, 0);
    // A queue that is short but old is stuck, which is the number an operator alerts on.
    assert_eq!(stats.oldest_pending_seconds, Some(90));
}

/// The fan-out's query returns what kind of client owns each subscription.
///
/// The whole point of the method, and the reason it exists beside `list_subscriptions` rather than
/// as an argument to it. A subscription's `clientID` says *who*; nothing on the wire says *what*,
/// and the fan-out has no credential to ask. A backend that loses the distinction turns every
/// business-logic subscriber into a VEN with an empty grant, which is told about no targeted object
/// at all — silently, on the one path where silence is the failure.
pub async fn the_fanout_query_carries_the_owner_kind(s: &dyn Storage) {
    let rule = |objects: Vec<ObjectType>| SubscriptionRequest {
        client_name: ClientName::new("c1").unwrap(),
        program_id: None,
        object_operations: vec![ObjectOperation {
            objects,
            operations: vec![Operation::Create],
            callback_url: "https://example.com/hook".into(),
            bearer_token: None,
        }],
        targets: Vec::new(),
    };
    let bl = ClientId::new("bl").unwrap();
    let ven = ClientId::new("ven-1").unwrap();
    s.create_subscription(
        rule(vec![ObjectType::Event]),
        bl.clone(),
        OwnerKind::BusinessLogic,
        now(),
        &Fanout::none(),
    )
    .await
    .unwrap();
    s.create_subscription(
        rule(vec![ObjectType::Event]),
        ven.clone(),
        OwnerKind::Ven,
        now(),
        &Fanout::none(),
    )
    .await
    .unwrap();
    // A subscription watching something else must not be swept up.
    s.create_subscription(
        rule(vec![ObjectType::Report]),
        ven.clone(),
        OwnerKind::Ven,
        now(),
        &Fanout::none(),
    )
    .await
    .unwrap();

    let subscribers = s.subscribers(&[ObjectType::Event]).await.unwrap();
    assert_eq!(subscribers.len(), 2, "{subscribers:?}");
    let kind = |client: &ClientId| {
        subscribers
            .iter()
            .find(|s| &s.subscription.client_id == client)
            .map(|s| s.owner)
    };
    assert_eq!(kind(&bl), Some(OwnerKind::BusinessLogic));
    assert_eq!(kind(&ven), Some(OwnerKind::Ven));

    // Creation order, like every other collection, so a fan-out is reproducible.
    assert!(
        subscribers[0].subscription.id <= subscribers[1].subscription.id,
        "the fan-out query is not in creation order"
    );

    // And nothing at all when nothing could match.
    assert!(s.subscribers(&[]).await.unwrap().is_empty());
    assert!(s.subscribers(&[ObjectType::Ven]).await.unwrap().is_empty());
}

/// The subscriber circuit breaker: consecutive abandonments, and everything that resets them.
///
/// Three backends and three ways to get an upsert-with-a-counter wrong. The one that matters most
/// is the last assertion: a delivery that *worked* clears the row entirely, so a subscriber that is
/// merely flaky is never cut off — a breaker that counted total failures rather than consecutive
/// ones would eventually cut off every subscriber in a busy VTN.
pub async fn the_breaker_counts_consecutive_abandonments(s: &dyn Storage) {
    let subscription = subscription_for(s, "c1").await;
    let policy = BreakerPolicy {
        threshold: 2,
        cooldown: core::time::Duration::from_secs(600),
    };

    assert!(s.subscriber_health().await.unwrap().is_empty());

    // A retry that has not been given up on counts for nothing.
    assert!(
        !s.note_delivery(&subscription, false, now(), &policy, None)
            .await
            .unwrap()
    );
    assert!(s.subscriber_health().await.unwrap().is_empty());

    // One abandonment: recorded, not yet cut off.
    assert!(
        !s.note_delivery(&subscription, true, now(), &policy, Some("500"))
            .await
            .unwrap(),
        "one failure below the threshold must not open the breaker"
    );
    let health = s.subscriber_health().await.unwrap();
    assert_eq!(health.len(), 1);
    assert_eq!(health[0].subscription_id, subscription);
    assert_eq!(health[0].consecutive_failures, 1);
    assert_eq!(health[0].last_error.as_deref(), Some("500"));
    assert!(!health[0].is_cut_off(now()), "cut off one failure early");

    // The second opens it, and says so exactly once.
    assert!(
        s.note_delivery(&subscription, true, now(), &policy, Some("503"))
            .await
            .unwrap(),
        "reaching the threshold must report that the breaker opened"
    );
    assert!(
        !s.note_delivery(&subscription, true, now(), &policy, Some("503"))
            .await
            .unwrap(),
        "an already-open breaker must not report opening again"
    );
    let health = s.subscriber_health().await.unwrap();
    assert_eq!(health[0].consecutive_failures, 3);
    assert!(health[0].is_cut_off(now()));
    // `cut_off_since` says when the outage started, not when it was last extended.
    assert_eq!(health[0].cut_off_since, Some(now()));

    // Past the cooldown the breaker is half-open: one probe is let through.
    let later = now().checked_add(jiff::Span::new().seconds(601)).unwrap();
    assert!(
        !health[0].is_cut_off(later),
        "the breaker never reopens, so nothing would ever probe it"
    );

    // And a delivery that works clears the row completely.
    s.note_delivery(&subscription, false, later, &policy, None)
        .await
        .unwrap();
    assert!(
        s.subscriber_health().await.unwrap().is_empty(),
        "a successful delivery must forget the failures, or a flaky subscriber accumulates for ever"
    );
}

/// Clearing every breaker is what `POST /admin/outbox/retry` does alongside reviving the queue.
pub async fn every_breaker_can_be_closed_at_once(s: &dyn Storage) {
    let policy = BreakerPolicy {
        threshold: 1,
        cooldown: core::time::Duration::from_secs(600),
    };
    for client in ["c1", "c2"] {
        let subscription = subscription_for(s, client).await;
        s.note_delivery(&subscription, true, now(), &policy, Some("gone"))
            .await
            .unwrap();
    }
    assert_eq!(s.subscriber_health().await.unwrap().len(), 2);
    assert_eq!(s.clear_subscriber_health().await.unwrap(), 2);
    assert!(s.subscriber_health().await.unwrap().is_empty());
    assert_eq!(s.clear_subscriber_health().await.unwrap(), 0);
}

/// One subscription owned by `client`, for the breaker behaviours.
async fn subscription_for(s: &dyn Storage, client: &str) -> ObjectId {
    s.create_subscription(
        SubscriptionRequest {
            client_name: ClientName::new(client).unwrap(),
            program_id: None,
            object_operations: vec![ObjectOperation {
                objects: vec![ObjectType::Event],
                operations: vec![Operation::Create],
                callback_url: "https://example.com/hook".into(),
                bearer_token: None,
            }],
            targets: Vec::new(),
        },
        ClientId::new(client).unwrap(),
        OwnerKind::Ven,
        now(),
        &Fanout::none(),
    )
    .await
    .unwrap()
    .id
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

/// Three reports at three ages, for the retention behaviours.
///
/// Written through `create_report` with an explicit `now`, because retention reads
/// `createdDateTime` and a fixture that set it any other way would be testing a column the write
/// path does not fill.
async fn three_aged_reports(s: &dyn Storage) -> (Vec<ObjectId>, Timestamp) {
    let p = s
        .create_program(program("retention"), now(), &Fanout::none())
        .await
        .unwrap();
    let e = s
        .create_event(EventRequest::new(p.id.clone()), now(), &Fanout::none())
        .await
        .unwrap();

    let mut ids = Vec::new();
    let mut at = now();
    for day in 0..3 {
        at = now()
            .checked_add(jiff::Span::new().hours(day * 24))
            .expect("inside the representable range");
        let r = s
            .create_report(
                ReportRequest::new(e.id.clone(), ClientName::new("ven-1").unwrap(), Vec::new()),
                Some(ClientId::new("c1").unwrap()),
                at,
                &Fanout::none(),
            )
            .await
            .unwrap();
        ids.push(r.id);
    }
    (ids, at)
}

/// A bounded sweep removes the *oldest* reports, not an arbitrary `limit` of them.
///
/// The bound exists so a first sweep over a year of data is many small transactions. A backend that
/// honoured the bound but chose arbitrary rows would make progress that never reaches the far end,
/// and the table would keep the oldest data for ever while appearing to be swept.
pub async fn retention_removes_the_oldest_reports_first(s: &dyn Storage) {
    let (ids, _) = three_aged_reports(s).await;
    let cutoff = now()
        .checked_add(jiff::Span::new().hours(48))
        .expect("inside the representable range");

    // Two are older than the cutoff; take one.
    assert_eq!(s.purge_reports(cutoff, 1).await.unwrap(), 1);
    assert!(s.get_report(&ids[0]).await.is_err(), "the oldest survived");
    assert!(s.get_report(&ids[1]).await.is_ok());
    assert!(s.get_report(&ids[2]).await.is_ok());

    // Then the second, and then nothing: the third is not old enough.
    assert_eq!(s.purge_reports(cutoff, 10).await.unwrap(), 1);
    assert_eq!(s.purge_reports(cutoff, 10).await.unwrap(), 0);
    assert!(
        s.get_report(&ids[2]).await.is_ok(),
        "a report inside the retention window was deleted"
    );
}

/// Retention removes reports and nothing else, and announces nothing.
///
/// The events and programmes a report hangs off are business logic's, and deleting one because a
/// report aged out would remove a schedule nobody asked to remove. Nor is a notification queued:
/// expiry is the VTN's housekeeping, and telling every subscriber that ten thousand old reports
/// have aged out is a fan-out nobody asked for.
pub async fn retention_touches_nothing_but_reports(s: &dyn Storage) {
    let (ids, _) = three_aged_reports(s).await;
    let event_id = s
        .get_report(&ids[0])
        .await
        .unwrap()
        .content
        .event_id
        .clone();
    let program_id = s
        .get_event(&event_id)
        .await
        .unwrap()
        .content
        .program_id
        .clone();

    // Drain whatever the fixture queued, so what is counted afterwards is retention's alone.
    s.claim_due(
        now(),
        1000,
        std::time::Duration::from_secs(60),
        "retention-test",
    )
    .await
    .unwrap();

    let far_future = now()
        .checked_add(jiff::Span::new().hours(3650 * 24))
        .expect("inside the representable range");
    assert_eq!(s.purge_reports(far_future, 100).await.unwrap(), 3);

    assert!(
        s.get_event(&event_id).await.is_ok(),
        "retention deleted an event"
    );
    assert!(
        s.get_program(&program_id).await.is_ok(),
        "retention deleted a programme"
    );

    let queued = s
        .claim_due(
            now(),
            1000,
            std::time::Duration::from_secs(60),
            "retention-test-2",
        )
        .await
        .unwrap()
        .len();
    assert_eq!(
        queued, 0,
        "retention queued {queued} notification(s); it must queue none"
    );
}

/// The report table reports its own size and age.
pub async fn the_report_stats_say_how_far_back_the_data_goes(s: &dyn Storage) {
    let empty = s.report_stats(now()).await.unwrap();
    assert_eq!(empty.count, 0);
    assert_eq!(empty.oldest_seconds, None);

    let (_, newest) = three_aged_reports(s).await;
    let stats = s.report_stats(newest).await.unwrap();
    assert_eq!(stats.count, 3);
    // The oldest was written two days before the newest.
    assert_eq!(stats.oldest_seconds, Some(2 * 86_400));
}

/// Run everything against one backend.
///
/// Each behaviour gets a freshly built backend, because they are not independent otherwise.
macro_rules! run_suite {
    // A backend that needs a server produces `Some` when it has one and `None` when it does not,
    // and the `None` is announced. Skipping is a decision the caller states out loud; the
    // alternative — a suite that quietly passes because it did nothing — is how a backend stops
    // being tested without anyone noticing.
    //
    // The probe runs once, for the decision. Each behaviour then builds its own backend, because
    // they are not independent otherwise.
    (@optional $make:expr) => {
        #[tokio::test]
        async fn storage_conformance() {
            if $make.await.is_none() {
                eprintln!(
                    "skipping storage conformance: no server for this backend, and none could be \
                     started"
                );
                return;
            }
            macro_rules! check {
                ($name:ident) => {{
                    let backend = $make
                        .await
                        .expect("the backend answered a moment ago");
                    super::super::suite::$name(backend.as_ref()).await;
                }};
            }
            super::super::suite::run_suite!(@list);
        }
    };
    ($make:expr) => {
        #[tokio::test]
        async fn storage_conformance() {
            macro_rules! check {
                ($name:ident) => {{
                    let backend = $make.await;
                    super::super::suite::$name(backend.as_ref()).await;
                }};
            }
            super::super::suite::run_suite!(@list);
        }
    };
    (@list) => {{
            // Listed explicitly rather than discovered, so that adding a behaviour to the suite
            // fails to compile until every backend runs it.
            check!(programme_names_are_unique);
            check!(an_event_needs_an_existing_programme);
            check!(deleting_a_programme_takes_its_events_and_reports);
            check!(deleting_a_ven_takes_its_resources);
            check!(one_ven_object_per_client);
            check!(ven_names_are_unique);
            check!(resource_names_are_unique_within_a_ven);
            check!(a_no_op_update_does_not_bump_the_modification_time);
            check!(identifiers_are_sequenced_per_kind);
            check!(a_missing_object_is_reported_not_invented);
            check!(pagination_is_ordered_and_complete);
            check!(creation_order_survives_sub_second_timestamps);
            check!(privacy_filtering_happens_before_pagination);
            check!(the_active_filter_narrows_before_pagination);
            check!(an_event_with_no_intervals_is_always_active);
            check!(an_updated_event_gets_a_fresh_active_window);
            check!(untargeted_objects_are_visible_to_everyone);
            check!(a_ven_listing_without_targets_sees_no_targeted_object);
            check!(ownership_filters_reports);
            check!(a_resource_is_owned_through_its_ven);
            check!(a_ven_is_found_by_the_client_that_owns_it);
            check!(an_update_keeps_the_identity_and_replaces_the_content);
            check!(a_delete_returns_the_object_and_then_it_is_gone);
            check!(a_reachable_backend_reports_itself_healthy);
            check!(waiting_for_work_returns_within_its_timeout);
            check!(subscriptions_filter_by_watched_object_type);
            check!(ownership_filters_subscriptions);
            check!(subscriptions_come_back_in_creation_order);
            check!(a_target_query_filters_owned_collections);
            check!(a_ven_sees_its_own_targeted_object_without_naming_targets);
            check!(name_lookups_are_exact);
            check!(a_grant_unions_ven_and_resource_targets);
            check!(an_unknown_client_has_an_empty_grant);
            check!(an_event_survives_storage_unchanged);
            check!(a_ven_survives_storage_unchanged);
            check!(a_queued_delivery_comes_back_intact);
            check!(a_claimed_entry_is_not_handed_to_a_second_dispatcher);
            check!(an_expired_lease_releases_the_entry);
            check!(completing_an_entry_removes_it);
            check!(a_failure_counts_and_is_retried_until_it_is_abandoned);
            check!(a_retry_waits_for_its_backoff);
            check!(a_permanent_failure_is_abandoned_on_the_first_attempt);
            check!(the_queue_drains_in_the_order_it_was_filled);
            check!(the_stats_say_how_far_behind_the_queue_is);
            check!(an_abandoned_entry_can_be_seen_and_revived);
            check!(a_write_queues_its_own_notifications);
            check!(a_cascade_announces_what_it_deleted);
            check!(a_programme_scoped_subscription_goes_with_its_programme);
            check!(an_event_deletion_announces_its_reports);
            check!(a_ven_deletion_announces_its_resources);
            check!(a_write_that_fails_queues_nothing);
            check!(the_fanout_query_carries_the_owner_kind);
            check!(the_breaker_counts_consecutive_abandonments);
            check!(every_breaker_can_be_closed_at_once);
            check!(retention_removes_the_oldest_reports_first);
            check!(retention_touches_nothing_but_reports);
            check!(the_report_stats_say_how_far_back_the_data_goes);
    }};
}

pub(crate) use run_suite;
