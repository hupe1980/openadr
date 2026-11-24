//! `openadr dev` — a VTN with two clients and data that moves, in one command.
//!
//! It is `openadr vtn` with presets, not a second server: the flags below are added to whatever
//! else was passed, so `--database`, `--webhooks` or `--mqtt-broker` work as they do there. What it
//! adds is the part a first run should not have to write: generated credentials (printed, and
//! written to an env file every `openadr` command reads), a price programme whose 24-hour curve
//! rolls forward every quarter hour, and a capacity programme with an event two minutes out.
//!
//! The seeding goes through the VTN's own API with the business-logic client, so what it creates
//! is exactly what any client could have created.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

use openadr::client::{BusinessLogic, Client, Credentials};
use serde_json::{Value, json};

use super::trigger::{CAPACITY_PROGRAM, capacity_event, rfc3339};

/// The business-logic client `dev` registers.
pub const BL_CLIENT: &str = "bl-1";
/// The VEN client `dev` registers.
pub const VEN_CLIENT: &str = "ven-1";
/// The programme carrying the price curve.
pub const PRICE_PROGRAM: &str = "dynamic-price";

/// Print the usage.
pub fn print_help() {
    println!(
        r#"openadr dev — a VTN with two clients and seeded data that moves, in one command

USAGE:
    openadr dev [OPTIONS] [VTN options]

OPTIONS:
    --listen <ADDR>        where to listen [default: 127.0.0.1:3000]; a non-loopback address
                           also needs --ephemeral, because the credentials are printed
    --env-file <PATH>      where to write the credentials [default: .openadr-dev.env]

    Server configuration flags pass through to the VTN. Dev supplies its own clients and tokens.

WHAT YOU GET:
    clients        bl-1 (business logic) and ven-1 (VEN), secrets generated per run
    dynamic-price  a 24 h PRICE curve in 15-minute intervals, rolled forward every 15 minutes
    capacity-limit an IMPORT_CAPACITY_LIMIT event starting two minutes after start

THEN:
    source .openadr-dev.env
    openadr get events                  openadr watch events
    openadr trigger event               openadr conformance --report report.html
    cargo run --features ven --example ven"#
    );
}

/// Run it.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.first().map(String::as_str) == Some("--help") {
        print_help();
        return Ok(());
    }
    let mut listen = "127.0.0.1:3000".to_string();
    let mut env_file = PathBuf::from(".openadr-dev.env");
    let mut ephemeral = false;
    let mut passthrough: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--listen" => listen = it.next().ok_or("--listen needs a value")?.clone(),
            "--env-file" => env_file = it.next().ok_or("--env-file needs a value")?.into(),
            "--client" | "--client-hashed" | "--bl-token" | "--ven-token" | "--anonymous" => {
                return Err(format!(
                    "{arg} is what `openadr dev` sets up for you; use `openadr vtn` to choose clients"
                )
                .into());
            }
            other => {
                if other == "--ephemeral" {
                    ephemeral = true;
                }
                passthrough.push(other.to_string());
            }
        }
    }

    // The credentials are printed and written to a file, so a dev VTN on a public interface hands
    // them to the network. Allowed — a container needs it — but only when said out loud
    // (dev-stack/SR-002).
    if !ephemeral && !super::serve::is_loopback(&listen) {
        return Err(format!(
            "refusing to serve a development VTN on {listen}: its credentials are printed and \
             written to {}.\nUse a loopback address, or pass --ephemeral if this is a container \
             you publish only to yourself.",
            env_file.display()
        )
        .into());
    }

    let bl_secret = secret();
    let ven_secret = secret();
    let local = local_url(&listen);
    write_env_file(&env_file, &local, &bl_secret, &ven_secret)?;

    let mut vtn_args = passthrough;
    // A per-run signing key: the warning `vtn` gives for a missing one is about restarts and
    // second instances, which a development server has neither of.
    if !vtn_args.iter().any(|a| a == "--token-key") {
        vtn_args.extend(["--token-key".into(), secret()]);
    }
    vtn_args.extend([
        "--inspector".into(),
        "--listen".into(),
        listen.clone(),
        "--client".into(),
        format!("{BL_CLIENT}:{bl_secret}:bl"),
        "--client".into(),
        format!("{VEN_CLIENT}:{ven_secret}:ven"),
    ]);

    let banner = Banner {
        url: local.clone(),
        env_file: env_file.clone(),
        bl_secret: bl_secret.clone(),
        ven_secret,
        durable: vtn_args.iter().any(|a| a == "--database"),
    };
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the seeder");
        runtime.block_on(async move {
            match seed_and_roll(&local, &bl_secret, &banner).await {
                Ok(()) => {}
                Err(e) => eprintln!("openadr dev: could not seed the VTN: {e}"),
            }
        });
    });

    super::serve::run_vtn(&vtn_args)
}

