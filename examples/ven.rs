//! A VEN: registers, follows a programme, acts on what is in force, and files its reports.
//!
//! Start a VTN first — `openadr dev` (then `source .openadr-dev.env`), or
//! `cargo run --features vtn --example vtn` — then:
//!
//! ```console
//! cargo run --features ven --example ven
//! ```
//!
//! It reads `OPENADR_URL`, `OPENADR_VEN_CLIENT_ID`/`OPENADR_VEN_CLIENT_SECRET` and
//! `OPENADR_VEN_TARGETS` when set, and otherwise talks to `examples/vtn` on its default port with
//! that example's static token and target.
//!
//! The whole loop is [`VenRuntime`]. What is left to write is the two things that are genuinely
//! this deployment's: what the load does with the values in force, and what the meter reads. Both
//! appear below, and both are about ten lines.
//!
//! Three things here are worth copying:
//!
//! * the poll is **conditional**, so a cycle that finds nothing new costs a `304` with no body;
//! * the wake-up is driven by the **timeline**, so a price change at 13:00 is acted on at 13:00
//!   rather than up to a poll interval later;
//! * the state that must not be lost — which reports have been filed — is **exported and
//!   restored**, because filing one twice is a duplicate in somebody's settlement data.

use std::time::Duration as StdDuration;

use openadr::{
    client::{Client, Credentials, VirtualEndNode},
    model::{Interval, ReportResource, ResourceName, Value, ValuesMap},
    ven::{DueReport, Meter, VenConfig, VenError, VenRuntime, VenState},
};

const VTN: &str = "http://localhost:3000/openadr3/3.1.0";
const STATE_FILE: &str = "ven-state.json";

/// What this VEN measures.
///
/// The one part the runtime cannot write: the runtime knows *when* a report is due and *which*
/// intervals it covers; only the deployment knows what the meter says. Returning an empty vector
/// leaves the report due, so a meter that is briefly unavailable costs a cycle rather than a
/// window.
#[derive(Debug)]
struct HouseMeter;

#[async_trait::async_trait]
impl Meter for HouseMeter {
    async fn read(&self, due: &DueReport) -> Result<Vec<ReportResource>, VenError> {
        println!(
            "  reporting {} for intervals {:?} ({} .. {:?})",
            due.payload_type.as_str(),
            due.interval_ids,
            due.covers_from,
            due.covers_to
        );
        Ok(vec![ReportResource {
            resource_name: ResourceName::new("water-heater").expect("a valid resource name"),
            interval_period: None,
            intervals: due
                .interval_ids
                .iter()
                .map(|id| {
                    // A real meter reads its register here.
                    Interval::new(
                        *id,
                        vec![ValuesMap::new(
                            due.payload_type.clone(),
                            vec![Value::Integer(1_200)],
                        )],
                    )
                })
                .collect(),
        }])
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("OPENADR_URL").unwrap_or_else(|_| VTN.to_string());
    let builder = Client::<VirtualEndNode>::builder(&url)?;
    let client = match (
        std::env::var("OPENADR_VEN_CLIENT_ID"),
        std::env::var("OPENADR_VEN_CLIENT_SECRET"),
    ) {
        (Ok(id), Ok(secret)) => builder.credentials(Credentials::new(id, secret)),
        _ => builder.bearer_token("ven-token"),
    }
    .build()?;

    let config = VenConfig::new("water-heater-7".parse()?)
        .with_resources(["water-heater".parse()?])
        // The example VTN aims its events at `group1`; anywhere else, name the targets this VEN
        // was granted in OPENADR_VEN_TARGETS (comma-separated), or none.
        .with_targets(match std::env::var("OPENADR_VEN_TARGETS") {
            Ok(list) => list
                .split(',')
                .filter(|t| !t.is_empty())
                .map(str::parse)
                .collect::<Result<Vec<_>, _>>()?,
            Err(_) if std::env::var("OPENADR_URL").is_ok() => Vec::new(),
            Err(_) => vec!["group1".parse()?],
        })
        .with_poll_interval(StdDuration::from_secs(60))
        // Every unit in a fleet needs a different seed, or `randomizeStart` spreads nothing.
        .with_randomization_seed(0xd15_9a7c4);

    let ven = VenRuntime::with_meter(client, config, HouseMeter);

    // Resume, if this process has run before. Only the reports already filed matter; the schedule
    // is re-read from the VTN on the first sync.
    if let Ok(saved) = std::fs::read_to_string(STATE_FILE)
        && let Ok(state) = serde_json::from_str::<VenState>(&saved)
    {
        ven.restore(state);
        println!("resumed from {STATE_FILE}");
    }

    // Registration is idempotent, and it checks the VTN's clock against ours first: every interval
    // in OpenADR is an absolute instant, so a VEN with a wrong clock acts at the wrong time.
    let registered = ven.register().await?;
    println!("registered as {} ({})", registered.ven_name, registered.id);

    // Push, when the VTN has a broker. It never delivers an instruction — it only shortens the
    // sleep below, so a VEN whose broker is down, wrong or absent behaves exactly as it does here.
    // Held until the end of `main` because dropping it unsubscribes.
    #[cfg(feature = "mqtt")]
    let _push = match openadr::ven::MqttPush::connect(&ven, Default::default()).await {
        Ok(Some(push)) => {
            println!("watching {} MQTT topic(s) for changes", push.topics().len());
            Some(push)
        }
        Ok(None) => None,
        // Not fatal: the loop below is the VEN, and push is an optimisation on top of it.
        Err(e) => {
            eprintln!("could not subscribe for push notifications, polling only: {e}");
            None
        }
    };

    loop {
        let outcome = ven.sync().await?;
        if outcome.changed {
            println!(
                "schedule changed: +{} ~{} -{}",
                outcome.added.len(),
                outcome.updated.len(),
                outcome.removed.len()
            );
        }

        match ven.active_at(ven.now()) {
            Some(segment) => println!(
                "in force from {} until {}: {}",
                segment.start,
                segment
                    .end
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "∞".into()),
                segment
                    .payloads
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            None => println!("nothing in force"),
        }

        for report in ven.submit_due_reports().await? {
            println!("filed report {}", report.id);
        }
        // Written after each cycle, because the cost of losing it is a duplicate report and the
        // cost of writing it is a few hundred bytes.
        std::fs::write(
            STATE_FILE,
            serde_json::to_vec_pretty(&ven.exported_state())?,
        )?;

        println!("sleeping up to {}s", ven.time_to_next_wakeup().as_secs());
        // Returns early if a push notification arrives.
        ven.wait_for_work().await;
    }
}
