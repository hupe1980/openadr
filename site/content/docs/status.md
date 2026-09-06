+++
title = "Status"
description = "What is built and tested in openadr, what it does not do, and where the specification is ambiguous."
weight = 110
+++

Pre-1.0, and honest about it. This page says what works and what does not, so a limitation is not
mistaken for a bug.

## Built and tested

| | Cargo feature | |
|---|---|---|
| Wire model — six objects, request/response split, sentinels, adapters | always on | Complete |
| Payload schema — all six Alliance enumeration files, generated: 75 payload types, units, reading types | always on | Complete |
| Domain core — interval expansion, timelines, object privacy, report schedules | always on | Complete |
| VTN — every endpoint, scopes, object privacy, `problem` bodies, `ETag` | `vtn` | Complete |
| Storage — in-memory, SQLite, PostgreSQL | `sqlite`, `postgres` | Complete |
| Authentication — own token endpoint, JWKS, pre-shared, anonymous | `internal-auth`, `external-auth` | Complete |
| Notification outbox — transactional, leased, concurrent, capped backoff, a circuit breaker for dead subscribers | `vtn` | Complete |
| Report retention — bounded, oldest-first, off by default | `vtn` | Complete |
| Webhook delivery — echo challenge, SSRF guard, HMAC, no redirects | `webhook` | Complete |
| Client — role as a type parameter, conditional reads, pagination, adapters | `client` | Complete |
| MQTT — discovery, topic endpoints, per-VEN fan-out, QoS 1 publisher, broker ACL callbacks, VEN-side subscriber | `mqtt` | Complete |
| Deployment — EMQX and Mosquitto configurations, a compose file, an assertion script | — | Complete |
| Local discovery — `_openadr3._tcp` advertised by the VTN, browsed by `openadr discover` | `mdns` | Complete |
| VEN runtime — registration, conditional sync, timelines, reports, clock-skew guard, push hints | `ven` | Complete |
| Command line — `openadr vtn`, and `get`/`post`/`put`/`delete`/`watch` | `vtn`, `client` | Complete |
| Observability — `GET /metrics`, dead-letter and subscriber-health views, retry | `vtn` | Complete |
| Transport security — TLS 1.2+ with ALPN, and a client CA that gates the connection | `tls` | Complete |
| OpenAPI — `GET /openapi.json`, narrowed to what this deployment serves, named by the mDNS record | `vtn` | Complete |
| Conformance kit — black-box, 48 clause-citing checks, runs against any VTN; 47 of them proven able to fail | `conformance` | Built; one reading against another implementation |
| External auth — end-to-end against a real Keycloak: realm, client-credentials grant, token, write | `external-auth` | Complete |

**481 tests**: 289 unit, 71 driving the real HTTP router, 17 property tests, 14 running the VEN
runtime against a real VTN, 13 running the client against one, 12 over the code generator and the
drift checks in `xtask`, 11 delivering webhooks over real sockets, 11 in the CLI, 11 doctests, 10
validating JWTs against a real JWKS, 8 running the conformance suite, 6 publishing to a broker over a
real socket, 6 completing real TLS handshakes, one against a real Keycloak end to end, and one mDNS
round trip over a real multicast group. Three of the unit tests are the storage conformance suite,
which is 57 behaviours run against each backend — PostgreSQL included, which starts its own container
when no server is configured rather than skipping.

Three more are `#[ignore]`d, because they measure rather than assert: `load` (write latency, broker
fan-out, drain rate, report ingest with and without a broker behind it), `interop` (this suite
against another VTN) and `broker` (object privacy across a real EMQX, from `deploy/compose.yaml`).

`cargo test` with nothing configured starts its own PostgreSQL and Keycloak containers; both skip,
loudly, on a machine with no Docker.

CI holds: `cargo fmt --check`; clippy with `-D warnings`; rustdoc with `-D warnings`;
`thumbv7em-none-eabihf` and `wasm32-unknown-unknown` builds (and the webhook signature verifier on
bare metal); every feature combination via `cargo hack`; the payload-table drift check *and* the
wire-model check *and* the OpenAPI copy against `openadr3.yaml`; `cargo deny` over advisories,
licences, banned crates and registries, with a CycloneDX SBOM published per build. The PostgreSQL
suite runs against a real server in CI, which is what stops "skips locally" from becoming "is never
run".