/// 24 bytes from a CSPRNG seeded by the operating system, as hex (dev-stack/SR-003).
fn secret() -> String {
    use rand::Rng as _;
    let bytes: [u8; 24] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The URL a command on this machine reaches the VTN at.
fn local_url(listen: &str) -> String {
    let (host, port) = listen.rsplit_once(':').unwrap_or((listen, "3000"));
    let host = match host {
        "0.0.0.0" | "[::]" | "" => "127.0.0.1",
        other => other,
    };
    format!("http://{host}:{port}{}", openadr::DEFAULT_BASE_PATH)
}

/// Write the env file new, mode 0600, never through a symlink (dev-stack/SR-001).
///
/// Whatever is at the path is removed first — a file from the last run, or a link somebody planted
/// — and the new file is created exclusively, so the write cannot land anywhere but here.
fn write_env_file(
    path: &Path,
    url: &str,
    bl_secret: &str,
    ven_secret: &str,
) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            return Err(std::io::Error::other(format!(
                "{} is a directory",
                path.display()
            )));
        }
        Ok(_) => std::fs::remove_file(path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    writeln!(
        file,
        "# openadr dev — credentials for this run only. `source` this file.\n\
         export OPENADR_URL={url}\n\
         export OPENADR_CLIENT_ID={BL_CLIENT}\n\
         export OPENADR_CLIENT_SECRET={bl_secret}\n\
         export OPENADR_VEN_CLIENT_ID={VEN_CLIENT}\n\
         export OPENADR_VEN_CLIENT_SECRET={ven_secret}"
    )
}

struct Banner {
    url: String,
    env_file: PathBuf,
    bl_secret: String,
    ven_secret: String,
    durable: bool,
}

impl Banner {
    fn print(&self) {
        println!("{}", self.render());
    }

    fn render(&self) -> String {
        let root = self.url.trim_end_matches(openadr::DEFAULT_BASE_PATH);
        let storage = if self.durable {
            "durable"
        } else {
            "in memory — gone when you stop it"
        };
        format!(
            "\n  OpenADR {spec} development VTN\n\n  \
             VTN        {url}   ({storage})\n  \
             Inspector  {root}/_inspect#bl={BL_CLIENT}:{bl}&ven={VEN_CLIENT}:{ven}\n  \
             Clients    {BL_CLIENT} (business logic)  {bl}\n             \
             {VEN_CLIENT} (VEN)             {ven}\n  \
             Seeded     {PRICE_PROGRAM}   24 h price curve, rolls forward every 15 minutes\n             \
             {CAPACITY_PROGRAM}  an event in 2 minutes, and one for feeder-7 that {VEN_CLIENT} cannot see\n\n  \
             Try        source {env}\n             \
             openadr get events\n             \
             openadr watch events\n             \
             openadr trigger event\n             \
             openadr conformance --report report.html\n             \
             cargo run --features ven --example ven\n",
            spec = openadr::SPEC_VERSION,
            url = self.url,
            bl = self.bl_secret,
            ven = self.ven_secret,
            env = self.env_file.display(),
        )
    }
}

