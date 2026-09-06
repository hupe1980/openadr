//! The client and the VTN, over a real socket.
//!
//! Unit tests can leave two halves of a system mutually consistent and both wrong. These drive the
//! actual HTTP client against the actual server: URL joining, token handling, serialization, status
//! mapping and pagination all have to line up for these to pass.

#![cfg(all(feature = "vtn", feature = "client"))]

use std::sync::Arc;

use openadr::{
    client::{BusinessLogic, Client, ClientError, Query, VirtualEndNode},
    core::FixedClock,
    model::{
        ClientId, ClientName, EventRequest, Interval, IntervalPeriod, ObjectId, ProgramName,
        ProgramRequest, ReportRequest, ReportResource, ResourceName, StartTime, Target, Timestamp,
        Value, ValuesMap, VenName, VenRequest, VenVenRequest,
    },
    vtn::{Vtn, VtnConfig, auth::StaticTokenAuth, store::MemoryStorage},
};
use rust_decimal::Decimal;

const BL: &str = "bl-secret";
const VEN: &str = "ven-secret";

fn now() -> Timestamp {
    "2026-02-11T06:00:00Z".parse().unwrap()
}

/// Start a VTN on an ephemeral port and return its base URL.
async fn start_vtn() -> String {
    let auth = StaticTokenAuth::new("http://unused/auth/token")
        .with_business_logic(BL, ClientId::new("bl").unwrap())
        .with_ven(VEN, ClientId::new("client-a").unwrap());

    let vtn = Vtn::builder()
        .storage(MemoryStorage::shared())
        .authenticator(Arc::new(auth))
        .clock(Arc::new(FixedClock::new(now())))
        .config(VtnConfig {
            base_path: "/openadr3/3.1.0".into(),
            ..Default::default()
        })
        .build();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = vtn.router();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    format!("http://{addr}/openadr3/3.1.0")
}

/// A client that presents a fixed bearer token.
///
/// The static-token authenticator does not implement the OAuth2 token endpoint, so tests inject the
/// token through the `Authorization` header directly, as an operator would with a pre-shared token.
fn bl_client(base: &str) -> Client<BusinessLogic> {
    Client::<BusinessLogic>::builder(base)
        .unwrap()
        .bearer_token(BL)
        .build()
        .unwrap()
}

fn ven_client(base: &str) -> Client<VirtualEndNode> {
    Client::<VirtualEndNode>::builder(base)
        .unwrap()
        .bearer_token(VEN)
        .build()
        .unwrap()
}

fn price(value: i64) -> ValuesMap {
    ValuesMap::single(
        "PRICE".parse().unwrap(),
        Value::Number(Decimal::from(value)),
    )
}

#[tokio::test]
async fn a_full_business_logic_workflow() {
    let base = start_vtn().await;
    let bl = bl_client(&base);

    // Create a programme.
    let program = bl
        .programs()
        .create(&ProgramRequest::new(ProgramName::new("day-ahead").unwrap()))
        .await
        .expect("create programme");
    assert_eq!(program.content.program_name.as_str(), "day-ahead");

    // Create an event in it.
    let event_request = EventRequest::new(program.id.clone())
        .with_interval_period(IntervalPeriod::new(
            StartTime::At("2026-02-12T00:00:00Z".parse().unwrap()),
            "PT1H".parse().unwrap(),
        ))
        .with_intervals(vec![
            Interval::new(0, vec![price(17)]),
            Interval::new(1, vec![price(3)]),
        ]);
    let event = bl
        .events()
        .create(&event_request)
        .await
        .expect("create event");
    assert_eq!(event.content.program_id, program.id);

    // Read it back.
    let fetched = bl.events().get(&event.id).await.expect("get event");
    assert_eq!(fetched.content.intervals.as_ref().unwrap().len(), 2);

    // Update it.
    let mut updated = event_request.clone();
    updated.event_name = Some("renamed".into());
    let updated = bl
        .events()
        .update(&event.id, &updated)
        .await
        .expect("update event");
    assert_eq!(updated.content.event_name.as_deref(), Some("renamed"));

    // Delete it.
    bl.events().delete(&event.id).await.expect("delete event");
    assert!(bl.events().try_get(&event.id).await.unwrap().is_none());
}

