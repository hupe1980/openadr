//! `openadr get`, `post`, `put`, `delete` and `watch` — talking to a VTN.
//!
//! The point is not to be a second API. It is to make the crate's own client the fastest way to
//! look at a VTN — anyone's VTN — and, less obviously, to be a *second consumer* of that client.
//! The first bug this arrangement found was that `list_if_changed` skipped the adapter chain, which
//! every unit test of either half had passed.
//!
//! So the shape follows the protocol rather than inventing verbs: a collection, an optional id, and
//! the specification's own query parameters. Output is the VTN's JSON, unaltered, so it pipes into
//! `jq` and comparing two implementations is a `diff`.
//!
//! `watch` is the one addition, and it exists because polling is what deployments actually do: it
//! holds the `ETag` and prints only when the collection changes, which is the client half of the
//! VTN's caching doing something visible.

use std::time::Duration as StdDuration;

use openadr::client::{BusinessLogic, Client, ClientError, Credentials, Query};
use openadr::model::ObjectId;

/// How to reach the VTN, from flags or the environment.
#[derive(Debug)]
struct Connection {
    url: String,
    token: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
}

impl Connection {
    fn build(&self) -> Result<Client<BusinessLogic>, Box<dyn std::error::Error>> {
        // Business logic, always. The role type parameter exists to stop a *program* calling an
        // endpoint its scopes forbid; at a command line the VTN's own `403` is the better teacher,
        // and a VEN token simply gets one.
        let mut builder = Client::<BusinessLogic>::builder(&self.url)?;
        match (&self.token, &self.client_id, &self.client_secret) {
            (Some(token), _, _) => builder = builder.bearer_token(token),
            (None, Some(id), Some(secret)) => {
                builder = builder.credentials(Credentials::new(id, secret));
            }
            (None, Some(_), None) | (None, None, Some(_)) => {
                return Err("--client-id and --client-secret go together".into());
            }
            // No credentials at all is a real configuration: a public tariff server.
            (None, None, None) => {}
        }
        Ok(builder.build()?)
    }
}

/// One parsed command line.
#[derive(Debug)]
struct Invocation {
    connection: Connection,
    collection: String,
    id: Option<String>,
    query: Query,
    body: Option<serde_json::Value>,
    all: bool,
    interval: StdDuration,
}

/// Print the usage for the client commands.
pub fn print_help() {
    println!(
        r#"openadr get|post|put|delete|watch — talk to a VTN

USAGE:
    openadr get <collection> [<id>] [FILTERS] [OPTIONS]
    openadr post <collection> --data <JSON> | --file <PATH> [OPTIONS]
    openadr put <collection> <id> --data <JSON> | --file <PATH> [OPTIONS]
    openadr delete <collection> <id> [OPTIONS]
    openadr watch <collection> [FILTERS] [--interval <SECONDS>] [OPTIONS]

COLLECTIONS:
    programs  events  reports  subscriptions  vens  resources
    notifiers                       what push transports the VTN offers
    auth/server                     where to get a token

CONNECTION:
    --url <URL>                base URL, including the base path
                               [env: OPENADR_URL]
    --token <TOKEN>            a pre-shared bearer token
                               [env: OPENADR_TOKEN]
    --client-id <ID>           OAuth2 client credentials, exchanged at the token
    --client-secret <SECRET>   endpoint the VTN advertises
                               [env: OPENADR_CLIENT_ID, OPENADR_CLIENT_SECRET]

    With none of these the request goes out unauthenticated, which is what a
    public tariff server expects.

FILTERS (the specification's own query parameters):
    --targets <A,B>            repeatable, or comma-separated
    --program <ID>             programID
    --event <ID>               eventID, for reports
    --ven <ID>                 venID, for resources
    --client-name <NAME>       clientName, for reports and subscriptions
    --name <NAME>              programName / venName / resourceName, by collection
    --active                   only events whose intervals have not all elapsed
    --objects <TYPE>           repeatable, for subscriptions
    --skip <N> --limit <N>     one page; the schema caps limit at 50
    --all                      follow pagination to the end instead

OPTIONS:
    --data <JSON>              request body, inline
    --file <PATH>              request body, from a file ("-" for stdin)
    --interval <SECONDS>       watch poll interval           [default: 30]

EXAMPLES:
    export OPENADR_URL=http://localhost:3000/openadr3/3.1.0 OPENADR_TOKEN=$TOKEN
    openadr get programs
    openadr get events --program prg-00000001 --active
    openadr post programs --data '{{"programName":"grid-aware-charging"}}'
    openadr watch events --targets group1 --interval 10
"#
    );
}

/// Run one client command.
pub fn run(verb: &str, args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.first().map(String::as_str) == Some("--help") {
        print_help();
        return Ok(());
    }
    let invocation = parse(args)?;
    let client = invocation.connection.build()?;

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move { dispatch(verb, &client, invocation).await })
}