/// Wait for the VTN, seed it, print the banner, then roll the price curve forward for ever.
async fn seed_and_roll(
    url: &str,
    bl_secret: &str,
    banner: &Banner,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = Client::<BusinessLogic>::builder(url)?
        .credentials(Credentials::new(BL_CLIENT, bl_secret))
        .build()?;

    // The server starts on the other thread; give it up to ten seconds.
    let mut ready = false;
    for _ in 0..100 {
        if client.auth_server().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !ready {
        return Err("the VTN did not answer within ten seconds".into());
    }

    let (price, _) =
        ensure_program(&client, PRICE_PROGRAM, "PRICE", "KWH", "Dynamic price").await?;
    let (capacity, new_capacity) = ensure_program(
        &client,
        CAPACITY_PROGRAM,
        "IMPORT_CAPACITY_LIMIT",
        "KW",
        "Capacity limit",
    )
    .await?;

    // On a durable VTN the curve from the last run is still there; roll it rather than add another.
    let existing: Value = client
        .get_json(
            "events",
            &openadr::client::Query::new().param("programID", price.as_str()),
        )
        .await?;
    let curve_id = match existing
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["eventName"] == "day-ahead-price")
        .and_then(|e| e["id"].as_str())
    {
        Some(id) => {
            let _: Value = client
                .send_json(
                    "PUT",
                    &format!("events/{id}"),
                    Some(&price_curve(&price, now())),
                )
                .await?;
            id.to_string()
        }
        None => {
            let curve: Value = client
                .send_json("POST", "events", Some(&price_curve(&price, now())))
                .await?;
            curve["id"].as_str().unwrap_or_default().to_string()
        }
    };
    if new_capacity {
        let _: Value = client
            .send_json(
                "POST",
                "events",
                Some(&capacity_event(
                    &capacity,
                    "capacity-limit-seeded",
                    now() + 120,
                    3600,
                    11.0,
                )),
            )
            .await?;
        // Aimed at a feeder ven-1 is not granted, so the inspector's "who sees what" has something
        // to hide: business logic sees this event, the VEN does not.
        let mut feeder = capacity_event(
            &capacity,
            "capacity-limit-feeder-7",
            now() + 1800,
            7200,
            4.0,
        );
        feeder["targets"] = json!(["feeder-7"]);
        let _: Value = client.send_json("POST", "events", Some(&feeder)).await?;
    }
    banner.print();

    // Roll the curve forward at every quarter hour, so the first interval is always the current one
    // and a watcher sees it change.
    loop {
        let wait = 900 - now().rem_euclid(900);
        tokio::time::sleep(Duration::from_secs(u64::try_from(wait).unwrap_or(900))).await;
        if let Err(e) = client
            .send_json::<Value>(
                "PUT",
                &format!("events/{curve_id}"),
                Some(&price_curve(&price, now())),
            )
            .await
        {
            eprintln!("openadr dev: could not roll the price curve forward: {e}");
        }
    }
}

/// The id of the programme named `name`, and whether it had to be created.
async fn ensure_program(
    client: &Client<BusinessLogic>,
    name: &str,
    payload_type: &str,
    units: &str,
    long_name: &str,
) -> Result<(String, bool), Box<dyn std::error::Error + Send + Sync>> {
    let found: Value = client
        .get_json(
            "programs",
            &openadr::client::Query::new().param("programName", name),
        )
        .await?;
    if let Some(id) = found
        .as_array()
        .and_then(|a| a.first())
        .and_then(|p| p["id"].as_str())
    {
        return Ok((id.to_string(), false));
    }
    let created: Value = client
        .send_json(
            "POST",
            "programs",
            Some(&json!({
                "programName": name,
                "programLongName": format!("{long_name} (seeded by openadr dev)"),
                "payloadDescriptors": [{
                    "objectType": "EVENT_PAYLOAD_DESCRIPTOR",
                    "payloadType": payload_type,
                    "units": units,
                }],
            })),
        )
        .await?;
    Ok((created["id"].as_str().unwrap_or_default().to_string(), true))
}