#[tokio::test]
async fn decimal_prices_survive_the_whole_round_trip() {
    let base = start_vtn().await;
    let bl = bl_client(&base);

    let program = bl
        .programs()
        .create(&ProgramRequest::new(ProgramName::new("prices").unwrap()))
        .await
        .unwrap();

    // A price that binary floating point cannot represent exactly.
    let exact = "0.17".parse::<Decimal>().unwrap();
    let event = bl
        .events()
        .create(
            &EventRequest::new(program.id.clone())
                .with_interval_period(IntervalPeriod::new(
                    StartTime::At("2026-02-12T00:00:00Z".parse().unwrap()),
                    "PT1H".parse().unwrap(),
                ))
                .with_intervals(vec![Interval::new(
                    0,
                    vec![ValuesMap::single(
                        "PRICE".parse().unwrap(),
                        Value::Number(exact),
                    )],
                )]),
        )
        .await
        .unwrap();

    let fetched = bl.events().get(&event.id).await.unwrap();
    let value = &fetched.content.intervals.as_ref().unwrap()[0].payloads[0].values[0];
    assert_eq!(value, &Value::Number(exact));
}

#[tokio::test]
async fn a_ven_registers_itself_and_files_a_report() {
    let base = start_vtn().await;
    let bl = bl_client(&base);
    let ven = ven_client(&base);

    // The VEN registers itself. It cannot name its own client id or targets.
    let registered = ven
        .vens()
        .create(&VenRequest::Ven(VenVenRequest {
            ven_name: VenName::new("charger-site-7").unwrap(),
            attributes: None,
        }))
        .await
        .expect("register");
    assert_eq!(registered.client_id.as_str(), "client-a");
    assert!(registered.targets.is_empty());

    // Business logic grants it a target.
    let granted = bl
        .vens()
        .update(
            &registered.id,
            &VenRequest::Bl(openadr::model::BlVenRequest {
                client_id: ClientId::new("client-a").unwrap(),
                ven_name: VenName::new("charger-site-7").unwrap(),
                targets: vec![Target::new("zone-a").unwrap()],
                attributes: None,
            }),
        )
        .await
        .expect("grant targets");
    assert_eq!(granted.targets, vec![Target::new("zone-a").unwrap()]);

    // An event for that zone.
    let program = bl
        .programs()
        .create(&ProgramRequest::new(ProgramName::new("gac").unwrap()))
        .await
        .unwrap();
    let event = bl
        .events()
        .create(
            &EventRequest::new(program.id.clone())
                .with_targets(vec![Target::new("zone-a").unwrap()])
                .with_interval_period(IntervalPeriod::new(
                    StartTime::At("2026-02-12T00:00:00Z".parse().unwrap()),
                    "PT15M".parse().unwrap(),
                ))
                .with_intervals(vec![Interval::new(
                    0,
                    vec![ValuesMap::single(
                        "IMPORT_CAPACITY_LIMIT".parse().unwrap(),
                        Value::Number(Decimal::from(60)),
                    )],
                )]),
        )
        .await
        .unwrap();

    // The VEN sees it once it names the target it was granted.
    let visible = ven
        .events()
        .list_with(&Query::new().target(&Target::new("zone-a").unwrap()))
        .await
        .unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, event.id);

    // And it files a report against it.
    let report = ven
        .reports()
        .create(&ReportRequest::new(
            event.id.clone(),
            ClientName::new("charger-site-7").unwrap(),
            vec![ReportResource {
                resource_name: ResourceName::new("charger-1").unwrap(),
                interval_period: None,
                intervals: vec![Interval::new(
                    0,
                    vec![ValuesMap::single(
                        "USAGE".parse().unwrap(),
                        Value::Number("12.5".parse().unwrap()),
                    )],
                )],
            }],
        ))
        .await
        .expect("file report");
    assert_eq!(report.client_id.as_ref().unwrap().as_str(), "client-a");

    // Business logic reads it.
    let reports = bl.reports().list().await.unwrap();
    assert_eq!(reports.len(), 1);
}