async fn dispatch(
    verb: &str,
    client: &Client<BusinessLogic>,
    invocation: Invocation,
) -> Result<(), Box<dyn std::error::Error>> {
    let collection = invocation.collection.as_str();
    match verb {
        "get" => match collection {
            // The two discovery endpoints are not collections and have no id or filters.
            "notifiers" => print(&client.notifiers().await.map_err(explain)?),
            "auth/server" | "auth" => print(&client.auth_server().await.map_err(explain)?),
            _ => get(client, &invocation).await?,
        },
        "post" => {
            let body = invocation
                .body
                .ok_or("post needs a body: pass --data or --file")?;
            print(&raw(client, "POST", collection, None, Some(body)).await?)
        }
        "put" => {
            let id = invocation.id.as_deref().ok_or("put needs an id")?;
            let body = invocation
                .body
                .ok_or("put needs a body: pass --data or --file")?;
            print(&raw(client, "PUT", collection, Some(id), Some(body)).await?)
        }
        "delete" => {
            let id = invocation.id.as_deref().ok_or("delete needs an id")?;
            print(&raw(client, "DELETE", collection, Some(id), None).await?)
        }
        "watch" => watch(client, &invocation).await?,
        other => return Err(format!("unknown command {other:?}").into()),
    }
    Ok(())
}

async fn get(
    client: &Client<BusinessLogic>,
    invocation: &Invocation,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = match &invocation.id {
        Some(id) => format!("{}/{id}", invocation.collection),
        None => invocation.collection.clone(),
    };
    if invocation.id.is_none() && invocation.all {
        // `list_all` on the typed collection would need the concrete type; at this layer the body
        // is JSON either way, so pagination is followed here in the same sequential shape and for
        // the same reason: `skip`/`limit` has no cursor, so a parallel fetch can miss a record.
        let mut out: Vec<serde_json::Value> = Vec::new();
        let mut skip = 0usize;
        const PAGE: usize = 50;
        loop {
            let mut query = invocation.query.clone();
            query = query.skip(skip).limit(PAGE);
            let page: Vec<serde_json::Value> =
                client.get_json(&path, &query).await.map_err(explain)?;
            let received = page.len();
            out.extend(page);
            if received < PAGE {
                break;
            }
            skip += PAGE;
        }
        print(&out);
        return Ok(());
    }
    let value: serde_json::Value = client
        .get_json(&path, &invocation.query)
        .await
        .map_err(explain)?;
    print(&value);
    Ok(())
}

/// Poll a collection, printing only when it changes.
async fn watch(
    client: &Client<BusinessLogic>,
    invocation: &Invocation,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut etag: Option<String> = None;
    eprintln!(
        "watching {} every {}s; ^C to stop",
        invocation.collection,
        invocation.interval.as_secs()
    );
    loop {
        let polled: Result<Option<openadr::client::Tagged<serde_json::Value>>, _> = client
            .list_json_if_changed(&invocation.collection, &invocation.query, etag.as_deref())
            .await;
        match polled {
            Ok(None) => {} // 304: nothing has moved, and nothing is printed.
            Ok(Some(tagged)) => {
                etag = tagged.etag;
                print(&tagged.value);
            }
            // A transient failure must not end a watch: the point of it is to survive the VTN
            // restarting underneath it.
            Err(e) => eprintln!("error: {}", explain(e)),
        }
        tokio::time::sleep(invocation.interval).await;
    }
}

async fn raw(
    client: &Client<BusinessLogic>,
    method: &str,
    collection: &str,
    id: Option<&str>,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let path = match id {
        Some(id) => {
            // Parsed rather than interpolated: an id with a slash in it would otherwise address a
            // different endpoint entirely.
            let id: ObjectId = id.parse()?;
            format!("{collection}/{id}")
        }
        None => collection.to_string(),
    };
    let value: serde_json::Value = client
        .send_json(method, &path, body.as_ref())
        .await
        .map_err(explain)?;
    Ok(value)
}

/// Turn a client error into something worth reading at a terminal.
fn explain(error: ClientError) -> Box<dyn std::error::Error> {
    match error {
        ClientError::Api { status, problem } => {
            let detail = problem
                .detail
                .as_deref()
                .or(problem.title.as_deref())
                .unwrap_or("no detail");
            let instance = problem
                .instance
                .as_deref()
                .map(|i| format!(" (request {i})"))
                .unwrap_or_default();
            format!("{status}: {detail}{instance}").into()
        }
        other => Box::new(other),
    }
}

fn print<T: serde::Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(rendered) => println!("{rendered}"),
        Err(e) => eprintln!("error: could not render the response: {e}"),
    }
}

