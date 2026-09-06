+++
title = "The client"
description = "A typed OpenADR 3.1 client whose role is a type parameter: token handling, conditional reads, pagination and profile adapters."
weight = 90
+++

```console
$ cargo add openadr --features client
```

```rust
use openadr::client::{BusinessLogic, Client, Credentials};

let bl = Client::<BusinessLogic>::builder("https://vtn.example.com/openadr3/3.1.0")?
    .credentials(Credentials::new("bl-1", &secret))
    .build()?;

let programs = bl.programs().list().await?;
```

## The role is a type parameter

```rust
let ven = Client::<VirtualEndNode>::builder(url)?.build()?;
ven.events().create(&request);   // ← does not compile
```

`Client<VirtualEndNode>` has no `events().create()` at all. The specification's scope model is
enforced by the compiler rather than by a `403` in production.

| | `BusinessLogic` | `VirtualEndNode` |
|---|---|---|
| `programs()`, `events()` | read, create, update, delete | read only |
| `reports()` | read, **delete** | read, create, update, delete |
| `subscriptions()` | read | read, create, update, delete |
| `vens()`, `resources()` | read, create, update, delete | read, create, update, delete |

Business logic may delete reports but not create them. That is a [deliberate
departure](@/docs/spec-notes.md): read literally, a report becomes unreachable once its VEN's
credentials are revoked, and nobody can delete it through the API.

## Credentials

```rust
// The OAuth2 client-credentials grant. The token endpoint is discovered from
// GET /auth/server unless you pin it.
.credentials(Credentials::new("client-id", &secret))
.credentials(Credentials::new("client-id", &secret).with_token_url(url))
.credentials(Credentials::new("client-id", &secret).with_scopes("read_targets"))

// Or a pre-shared token: a gateway that already authenticated you, or a profile
// like Fluvius' NetFlex that authenticates with mutual TLS and issues no tokens.
.bearer_token(token)
```

Tokens are cached and refreshed at 90 % of the stated lifetime. `Credentials` has a hand-written
`Debug` that redacts the secret, so it cannot appear in a log line or a panic message.

`auth_server()` deliberately bypasses the token path — the endpoint is unauthenticated, because it is
how a client learns where to get a token in the first place. Routing it through token acquisition
would be a cycle, and the compiler said so before the specification did.

`access_token()` hands the current token out, and `client_id()` the identity it was minted for.
OpenADR itself needs both somewhere other than an HTTP header: the MQTT binding presents the access
token as the broker password and derives authorization from the username `[Notifiers §12.2]`. Taking
them from here rather than running a second OAuth2 client beside this one means the *same* token, so
revoking a credential closes both surfaces at once. `client_id()` is `None` for a pre-shared bearer
token — the identity is inside the token and this client never looks — which is why anything that
has to name it out of band asks rather than guesses.

## Reading

```rust
// One page (up to 50, the specification's cap).
let events = bl.events().list().await?;
let events = bl.events().list_with(&query).await?;

// Every page, followed to the end.
let all = bl.events().list_all(&query).await?;

// One object. `try_get` turns 404 into None — which is also what a targeted
// object you may not see returns, deliberately.
let event = bl.events().get(&id).await?;
let maybe = bl.events().try_get(&id).await?;
```

`list_all` fetches pages sequentially. `skip`/`limit` has no cursor, so a parallel fetch could miss
or duplicate a record if the collection changes underneath it.

**Name the limit if you page by hand.** `openadr3.yaml` gives `limit` a *maximum* of 50 and **no
default**, so a VTN sent none may answer with a page of any size. A short page means "that was all of
them" only if you chose the page size. `list_all` sends `limit` on every request for that reason, and
so does the VEN runtime's event sync.

### Conditional reads

The VTN's headline feature for pollers is only a feature if a client can use it:

```rust
let mut etag: Option<String> = None;

loop {
    match ven.events().list_if_changed(&query, etag.as_deref()).await? {
        Some(fresh) => {
            etag = fresh.etag;
            rebuild_timeline(&fresh.value);
        }
        None => { /* 304 — no body crossed the wire */ }
    }
    sleep(interval).await;
}
```

`None` means the VTN answered `304`. OpenADR has no delta sync, so a polling VEN otherwise re-fetches
the whole collection every cycle; this is the difference between a kilobyte and a megabyte.
`get_if_changed` does the same for one object. A VTN with caching switched off never answers `304`,
so the code path is the same either way.

## Filters