#[tokio::test]
async fn a_ven_that_names_no_targets_sees_no_targeted_events() {
    let base = start_vtn().await;
    let bl = bl_client(&base);
    let ven = ven_client(&base);

    ven.vens()
        .create(&VenRequest::Ven(VenVenRequest {
            ven_name: VenName::new("ven-1").unwrap(),
            attributes: None,
        }))
        .await
        .unwrap();

    let program = bl
        .programs()
        .create(&ProgramRequest::new(ProgramName::new("p").unwrap()))
        .await
        .unwrap();
    bl.events()
        .create(
            &EventRequest::new(program.id)
                .with_targets(vec![Target::new("zone-a").unwrap()])
                .with_interval_period(IntervalPeriod::new(
                    StartTime::At("2026-02-12T00:00:00Z".parse().unwrap()),
                    "PT1H".parse().unwrap(),
                ))
                .with_intervals(vec![Interval::new(0, vec![price(1)])]),
        )
        .await
        .unwrap();

    assert!(ven.events().list().await.unwrap().is_empty());
}

#[tokio::test]
async fn pagination_walks_past_the_fifty_record_page() {
    let base = start_vtn().await;
    let bl = bl_client(&base);

    for i in 0..120 {
        bl.programs()
            .create(&ProgramRequest::new(
                ProgramName::new(format!("tariff-{i:03}")).unwrap(),
            ))
            .await
            .unwrap();
    }

    // One page stops at the schema's maximum.
    assert_eq!(bl.programs().list().await.unwrap().len(), 50);

    // Following pagination reaches everything, exactly once.
    let all = bl.programs().list_all(&Query::new()).await.unwrap();
    assert_eq!(all.len(), 120);
    let mut names: Vec<&str> = all
        .iter()
        .map(|p| p.content.program_name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 120, "pagination returned duplicates");
}

#[tokio::test]
async fn errors_arrive_as_typed_problems() {
    let base = start_vtn().await;
    let bl = bl_client(&base);

    let missing = ObjectId::new("prg-99999999").unwrap();
    match bl.programs().get(&missing).await {
        Err(ClientError::Api { status, problem }) => {
            assert_eq!(status, 404);
            assert_eq!(problem.status, Some(404));
            assert!(problem.detail.is_some());
        }
        other => panic!("expected a 404 problem, got {other:?}"),
    }

    // A duplicate name is a 409, not a generic failure.
    let request = ProgramRequest::new(ProgramName::new("dup").unwrap());
    bl.programs().create(&request).await.unwrap();
    match bl.programs().create(&request).await {
        Err(ClientError::Api { status, .. }) => assert_eq!(status, 409),
        other => panic!("expected a 409, got {other:?}"),
    }
}

#[tokio::test]
async fn a_ven_cannot_reach_business_logic_endpoints() {
    let base = start_vtn().await;
    let bl = bl_client(&base);

    let program = bl
        .programs()
        .create(&ProgramRequest::new(ProgramName::new("p").unwrap()))
        .await
        .unwrap();

    // The typed client will not even offer `create` on a VEN's `events()`, so build a
    // business-logic client around the VEN's token to prove the server refuses it too.
    let ven_as_bl = Client::<BusinessLogic>::builder(&base)
        .unwrap()
        .bearer_token(VEN)
        .build()
        .unwrap();
    match ven_as_bl
        .events()
        .create(&EventRequest::new(program.id))
        .await
    {
        Err(ClientError::Api { status, .. }) => assert_eq!(status, 403),
        other => panic!("expected a 403, got {other:?}"),
    }
}

#[tokio::test]
async fn discovery_endpoints_answer_without_credentials() {
    let base = start_vtn().await;
    // No token at all.
    let anonymous = Client::<VirtualEndNode>::builder(&base)
        .unwrap()
        .build()
        .unwrap();

    let info = anonymous.auth_server().await.expect("auth server info");
    assert!(info.token_url.contains("/auth/token"));
}

#[tokio::test]
async fn an_adapter_rewrites_the_wire_without_touching_the_model() {
    // The adapter chain is the alternative to loosening the model for one peer's deviation, so it
    // has to be wired into the client rather than merely existing. Against a canonical VTN it must
    // be a no-op: an adapter that quietly changed conformant traffic would be worse than none.
    let base = start_vtn().await;
    let plain = bl_client(&base);
    let adapted = Client::<BusinessLogic>::builder(&base)
        .unwrap()
        .bearer_token(BL)
        .adapter(openadr::model::adapt::Fluvius)
        .build()
        .unwrap();

    let request = ProgramRequest::new(ProgramName::new("netflex").unwrap());
    let created = adapted.programs().create(&request).await.unwrap();
    assert_eq!(created.content.program_name.as_str(), "netflex");

    // And a canonical client reads back exactly what the adapted one wrote.
    let read = plain.programs().get(&created.id).await.unwrap();
    assert_eq!(read, created);
}

