+++
title = "Reading the specification"
description = "Every place the OpenADR 3.1 specification is ambiguous or self-contradictory, what this implementation chose, and why — plus its optional additions."
weight = 120
+++

Every place where the OpenADR 3.1 specification is ambiguous, self-contradictory, or where this
implementation deliberately departs from it. Each entry says what the source material does, what we
do, and why.

If you are integrating against a VTN built on this crate, this is the page that tells you where its
behaviour is a choice rather than a requirement.

Sources are the mirror in `specs/openadr3-specification/`: `openadr3.yaml` (the OpenAPI document,
normative), `Definition.md` and `User_Guide.md`. The wire model tracks **3.1.0**, the released
version; where the 3.1.1 development copy corrects 3.1.0 rather than changing it, that is noted
below and both shapes are accepted.

---

## Contradictions in the source

### `ven` and `resource` responses have two conflicting `objectType` values

`openadr3.yaml` composes the `ven` response as `allOf: [objectMetadata, BlVenRequest]`.
`objectMetadata.objectType` is the `objectTypes` enumeration, whose VEN member is `VEN`;
`BlVenRequest.objectType` is enumerated as `BL_VEN_REQUEST`. A response cannot be both. `resource`
has the same defect.

**We emit `VEN` and `RESOURCE`.** That is what the notification discriminator needs — a subscriber
receiving `{"objectType": "BL_VEN_REQUEST"}` could not match it against the `objectTypes` union — and
it is what every implementation in the field does.

### `reportDescriptor.startInterval` means two different things

User Guide §7.5 works the same field two ways.

*"Report on a subset of intervals"* (`startInterval = 1`, `numIntervals = 3`) shows a report covering
intervals 1–3, i.e. `startInterval` is the **first covered** interval.

*"Report on a subset of intervals at a regular period"* (`startInterval = 1`, `numIntervals = 2`,
`frequency = 2`, `repeat = 3`) shows report 1 covering intervals 0–1 and generated at the end of
interval 1 — i.e. `startInterval` is the **generation point**, with `numIntervals` counted backwards
from it because `historical` defaults to true. The rolling-forecast example agrees with this reading.

**We implement the generation-point reading**, which is the one that makes both fully-specified
examples work. `ScheduleOptions::start_interval_is_first_covered` selects the other reading for a
deployment that has settled on it.

### And `-1` points at a different end depending on direction

`startInterval = -1` is documented as "end of last interval". For a historical report that is the
last interval, and it is what we do. For a forecast — `historical = false`, where `numIntervals` runs
*forwards* — the last interval is the wrong end: §7.5's own *"Forecast reporting"* example is all
defaults but `historical = false`, and its diagram shows the report covering **all four** intervals
and generated at the beginning of the first.

**We read `-1` as the far end in the direction of travel**: the last interval when reporting
backwards, the first when reporting forwards. Read literally in both directions, that example would
produce a report covering the last interval only.

### When a forecast is generated

§7.5's rolling-report timeline draws "generate report N" at the *end* of the range each report
covers, in both directions. §8.7 and §8.8 — the two complete worked forecast scenarios — say the
opposite: *"startInterval = 0 indicates that a report should be generated when the first interval
has begun (see historical = False)"*. §7.5's own forecast diagram agrees with §8.7.

**We follow the prose.** A report is due at the boundary of the interval `startInterval` anchors on:
its **end** when reporting backwards, its **start** when reporting forwards. A forty-eight-hour
capability forecast delivered at the end of the forty-eight hours it forecasts is a historical
record, and §8.7 exists to describe a deployment that wanted a forecast.

### An event may imply intervals it does not list

An event with no `intervals` array, an `intervalPeriod` carrying a start and a duration, and one or
more `reportDescriptors` has an **implied** interval structure: one interval of that duration,
repeating from that start `[UG §7.3, §8.7, §8.8]`. The count is not in the event — a descriptor's
`numIntervals` supplies it — which is how a forty-eight-hour forecast is requested without
forty-eight empty intervals in the body.

