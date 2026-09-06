//! `openadr` — one binary for both sides of the protocol.
//!
//! `openadr vtn` runs a Virtual Top Node. `openadr get|post|put|delete|watch` talks to one — this
//! one, or anybody's. Keeping them in one binary is not tidiness: the client commands are a second
//! consumer of the crate's own client library, and a second consumer is what finds the gaps a
//! library's own tests cannot.

mod serve;

#[cfg(feature = "conformance")]
mod conform;
#[cfg(feature = "client")]
mod query;

fn main() {
    // `main` returning `Result` prints the error with `Debug`, which escapes newlines and wraps
    // the message in quotes. An operator reading a refusal deserves better than that.
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// Split `--flag=value` into two arguments.
///
/// Every parser here reads `--flag value`, and every other command-line tool also reads
/// `--flag=value`. A Compose file, a systemd unit and a Kubernetes manifest all write the second
/// form by habit — and `unknown option "--listen=0.0.0.0:3000"` is a message that sends the reader
/// looking for a typo in the flag name.
///
/// Split once, here, so the four parsers below cannot disagree about it. `--` is left alone, and so
/// is anything that is not a flag: a JSON body passed to `--data` may contain `=` and is a value,
/// not a flag.
fn split_inline_values(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") && flag.len() > 2 => {
                out.push(flag.to_string());
                out.push(value.to_string());
            }
            _ => out.push(arg.clone()),
        }
    }
    out
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = split_inline_values(&std::env::args().skip(1).collect::<Vec<_>>());
    let command = args.first().map(String::as_str);

    // Only the server logs; the client commands write JSON to stdout and anything else to stderr,
    // so that `openadr get events | jq` works without an environment variable.
    if command == Some("vtn") {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "openadr=info,warn".into()),
            )
            .init();
    }

    match command {
        Some("vtn") => {
            if args.get(1).map(String::as_str) == Some("--help") {
                serve::print_help();
                return Ok(());
            }
            serve::run_vtn(&args[1..])
        }
        Some(verb @ ("get" | "post" | "put" | "delete" | "watch")) => {
            client_command(verb, &args[1..])
        }
        Some("conformance") => conformance_command(&args[1..]),
        Some("discover") => discover_command(&args[1..]),
        Some("hash-secret") => hash_secret(&args[1..]),
        Some("version") | Some("--version") => {
            println!(
                "openadr {} (OpenADR {})",
                env!("CARGO_PKG_VERSION"),
                openadr::SPEC_VERSION
            );
            Ok(())
        }
        Some("help") | Some("--help") | None => {
            print_help();
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command {other:?}\n");
            print_help();
            std::process::exit(2);
        }
    }
}

#[cfg(feature = "client")]
fn client_command(verb: &str, args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    query::run(verb, args)
}

#[cfg(not(feature = "client"))]
fn client_command(verb: &str, _args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    Err(
        format!("`openadr {verb}` needs the `client` feature; rebuild with --features client")
            .into(),
    )
}

#[cfg(feature = "conformance")]
fn conformance_command(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    conform::run(args)
}