#[tokio::test]
async fn a_conditional_read_costs_nothing_when_nothing_changed() {
    // The VTN's headline feature for pollers is only a feature if a client can use it.
    let base = start_vtn().await;
    let bl = bl_client(&base);
    bl.programs()
        .create(&ProgramRequest::new(ProgramName::new("tou").unwrap()))
        .await
        .unwrap();

    let first = bl
        .programs()
        .list_if_changed(&Query::new(), None)
        .await
        .unwrap()
        .expect("the first read always returns a body");
    assert_eq!(first.value.len(), 1);
    let tag = first.etag.expect("the VTN tags every GET");

    // Same collection, same tag: no body at all.
    assert!(
        bl.programs()
            .list_if_changed(&Query::new(), Some(&tag))
            .await
            .unwrap()
            .is_none(),
        "an unchanged collection must answer 304"
    );

    // A write changes the tag, so the next poll gets the new page.
    bl.programs()
        .create(&ProgramRequest::new(
            ProgramName::new("critical-peak").unwrap(),
        ))
        .await
        .unwrap();
    let second = bl
        .programs()
        .list_if_changed(&Query::new(), Some(&tag))
        .await
        .unwrap()
        .expect("a changed collection must return a body");
    assert_eq!(second.value.len(), 2);
    assert_ne!(second.etag.as_deref(), Some(tag.as_str()));
}

/// An adapter with a visible effect on the way in, so a path that skips the chain is detectable.
#[derive(Debug, Clone, Copy)]
struct ShoutingNames;

impl openadr::model::adapt::WireAdapter for ShoutingNames {
    fn name(&self) -> &'static str {
        "shouting-names"
    }
    fn inbound(&self, body: &mut serde_json::Value) {
        fn walk(v: &mut serde_json::Value) {
            match v {
                serde_json::Value::Array(items) => items.iter_mut().for_each(walk),
                serde_json::Value::Object(map) => {
                    if let Some(serde_json::Value::String(name)) = map.get_mut("programName") {
                        *name = name.to_uppercase();
                    }
                    for (_, child) in map.iter_mut() {
                        walk(child);
                    }
                }
                _ => {}
            }
        }
        walk(body);
    }
}

#[tokio::test]
async fn every_read_path_runs_the_adapter_chain() {
    // `list` and `list_if_changed` decode the same bytes, so they must decode them the same way.
    // They did not: the conditional path deserialised straight from the socket and skipped the
    // chain, so a client speaking to a peer that bends the schema got adapted objects from one
    // method and raw ones from the other.
    let base = start_vtn().await;
    bl_client(&base)
        .programs()
        .create(&ProgramRequest::new(ProgramName::new("tou").unwrap()))
        .await
        .unwrap();

    let adapted = Client::<BusinessLogic>::builder(&base)
        .unwrap()
        .bearer_token(BL)
        .adapter(ShoutingNames)
        .build()
        .unwrap();

    let listed = adapted.programs().list().await.unwrap();
    assert_eq!(listed[0].content.program_name.as_str(), "TOU");

    let conditional = adapted
        .programs()
        .list_if_changed(&Query::new(), None)
        .await
        .unwrap()
        .expect("the first read returns a body");
    assert_eq!(conditional.value[0].content.program_name.as_str(), "TOU");

    let one = adapted
        .programs()
        .get_if_changed(&listed[0].id, None)
        .await
        .unwrap()
        .expect("the first read returns a body");
    assert_eq!(one.value.content.program_name.as_str(), "TOU");
}

#[tokio::test]
async fn typed_filters_reach_the_right_query_parameters() {
    let base = start_vtn().await;
    let bl = bl_client(&base);
    let tou = bl
        .programs()
        .create(&ProgramRequest::new(ProgramName::new("tou").unwrap()))
        .await
        .unwrap();
    bl.programs()
        .create(&ProgramRequest::new(
            ProgramName::new("critical-peak").unwrap(),
        ))
        .await
        .unwrap();

    let found = bl
        .programs()
        .list_with(&Query::new().program_name(&ProgramName::new("tou").unwrap()))
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, tou.id);
}
