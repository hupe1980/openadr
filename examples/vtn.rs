//! A VTN you can talk to, in about thirty lines — with just enough in it for the VEN example to
//! have something to follow.
//!
//! ```console
//! cargo run --features vtn --example vtn
//! curl -H 'Authorization: Bearer bl-token' -H 'Content-Type: application/json' \
//!      -d '{"programName":"day-ahead"}' localhost:3000/openadr3/3.1.0/programs
//! ```
//!
//! The seeding below is what business logic does and a VEN cannot: it grants the VEN a target. A
//! VEN's own request body has no `targets` member at all, so the privilege is unreachable rather
//! than merely unauthorised — and without the grant the VEN example would register successfully,
//! ask for `group1`, and correctly see nothing.

use std::sync::Arc;

use openadr::core::SystemClock;
use openadr::model::{
    EventPayloadDescriptor, EventRequest, Interval, IntervalPeriod, ObjectType, ProgramRequest,
    ReadingType, ReportDescriptor, StartTime, Value, ValuesMap, Ven,
};
use openadr::vtn::{Vtn, auth::StaticTokenAuth, notify::Fanout, store::MemoryStorage};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("openadr=debug,info")
        .init();

    let auth = StaticTokenAuth::new("http://localhost:3000/openadr3/3.1.0/auth/token")
        .with_business_logic("bl-token", "business-logic".parse()?)
        .with_ven("ven-token", "ven-1".parse()?);

    let storage = MemoryStorage::shared();
    seed(&storage).await?;

    let vtn = Vtn::builder()
        .storage(storage)
        .authenticator(Arc::new(auth))
        .build();

    println!("VTN on http://localhost:3000/openadr3/3.1.0");
    println!("  business logic: Bearer bl-token");
    println!("  VEN:            Bearer ven-token");
    println!("  seeded:         programme `day-ahead`, a price event running now, and a grant of");
    println!("                  `group1` to ven-1 — run `cargo run --features ven --example ven`");
    vtn.serve("0.0.0.0:3000").await?;
    Ok(())
}

/// What business logic would have provisioned before a VEN was ever switched on.
///
/// Written through the `Storage` trait rather than over HTTP because there is nothing to
/// demonstrate about a VTN calling itself. Everything here is an ordinary `POST` a business-logic
/// client could make; `openadr post programs --data …` is the same thing from a shell.
async fn seed(
    storage: &openadr::vtn::store::SharedStorage,
) -> Result<(), Box<dyn std::error::Error>> {
    use openadr::core::Clock;
    let now = SystemClock.now();

    // The VEN object, with the grant on it. The VEN example registers under this same `venName`,
    // finds this object rather than creating a second, and inherits the target.
    storage
        .create_ven(
            Ven {
                id: "pending".parse()?,
                created_date_time: now,
                modification_date_time: now,
                object_type: ObjectType::Ven,
                client_id: "ven-1".parse()?,
                ven_name: "water-heater-7".parse()?,
                targets: vec!["group1".parse()?],
                attributes: None,
            },
            &Fanout::none(),
        )
        .await?;

    let program = storage
        .create_program(
            ProgramRequest::new("day-ahead".parse()?),
            now,
            &Fanout::none(),
        )
        .await?;

    // `0001-01-01` means "now, from the reader's point of view" — a price that is already running
    // when the VEN first reads it. Three prices in a `PT3H` interval subdivide it into three hourly
    // sub-intervals, which is the compact form 3.1 added.
    let mut event = EventRequest::new(program.id.clone())
        .with_targets(vec!["group1".parse()?])
        .with_interval_period(IntervalPeriod::new(StartTime::Now, "PT3H".parse()?))
        .with_intervals(vec![Interval::new(
            0,
            vec![ValuesMap::new(
                "PRICE".parse()?,
                vec![
                    Value::Number("0.21".parse()?),
                    Value::Number("0.34".parse()?),
                    Value::Number("0.12".parse()?),
                ],
            )],
        )]);
    event.event_name = Some("live-price".into());
    event.payload_descriptors = Some(vec![
        EventPayloadDescriptor::new("PRICE".parse()?)
            .with_units("KWH".parse()?)
            .with_currency("EUR"),
    ]);
    // Two reports, because the two directions are the part of §7.5 worth seeing.
    event.report_descriptors = Some(vec![
        // A forecast: due when its window *opens*, so the VEN files one on its first cycle. A
        // forty-eight-hour forecast delivered at the end of the forty-eight hours it forecasts is
        // a historical record, which is why the boundary differs by direction.
        ReportDescriptor {
            reading_type: Some(ReadingType::Forecast),
            historical: false,
            num_intervals: 3,
            frequency: 3,
            repeat: 1,
            start_interval: 0,
            ..ReportDescriptor::new("USAGE".parse()?)
        },
        // And what was actually used, once per hourly sub-interval, each due at the end of the
        // hour it covers.
        ReportDescriptor {
            num_intervals: 1,
            frequency: 1,
            repeat: 3,
            start_interval: 0,
            ..ReportDescriptor::new("USAGE".parse()?)
        },
    ]);

    storage.create_event(event, now, &Fanout::none()).await?;
    Ok(())
}
