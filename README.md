# openadr

**OpenADR 3.1 in Rust** — a VTN server, a VEN runtime and a typed client, in one crate sliced with
Cargo features.

[OpenADR](https://www.openadr.org/) is the standard utilities and grid operators use to ask flexible
load to move: dynamic prices, capacity limits, curtailment. A **VTN** publishes programmes and
events; a **VEN** — a charge point, a battery, a heat pump — reads them, acts, and reports back.
Version 3 replaced 2.0b's SOAP with REST and JSON; 3.1 added push over MQTT, object privacy and
string targets. This crate implements 3.1, and ships both ends of it.

**[Documentation](https://hupe1980.github.io/openadr)** ·
[Getting started](https://hupe1980.github.io/openadr/docs/getting-started/) ·
[What OpenADR is](https://hupe1980.github.io/openadr/docs/what-is-openadr/) ·
[API reference](https://docs.rs/openadr)

```rust,ignore
use openadr::prelude::*;

// A day-ahead price curve, resolved to absolute time.
let expander = IntervalExpander::at("2026-02-11T06:00:00Z".parse()?);
let intervals = expander.expand(&event)?;

assert_eq!(intervals[0].start, "2026-02-11T00:00:00Z".parse()?);
assert_eq!(intervals[0].end, Some("2026-02-11T01:00:00Z".parse()?));
```

---

## Why this one

Eight things, each answering a gap that is real in the field rather than theoretical.

**Object privacy holds on every path.** A VEN must never learn which other groups an event targets —
that is a competitor's dispatch schedule. One `Access` value decides visibility for REST reads,
webhook delivery *and* MQTT topic routing, and renders itself into the `WHERE` clause so filtering
happens inside the query rather than over a page already cut. On a broker the VTN also answers the
authorization callback, so one VEN cannot subscribe to another's topic — required by the
specification and left entirely to the implementer.

**A dispatch instruction cannot be lost.** Notifications are written to a transactional outbox in
the *same transaction* as the object that caused them, then delivered by a leased dispatcher. There
is no window in which an event exists and nobody has been told, and nothing in OpenADR can say
afterwards that a notification was never sent. Attempts, backoff and abandonment are rows an
operator can query and retry — and a subscriber that has stopped answering is **cut off** rather
than retried for ever, because the retry policy bounds what one notification costs and nothing else
bounds what one dead endpoint costs.

**Interoperability is measured.** `openadr conformance` runs 48 black-box checks against *any*
OpenADR 3.1 VTN, each citing the sentence it tests, with a skip that is never counted as a pass.
Run against another implementation it found six required divergences there — and two defects here.

**Money is decimal.** `Value::Number` holds a `rust_decimal::Decimal`, not an `f64`, so
`0.1 + 0.2 == 0.3` holds exactly and a tariff round-trips through JSON unchanged.

**Sentinels are types.** `P9999Y` is `Duration::Forever` and `0001-01-01` is `StartTime::Now` — not
strings one forgotten comparison away from a schedule that runs for nine thousand years.

**The intervals an event does not list still exist.** A tariff that loops for ever and a capability
forecast that declares no intervals at all are both ordinary indexing into an `IntervalSequence`:
interval fifty thousand of an hourly tariff running since 2020 costs two multiplications, not fifty
thousand clones. It is what makes `repeat: -1` and `numIntervals: 48` mean what the User Guide's
worked scenarios say they mean.

**Polling is the normal case**, because in every deployment surveyed it is. Every `GET` carries an
`ETag`, and both the client and the VEN runtime use it: a quiet cycle costs a `304` with no body.
Push is here too, on both ends — and on the VEN side a notification is a *hint*: it wakes the loop,
the loop re-reads from the VTN, and the payload is never deserialised. A broker can cost a VEN
latency and cannot cost it correctness.

**It runs where the load is.** The wire model, payload schema and domain core are `no_std` + alloc
and build for `thumbv7em-none-eabihf` and WebAssembly. No C *library* to install for any feature: no
OpenSSL to find, no Avahi, nothing for `pkg-config` to locate.

## Install

```console
$ cargo add openadr                             # model + schema + core, no_std-capable
$ cargo add openadr --features vtn,postgres     # the server
$ cargo add openadr --features ven              # the VEN loop, on top of the client
$ cargo add openadr --features conformance      # measure any VTN
```

Features: `std` (default) · `client` · `ven` · `conformance` · `vtn` · `sqlite` · `postgres` ·
`internal-auth` · `external-auth` · `webhook` · `webhook-signature` · `mqtt` · `mdns` · `tls`. Every one is
read by a `cfg`, and CI builds each combination. Two imply no role, because both ends of the thing they
describe are here: `mqtt` is the VTN's publisher *and* the VEN's subscriber, and
`webhook-signature` is the receiver's half of the signature scheme on its own — `no_std`-capable,
with no HTTP stack and no server. See [Embedding and `no_std`](https://hupe1980.github.io/openadr/docs/embedding/).

## Quickstart

```console
$ cargo install openadr --features vtn,client,internal-auth,sqlite

$ openadr vtn --database ./openadr.sqlite \
      --client-hashed "bl-1:$(openadr hash-secret "$BL_SECRET"):bl"
2026-02-11T09:00:00Z  INFO openadr::vtn: VTN listening addr=0.0.0.0:3000 base_path=/openadr3/3.1.0
```

Add `--tls-cert`/`--tls-key` and it serves HTTPS itself; add `--tls-client-ca` and a peer whose
certificate that CA did not issue is refused during the handshake. `GET /openapi.json` describes
whatever the running VTN actually offers.

That is a complete VTN: every endpoint, an OAuth2 client-credentials grant with Argon2id-hashed
secrets, object privacy, durable storage, a notification queue, `GET /metrics` and a dead-letter
view. Swap `--database` for a `postgres://` URL and the same binary is the multi-instance
deployment.

The same binary is the client, so nothing above needs a second tool:

```console
$ export OPENADR_URL=http://localhost:3000/openadr3/3.1.0 OPENADR_TOKEN=$TOKEN

$ openadr post programs --data '{"programName":"grid-aware-charging"}'
$ openadr get events --program prg-00000001 --active
$ openadr watch events --targets group1        # prints only when something changes
$ openadr conformance --ven-token $VEN --ven-client-id ven-1
```

And a VEN that acts on what it reads:

```rust,ignore
let ven = VenRuntime::with_meter(client, config, HouseMeter);
ven.register().await?;                     // idempotent, and checks the VTN's clock

loop {
    ven.sync().await?;                     // conditional: a quiet cycle is a 304
    if let Some(segment) = ven.active_at(ven.now()) {
        act_on(&segment.payloads);         // priority already resolved
    }
    ven.submit_due_reports().await?;       // each window filed exactly once
    ven.wait_for_work().await;   // or a push notification, whichever comes first
}
```

Authentication, storage, push transports and the operational endpoints each have a page in the
**[documentation](https://hupe1980.github.io/openadr/docs/)**; `openadr vtn --help`,
`openadr get --help` and `openadr conformance --help` list the flags.

## Layout

| Module | Feature | Contents |
|---|---|---|
| `model` | always (`no_std`) | wire types, validated newtypes, spec sentinels, profile adapters |
| `schema` | always (`no_std`) | all six of the Alliance's enumeration files, generated: 75 payload types across four groups, plus units and reading types |
| `core` | always (`no_std`) | interval expansion, timelines, object privacy, report scheduling |
| `webhook` | `webhook-signature` (`no_std`) | the webhook signature scheme — signing *and* verifying |
| `discovery` | always (socket behind `mdns`) | finding a local VTN over mDNS/DNS-SD — advertising *and* browsing |
| `client` | `client` | BL and VEN HTTP clients, role-checked at compile time |
| `ven` | `ven` | the VEN loop: registration, conditional sync, timeline, report scheduling |
| `ven::MqttPush` | `ven` + `mqtt` | subscribes to this VEN's topics and wakes the loop; never reads a payload |
| `conformance` | `conformance` | a black-box suite that measures any VTN against the specification |
| `vtn` | `vtn` | axum server: storage, auth, handlers, the notification outbox, caching, metrics |
| `vtn::auth` | `internal-auth`, `external-auth` | the VTN's own token endpoint; JWT validation against a JWKS |
| `vtn::notify` | `webhook`, `mqtt` | the two transports behind one channel-routed `Notifiers` |
| `vtn::store` | `sqlite`, `postgres` | in-memory, SQLite and PostgreSQL behind one conformance-tested trait |
| `vtn::retention` | `vtn` | ageing out `report`, the only object OpenADR grows without bound |
| `vtn::tls` | `tls` | TLS 1.2+ for the VTN's own listener, and a client CA that gates the connection |
| `vtn::openapi` | `vtn` | `GET /openapi.json`, narrowed to what this deployment actually serves |

`IntervalExpander` turns an event's declared intervals into absolute windows — inherited timing,
contiguity, the `now` sentinel, unbounded durations, `event.duration` looping and truncation, and
the rule that a scalar payload carrying several values subdivides its interval. `IntervalSequence`
carries that past the declared list, for a tariff that loops and for an event whose structure is
implied by its `intervalPeriod` alone. `Timeline` then merges concurrent events by priority,
splitting the loser *around* the winner:

```text
input   |------------------ day-ahead prices (priority 10) ------------------|
                   |-- curtailment (priority 0) --|

result  |--prices--|-------- curtailment ---------|-------- prices ----------|
```

`ReportSchedule` turns a `reportDescriptor` — four integers with `-1` sentinels, a boolean and an
enum — into a list of due times. See [the domain core](https://hupe1980.github.io/openadr/docs/domain-core/).

## Status

Pre-1.0. Working, and tested against a running server: the six objects with request and response
types kept distinct; object privacy end to end, including the broker authorization callback; three
authentication backends and OAuth2 scopes; `problem` bodies carrying the request id; three storage
backends held to one conformance suite; the transactional outbox with a dead-letter view; webhook
delivery over real sockets with the echo challenge, HMAC signatures and an SSRF guard in the
connector's own resolver; both ends of MQTT push over real sockets to a broker that routes; a VEN
runtime; mDNS discovery of a local VTN from both ends; and a black-box conformance suite.

Named rather than glossed: the conformance run against another implementation has happened **once,
by hand** — nothing runs it on a schedule or publishes a matrix. Nothing is certified against the
Alliance test tool. MQTT 5, ACME, mapping a client certificate to a `clientID`, and an inbound
webhook receiver on the VEN side are not written.
[The full picture](https://hupe1980.github.io/openadr/docs/status/).

**481 tests**, and the distribution is the point: most of the effort is at the seams. Seventy-one
drive the real HTTP router; the storage conformance suite is 57 behaviours run against all three
backends; and the rest run against real servers rather than mocks — a real JWKS, a real Keycloak, a
real MQTT broker that routes, real TLS handshakes, a real multicast group. Three further harnesses
are `#[ignore]`d because they are measurements rather than assertions: load, interoperability
against another VTN, and object privacy across a real broker.
[The breakdown](https://hupe1980.github.io/openadr/docs/status/).

```console
$ cargo test --all-features
$ cargo test --all-features --test load -- --ignored --nocapture
                             # write latency, broker fan-out, drain rate, report ingest
$ cargo xtask check-drift    # fails if the specification's enumerations changed
$ cargo xtask check-model    # fails if the wire model and openadr3.yaml disagree
$ cargo xtask check-paths    # drives the real router: fails if a declared endpoint is not
                             # routed, or demands scopes the document does not
$ cargo test --all-features --test interop -- --ignored --nocapture
                             # the conformance suite against another implementation

# object privacy through a real EMQX, from the shipped deploy/compose.yaml
$ cargo test --all-features --test broker -- --ignored --nocapture
```

`cargo test` starts what it needs: PostgreSQL for the storage suite, a real Keycloak for the OAuth2
exchange. Both skip, loudly, without Docker.

CI holds `cargo fmt --check`, clippy and rustdoc at `-D warnings`, every feature combination via
`cargo hack`, `thumbv7em-none-eabihf` and `wasm32-unknown-unknown` builds, four checks against
the specification — the payload enumerations, the wire model, the routed paths, and every `MUST` and
`SHALL` in the prose — `cargo deny` over advisories, licences, banned crates and registries with a
CycloneDX SBOM per build, and a conformance run against the freshly built binary.

**The instrument is measured too.** A conformance suite that cannot fail measures nothing, so the kit
is run against a table of deliberately broken VTNs, each fault naming exactly the checks that must
catch it: **47 of the 48 checks are proven able to fail**, and the one that is not is named with its
reason.

**Every requirement is traced to something that runs.** `cargo xtask trace` matches every `MUST` and
`SHALL` in the Definitions and the notifier binding document against the citations in the source, and
grades each by where it is written — a conformance check, a test, or a doc comment. **34 of 34 are
named by a check or a test**, nothing exempted, and the number is a ratchet CI enforces. Two sibling
checks hold the same shape: every `problem.type` URI resolves, and the storage suite calls every
method of the storage trait.

## Specification

The wire model tracks OpenADR **3.1.0**, with the clarifications 3.1.1 makes to the resource request
bodies accepted in both shapes. There is no 3.0 compatibility: 3.1 is not backwards compatible with
it, and carrying both would compromise the model. Peers that bend the schema — Fluvius' NetFlex
profile sends `reportDescriptor.frequency` as an ISO duration — are handled by explicit, tested
adapters (`ClientBuilder::adapter`) rather than by loosening the types.

Every ambiguity and deliberate departure is recorded in
[Reading the specification](https://hupe1980.github.io/openadr/docs/spec-notes/).

The specification is published by the [OpenADR Alliance](https://www.openadr.org/specification)
under Apache-2.0. `cargo xtask spec-sync` fetches the
[public mirror](https://github.com/grid-coordination/openadr3-specification) into `specs/`, which is
not vendored here. This project is independent, and is not affiliated with or endorsed by the
Alliance.

## Contributing

The most useful thing anyone can send is a disagreement found by running `openadr conformance`
against a real VTN — see [CONTRIBUTING.md](CONTRIBUTING.md). Vulnerabilities go through
[SECURITY.md](SECURITY.md), privately. What changed between releases is in
[CHANGELOG.md](CHANGELOG.md).

## Licence

Apache-2.0 OR MIT, at your option.
