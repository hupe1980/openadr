# openadr

**OpenADR 3.1 in Rust** — a VTN server, a VEN runtime, a typed client and an open conformance
suite, in one crate sliced with Cargo features and one binary.

[OpenADR](https://www.openadr.org/) is how utilities and grid operators ask flexible load to move:
dynamic prices, capacity limits, curtailment. A **VTN** publishes programmes and events; a **VEN** —
a charge point, a battery, a heat pump — reads them, acts, and reports back.

**[Documentation](https://hupe1980.github.io/openadr)** ·
[Getting started](https://hupe1980.github.io/openadr/docs/getting-started/) ·
[What OpenADR is](https://hupe1980.github.io/openadr/docs/what-is-openadr/) ·
[API reference](https://docs.rs/openadr)

## Quickstart

```console
$ brew install hupe1980/tap/openadr
$ openadr dev
  VTN        http://127.0.0.1:3000/openadr3/3.1.0   (in memory — gone when you stop it)
  Inspector  http://127.0.0.1:3000/_inspect#…
  Clients    bl-1 (business logic) · ven-1 (VEN)
  Seeded     dynamic-price   24 h price curve, rolls forward every 15 minutes
             capacity-limit  an event in 2 minutes, and one for feeder-7 that ven-1 cannot see
```

In another terminal:

```console
$ source .openadr-dev.env
$ openadr get events
$ openadr watch events                      # prints only when something changes
$ openadr trigger event                     # a capacity limit one minute out; also cancel, report
$ openadr conformance --report report.html  # 52 checks, as a page you can send
$ cargo run --features ven --example ven    # a VEN reading the curve (from a checkout)
```

The inspector shows the timeline against *now*, which events the VEN can and cannot see — asked of
the VTN with the VEN's own credential — and every delivery. Errors end with the `openadr explain`
command that describes them. CI holds this quickstart to under 60 seconds.

## Why this one

- **Object privacy holds on every path.** One `Access` value decides visibility for REST reads,
  webhook delivery and MQTT topic routing, inside the SQL query. On a broker the VTN answers the
  authorization callback, so one VEN cannot subscribe to another's topic.
- **A dispatch instruction cannot be lost.** Notifications are written to an outbox in the same
  transaction as the object, delivered by a leased dispatcher, with dead letters you can query and
  retry, and a circuit breaker for subscribers that stop answering.
- **Interoperability is measured.** `openadr conformance` runs 52 black-box checks against any VTN,
  each citing the sentence it tests; 51 are proven able to fail against deliberately broken VTNs. It
  writes an HTML report and a badge, runs in your CI as a GitHub Action, and runs weekly here
  against another implementation's current release.
- **Exact values.** Prices are `Decimal` (`0.1 + 0.2 == 0.3`); `P9999Y` and `0001-01-01` are enum
  variants, not strings; a looping tariff is indexed, not cloned.
- **Polling is the normal case**, with `ETag` on every read. Push over webhooks and MQTT wakes the
  VEN loop, which re-reads from the VTN: a broker can cost a VEN latency, never correctness.
- **Microcontroller to cloud.** The wire model, payload schema and domain core are `no_std` + alloc
  (`thumbv7em-none-eabihf`, `wasm32`). One binary on SQLite, or many on PostgreSQL. No system C
  library to install; `cargo deny` passes, with two documented exceptions in test and build tooling
  and none in the crate.

## Install

```console
$ brew install hupe1980/tap/openadr                  # the binary (macOS arm64, Linux)
$ docker run --rm -p 127.0.0.1:3000:3000 ghcr.io/hupe1980/openadr dev --listen 0.0.0.0:3000 --ephemeral
$ cargo install openadr --features vtn,client,internal-auth,sqlite,conformance

$ cargo add openadr                                  # model + schema + core, no_std-capable
$ cargo add openadr --features vtn,postgres          # the server
$ cargo add openadr --features ven                   # the VEN loop, on top of the client
$ cargo add openadr --features conformance           # measure any VTN
```

Features: `std` (default) · `client` · `ven` · `conformance` · `vtn` · `sqlite` · `postgres` ·
`internal-auth` · `external-auth` · `webhook` · `webhook-signature` · `mqtt` · `mdns` · `tls`. CI
builds every combination. See [Embedding and `no_std`](https://hupe1980.github.io/openadr/docs/embedding/).

## A durable VTN

```console
$ openadr vtn --database ./openadr.sqlite \
      --client-hashed "bl-1:$(openadr hash-secret "$BL_SECRET"):bl"
```

Every endpoint, an OAuth2 client-credentials grant with Argon2id-hashed secrets, object privacy, a
notification outbox, `GET /metrics` and `GET /openapi.json`. A `postgres://` URL makes it the
multi-instance deployment; `--tls-cert`/`--tls-key` serve HTTPS directly. See
[Running a VTN](https://hupe1980.github.io/openadr/docs/vtn/).

## A VEN

```rust,ignore
let ven = VenRuntime::with_meter(client, config, HouseMeter);
ven.register().await?;                     // idempotent, and checks the VTN's clock

loop {
    ven.sync().await?;                     // conditional: a quiet cycle is a 304
    if let Some(segment) = ven.active_at(ven.now()) {
        act_on(&segment.payloads);         // priority already resolved
    }
    ven.submit_due_reports().await?;       // each window filed exactly once
    ven.wait_for_work().await;             // or a push notification, whichever comes first
}
```

See [the VEN runtime](https://hupe1980.github.io/openadr/docs/ven-runtime/) and
[the domain core](https://hupe1980.github.io/openadr/docs/domain-core/).

## Layout

| Module | Feature | Contents |
|---|---|---|
| `model` | always (`no_std`) | wire types, validated newtypes, spec sentinels, profile adapters |
| `schema` | always (`no_std`) | the Alliance's six enumeration files, generated |
| `core` | always (`no_std`) | interval expansion, timelines, object privacy, report scheduling |
| `webhook` | `webhook-signature` (`no_std`) | the webhook signature scheme — signing and verifying |
| `discovery` | always (socket behind `mdns`) | finding a local VTN over mDNS/DNS-SD |
| `client` | `client` | BL and VEN HTTP clients, role-checked at compile time |
| `ven` | `ven` | the VEN loop: registration, conditional sync, timeline, reports |
| `mqtt` | `mqtt` | this crate's MQTT 3.1.1 client |
| `conformance` | `conformance` | the black-box suite, its HTML report and badge |
| `vtn` | `vtn` | axum server: storage, auth, handlers, outbox, caching, metrics, inspector |
| `vtn::store` | `sqlite`, `postgres` | in-memory, SQLite and PostgreSQL behind one tested trait |

## Status

Pre-1.0: breaking changes are named in [CHANGELOG.md](CHANGELOG.md). Working and tested against
real servers — a real JWKS and Keycloak, a routing MQTT broker, PostgreSQL, TLS handshakes,
multicast. Every `MUST` and `SHALL` in the specification's prose is named by a test or a
conformance check (34 of 34, a ratchet CI enforces). Not done: certification against the Alliance
test tool, MQTT 5, mapping a client certificate to a `clientID`.
[The full picture](https://hupe1980.github.io/openadr/docs/status/).

```console
$ cargo test --all-features
$ cargo xtask spec-sync && cargo xtask check-model        # the wire model against the pinned spec
$ cargo test --all-features --test interop -- --ignored   # the suite against another VTN
```

## Specification

The wire model tracks OpenADR **3.1.0**, accepting 3.1.1's clarified resource bodies. Peers that
bend the schema get explicit, tested adapters (`ClientBuilder::adapter`) rather than looser types.
Every ambiguity and departure is in
[Reading the specification](https://hupe1980.github.io/openadr/docs/spec-notes/).

The specification is published by the [OpenADR Alliance](https://www.openadr.org/specification),
free with registration. Its **OpenAPI document** is Apache-2.0, and the VTN serves a transcoded copy
with attribution; the Definition and User Guide are the Alliance's copyright and are not
redistributed. `cargo xtask spec-sync` fetches the
[public mirror](https://github.com/grid-coordination/openadr3-specification) at the commit
`spec.lock` pins. This project is independent of the Alliance.

## Contributing

The most useful thing to send is a disagreement found by running `openadr conformance` against a
real VTN — see [CONTRIBUTING.md](CONTRIBUTING.md). Vulnerabilities: [SECURITY.md](SECURITY.md).

## Licence

Apache-2.0 OR MIT, at your option.
