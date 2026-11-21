//! `openadr trigger <scenario>` — make something happen on a VTN `openadr dev` seeded.
//!
//! Three named scenarios and nothing else: a demo command with a fixture language is how a demo
//! command becomes a product. Anything else is `openadr post … --data`.

use openadr::client::{BusinessLogic, Client, Credentials, Query, VirtualEndNode};
use serde_json::{Value, json};

/// The programme `openadr trigger event` writes to.
pub const CAPACITY_PROGRAM: &str = "capacity-limit";

/// An RFC 3339 instant from seconds since the epoch.
pub fn rfc3339(seconds: i64) -> String {
    openadr::model::Timestamp::from_second(seconds)
        .map(|t| t.to_string())
        .unwrap_or_default()
}

/// An `IMPORT_CAPACITY_LIMIT` event on `program_id`.
pub fn capacity_event(program_id: &str, name: &str, start: i64, seconds: i64, kw: f64) -> Value {
    json!({
        "programID": program_id,
        "eventName": name,
        "intervalPeriod": { "start": rfc3339(start), "duration": format!("PT{}M", seconds / 60) },
        "intervals": [{ "id": 0, "payloads": [{ "type": "IMPORT_CAPACITY_LIMIT", "values": [kw] }] }],
    })
}

const SCENARIOS: &str = "\
    event    a capacity limit of 5 kW, starting in one minute, for 30 minutes\n    \
    cancel   delete the most recent triggered capacity event\n    \
    report   the VEN files a usage report against the price curve";

/// Print the usage.
pub fn print_help() {
    println!(
        "openadr trigger — make something happen on a VTN `openadr dev` seeded\n\n\
         USAGE:\n    openadr trigger <scenario>\n\nSCENARIOS:\n    {SCENARIOS}\n\n\
         Reads OPENADR_URL, OPENADR_CLIENT_ID/SECRET and OPENADR_VEN_CLIENT_ID/SECRET, which\n\
         `source .openadr-dev.env` sets."
    );
}

/// Run it.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let scenario = match args.first().map(String::as_str) {
        None | Some("--help") => {
            print_help();
            return Ok(());
        }
        Some(s) => s.to_string(),
    };
    if !matches!(scenario.as_str(), "event" | "cancel" | "report") {
        return Err(
            format!("no scenario called {scenario:?}. There are three:\n    {SCENARIOS}").into(),
        );
    }
    let url = env("OPENADR_URL")?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            match scenario.as_str() {
                "event" => event(&business_logic(&url)?).await,
                "cancel" => cancel(&business_logic(&url)?).await,
                _ => report(&url).await,
            }
        })
}

fn env(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    std::env::var(name).map_err(|_| {
        format!("{name} is not set — run `openadr dev`, then `source .openadr-dev.env`").into()
    })
}

fn business_logic(url: &str) -> Result<Client<BusinessLogic>, Box<dyn std::error::Error>> {
    Ok(Client::<BusinessLogic>::builder(url)?
        .credentials(Credentials::new(
            env("OPENADR_CLIENT_ID")?,
            env("OPENADR_CLIENT_SECRET")?,
        ))
        .build()?)
}

async fn program_id(
    client: &Client<BusinessLogic>,
    name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let found: Value = client
        .get_json("programs", &Query::new().param("programName", name))
        .await?;
    found
        .as_array()
        .and_then(|a| a.first())
        .and_then(|p| p["id"].as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            format!("no programme {name:?} — is this a VTN `openadr dev` seeded?").into()
        })
}

async fn event(client: &Client<BusinessLogic>) -> Result<(), Box<dyn std::error::Error>> {
    let program = program_id(client, CAPACITY_PROGRAM).await?;
    let now = openadr::model::Timestamp::now().as_second();
    let start = now + 60 - now.rem_euclid(60);
    let name = format!("triggered-{now}");
    let created: Value = client
        .send_json(
            "POST",
            "events",
            Some(&capacity_event(&program, &name, start, 1800, 5.0)),
        )
        .await?;
    println!(
        "created event {} ({name}): 5 kW from {} for 30 minutes",
        created["id"].as_str().unwrap_or("?"),
        created["intervalPeriod"]["start"].as_str().unwrap_or("?")
    );
    Ok(())
}

async fn cancel(client: &Client<BusinessLogic>) -> Result<(), Box<dyn std::error::Error>> {
    let program = program_id(client, CAPACITY_PROGRAM).await?;
    let events: Value = client
        .get_json("events", &Query::new().param("programID", program.as_str()))
        .await?;
    let latest = events
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| {
            e["eventName"]
                .as_str()
                .is_some_and(|n| n.starts_with("triggered-"))
        })
        .max_by_key(|e| {
            e["createdDateTime"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .and_then(|e| e["id"].as_str().map(str::to_string))
        .ok_or("no triggered event to cancel — `openadr trigger event` makes one")?;
    let _: Value = client
        .send_json("DELETE", &format!("events/{latest}"), None)
        .await?;
    println!("deleted event {latest}");
    Ok(())
}

async fn report(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let ven = Client::<VirtualEndNode>::builder(url)?
        .credentials(Credentials::new(
            env("OPENADR_VEN_CLIENT_ID")?,
            env("OPENADR_VEN_CLIENT_SECRET")?,
        ))
        .build()?;
    let events: Value = ven.get_json("events", &Query::new()).await?;
    let curve = events
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["eventName"] == "day-ahead-price")
        .ok_or("the VEN sees no price curve — is this a VTN `openadr dev` seeded?")?;
    let event_id = curve["id"].as_str().unwrap_or_default();
    let filed: Value = ven
        .send_json(
            "POST",
            "reports",
            Some(&json!({
                "programID": curve["programID"],
                "eventID": event_id,
                "clientName": "ven-1",
                "reportName": format!("usage-{}", openadr::model::Timestamp::now().as_second()),
                "payloadDescriptors": [{
                    "objectType": "REPORT_PAYLOAD_DESCRIPTOR",
                    "payloadType": "USAGE",
                    "readingType": "DIRECT_READ",
                    "units": "KWH",
                }],
                "resources": [{
                    "resourceName": "water-heater",
                    "intervalPeriod": curve["intervalPeriod"],
                    "intervals": [{ "id": 0, "payloads": [{ "type": "USAGE", "values": [0.42] }] }],
                }],
            })),
        )
        .await?;
    println!(
        "filed report {} for event {event_id}: 0.42 kWh in its first interval",
        filed["id"].as_str().unwrap_or("?")
    );
    Ok(())
}