**We resolve it.** Such an event is `active` from its `intervalPeriod.start` and, unless
`event.duration` says otherwise, never ends. Its intervals carry no payloads, so they place nothing
on a VEN's timeline — and neither do the explicit `payloads: []` intervals of the other report-only
form. Priority resolves conflicting *instructions*, and a report request is not one; letting a
`priority: 0` report request win its window against a real curtailment would silence the
curtailment.

The specification leaves the interval ids of an implied structure to the VEN, so a report against
one quotes the VEN's own.

### A report covering sub-intervals quotes the parent interval's id

3.1 added a compact form for serial data: a scalar payload carrying several values subdivides its
interval, so three prices in a `PT3H` interval are three hourly sub-intervals `[UG §7.3]`. Report
intervals quote the **event's** interval ids so the VTN can correlate them `[UG §7.5]` — and the
compact form gives sub-intervals no ids of their own.

**We quote the parent's id, once per sub-interval.** A report covering three sub-intervals of
interval `0` carries three intervals, all with `id: 0`, in time order. There is no other id to
quote: inventing one would be a number the event never mentioned, and collapsing the three into one
would lose the timing the report exists to carry. `ReportResource.intervalPeriod` and each interval's
own period say which sub-interval each reading belongs to.

If a program requires distinguishable ids, an event can simply declare the sub-intervals explicitly
instead of using the compact form; the two are otherwise equivalent.

### The resource request bodies changed between 3.1.0 and 3.1.1

3.1.0 requires `clientID` on `BL_RESOURCE_REQUEST` and `venID` on `VEN_RESOURCE_REQUEST`. 3.1.1
removes both, because `venID` already determines the client, and a VEN's own resources belong to
whichever VEN its token identifies — so the fields could only restate what the VTN already knows, or
contradict it.

**Both fields are optional here**, so a 3.1.0 peer's body parses unchanged. Ownership is derived
from `venID` and from the token regardless. A `VEN_RESOURCE_REQUEST` naming a *different* VEN is
refused rather than silently redirected.

The `resource` **response** is the one place we side with 3.1.1 outright: it does not carry
`clientID`. 3.1.0 composes the response from `BL_RESOURCE_REQUEST`, so a strict 3.1.0 reader expects
the field; carrying it would mean a denormalised column and a join on every read, for a field the
next release deletes and which `venID` already answers. `cargo xtask check-model` knows about this
one departure by name and would fail on any other.

### Versioning does not follow the semantic versioning it claims

The Definition's Revision chapter states that OpenADR follows semantic versioning. 3.1.0 is not
backwards compatible with 3.0.1 — endpoints, request bodies and query parameters all changed — and
the 3.1.0 changelog says so outright.

**We track 3.1 only.** Carrying 3.0 in the same model would mean two shapes for `targets`, two
locations for `/resources`, and flat programme fields alongside `attributes`. Older peers are served
by adapters at the edge (`model::adapt`), not by weakening the model.

---

## Deliberate departures

### Business logic may delete reports but not create them