Three further guards check claims this project makes about *itself*: that every `problem.type` URI it
mints resolves, that the storage suite calls every method of the storage trait and runs every
behaviour written for it, and the traceability below.

## Traceability

`cargo xtask trace` extracts every `MUST`, `MUST NOT`, `SHALL` and `SHALL NOT` from the Definitions
and the notifier binding document and matches each against the citations in the source. `SHOULD` and
`MAY` are excluded — counting recommendations would make the number an opinion.

Each citation is graded by where it is written: a `clause` in the conformance suite is a **check**,
one inside a test is a **test**, and anything else is a **mention** — a claim by whoever wrote the
comment.

**34 of 34 are named by a check or a test**, nothing exempted, and the number is a ratchet: a release
that lets it fall fails the build.

The unit is the section, so a section carrying three requirements is credited by a test covering one
of them. Naming the sentence is the half still open.

## What it does not do

**Issue or renew certificates.** `--tls-cert`/`--tls-key` serve TLS 1.2+ directly and
`--tls-client-ca` gates the connection, but there is no ACME, no rate limiting, and no mapping from a
client certificate's subject to a `clientID` — the connection gate exists, the identity half is
deliberately yours and needs an X.509 parser this crate does not ship.

**Export traces.** `GET /metrics` serves request rate and latency by matched route, delivery
outcomes by channel, and the outbox gauges. There is no OTLP export and no trace context propagated
into webhook calls or MQTT user properties, which is what would make the request id useful *across*
services rather than within one.

**Receive webhooks on the VEN side.** The MQTT subscriber wakes the runtime's loop and never acts on
a payload; there is no inbound HTTP endpoint, because a VEN that could accept one could have been
reached without a broker in the first place. `Waker` is public, so wiring your own handler to it is
three lines.

**Speak MQTT 5.** The binding says brokers and clients **SHOULD** support it; the publisher speaks
3.1.1, which is the version that is required.

## Known limitations

**Interoperability has one reading.** [The conformance suite](@/docs/conformance.md) has been run
against another implementation once, and `tests/interop.rs` reproduces it: 30 of 36 attempted
required checks passed there, six failed, and the run found two defects here as well.
That is a data point, not a measurement — nothing runs it on a schedule or against more peers, and
the checks are still written from this project's reading of the specification.

**Nothing is certified.** Certification uses a closed online test tool. Several behaviours it checks
are unspecified in the public documents, so this implementation makes documented guesses — list
ordering, whether a no-op `PUT` bumps `modificationDateTime`, the exact `problem.title` strings.
Each is a single, isolated change if a test says otherwise. Nothing here should be called
"certified" before a run.

**Delivery is at least once.** A dispatcher can deliver and then die before recording that it did;
the entry is redelivered when its lease expires. Every notification carries the object's identity and
`modificationDateTime`, and `X-OpenADR-Attempt` names the try, so a receiver can be idempotent.
Exactly-once would need a two-phase commit with the receiver, which the protocol does not offer.

**Notification fan-out is proportional to its recipients, and the broker fan-out is proportional to
the VENs.** The network is off the request path, but computing who is entitled is not: a write takes
a snapshot and inserts one row per recipient. The snapshot is narrowed by the object type being
written, so the database does the narrowing rather than the VTN loading everything and discarding
most of it — but per-VEN topics mean a private copy per entitled VEN, so an *untargeted* event on a
VTN with ten thousand VENs is ten thousand rows by construction.

Deferring the fan-out to the dispatcher would make writes O(1) and is the wrong trade twice over:
recipients must be decided against the subscriptions that existed when the change happened, and a
delivery computed after the transaction cannot be inside it.