fn parse(args: &[String]) -> Result<Invocation, Box<dyn std::error::Error>> {
    let mut positional: Vec<String> = Vec::new();
    let mut query = Query::new();
    let mut body_text: Option<String> = None;
    let mut file: Option<String> = None;
    let mut all = false;
    let mut interval = StdDuration::from_secs(30);
    let mut connection = Connection {
        url: std::env::var("OPENADR_URL").unwrap_or_default(),
        token: std::env::var("OPENADR_TOKEN").ok(),
        client_id: std::env::var("OPENADR_CLIENT_ID").ok(),
        client_secret: std::env::var("OPENADR_CLIENT_SECRET").ok(),
    };

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--url" => connection.url = value()?,
            "--token" => connection.token = Some(value()?),
            "--client-id" => connection.client_id = Some(value()?),
            "--client-secret" => connection.client_secret = Some(value()?),
            "--targets" => {
                // Both wire forms, because both are in the field.
                for target in value()?.split(',').map(str::trim).filter(|t| !t.is_empty()) {
                    query = query.target(&target.parse()?);
                }
            }
            "--program" => query = query.program(&value()?.parse()?),
            "--event" => query = query.event(&value()?.parse()?),
            "--ven" => query = query.ven(&value()?.parse()?),
            "--client-name" => query = query.param("clientName", value()?),
            "--name" => query = query.param("__name", value()?),
            "--objects" => query = query.param("objects", value()?),
            "--active" => query = query.active(true),
            "--skip" => query = query.skip(value()?.parse()?),
            "--limit" => query = query.limit(value()?.parse()?),
            "--all" => all = true,
            "--data" => body_text = Some(value()?),
            "--file" => file = Some(value()?),
            "--interval" => interval = StdDuration::from_secs(value()?.parse()?),
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other:?}").into());
            }
            other => positional.push(other.to_string()),
        }
    }

    if connection.url.is_empty() {
        return Err("no VTN: pass --url or set OPENADR_URL".into());
    }
    let collection = positional
        .first()
        .cloned()
        .ok_or("which collection? try `openadr get --help`")?;
    // `--name` means a different parameter per collection, which is the specification's doing:
    // `programName`, `venName` and `resourceName` are three parameters for one idea.
    let query = rename_name_parameter(query, &collection);

    if body_text.is_some() && file.is_some() {
        return Err("pass one of --data and --file, not both".into());
    }
    let body = match (body_text, file) {
        (Some(text), _) => Some(serde_json::from_str(&text)?),
        (None, Some(path)) => {
            let text = if path == "-" {
                std::io::read_to_string(std::io::stdin())?
            } else {
                std::fs::read_to_string(&path)?
            };
            Some(serde_json::from_str(&text)?)
        }
        (None, None) => None,
    };

    Ok(Invocation {
        connection,
        collection,
        id: positional.get(1).cloned(),
        query,
        body,
        all,
        interval,
    })
}

/// Resolve `--name` to the parameter this collection actually uses.
fn rename_name_parameter(query: Query, collection: &str) -> Query {
    let Some(value) = query.value("__name") else {
        return query;
    };
    let key = match collection {
        "programs" => "programName",
        "vens" => "venName",
        "resources" => "resourceName",
        // Events and reports have no name parameter; passing one through unchanged lets the VTN
        // say so rather than having the CLI guess a different meaning.
        _ => "name",
    };
    query.without("__name").param(key, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_collection_and_filters_parse() {
        let i = parse(&args(&[
            "events",
            "--url",
            "http://vtn.test/openadr3/3.1.0",
            "--program",
            "prg-1",
            "--targets",
            "group1,group2",
            "--active",
        ]))
        .unwrap();
        assert_eq!(i.collection, "events");
        assert!(i.id.is_none());
        let rendered = i.query.pairs();
        assert!(rendered.contains(&("programID".into(), "prg-1".into())));
        assert_eq!(
            rendered.iter().filter(|(k, _)| k == "targets").count(),
            2,
            "a comma-separated list is two targets, not one"
        );
        assert!(rendered.contains(&("active".into(), "true".into())));
    }

    #[test]
    fn name_resolves_to_the_parameter_the_collection_uses() {
        for (collection, expected) in [
            ("programs", "programName"),
            ("vens", "venName"),
            ("resources", "resourceName"),
        ] {
            let i = parse(&args(&[
                collection,
                "--url",
                "http://vtn.test",
                "--name",
                "tou",
            ]))
            .unwrap();
            assert!(
                i.query.pairs().contains(&(expected.into(), "tou".into())),
                "{collection}: {:?}",
                i.query.pairs()
            );
            assert!(!i.query.pairs().iter().any(|(k, _)| k == "__name"));
        }
    }

    #[test]
    fn a_missing_url_is_refused_before_anything_is_sent() {
        // Otherwise the failure is a URL parse error from three layers down.
        unsafe { std::env::remove_var("OPENADR_URL") };
        let err = parse(&args(&["programs"])).unwrap_err().to_string();
        assert!(err.contains("OPENADR_URL"), "{err}");
    }

    #[test]
    fn two_bodies_are_refused_rather_than_one_silently_winning() {
        let err = parse(&args(&[
            "programs",
            "--url",
            "http://vtn.test",
            "--data",
            "{}",
            "--file",
            "body.json",
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("not both"), "{err}");
    }

    #[test]
    fn an_id_is_the_second_positional() {
        let i = parse(&args(&["events", "evt-1", "--url", "http://vtn.test"])).unwrap();
        assert_eq!(i.id.as_deref(), Some("evt-1"));
    }
}
