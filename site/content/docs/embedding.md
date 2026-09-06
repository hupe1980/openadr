+++
title = "Embedding and no_std"
description = "Using the wire model and domain core without the server, building for a microcontroller or WebAssembly, and the Cargo feature matrix."
weight = 100
+++

The crate is one library sliced with Cargo features. The three lower layers — the wire model, the
payload schema and the domain core — are always present, have no I/O and no clock of their own, and
compile without `std`.

## The feature matrix

| Feature | Adds | Pulls in |
|---|---|---|
| *(none)* | `model`, `schema`, `core` | `serde`, `serde_json`, `jiff`, `rust_decimal`, `thiserror` |
| `std` *(default)* | `std` implementations, `SystemClock` | — |
| `client` | the HTTP client | `reqwest`, `tokio`, `url` |
| `ven` | the VEN runtime loop, on top of `client` | `async-trait` |
| `conformance` | the black-box suite, on top of `client` | — |
| `vtn` | the server | `axum`, `tower-http`, `tokio`, `uuid` |
| `sqlite` | SQLite storage | `sqlx` |
| `postgres` | PostgreSQL storage | `sqlx` |
| `internal-auth` | the VTN's own `/auth/token` | `jsonwebtoken`, `argon2`, `rand` |
| `external-auth` | JWT validation against a JWKS | `jsonwebtoken`, `reqwest` |
| `webhook` | webhook delivery | `reqwest`, `hmac`, `sha2` |
| `webhook-signature` | verifying a delivery's signature — **`no_std`-capable**, builds for `thumbv7em-none-eabihf` | `hmac`, `sha2` |
| `mqtt` | the MQTT publisher (with `vtn`) and the VEN's subscriber (with `ven`) | `rumqttc`, `rustls` |
| `mdns` | advertising a local VTN (with `vtn`) and browsing for one (with `ven`) | `mdns-sd` |
| `tls` | the VTN's own TLS listener, and an optional client-certificate gate | `tokio-rustls`, `hyper`, `hyper-util` |

Every one of those flags is read by a `cfg` somewhere. A feature flag nothing reads is a claim the
manifest makes and nothing keeps, and CI checks that every combination builds.

Three combinations are worth naming. `ven` is the whole VEN side and pulls in no server;
`vtn,client` is what the command-line binary wants, because `openadr get` talks to a VTN with the
same client library anybody else would use; and `conformance` deliberately depends on `client` and
on nothing in `vtn`, because a suite that could reach into the server it measures would be measuring
that server's opinion of itself.

## Using the model alone

Parsing, validating and rendering OpenADR JSON needs no server and no client:

```console
$ cargo add openadr --no-default-features
```

```rust
use openadr::model::{Event, EventRequest};

let event: Event = serde_json::from_slice(&body)?;
let json = serde_json::to_vec(&event)?;
```

What you get beyond `serde` derives:

**Newtypes that cannot hold an illegal value.** `ObjectId` enforces the schema's
`^[a-zA-Z0-9_-]{1,128}$` and additionally refuses `null`, `undefined`, `.` and `..`, which would be
ambiguous in a path or in JSON. Same for `ClientId`, `Target`, `VenName`, `ResourceName`,
`ClientName`, `ProgramName`, `PayloadType`, `Unit`. Nothing downstream asks whether a name is well
formed.

**Sentinels as variants.** `StartTime::{Now, At}` and `Duration::{Forever, Finite}`.

**Requests and responses as distinct types.** `ProgramRequest` has no `id` field, so a `POST` body
cannot name one.

**Decimal values.** `Value::Number` holds a `rust_decimal::Decimal`, serialised as a JSON *number*
(`rust_decimal` defaults to strings, which no other implementation would accept). `Value` equality is
semantic, not representational: `Integer(60)` equals `Number(60)`, because JSON has one number type
and which variant a parse produces depends only on how the peer wrote it.

**A duration parser that follows the schema, not ISO 8601.** The specification's pattern forbids
fractional hours and minutes and mutually excludes days and weeks; ISO 8601 does not. `PT1.5H` is
refused here and accepted by a general-purpose parser.

`Duration` equality is *fieldwise*: `PT60M` and `PT1H` are different values, because they render
differently and a round trip must be lossless. Compare lengths with `as_secs` or `as_nanos`.

## `no_std`

```console
$ cargo add openadr --no-default-features
```

Verified in CI for `thumbv7em-none-eabihf` (Cortex-M4F) and `wasm32-unknown-unknown`. The
specification's own security chapter has the appliance-class VEN in mind, and no other
implementation targets it.

**`no_std` here means no operating system, not no heap.** The wire model is `String`s and `Vec`s —
there is no version of OpenADR that works without an allocator — so the crate always does
`extern crate alloc`. There is no `alloc` feature, because it would have had one state and one build
failure.

What that build gives you: parse and render every object, expand an event's intervals into absolute
windows, resolve concurrent events into a timeline, compute report schedules, and evaluate the
payload enumerations. What it does not: any HTTP, and `SystemClock` — supply the time yourself.

```rust
use openadr::core::{Clock, IntervalExpander};
use openadr::model::Timestamp;

#[derive(Debug)]
struct RtcClock;

impl Clock for RtcClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_second(rtc_seconds()).unwrap()
    }
}

let timeline = Timeline::build(events, &IntervalExpander::new(&RtcClock), from, to);
```

Bare-metal builds are also a *guard* rather than only a product feature: they are the cheapest way to
notice that somebody reached for `std::time::SystemTime` in the domain core.

## Embedding the VTN

`Vtn::router()` returns an ordinary `axum::Router`, so the server mounts inside a larger service:

```rust
let vtn = Vtn::builder()
    .storage(PostgresStorage::shared(&url).await?)
    .authenticator(Arc::new(auth))
    .build();

let app = axum::Router::new()
    .nest("/openadr", vtn.router())
    .merge(my_own_routes());

// `Vtn::serve` spawns this. If you run the router yourself, you must too —
// otherwise nothing is ever delivered.
vtn.dispatcher().clone().spawn();
```

Calling a `Storage` method directly rather than through the API means supplying a
[`Fanout`](@/docs/notifications.md) — the snapshot of who should be told:

```rust
storage.create_program(request, now, &Fanout::none()).await?;
```

`Fanout::none()` is the whole ceremony for an embedder that does not want notifications. Every
mutating method takes one because the notification has to be queued in the *same transaction* as the
change, and a backend cannot work out the recipients from inside its own write.

## Deterministic tests

Time and randomness are injected, so a schedule can be replayed exactly:

```rust
let vtn = Vtn::builder()
    .storage(MemoryStorage::shared())
    .clock(Arc::new(FixedClock::new("2026-02-11T06:00:00Z".parse()?)))
    .notifier(RecordingNotifier::shared())
    .build();

// Drain the outbox instead of sleeping: `drain` runs until nothing is due, so
// retry counts are exact rather than timing-dependent.
vtn.dispatcher().drain().await;
assert_eq!(notifier.delivered().len(), 1);
```

## Versioning

Pre-1.0. Breaking changes are possible on any release, the SQL schema is authoritative rather than
historical, and there are no migrations — a schema change is a reason to recreate the database.

The crate tracks OpenADR **3.1.0**, accepting the resource request bodies of both 3.1.0 and the
3.1.1 development copy. There is no 3.0 compatibility: 3.1 is not backwards compatible, and carrying
both would mean two shapes for `targets`, two locations for `/resources`, and flat programme fields
alongside `attributes` — permanently, in the middle of the model. Older peers are served by adapters
at the edge instead.