On PostgreSQL, `POST /events` costs roughly 43 ms p50 with a thousand matching subscriptions, and
72 ms with a thousand entitled VENs on the broker path — linear in recipients, and inside what a
pilot needs. Measure it yourself with
`cargo test --all-features --test load -- --ignored --nocapture`; treat published figures as shapes
rather than a capacity plan. Ten thousand VENs, and server-class hardware, are not measured.

**Broker fan-out is a copy per entitled VEN.** Per-VEN topics are what keep object privacy intact on
a shared broker, and the cost of them is the number of them: an event a thousand VENs are entitled to
is a thousand publishes. Those publishes overlap — the publisher serialises only the microseconds
between queueing a packet and writing it, not the broker round trip — so the bound is
`DispatchConfig::concurrency` and the broker, not one round trip at a time. Running several
dispatcher processes multiplies it, which the outbox's leases already make safe;
`MqttConfig::qos = AtMostOnce` removes the acknowledgement and its guarantee together.

**Subscription bearer tokens are stored as given.** Encryption at rest for the secrets a VTN holds on
behalf of clients — subscription `bearerToken`s and webhook signing keys — is not implemented. That
is acceptable for the in-memory backend and is not acceptable for a durable one.

**No 3.0 compatibility.** 3.1 is not backwards compatible with 3.0, and carrying both would mean two
shapes for `targets`, two locations for `/resources`, and flat programme fields alongside
`attributes` — permanently, in the middle of the model. Dutch GAC 2.0 deployments run on 3.0; an
adapter at the edge is the intended answer and is not written.

**Pre-1.0 API and schema.** Breaking changes are possible on any release. The SQL schema is
authoritative rather than historical and there are no migrations.

**Five advisories stand against one transitive dependency.** `rumqttc 0.25.1`, its newest release,
pins `rustls-webpki 0.102` — the fixes are in 0.103 — and the archived `rustls-pemfile`. `deny.toml`
records each with the reason it is not reachable here: two need a misissued certificate, two need a
certificate revocation list and this crate supplies none, and nothing here calls `rustls-pemfile` at
all. Each names the version that ends the exception.

## Where the specification is ambiguous

Roughly a dozen places, each of which this implementation resolves one way and documents. The ones
most likely to matter to an integrator:

- `reportDescriptor.startInterval` means two different things in two §7.5 examples.
- The `ven` and `resource` response schemas specify two conflicting `objectType` values.
- 3.1.0 and 3.1.1 disagree about which fields the resource request bodies carry.
- The specification is silent on single-object reads by a client that was granted nothing.
- `?targets=` is a privacy gate on programmes and events, and only a filter elsewhere.

All of them, with the reasoning, are in [Reading the specification](@/docs/spec-notes.md).

## Choosing this one

Other OpenADR 3 implementations exist, some of them production-proven at scale and certified, and
one of those is the right answer if what you need is something already deployed. This page is about
what is here, not about them.

What is unusual about this implementation, and worth weighing:

- **Webhook and MQTT delivery, with object privacy on both.** Per-VEN topic copies with target
  hiding, and the broker authorization callback the specification requires and leaves to the
  implementer.
- **A transactional outbox.** A notification is written in the same transaction as the object that
  caused it, so there is no window in which an event exists and nobody has been told.
- **A VEN runtime**, not only a client: registration, conditional sync, timelines, report
  scheduling, a clock-skew guard.
- **A black-box conformance kit** that runs against *any* VTN, including yours.
- **Decimals, generated payload typing, and `no_std`** for the model, schema and core.
- **No C library to install**, for any feature: no OpenSSL, no Avahi, nothing `pkg-config` has to
  find. What C there is — `ring`, and SQLite under the `sqlite` feature — is vendored source cargo
  builds, which is what makes cross-compilation ordinary.

And what is not here: certification, and the deployment history that comes with it.
[What it does not do](#what-it-does-not-do) above is the rest of the answer.

## Contributing

The repository is at [github.com/hupe1980/openadr](https://github.com/hupe1980/openadr), and
`CONTRIBUTING.md` says what a change should come with. The most valuable thing anyone can send is a
correctness report about object privacy or the notification outbox, because both fail silently.
