+++
title = "Getting started"
description = "A running OpenADR 3.1 VTN with durable storage and OAuth2, a programme, an event, and a VEN reading it — in about five minutes."
weight = 10
+++

By the end of this you will have a VTN with durable storage and a real OAuth2 grant, a programme
with a price curve on it, and a Rust client reading that curve back and resolving it into absolute
time.

If the words *VTN*, *VEN*, *programme* and *event* are new, read
[What OpenADR is](@/docs/what-is-openadr.md) first — it takes about fifteen minutes and the rest of
this page will make more sense.

## Install

```console
$ cargo install openadr --features vtn,internal-auth,sqlite
```

Those three features are the "one binary, one file" build: the server, its own OAuth2 token
endpoint, and SQLite storage. Other combinations are on the [status page](@/docs/status.md); the
crate compiles down to just the wire model and domain core if that is all you need.

## Start a VTN

```console
$ export BL_SECRET=$(openssl rand -hex 24)
$ export VEN_SECRET=$(openssl rand -hex 24)

$ openadr vtn \
      --database ./openadr.sqlite \
      --client bl-1:$BL_SECRET:bl \
      --client ven-1:$VEN_SECRET:ven
2026-02-11T09:00:00Z  INFO openadr::vtn: VTN listening addr=0.0.0.0:3000 base_path=/openadr3/3.1.0
```

Two clients, with the two roles OpenADR distinguishes:

- **`bl`** — *business logic*: the utility's side. Creates programmes and events, reads everything.
- **`ven`** — a *virtual end node*: the device side. Reads what it has been granted, writes reports
  about itself.

The secrets are hashed with Argon2id before they are stored, so what is in memory is not what you
typed. `openadr vtn --help` lists every option.

> **Without `--database`** the VTN keeps everything in memory and loses it on restart. That is fine
> for a first look on `localhost`, and the binary refuses to bind a public address that way unless
> you pass `--ephemeral` — a VTN that silently forgets its programmes should be a deliberate choice.

## Get a token

```console
$ TOKEN=$(curl -s \
      -d grant_type=client_credentials \
      -d client_id=bl-1 \
      -d client_secret=$BL_SECRET \
      localhost:3000/openadr3/3.1.0/auth/token | jq -r .access_token)
```

That is the OAuth2 client-credentials grant, exactly as RFC 6749 describes it. A client that does
not know where the token endpoint is asks first:

```console
$ curl -s localhost:3000/openadr3/3.1.0/auth/server
{"tokenURL":"http://localhost:3000/openadr3/3.1.0/auth/token"}
```

`GET /auth/server` is required of every VTN, even one that delegates to Keycloak — it is how a
client discovers where to authenticate, so it is itself unauthenticated. See
[Authentication](@/docs/authentication.md) for the other backends.

## Create a programme

A **programme** is the container for everything else: a tariff, a flexibility product, a curtailment
scheme. Events belong to exactly one.

```console
$ curl -s -X POST localhost:3000/openadr3/3.1.0/programs \
      -H "Authorization: Bearer $TOKEN" \
      -H 'Content-Type: application/json' \
      -d '{"programName": "day-ahead"}'
{"id":"prg-00000001","createdDateTime":"…","modificationDateTime":"…",
 "objectType":"PROGRAM","programName":"day-ahead"}
```

`programName` is the only required field, and it is unique within a VTN. The VTN assigns the `id`
and both timestamps; a request body cannot even name them, because the request and response are
different types.

## Publish a price curve

```console
$ curl -s -X POST localhost:3000/openadr3/3.1.0/events \
      -H "Authorization: Bearer $TOKEN" \
      -H 'Content-Type: application/json' \
      -d '{
            "programID": "prg-00000001",
            "eventName": "today",
            "intervalPeriod": { "start": "0001-01-01", "duration": "PT1H" },
            "intervals": [
              { "id": 0, "payloads": [{ "type": "PRICE", "values": [0.17, 0.31, 0.11] }] }
            ]
          }'
```

Two pieces of OpenADR are doing real work in that body:

**`start: "0001-01-01"`** is not a date in the year 1. It is the specification's sentinel for
*now* — a "do it now" event, whose first interval began before any client read it. Clients resolve
it against their own clock. In this crate it is `StartTime::Now`, an enum variant rather than a
timestamp, so no code path can compare it by accident.

**Three values in one `PT1H` interval** is not a malformed payload. `PRICE` is a *scalar* payload
type, so several values mean the interval subdivides into that many equal parts — three prices in an
hour are three twenty-minute sub-intervals. Which payload types are scalar comes from the Alliance's
own enumeration files, compiled into the crate.

## Read it back as a schedule

```rust
use openadr::client::{Client, Query, VirtualEndNode};
use openadr::core::{IntervalExpander, Timeline};
use openadr::model::{ProgramName, Timestamp};

let ven = Client::<VirtualEndNode>::builder("http://localhost:3000/openadr3/3.1.0")?
    .credentials(openadr::client::Credentials::new("ven-1", &ven_secret))
    .build()?;

let programs = ven.programs()
    .list_with(&Query::new().program_name(&ProgramName::new("day-ahead")?))
    .await?;
let program = &programs[0];

let now = Timestamp::now();
let events = ven.events()
    .list_all(&Query::new().program(&program.id).active(true))
    .await?;

// Resolve the declared intervals into absolute windows, then merge concurrent
// events by priority into one conflict-free schedule.
let timeline = Timeline::build(
    events.iter().map(|e| (&e.id, &e.content)),
    &IntervalExpander::at(now),
    now,
    now + jiff::Span::new().hours(24),
);

if let Some(segment) = timeline.at(now) {
    println!("in force until {:?}: {:?}", segment.end, segment.payloads);
}
```

The role is a type parameter. `Client<VirtualEndNode>` has no `events().create()` at all — the
specification's scope model is enforced by the compiler rather than by a `403` in production.

The runnable version of both halves is in the repository: `examples/vtn` and `examples/ven` are
written to run against each other.

```console
$ cargo run --features vtn --example vtn
$ cargo run --features ven --example ven   # in another terminal
```

## Without writing any Rust

The same binary is the client, so everything above is one command each:

```console
$ export OPENADR_URL=http://localhost:3000/openadr3/3.1.0 OPENADR_TOKEN=$TOKEN
$ openadr post programs --data '{"programName":"day-ahead"}'
$ openadr get events --program prg-00000001 --active
$ openadr watch events --interval 10        # prints only when something changes
```

See **[the command line](@/docs/cli.md)**.

## What to read next

- **[What OpenADR is](@/docs/what-is-openadr.md)** — the six objects and how they relate.
- **[Object privacy](@/docs/object-privacy.md)** — how a VEN is granted visibility of an event, and
  why it never sees the other groups that event targets. This is the part most worth understanding
  before a pilot.
- **[Running a VTN](@/docs/vtn.md)** — configuration, endpoints, caching, health, metrics.
- **[The VEN runtime](@/docs/ven-runtime.md)** — the other end: registration, sync, timeline and
  reports, with only the meter left to write.
- **[Notifications](@/docs/notifications.md)** — webhooks and MQTT, both ends, so a VEN hears
  about a change instead of waiting for its next poll.
- **[The security model](@/docs/security.md)** — what a VEN structurally cannot do, how secrets are
  held, and what is left to your deployment. Read it before a pilot.
- **[Measuring a VTN](@/docs/conformance.md)** — how much of the specification an implementation
  actually implements, yours or anyone's.