```rust
use openadr::client::Query;

Query::new()
    .program(&program_id)          // programID=
    .targets(&[gold, silver])      // targets=gold&targets=silver
    .active(true)                  // active=true — drop elapsed events
    .limit(20)
    .skip(40);

Query::new().program_name(&name);       // GET /programs
Query::new().ven_name(&name);           // GET /vens
Query::new().ven(&ven_id).resource_name(&name);   // GET /resources
Query::new().event(&event_id).client_name(&name); // GET /reports
Query::new().watching(ObjectType::Event);         // GET /subscriptions
Query::new().param("x-vendor-flag", "1");         // anything else
```

`.targets(…)` is how a VEN asks for the targeted objects it has been granted. Naming a target
returns **only** objects carrying it — untargeted objects are reached by naming none. See
[Object privacy](@/docs/object-privacy.md).

## Errors

```rust
match ven.events().get(&id).await {
    Ok(event) => …,
    Err(ClientError::Api { status, problem }) => {
        eprintln!("{status}: {}", problem.detail.as_deref().unwrap_or("no detail"));
        // problem.instance is the request id — quote it to the VTN's operator.
    }
    Err(ClientError::Auth(why)) => …,     // the token exchange failed
    Err(ClientError::Transport(e)) => …,  // it never reached the VTN
    Err(e) => …,
}
```

`ClientError::Api` carries the full `problem` body. A peer that returns something else gets a
synthetic one, so the variant is always usable.

## Profile adapters

Deployed profiles bend the schema. Fluvius' NetFlex sends `reportDescriptor.frequency` as an ISO
duration where the schema says integer, and adds a `required` boolean the schema does not define.

Widening the model for one peer would weaken it for every other deployment, so the deviation is an
adapter at the edge instead:

```rust
let client = Client::<VirtualEndNode>::builder(url)?
    .bearer_token(token)
    .adapter(openadr::model::adapt::Fluvius)
    .build()?;
```

An adapter rewrites the JSON before parsing and after serialising. Non-schema values are parked under
a preserved key and put back on the way out, so a round trip is exact and no semantics are invented —
the canonical field falls back to its schema default rather than guessing.

Adapters apply in the order added on the way out and in reverse on the way in. A client with none
pays nothing.

## A VEN loop

The shape of every real VEN, and what `examples/ven` does:

```rust
let mut etag = None;
let mut timeline = Timeline::default();

loop {
    let now = Timestamp::now();
    let query = Query::new().program(&program.id).active(true);

    if let Some(fresh) = client.events().list_if_changed(&query, etag.as_deref()).await? {
        etag = fresh.etag;
        timeline = Timeline::build(
            fresh.value.iter().map(|e| (&e.id, &e.content)),
            &IntervalExpander::at(now),
            now,
            now + jiff::Span::new().hours(48),
        );
        for (id, why) in timeline.skipped() {
            eprintln!("skipped event {id}: {why}");
        }
    }

    match timeline.at(now) {
        Some(segment) => apply(&segment.payloads),
        None => release(),
    }

    // Wake when the schedule changes, not on a fixed timer — capped, because the
    // VTN may publish a new event before then.
    let sleep = timeline.next_change(now)
        .and_then(|next| Duration::try_from(next - now).ok())
        .unwrap_or(MAX_SLEEP)
        .min(MAX_SLEEP);
    tokio::time::sleep(sleep).await;
}
```

Run it against the bundled server:

```console
$ cargo run --features vtn --example vtn
$ cargo run --features ven --example ven
```

Most of what a product needs beyond this loop — idempotent registration, resource reconciliation,
report scheduling, `randomizeStart`, a clock-skew guard, and state that survives a restart — is
already written. It is **[the VEN runtime](@/docs/ven-runtime.md)**, and it is what `examples/ven`
actually uses. Write the loop above by hand only if you want something the runtime does not do.

## Talking to a VTN this crate does not model

Two escape hatches under the typed collections, for a VTN extension or for a tool that wants the
VTN's bytes rather than this crate's reading of them:

```rust
let raw: serde_json::Value = client.get_json("events", &Query::new().active(true)).await?;
let created: serde_json::Value = client.send_json("POST", "programs", Some(&body)).await?;
let polled = client.list_json_if_changed::<Value>("events", &query, etag).await?;
```

All three decode through the same function the typed methods use, so an adapter chain applies to
the raw path as well — including the conditional read, which is the easy one to leave out.

`client.server_time()` reads the VTN's `Date` header, which is the only clock reference the protocol
offers and is what [the VEN runtime](@/docs/ven-runtime.md) checks its own against.
