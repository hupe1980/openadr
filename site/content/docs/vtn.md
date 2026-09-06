+++
title = "Running a VTN"
description = "Configuration, the endpoint surface, HTTP caching, error bodies, health checks and deployment shapes for an OpenADR 3.1 VTN."
weight = 30
+++

Two ways to run one: the `openadr` binary, or `Vtn::builder()` inside your own service. They are the
same server — the binary is a thin argument parser over the builder.

## The binary

```console
$ openadr vtn [OPTIONS]
```

| Option | |
|---|---|
| `--listen <ADDR>` | Bind address. Default `0.0.0.0:3000`. |
| `--public-url <URL>` | How clients reach this VTN. Used for `GET /auth/server`. |
| `--base-path <PATH>` | Mount point. Default `/openadr3/3.1.0`. |
| `--database <TARGET>` | A `postgres://` URL, or a SQLite file path. |
| `--payload-validation <P>` | `off`, `warn` (default) or `strict`. |
| `--no-cache` | Turn off `ETag` / `If-None-Match`. |
| `--ephemeral` | Acknowledge in-memory storage on a public address. |
| `--webhooks`, `--webhook-key <SECRET>` | Deliver notifications to subscriber callbacks, HMAC-signed. |
| `--mqtt-broker <URL>` | Publish notifications to a broker. `mqtt://` or `mqtts://`. |
| `--mqtt-advertise <URL>` | What clients are told to connect to, when the VTN reaches the broker privately. |
| `--mqtt-username`, `--mqtt-password` | The VTN's own broker credentials. |
| `--mqtt-topic-prefix <P>` | So one broker can serve several VTNs. Default `openadr3/<version>`. |
| `--mqtt-retain` | Publish with the retain flag. Off by default. |
| `--mqtt-bl-client <ID>` | A `clientID` allowed to subscribe to collection-wide topics. Repeatable, empty by default. |
| `--report-retention <FOR>` | Delete reports older than this — `90d`, `12h`, `3600s`. Off by default. |
| `--tls-cert <PEM>`, `--tls-key <PEM>` | Serve HTTPS directly. TLS 1.2+, ALPN h2 and http/1.1. |
| `--tls-client-ca <PEM>` | Refuse a connection whose client certificate this CA did not issue. |
| `--tls-client-optional` | Ask for a client certificate but serve a peer that presents none. |
| `--mdns`, `--mdns-name`, `--mdns-host`, `--mdns-program` | Advertise on the local network. See [Discovery](@/docs/discovery.md). |

Authentication options are on their [own page](@/docs/authentication.md); storage on
[another](@/docs/storage.md).

`--listen` is a *bind* address and `--public-url` is a *URL*, and the difference matters:
`0.0.0.0` means "every interface on this machine", so advertising it from `/auth/server` sends every
client to itself. Without `--public-url` the binary substitutes `localhost`, which is right for
development and wrong for anything else. With `--tls-cert` the advertised scheme becomes `https`.