/// `openadr discover` — the VEN's half of `[Def §Discovery and Configuration of Local VTNs]`.
///
/// Prints what answered rather than picking one: which local VTN to enrol with is a decision about
/// the site, and a tool that chose would be choosing for somebody who can see the room.
#[cfg(feature = "mdns")]
fn discover_command(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut seconds = 3u64;
    let mut json = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--timeout" => {
                seconds = it
                    .next()
                    .ok_or("--timeout needs a value")?
                    .parse()
                    .map_err(|e| format!("--timeout: {e}"))?;
            }
            "--json" => json = true,
            "--help" => {
                println!(
                    "openadr discover [--timeout <SECONDS>] [--json]\n\n\
                     Browse the local network for VTNs advertising `_openadr3._tcp`, and print\n\
                     what answered. Nothing found is not an error: a site with no local VTN is the\n\
                     ordinary case for a VEN configured with a cloud URL.\n\n\
                         --timeout <SECONDS>   how long to listen          [default: 3]\n\
                         --json                machine-readable output"
                );
                return Ok(());
            }
            other => return Err(format!("unknown option {other:?}").into()),
        }
    }

    let found = openadr::discovery::discover(std::time::Duration::from_secs(seconds))?;
    if json {
        let records: Vec<serde_json::Value> = found
            .iter()
            .map(|s| {
                serde_json::json!({
                    "instance": s.instance,
                    "hostname": s.hostname,
                    "port": s.port,
                    "url": s.local_url(),
                    "version": s.version,
                    "programNames": s.program_names,
                    "requiresAuth": s.requires_auth,
                    "openapiUrl": s.openapi_url,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&records)?);
        return Ok(());
    }

    if found.is_empty() {
        eprintln!("no VTN answered on this network within {seconds}s");
        return Ok(());
    }
    for service in &found {
        println!("{}", service.local_url());
        println!("  instance      {}", service.instance);
        println!("  version       {}", service.version);
        println!(
            "  requires auth {}",
            if service.requires_auth { "yes" } else { "no" }
        );
        if !service.program_names.is_empty() {
            println!("  programmes    {}", service.program_names.join(", "));
        }
        if let Some(url) = &service.openapi_url {
            println!("  openapi       {url}");
        }
    }
    Ok(())
}

#[cfg(not(feature = "mdns"))]
fn discover_command(_args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    Err("`openadr discover` needs the `mdns` feature; rebuild with --features mdns".into())
}

#[cfg(not(feature = "conformance"))]
fn conformance_command(_args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    Err("`openadr conformance` needs the `conformance` feature; rebuild with --features conformance"
        .into())
}

/// `openadr hash-secret` — turn a client secret into something safe to store.
///
/// `--client id:secret:role` puts a plaintext secret in the process table and in shell history.
/// `--client-hashed id:HASH:role` does not, and the VTN has always been able to read one — there
/// was simply no way to *produce* one, which made the safer flag documentation rather than a
/// feature.
#[cfg(feature = "internal-auth")]
fn hash_secret(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.is_empty() || args[0] == "--help" {
        println!(
            "openadr hash-secret <SECRET>

Print an Argon2id PHC hash of a client secret, for `openadr vtn --client-hashed id:HASH:role`.

    $ openadr vtn --client-hashed bl-1:\"$(openadr hash-secret $SECRET)\":bl

The hash is safe to keep in a configuration file or a database: it identifies the secret without
being usable as one. Reading it back is what `--client-hashed` does.

Pass `-` to read the secret from stdin instead, which keeps it out of the process table too."
        );
        return Ok(());
    }
    let secret = if args[0] == "-" {
        std::io::read_to_string(std::io::stdin())?
            .trim_end_matches(['\n', '\r'])
            .to_string()
    } else {
        args[0].clone()
    };
    println!(
        "{}",
        openadr::vtn::auth::InternalAuth::hash_secret(&secret)?
    );
    Ok(())
}

#[cfg(not(feature = "internal-auth"))]
fn hash_secret(_args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    Err("`openadr hash-secret` needs the `internal-auth` feature; rebuild with --features internal-auth"
        .into())
}

fn print_help() {
    println!(
        "openadr {} — OpenADR {} tooling

USAGE:
    openadr vtn [OPTIONS]              run a Virtual Top Node
    openadr get <collection> [<id>]    read from a VTN
    openadr post <collection>          create an object
    openadr put <collection> <id>      replace an object
    openadr delete <collection> <id>   remove an object
    openadr watch <collection>         poll, printing only what changed
    openadr conformance                measure a VTN against the specification
    openadr discover                   find local VTNs on this network over mDNS
    openadr hash-secret <SECRET>       hash a client secret for --client-hashed
    openadr version                    print the version
    openadr help                       print this message

    `openadr vtn --help`, `openadr get --help` and `openadr conformance --help` have the
    details.

GETTING STARTED:
    $ openadr vtn --listen 127.0.0.1:3000 --client bl-1:secret:bl &
    $ export OPENADR_URL=http://localhost:3000/openadr3/3.1.0
    $ export OPENADR_TOKEN=$(curl -s -d grant_type=client_credentials -d client_id=bl-1 \\
          -d client_secret=secret $OPENADR_URL/auth/token | jq -r .access_token)
    $ openadr post programs --data '{{\"programName\":\"grid-aware-charging\"}}'
    $ openadr get programs

ENVIRONMENT:
    OPENADR_URL, OPENADR_TOKEN, OPENADR_CLIENT_ID, OPENADR_CLIENT_SECRET
    RUST_LOG                           tracing filter for `openadr vtn`
",
        env!("CARGO_PKG_VERSION"),
        openadr::SPEC_VERSION,
    );
}

#[cfg(test)]
mod tests {
    use super::split_inline_values;

    fn split(args: &[&str]) -> Vec<String> {
        split_inline_values(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn a_flag_may_carry_its_value_inline() {
        assert_eq!(
            split(&["vtn", "--listen=0.0.0.0:3000", "--ephemeral"]),
            vec!["vtn", "--listen", "0.0.0.0:3000", "--ephemeral"]
        );
        // Both forms, in one command line.
        assert_eq!(
            split(&["--url", "http://x", "--token=abc"]),
            vec!["--url", "http://x", "--token", "abc"]
        );
    }

    #[test]
    fn only_the_first_equals_separates_a_flag_from_its_value() {
        // A client secret, a Postgres URL and a JSON body all contain `=` legitimately.
        assert_eq!(
            split(&["--client=bl:se=cret:bl"]),
            vec!["--client", "bl:se=cret:bl"]
        );
        // A bare value is never split, whatever it contains.
        assert_eq!(
            split(&["--data", r#"{"a":"b=c"}"#]),
            vec!["--data", r#"{"a":"b=c"}"#]
        );
        // `--` is not a flag with an empty name.
        assert_eq!(split(&["--", "--x=1"]), vec!["--", "--x", "1"]);
    }
}
