//! `openadr conformance` — measure a VTN against the specification.
//!
//! The command that makes [`openadr::conformance`] usable by somebody who is not writing Rust: the
//! point of a black-box suite is that it runs against implementations whose authors have never
//! heard of this crate.

use openadr::conformance::{Credential, Runner, Severity, Target};

/// Print the usage.
pub fn print_help() {
    println!(
        r#"openadr conformance — measure a VTN against the OpenADR 3.1 specification

USAGE:
    openadr conformance [OPTIONS]

CONNECTION:
    --url <URL>                base URL, including the base path
                               [env: OPENADR_URL]
    --bl-token <TOKEN>         a business-logic bearer token
                               [env: OPENADR_TOKEN]
    --bl-client <ID:SECRET>    or business-logic client credentials
    --ven-token <TOKEN>        a VEN bearer token
    --ven-client <ID:SECRET>   or VEN client credentials
    --ven-client-id <ID>       the clientID the VEN credential authenticates as

OUTPUT:
    --json                     machine-readable, for a published matrix
    --strict                   exit non-zero if any check failed, not just a required one

WHAT IT NEEDS:
    Business-logic credentials run most of the suite. **Object privacy is the half most worth
    measuring**, and it needs a VEN credential *and* the clientID it authenticates as — a VTN maps
    a token to a clientID by means the specification leaves unspecified, so the suite cannot
    discover it. Without them those checks are reported as skipped, which is not a pass.

WHAT IT DOES TO THE VTN:
    It writes. Conformance cannot be observed from reads alone. Programmes, events and VEN objects
    are created under names prefixed `oadr-conformance-` and deleted afterwards, including when a
    check fails. Do not point it at a VTN whose data matters.

EXIT STATUS:
    0   no required check failed
    1   a required check failed, or (with --strict) any check did
    2   the suite could not be run at all

EXAMPLES:
    openadr conformance --url http://localhost:3000/openadr3/3.1.0 \
        --bl-token $BL --ven-token $VEN --ven-client-id ven-client-1

    openadr conformance --url https://someone-elses-vtn.example.com/openadr3/3.1.0 \
        --bl-client bl-1:$SECRET --json > matrix.json
"#
    );
}

/// Run the suite.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.first().map(String::as_str) == Some("--help") {
        print_help();
        return Ok(());
    }

    let mut url = std::env::var("OPENADR_URL").unwrap_or_default();
    let mut bl_token = std::env::var("OPENADR_TOKEN").ok();
    let mut bl_client: Option<String> = None;
    let mut ven_token: Option<String> = None;
    let mut ven_client: Option<String> = None;
    let mut ven_client_id: Option<String> = None;
    let mut json = false;
    let mut strict = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--url" => url = value()?,
            "--bl-token" => bl_token = Some(value()?),
            "--bl-client" => bl_client = Some(value()?),
            "--ven-token" => ven_token = Some(value()?),
            "--ven-client" => ven_client = Some(value()?),
            "--ven-client-id" => ven_client_id = Some(value()?),
            "--json" => json = true,
            "--strict" => strict = true,
            other => return Err(format!("unknown option {other:?}").into()),
        }
    }

    if url.is_empty() {
        return Err("no VTN: pass --url or set OPENADR_URL".into());
    }

    let mut target = Target::new(url);
    if let Some(credential) = credential(bl_token, bl_client, "--bl-client")? {
        target = target.with_business_logic(credential);
    }
    match (
        credential(ven_token, ven_client, "--ven-client")?,
        ven_client_id,
    ) {
        (Some(credential), Some(client_id)) => target = target.with_ven(credential, client_id),
        (Some(_), None) => {
            // Refused rather than half-run: without the clientID the privacy checks skip, and a
            // report full of skips looks like a report full of nothing wrong.
            return Err(
                "--ven-token/--ven-client also needs --ven-client-id, which is the \
                        clientID that credential authenticates as"
                    .into(),
            );
        }
        (None, _) => {}
    }

    let runner = Runner::new(target)?;
    let report = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async { runner.run().await });

    if json {
        println!("{}", serde_json::to_string_pretty(&report.to_json())?);
    } else {
        print!("{report}");
    }

    let failed_anything = report.failures().count() > 0;
    let exit = if strict && failed_anything {
        1
    } else if report.is_conformant() {
        0
    } else {
        1
    };
    if exit != 0 {
        std::process::exit(exit);
    }
    Ok(())
}

/// A token or an `id:secret` pair, whichever was given.
fn credential(
    token: Option<String>,
    pair: Option<String>,
    flag: &str,
) -> Result<Option<Credential>, Box<dyn std::error::Error>> {
    match (token, pair) {
        (Some(_), Some(_)) => Err(format!("pass a token or {flag}, not both").into()),
        (Some(token), None) => Ok(Some(Credential::Token(token))),
        (None, Some(pair)) => {
            // Split once from the left: a secret may contain colons, an id may not.
            let (id, secret) = pair
                .split_once(':')
                .ok_or_else(|| format!("{flag} takes id:secret"))?;
            Ok(Some(Credential::ClientCredentials {
                id: id.to_string(),
                secret: secret.to_string(),
            }))
        }
        (None, None) => Ok(None),
    }
}

/// Every severity, so the help and the report agree on the vocabulary.
#[allow(dead_code)]
const SEVERITIES: [Severity; 3] = [
    Severity::Required,
    Severity::Recommended,
    Severity::Extension,
];