/// Seconds since the epoch, now.
fn now() -> i64 {
    openadr::model::Timestamp::now().as_second()
}

/// 96 quarter-hour prices from the current quarter: a morning and an evening peak, a midday dip.
fn price_curve(program_id: &str, at: i64) -> Value {
    let start = at - at.rem_euclid(900);
    let intervals: Vec<Value> = (0..96)
        .map(|i| {
            let hour = ((start + i * 900).rem_euclid(86_400)) as f64 / 3600.0;
            let price = price_at(hour);
            json!({ "id": i, "payloads": [{ "type": "PRICE", "values": [price] }] })
        })
        .collect();
    json!({
        "programID": program_id,
        "eventName": "day-ahead-price",
        "intervalPeriod": { "start": rfc3339(start), "duration": "PT15M" },
        "intervals": intervals,
    })
}

/// EUR/kWh at an hour of the (UTC) day, rounded to four places.
fn price_at(hour: f64) -> f64 {
    let bump = |centre: f64, width: f64| (-((hour - centre) / width).powi(2)).exp();
    let price = 0.18 + 0.10 * bump(8.0, 1.5) + 0.16 * bump(19.0, 2.0) - 0.08 * bump(13.0, 2.0);
    (price * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openadr-dev-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // dev-stack/SR-001: the env file is ours, private, and never written through a link.
    #[cfg(unix)]
    #[test]
    fn the_env_file_is_private_and_never_follows_a_symlink() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("env");
        let victim = dir.join("victim");
        std::fs::write(&victim, "precious").unwrap();
        let path = dir.join(".openadr-dev.env");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        write_env_file(&path, "http://127.0.0.1:3000/openadr3/3.1.0", "aa", "bb").unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("export OPENADR_CLIENT_SECRET=aa"), "{text}");
        assert!(
            text.contains("export OPENADR_VEN_CLIENT_ID=ven-1"),
            "{text}"
        );
    }

    // dev-stack/SR-002: a public address needs saying so.
    #[test]
    fn a_public_address_is_refused_without_ephemeral() {
        let err = run(&["--listen".into(), "0.0.0.0:3000".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("--ephemeral"), "{err}");
    }

    // dev-stack/SR-003: 192 bits, different every time.
    #[test]
    fn secrets_are_long_and_never_repeat() {
        let (a, b) = (secret(), secret());
        assert_eq!(a.len(), 48);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn the_curve_starts_at_the_current_quarter_and_covers_a_day() {
        let curve = price_curve("p", 1_700_000_123);
        let quarter = 1_700_000_123 - 1_700_000_123_i64.rem_euclid(900);
        assert_eq!(curve["intervalPeriod"]["start"], rfc3339(quarter));
        assert_eq!(curve["intervals"].as_array().unwrap().len(), 96);
        assert!(
            price_at(19.0) > price_at(13.0),
            "the evening peak is above the midday dip"
        );
    }

    // inspector/SR-002: the inspector link carries credentials in the fragment, which a browser
    // never sends, and never in a query string.
    #[test]
    fn the_inspector_link_keeps_credentials_out_of_the_request() {
        let banner = Banner {
            url: local_url("127.0.0.1:3000"),
            env_file: ".openadr-dev.env".into(),
            bl_secret: "s1".into(),
            ven_secret: "s2".into(),
            durable: false,
        }
        .render();
        let link = banner.lines().find(|l| l.contains("_inspect")).unwrap();
        assert!(link.contains("/_inspect#bl=bl-1:s1&ven=ven-1:s2"), "{link}");
        assert!(!link.contains('?'), "{link}");
    }

    #[test]
    fn a_wildcard_bind_is_reached_on_loopback() {
        assert_eq!(
            local_url("0.0.0.0:3000"),
            "http://127.0.0.1:3000/openadr3/3.1.0"
        );
        assert_eq!(
            local_url("127.0.0.1:4000"),
            "http://127.0.0.1:4000/openadr3/3.1.0"
        );
    }
}