TLS and mutual TLS are on the [security page](@/docs/security.md#transport-security); the client
certificate is a *network* gate rather than an identity, which is worth reading before you rely on
it.

## As a library

```rust
use openadr::vtn::{Vtn, VtnConfig, auth::StaticTokenAuth, store::SqliteStorage};

let vtn = Vtn::builder()
    .storage(SqliteStorage::shared("openadr.sqlite").await?)
    .authenticator(std::sync::Arc::new(auth))
    .config(VtnConfig {
        base_path: "/openadr3/3.1.0".into(),
        max_body_bytes: 8 * 1024 * 1024,
        ..Default::default()
    })
    .build();

vtn.serve("0.0.0.0:3000").await?;
```

The builder is a typestate: `build()` exists only after `storage()` has been called, so a VTN
without a backing store is a compile error rather than a panic at start-up.

To mount the router inside a larger axum application, take `vtn.router()` instead of calling
`serve` — but then **spawn the dispatcher yourself**, or nothing will ever be delivered:

```rust
let app = my_app.nest("/openadr", vtn.router());
vtn.dispatcher().clone().spawn();
```

`Vtn::serve` does that for you. In tests, `dispatcher().drain()` is the deterministic alternative:
it runs until nothing is due, so assertions about retry counts are exact rather than timing-dependent.

## The endpoint surface

Everything is mounted under `--base-path` *and* at the root, because the specification's enrollment
flow permits a client configured with a bare base URL.

**The six collections** — `/programs`, `/events`, `/reports`, `/subscriptions`, `/vens`,
`/resources` — each with `GET` and `POST`, and `GET`/`PUT`/`DELETE` on `/{id}`.

**Query parameters** follow the OpenAPI document, plus one addition:

| Collection | Parameters |
|---|---|
| `/programs` | `targets`, `skip`, `limit`, and `programName` <sup>†</sup> |
| `/events` | `programID`, `targets`, `active`, `skip`, `limit` |
| `/reports` | `programID`, `eventID`, `clientName`, `skip`, `limit` |
| `/subscriptions` | `programID`, `clientName`, `objects`, `skip`, `limit` |
| `/vens` | `venName`, `targets`, `skip`, `limit` |
| `/resources` | `venID`, `resourceName`, `targets`, `skip`, `limit` |

<sup>†</sup> `?programName=` is not in the specification. It is proposed upstream and already
implemented by public price servers, where finding one tariff among hundreds otherwise means paging
the entire collection. Turn it off with `VtnConfig::program_name_lookup`.

List parameters are accepted in both forms found in the field: `?targets=a&targets=b` and
`?targets=a,b`.

**Authentication** — `GET /auth/server` (required, unauthenticated) and `POST /auth/token`
(optional; `501` unless the VTN issues its own tokens).

**Notifiers** — `GET /notifiers` plus twelve `/notifiers/mqtt/topics/…` paths from the document and
two beyond it. See [Notifications](@/docs/notifications.md).

**`GET /openapi.json`** — under the base path *and* at the root, and unauthenticated, because a
client reads it before it has a credential. It is `openadr3.yaml` narrowed to this deployment:
`servers` names your base path, the MQTT topic paths are absent when you have no broker (they would
answer `501`, and a generated client with a method that cannot work is worse than no method), and
the endpoints and parameters this VTN adds beyond the document carry `x-openadr-extension` so you
can tell them from the standard ones. A local VTN's mDNS record names it in `openapi_url`.

```console
$ curl -s localhost:3000/openadr3/3.1.0/openapi.json | jq '.servers, (.paths | keys | length)'
```

**Off the OpenADR surface.** Six endpoints, mounted at the root only rather than under the base
path — they are operational rather than protocol, so they should never be mistaken for it and
should be easy to keep off a public listener:

| | |
|---|---|
| `GET /health` | Storage reachability, the notification backlog, and how many subscribers are cut off. |
| `GET /metrics` | Prometheus exposition. |
| `GET /admin/outbox` | The notifications that were given up on. Business logic's. |
| `GET /admin/subscribers` | The subscriptions whose deliveries are failing. Business logic's. |
| `POST /admin/outbox/retry` | Make them due again, and close every circuit breaker. Business logic's. |
| `POST /internal/mqtt/{auth,acl}` | The broker's authorization callbacks. Expose to the broker and nothing else. |

## Report retention

`report` is the only object OpenADR grows without bound: every other one is created by business logic
and deleted by it, while reports arrive from a fleet on a schedule the VTN itself asked for. A
thousand resources on a quarter-hourly descriptor is roughly thirty-five million rows a year.

```console
$ openadr vtn --database ./openadr.sqlite --report-retention 90d
```

- **Off by default, and `0` is refused.** Reports are settlement data.
- **Permanent and silent.** OpenADR has no notification meaning "a report you filed has been
  forgotten", so nothing is queued — ten thousand expiries do not become ten thousand deliveries.
- **Oldest-first, in bounded batches**, so the first pass over a year of data is many small
  transactions rather than one long lock. On PostgreSQL the claim is `FOR UPDATE SKIP LOCKED`, so
  several instances divide the work.
- **Export first.** `GET /reports` paginates and filters by `eventID` and `clientName`; there is no
  archival format built in.

`GET /health` and `GET /metrics` carry how much is stored and how old the oldest is — which is how a
working sweeper is told from one that is configured and not running.

## Filtering, ordering, pagination — in that order

Every list request applies its filters *inside* the storage query, then orders, then cuts the page.
The order is a correctness property, not a performance one.

Filtering a page after it has been cut silently drops records. A VEN granted `group1` that asks for
`?targets=group1,group2` matches either target at the storage layer; a privacy filter applied
afterwards drops every `group2` object from the page it was handed. With sixty `group2` events ahead
of four `group1` ones, the first page of fifty comes back holding **one** of the VEN's four events.
Nothing errors — and a client that stops when a page is shorter than `limit`, which is the normal way
to consume offset pagination, takes that as the end of the list.

Collections are ordered oldest-first with the object id as a tie-breaker. The specification does not
prescribe an order, but offset pagination without one returns non-repeatable pages, and creation
order additionally means an append-only collection never reshuffles pages a client has already
walked. The ordering holds to the nanosecond.

## HTTP caching

Every `GET` carries an `ETag`, and `If-None-Match` produces a `304` with no body. `*`,
comma-separated lists and either form of the tag are all handled.

The tag is **weak** — `W/"…"` — and deliberately. It is computed over the JSON, and the response
then passes through a compression layer that rewrites the body; a strong validator has to change
whenever the representation changes, and a content-coding is part of the representation (RFC 9110
§8.8.1). The same strong tag on a 7 kB body and on the 650-byte gzip of it is a validator that lies,
and `Vary: Accept-Encoding` only helps a cache that implements it — not a client that stores the
compressed bytes and later revalidates asking for identity. nginx weakens its tags when it
compresses, for the same reason. Nothing is lost: `If-None-Match` is defined to compare weakly
(§13.1.2), so every `304` still happens.

This is not decoration. OpenADR has no delta-sync mechanism, so a client that wants to know whether
anything changed re-fetches the collection — and every large deployment does exactly that on a
timer. A `304` is the difference between a kilobyte and a megabyte per client per poll.

The tag is computed over the **rendered, privacy-filtered** body, so two readers with different
visibility of the same URL get different tags. A shared tag would let one reader validate another's
view.

Clients built on this crate use it through `list_if_changed` — see [The client](@/docs/client.md).

An `ETag` says whether a copy is still good; it says nothing about whether one should have been kept,
or by whom. Object privacy makes that a real question, so every read also carries
`Cache-Control: private, no-cache` and `Vary: Authorization`, and every token response
`Cache-Control: no-store` (RFC 6749 §5.1). `no-cache` means "revalidate", not "do not store" — the
`304` above still happens on every poll.

## Errors

Every 4xx and 5xx carries a [problem](https://www.rfc-editor.org/rfc/rfc9457) body, which the
specification requires:

```json
{
  "type": "https://openadr.dev/problems/missing-scope",
  "title": "Forbidden",
  "status": 403,
  "detail": "the write_events scope is required",
  "instance": "0f2f7d13-6b2f-4a5b-9f2e-2b2a6b1c9d40"
}
```

`instance` is the request id, and it is also the `X-Request-Id` header on the same response and the
`request_id` field on the tracing span. So a client quoting an error and an operator searching the
log are quoting the same string. An id supplied by an upstream proxy is kept rather than replaced.

One mapping is worth knowing: **an object you may not see returns `404`, never `403`**. A `403`
confirms the object exists, which is exactly what targeting conceals.

That includes errors no handler produced — an oversized body (`413`), a request past the timeout
(`504`), an unrouted method (`405`) — so each carries the same `type`, `status` and `instance`. The
exception is `POST /auth/token`, whose errors are RFC 6749 §5.2 `authError` bodies
(`{"error": "invalid_client", …}`), because clients parse that shape.

**A body must say it is JSON.** A write labelled anything but `application/json` or an RFC 6839
`+json` type is refused with `415`; a body carrying no `Content-Type` is read, because it has claimed
nothing. Worth knowing if you reach for `curl -d`, which labels its body
`application/x-www-form-urlencoded` — add `-H 'content-type: application/json'`, or use the `openadr`
CLI. `POST /auth/token` reads both form encoding and JSON.

## Middleware

Attached to the router, and each asserted by a test rather than merely enabled in the manifest:

- **Request id** — minted at the edge, propagated to the response and into the span.
- **Compression** — gzip and brotli. The specification encourages it for bandwidth-constrained
  links, and an event carrying a year of quarter-hourly intervals compresses by an order of
  magnitude.
- **Body limit** — 8 MB by default (`VtnConfig::max_body_bytes`).
- **Timeout** — 30 s by default, answering `504`: the deadline was the server's, not the client's.
- **Problem bodies** — outside the two above, so their answers become problem documents; inside
  compression, because it reads an error body back to stamp the request id on it. The order is a
  correctness property, not a preference.
- **Tracing spans** — method, path and request id.

## Health

```console
$ curl -s localhost:3000/health
{"status":"ok","outbox":{"pending":3,"dead":0,"oldestPendingSeconds":2},"subscribersCutOff":0,
 "reports":{"count":18432,"oldestSeconds":7775990}}
```

`pending` counts entries still to be delivered, including ones waiting on a retry. `dead` counts
entries that exhausted their attempts and were abandoned — kept, not deleted, because a notification
that was never delivered is exactly what an operator needs to be able to find.

`reports` is the other unbounded thing. `oldestSeconds` sitting just under your retention period,
as above, is a sweeper doing its job; `oldestSeconds` climbing past it is a sweeper that is not
running.

`oldestPendingSeconds` is the number to alert on. A queue that is long but young is busy; a queue
that is short but old is stuck.

A `500` from this endpoint means storage is unreachable.

## Metrics

```console
$ curl -s localhost:3000/metrics
openadr_http_requests_total{method="GET",route="/openadr3/3.1.0/events",status="200"} 1417
openadr_http_request_duration_seconds_bucket{method="GET",route="/openadr3/3.1.0/events",le="0.01"} 1390
openadr_notification_attempts_total{channel="webhook",outcome="delivered"} 902
openadr_notification_attempts_total{channel="mqtt",outcome="retrying"} 3
openadr_outbox_pending 3
openadr_outbox_dead 0
openadr_outbox_oldest_pending_seconds 2
openadr_subscribers_cut_off 0
openadr_reports_stored 18432
openadr_reports_oldest_seconds 7775990
openadr_reports_purged_total 96
```

Prometheus exposition, no scrape credentials — put it behind whatever your other services are behind.

Two details are load-bearing. Route labels come from the **matched path** (`/events/{id}`), never
the request URI, so a client walking event ids cannot mint a new time series per request — which is
how a metrics endpoint becomes the memory leak it was installed to detect. And the outbox numbers
are read from the store at scrape time rather than counted in process, because a second VTN instance
shares the queue and a per-process counter would report only its own share of it.

`openadr_outbox_oldest_pending_seconds` is the one to page on. A notification that is never
delivered produces no error anywhere — OpenADR has no way for a VEN to say "you never told me" — so
a queue that stops draining is otherwise invisible.

`openadr_subscribers_cut_off` is the other half of that. A subscriber the circuit breaker has cut off
**queues nothing**, so without this gauge an empty outbox means either "everything was delivered" or
"nobody is being told any more".

Alert on `openadr_reports_oldest_seconds` exceeding your retention period: that is a sweeper that has
stopped, and no other number here shows it.

## The dead-letter view

`GET /health` says `dead: 4`. `GET /admin/outbox` says which four:

```console
$ curl -s localhost:3000/admin/outbox -H "Authorization: Bearer $BL_TOKEN"
[{"id":41,"enqueuedAt":"2026-02-11T12:00:03.914Z","attempts":8,
  "lastError":"callback returned 502","subscriptionId":"sub-00000003",
  "objectType":"EVENT","objectId":"evt-00000117",
  "destination":"https://cpo-a.example.com/openadr/hook"}]

$ curl -sX POST localhost:3000/admin/outbox/retry -H "Authorization: Bearer $BL_TOKEN"
{"revived":4,"subscribersRestored":1}
```

Neither returns the notification body or the subscriber's `bearerToken`. Reviving resets the attempt
counters with the entries, because the attempts they spent were spent against a receiver that was
broken — and it closes every **circuit breaker** too, because reviving a backlog for a subscriber the
VTN has stopped queueing for is half a recovery.

## When a subscriber stops answering

`max_attempts` bounds what one notification costs. Nothing bounds what a *subscriber* costs, so an
endpoint that has been gone for a week would charge three to eight HTTP round trips to every write in
the VTN, for ever.

Three notifications abandoned **in a row** (`--breaker-threshold`) cut that subscription off:
nothing more is queued for it until `--breaker-cooldown` has passed and one probe is let through. A
delivery that succeeds clears the record, so a merely flaky subscriber is never cut off.

```console
$ curl -s localhost:3000/admin/subscribers -H "Authorization: Bearer $BL_TOKEN" | jq '.[0]'
{"subscriptionId":"sub-00000003","consecutiveFailures":3,
 "cutOffSince":"2026-02-11T09:14:02.000000000Z","retryAt":"2026-02-11T09:29:02.000000000Z",
 "lastError":"callback returned 503"}
```

A cut-off subscriber queues nothing, so `GET /health`'s `subscribersCutOff` and
`openadr_subscribers_cut_off` are what stop an empty outbox meaning two different things.
`--breaker-threshold 0` switches it off. Broker deliveries are exempt: a topic is not a subscriber.

## Payload validation

The specification places content validation on the client and explicitly permits privately agreed
payload types, so validation here is a *policy*, not a rule:

| Policy | |
|---|---|
| `off` | Do not look. |
| `warn` (default) | Check against the Alliance's enumerations and log anything that contradicts one. Unknown types pass silently — private strings are legal. |
| `strict` | Reject a payload that contradicts its enumeration with a `400`. Unknown types still pass. |

It applies to four things, because the Alliance enumerates four: an event's interval payloads, a
report's, a `program`'s `attributes`, and a `ven`'s or `resource`'s. It applies on `POST` **and**
`PUT` — a rule one of two write paths applies is a rule with a way round it — so under `strict` a
`LOCATION` attribute carrying one coordinate, or `BINDING_EVENTS` carrying a string, is a `400`
wherever it is written.

## Deployment shapes

**A site controller or single-tenant pilot.** One binary, SQLite, the VTN's own token endpoint. No
database to run, no authorization server beside it. With `--mdns` a VEN on the same network
[finds it by browsing](@/docs/discovery.md).

**A utility-scale VTN.** PostgreSQL, several instances behind a load balancer, JWTs from the
utility's existing authorization server. The notification queue is where Postgres earns its place:
`FOR UPDATE SKIP LOCKED` lets dispatchers scale, and `LISTEN`/`NOTIFY` wakes one the instant a write
commits rather than on a poll interval.

**A public tariff server.** No credentials at all. An anonymous principal holds no scopes and every
write requires one, so the VTN is read-only by construction rather than by configuration. Put a CDN
in front of it and the `ETag`s do the rest.

TLS termination is the deployment's job — a reverse proxy or a load balancer. The binary does not
speak TLS itself.