The specification says only VENs write reports (`write_reports`: *"only VENs can write to
reports"*). Taken literally, a report becomes unreachable once its VEN's credentials are revoked:
nobody can delete it through the API.

**We let business logic read and delete reports, but not create or update them.** A VEN still writes
only its own. This is the smallest deviation that keeps the data manageable, and it is narrower than
the alternative some implementations chose (splitting `write_reports` into two non-standard scopes).

### One VEN object per `clientID`

The specification does not state a cardinality. But `VEN_VEN_REQUEST` carries no identity — the VTN
must infer the VEN from the token — so a second VEN object for the same client makes `PUT /vens/{id}`
and VEN-written resource creation ambiguous.

**We enforce one VEN per client**, returning `409` on the second. An aggregator representing many
sites uses one VEN with many resources, or one credential per VEN.

### A hidden object reads as `404`, not `403`

The specification says a read that does not match returns an empty set, and is silent about
single-object reads.

**We return `404`.** A `403` would confirm that the object exists, which is exactly what targeting
is meant to conceal. The same reasoning applies to VEN-scoped notifier topics: asking for another
VEN's topics gives `404`, so ids cannot be enumerated.

### Reading one object by id does not require naming targets

`GET /events/{eventID}` carries the `read_targets` scope, and the Definition's Object Privacy chapter
describes target matching in terms of "a VEN request to read a program or event object that has
targets" without distinguishing a list from a single fetch.

**We require the grant but not the query parameter.** The id *is* the request. The reader's grant is
still intersected with the object's targets, so guessing ids reveals nothing it was not entitled to,
and target hiding still applies to the response. Requiring the parameter as well would mean a VEN
that followed a link from a notification could not fetch the object the notification named.

### `GET /notifiers` is readable by any authenticated client

The OpenAPI document marks it `read_all`, which is business logic's scope. But `/notifiers` is how a
client learns the broker URI and how to authenticate to it, and 3.1's VEN-scoped topics exist for
VENs — so a VEN that cannot read `/notifiers` can never use the feature that was added for it.

**Any authenticated reader may call it.** It carries no object data. Every *topic* endpoint keeps
the document's scope exactly, including the programme-scoped ones, which stay business logic's
because the topics they name are collection-wide: subscribing to one yields every event of that
programme with its full target set. A VEN reads its own under `mqtt/topics/vens/{venID}/…`.

This is the **only** scope departure, and it is enforced as such: `cargo xtask check-paths` drives
the VTN's real router with a business-logic credential and then with a VEN's, compares both answers
against the document's `security` blocks, and fails on any difference it has not been told about by
name.

### A no-op `PUT` does not bump `modificationDateTime`

The specification does not say. Bumping it on every `PUT` would wake every subscriber whenever a
client re-sent an unchanged object, which is a common idempotent-write pattern.

**We bump it only when the content actually changed.** If a conformance suite requires the opposite,
this is one comparison in the storage layer.

### Error bodies are `application/problem+json`

`openadr3.yaml` declares every error response as `application/json` carrying the `problem` schema.
RFC 9457 §3 defines `application/problem+json` for exactly that body, and so do the Zalando API
guidelines the `problem` shape comes from.

**We send `application/problem+json`.** The body is byte-identical either way, so a client that
parses JSON is unaffected; one generated from the document's own schema and configured to refuse an
unlisted media type would be. `openadr conformance` reports which type a peer uses as an
**extension** check — information rather than non-conformance, because the document's reading is
defensible too.

### An event whose intervals cannot be resolved is rejected

The specification places content validation on the client and says a VTN "may" ignore unknown
content. An interval with no start and nothing to inherit one from, however, cannot be placed in
time by any VEN — and neither can one with no duration whose successor does not say when it begins.

**We return `400`.** Catching it at the boundary turns a silent field failure into an error the
publisher sees immediately.

---

## Additions, all optional

None of these change a specified shape, and a client that ignores them sees a conformant VTN.

| Addition | Why |
|---|---|
| `ETag` / `If-None-Match` on every `GET` | OpenADR has no delta sync; every deployment polls. Plain HTTP, invisible to clients that ignore it. |
| `GET /programs?programName=` | Finding one tariff among hundreds otherwise means paging the whole collection. Proposed upstream as [specification#418](https://github.com/oadr3-org/specification/issues/418); already implemented by public price servers. |
| `targets=a,b` as well as `targets=a&targets=b` | Both forms occur in the field. It costs one thing, named here rather than discovered: a target containing a literal comma cannot be expressed in a query, because the comma is read as a separator. `target` is an unconstrained 1–128 character string, so such a value is legal; use the repeated form and avoid commas in target names. |
| Private-address check on webhook callbacks | The security chapter requires the HTTPS check and describes the server-side request forgery risk without mandating a check. This is the other half of that advice. |
| `problem.type` as a dereferenceable URI | The schema allows any URI; `about:blank` helps nobody. |
| `problem.instance` and `X-Request-Id` | The same id on both, so a client quoting one is quoting the other and an operator can find the request in the log. An id supplied by a proxy is kept. |
| gzip and brotli response compression | "Support for a given compression format is optional for VTNs, although gzip is encouraged." |
| A request body size limit and a request timeout | The security chapter delegates rate limiting to an API gateway. Not every deployment has one. |
| `X-OpenADR-Signature` on webhook deliveries | The security chapter recommends HMAC-signing payloads but defines no header. Ours is HMAC-SHA256 over the exact body, hex-encoded. |
| `X-OpenADR-Attempt` on webhook deliveries | The dispatcher's durable attempt count, so a receiver can recognise a retry and stay idempotent. |
| `POST /auth/token` | Optional in the specification, and `501` unless the VTN is configured to issue tokens. With `internal-auth` it is a real client-credentials grant. |
| `GET /health` | Not a specification endpoint, and outside the API's base path. Reports storage reachability and the notification backlog — pending, abandoned, and the age of the oldest undelivered entry. A subscriber that has stopped answering is otherwise invisible. |
| `GET /metrics` | Prometheus exposition, outside the base path. Request rate and latency by matched route, delivery outcomes by channel, and the outbox gauges. |
| `GET /admin/outbox`, `POST /admin/outbox/retry` | Outside the base path, business logic's. `dead: 4` from `/health` is a number to alert on; these are what an operator acts on it with. Neither returns a notification body or a subscriber's `bearerToken`. |
| `POST /internal/mqtt/auth`, `/internal/mqtt/acl` | Outside the base path, and a private contract between this VTN and its broker rather than an API. The specification requires the VTN to stop one VEN subscribing to another's topics and declares the mechanism out of scope; this is the mechanism, in the shape EMQX and `mosquitto-go-auth` already call. |
| `GET /notifiers/mqtt/topics/vens/{venID}/reports` and `…/subscriptions` | 3.1.0 defines VEN-scoped topic endpoints for events, programmes and resources, and none for reports or subscriptions — but those are owned objects, so each notification goes to exactly one VEN, and the fan-out publishes to per-VEN topics for them too. Without these two endpoints a client had no way to discover a topic the VTN was publishing to. |

---

## Things the specification leaves to the implementation

Recorded here because the choices are load-bearing.

**Identifier format.** `objectID` is `^[a-zA-Z0-9_-]{1,128}$`. We additionally refuse `null`,
`undefined`, `.` and `..`, which would be ambiguous in a path or in JSON.

**Ordering.** Unspecified. Every collection is returned oldest-first with the object id as a
tie-breaker. Offset pagination without an order returns non-repeatable pages; creation order also
means an append-only collection never reshuffles the pages a client has already walked. The order
holds to the nanosecond, including for objects created within the same second.

**Filtering and pagination.** Every filter — targets, object privacy, `?active=` — is applied inside
the query, before the page is cut. Filtering a page after cutting it returns short pages, and a
client that treats a short page as the end of a collection would silently miss records it is
entitled to.

**What `?targets=` means.** "Targeting criteria can be used to filter responses to include only
those objects that include target terms found in the query", and filters are additive
(§Response Filtering). So a request naming a target returns **only** objects carrying one of the
named targets — an untargeted object is not gated by targeting, but it does not carry the term
either, so it does not match. Untargeted objects are reached by naming no target at all. On `/vens`
and `/resources` the parameter is *only* a filter: those collections are gated by ownership, and
§Object Privacy says target hiding is deliberately not performed on them, so a VEN reads its own
objects with their full targets and without naming any.

**Numeric precision.** Payload values are decimals, exchanged as JSON numbers. JSON numbers are
IEEE-754 doubles on the wire, so a round trip is exact to about fifteen significant digits — far
beyond anything the enumerations describe. Arithmetic on a parsed value is exact base-10 arithmetic,
which is the half that matters for settlement. Exchanging longer values would require
`serde_json/arbitrary_precision`, a global feature that changes `serde_json::Value` for every crate
in a downstream dependency graph.

**MQTT topic names.** The specification defines the *endpoints* that publish topic names, not the
names themselves. Ours are `{prefix}/{collection}/{operation}`, with
`{prefix}/{collection}/vens/{venID}/{operation}` for the VEN-scoped forms. The prefix is configurable
so one broker can serve several VTNs.

**Broker access control.** The specification says a VTN "MUST" enforce topic access "by any means
necessary" and declares the mechanism out of scope. Publishing a private per-VEN copy is only half
the job; the broker must also refuse cross-VEN subscriptions. That is deployment configuration —
EMQX's HTTP authorizer or `mosquitto-go-auth` — and belongs with the deployment, not the binary.

**Which objects a VEN's private topic carries.** `program` and `event` copies are filtered by
targeting and carry only that VEN's own targets. `ven`, `resource`, `report` and `subscription` are
filtered by *ownership* instead: a report carries no targets at all, so evaluating the targeting
rule on it would admit every VEN.

**Webhook retry and abandonment.** The specification asks a VTN to "retry to some degree, but not
constantly and not forever", to back off, and to mark a persistently unreachable endpoint broken. The
numbers are ours and all configurable: eight attempts, exponential backoff from two seconds capped at
five minutes, and a `4xx` other than `408` or `429` never retried at all, because the receiver is
saying "not this, ever" rather than "not now". An entry that exhausts its attempts is *abandoned*,
not deleted — it stays queryable and is counted by `GET /health`, since a notification that was
never delivered is exactly the thing an operator needs to be able to find.

**When the recipients of a notification are decided.** At write time, inside the transaction, not
when the delivery is attempted. A subscription created a moment after an event is therefore not told
about that event — which matches the specification's framing, where the subscription's conditions
are evaluated when the operation happens. Deferring the fan-out to delivery time would make the write
cheaper but would let a subscription receive notifications for changes that predate it.

**Notifications are queued, not delivered inline.** A write records one row per entitled recipient
in the same transaction as the object itself, and returns; a dispatcher delivers afterwards. So a
notification can arrive slightly after the response that caused it, and it survives a VTN restart in
between. A change that is refused announces nothing, and a change that is accepted is always
announced eventually — including the objects a cascade removes, so deleting a programme announces
the events and reports that went with it rather than only itself.

**Delivery is at least once.** A dispatcher can deliver and then die before recording that it did, in
which case the entry is delivered again. Every notification carries the object's identity and its
`modificationDateTime`, so a receiver that cares can recognise a repeat; `X-OpenADR-Attempt` says
which try this is. Exactly-once would need a two-phase commit with the receiver, which the protocol
does not offer.

**Callback address checks.** Refused: plain HTTP, and any host that is or resolves to a loopback,
private, link-local, carrier-grade-NAT, multicast, reserved or IPv4-mapped-IPv6 private address. The
URL as written is checked when the subscription is created and again before each delivery; what a
*name* resolves to is checked inside the HTTP client's own resolver, so the answer that is inspected
is the answer the socket is opened against. A subscription is not created at all until its endpoint
answers the echo challenge.

**`GET /notifiers` reports what this VTN can actually deliver.** The specification expects
`WEBHOOK: true` from a conformant VTN, and a VTN started without a webhook transport reports
`false`. It is the smaller lie: the binding key exists so that a client *can* be told, and reporting
`true` would have a subscriber create a subscription, receive nothing, and have no way to find out
why. The same VTN answers `501` to `POST /subscriptions`, which is the only moment the subscriber is
listening. Configure a transport and both become `true` and `201`.

**Payload validation strictness.** The Definitions say content validation is the client's business
and private payload types are legal. We default to `warn`: unknown types pass silently, known types
that contradict their enumeration are logged. `strict` and `off` are configurable.

**Token issuance.** `POST /auth/token` is optional. A VTN configured to delegate answers `501` and
points at `GET /auth/server`. One configured with `internal-auth` runs the client-credentials grant
itself: secrets are stored Argon2id-hashed, an unknown `client_id` costs the same as a known one so
response time does not enumerate clients, a client may ask for fewer scopes than it holds but never
for more, and errors are RFC 6749 `authError` bodies with a `400` — including `invalid_client`,
because §5.2 requires `401` only when the client authenticated through the `Authorization` header,
and here the credentials are in the body.
