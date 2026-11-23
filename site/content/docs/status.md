+++
title = "Status"
description = "What is built and tested in openadr, what it does not do, and where the specification is ambiguous."
weight = 110
+++

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
| Command line — `openadr vtn`, `dev`, `trigger`, `explain`, and `get`/`post`/`put`/`delete`/`watch` | `vtn`, `client` | Complete |
| Inspector — timeline, who sees what, deliveries; `/_inspect` | `vtn` | Complete |
| Observability — `GET /metrics`, dead-letter and subscriber-health views, retry | `vtn` | Complete |
| Transport security — TLS 1.2+ with ALPN, and a client CA that gates the connection | `tls` | Complete |
| OpenAPI — `GET /openapi.json`, narrowed to what this deployment serves, named by the mDNS record | `vtn` | Complete |
| Conformance kit — black-box, 52 clause-citing checks against any VTN, 51 proven able to fail; HTML report, badge, GitHub Action | `conformance` | Complete; weekly against another implementation |
| External auth — end-to-end against a real Keycloak: realm, client-credentials grant, token, write | `external-auth` | Complete |

CI exercises the HTTP API, VEN runtime, TLS providers, storage backends, MQTT broker and Keycloak.
Run the load and external interoperability measurements with the instructions in
[CONTRIBUTING.md](https://github.com/hupe1980/openadr/blob/main/CONTRIBUTING.md).

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

**Speak MQTT 5.** The binding says brokers and clients **SHOULD** support it; this crate's MQTT
client speaks 3.1.1, which is the version that is required.

## Known limitations

**Interoperability is measured against one peer.** [The conformance suite](@/docs/conformance.md)
runs weekly against one peer’s latest image, resolved by digest for the run. Each report records the image digest and measurement time.

**Nothing is certified.** Certification uses a closed online test tool. Several behaviours it checks
are unspecified in the public documents, so this implementation makes documented guesses — list
ordering, whether a no-op `PUT` bumps `modificationDateTime`, the exact `problem.title` strings.
Each is a single, isolated change if a test says otherwise. Nothing here should be called
"certified" before a run.

**Delivery is at least once.** A dispatcher can deliver and then die before recording that it did;
the entry is redelivered when its lease expires. Every notification carries the object's identity and
`modificationDateTime`, and `X-OpenADR-Attempt` names the try, so a receiver can be idempotent.
Exactly-once would need a two-phase commit with the receiver, which the protocol does not offer.

**Fan-out is proportional to its recipients.** The network is off the request path, but computing
who is entitled is not: a write takes a snapshot, narrowed by the object type being written, and
inserts one row per recipient. Per-VEN topics mean a private copy per entitled VEN, so an
*untargeted* event on a VTN with ten thousand VENs is ten thousand rows by construction — that is
what keeps object privacy intact on a shared broker, and the cost of it is the number of them.

Those publishes overlap: the client serialises only the microseconds between queueing a packet and
writing it, not the broker round trip, so the bound is `DispatchConfig::concurrency` and the broker.
Running several dispatcher processes multiplies it, which the outbox's leases make safe.

On PostgreSQL, `POST /events` costs roughly 43 ms p50 with a thousand matching subscriptions and
72 ms with a thousand entitled VENs on the broker path — linear in recipients, and inside what a
pilot needs. Measure it yourself with
`cargo test --all-features --test load -- --ignored --nocapture`; treat published figures as shapes
rather than a capacity plan. Ten thousand VENs, and server-class hardware, are not measured.

**No 3.0 compatibility.** 3.1 is not backwards compatible with 3.0, and carrying both would mean two
shapes for `targets`, two locations for `/resources`, and flat programme fields alongside
`attributes` — permanently, in the middle of the model. Dutch GAC 2.0 deployments run on 3.0 and are
not served.

**Pre-1.0 API and schema.** Breaking changes are possible on any release. The SQL schema is
authoritative rather than historical and there are no migrations.

## Where the specification is ambiguous

Roughly a dozen places, each of which this implementation resolves one way and documents. The ones
most likely to matter to an integrator:

- `reportDescriptor.startInterval` means two different things in two §7.5 examples.
- The `ven` and `resource` response schemas specify two conflicting `objectType` values.
- 3.1.0 and 3.1.1 disagree about which fields the resource request bodies carry.
- The specification is silent on single-object reads by a client that was granted nothing.
- `?targets=` is a privacy gate on programmes and events, and only a filter elsewhere.

All of them, with the reasoning, are in [Reading the specification](@/docs/spec-notes.md).
