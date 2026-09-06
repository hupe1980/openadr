+++
title = "Notifications"
description = "Webhooks, MQTT topics and the transactional outbox: how a VTN tells a VEN something changed without losing the message or leaking it to the wrong VEN."
weight = 70
+++

Polling is the normal case in every large OpenADR deployment, and this crate treats it as such. But
a curtailment instruction that a VEN learns about on its next five-minute poll is a curtailment
instruction delivered late, so the specification defines two push mechanisms. Both are implemented,
and both only *announce*: the notification carries the object, but a VEN that missed one recovers by
re-reading.

## The message

The same JSON on every transport:

```json
{
  "objectType": "EVENT",
  "operation": "CREATE",
  "object": { "id": "evt-1", "programID": "prg-1", "targets": ["cpo-a-sites"], "…": "…" },
  "targets": ["cpo-a-sites"]
}
```

`operation` is `CREATE`, `UPDATE` or `DELETE`. The top-level `targets` are the ones that caused this
recipient to be told — subject to the same [target hiding](@/docs/object-privacy.md) as a read, so a
subscriber never learns of a group it was not granted.

## Webhooks

A VEN registers a subscription:

```console
$ curl -X POST …/subscriptions -H "Authorization: Bearer $VEN_TOKEN" \
       -H 'Content-Type: application/json' -d '{
    "clientName": "cpo-a-backend",
    "programID": "prg-1",
    "targets": ["cpo-a-sites"],
    "objectOperations": [{
      "objects": ["EVENT"],
      "operations": ["CREATE", "UPDATE", "DELETE"],
      "callbackUrl": "https://cpo-a.example.com/openadr/hook",
      "bearerToken": "the-token-you-will-present-to-me"
    }]
  }'
```

Enable delivery with `--webhooks`, and sign it with `--webhook-key`:

```console
$ openadr vtn --database ./openadr.sqlite --webhooks --webhook-key $HMAC_SECRET
```

A delivery is a `POST` of the notification body carrying:

| Header | |
|---|---|
| `Authorization: Bearer …` | the `bearerToken` the subscription supplied, so the receiver can authenticate the VTN |
| `X-OpenADR-Signature` | `v1=<hex>` — HMAC-SHA256 over `v1.<timestamp>.<body>`, when a signing key is configured |
| `X-OpenADR-Timestamp` | when the delivery was signed, in Unix seconds — part of what is signed |
| `X-OpenADR-Attempt` | which try this is, so a receiver can recognise a redelivery |

The headers are an addition: the specification's security chapter recommends HMAC-signing payloads
but defines no header, and its wording — "signs the webhook payloads … and sends the signature in
the request header" — describes a signature over the body alone, which is valid for ever. Whoever
captures one delivery could post it back at any time, and a replayed "curtail to 0 kW" is a real
outage. So the instant is part of what is signed and travels with it, which is the scheme Stripe,
GitHub and the Standard Webhooks specification converged on.

### Verifying a delivery

The verifier ships with the crate, behind the `webhook-signature` feature. It does not pull in
`vtn` — a subscriber is not a server — and it is `no_std` + alloc, so it builds for an appliance:

```rust
use openadr::model::Timestamp;
use openadr::webhook::{self, Signature, SignatureError};

fn check(headers: &HeaderMap, body: &[u8], key: &[u8]) -> Result<(), SignatureError> {
    let sent_at = webhook::parse_timestamp(header(headers, webhook::TIMESTAMP_HEADER))
        .ok_or(SignatureError::Malformed)?;
    Signature::parse(header(headers, webhook::SIGNATURE_HEADER))?
        // Constant-time, and it refuses a genuine signature on a delivery older than the tolerance.
        .verify(key, body, sent_at, Timestamp::now(), webhook::DEFAULT_TOLERANCE)
}
```

Verify over the **raw bytes**, not a re-serialised body: JSON key order and number formatting are
not preserved by a round trip through a parser, and a signature is over bytes.

### The threat model is the VTN itself

A subscriber names a URL and the VTN then makes requests to it from inside the operator's network.
That is server-side request forgery with the specification's blessing, so most of the webhook chapter
is about refusals. All of it is implemented.

**The echo challenge runs before the subscription exists.** `POST`/`PUT /subscriptions` sends
`GET callbackUrl?echo=<random>` and requires the value back; a `400` otherwise. The timing is the
defence — a subscription aimed at a third party is never created, rather than created and found
undeliverable later.

**The URL as written** must be HTTPS with a public host. Loopback, RFC 1918, link-local,
carrier-grade NAT, multicast, reserved, IPv6 unique-local, IPv4-mapped IPv6, NAT64 and `localhost`
are all refused — at subscription time and again before every delivery.

**What a name resolves to** is refused inside the HTTP client's own DNS resolver. Resolving
separately and then handing the URL to a client that resolves it again inspects one answer and
connects to another, which is DNS rebinding with a check in front of it. One private answer poisons
the set: a name resolving to both a public and a private address is a rebinding attempt, not a
multi-homed server.

**Redirects are not followed** — a redirect is the same attack with an extra hop.

For local development, `CallbackPolicy::permissive()` disables the address rules. It re-enables
exactly the attack the rest of the module exists to prevent, which is why it is useful in tests and
nowhere else.

### Retry and abandonment

The specification asks a VTN to "retry to some degree, but not constantly and not forever", to back
off, and to mark a persistently unreachable endpoint broken. The numbers are this crate's and all
configurable:

```rust
DispatchConfig {
    batch: 32,                                   // entries claimed per pass
    concurrency: 8,                              // deliveries in flight at once
    lease: Duration::from_secs(60),              // before another dispatcher may take an entry
    retry: RetryPolicy {
        max_attempts: 8,
        base_delay: Duration::from_secs(2),      // doubling
        max_delay: Duration::from_secs(300),     // capped
    },
    ..Default::default()
}
```

A `4xx` other than `408` or `429` is **never retried**: the receiver is saying "not this, ever", and
seven more attempts are only traffic. Everything else is worth another try.

An entry that exhausts its attempts is **abandoned, not deleted**. It stays queryable and is counted
by `GET /health`, because a notification that was never delivered is exactly the thing an operator
needs to be able to find.

Counting it is where the loop *starts*, not where it ends. `dead: 4` is a number to alert on;
acting on it needs to know which subscriber, which object and what error:

```console
$ curl -s localhost:3000/admin/outbox -H "Authorization: Bearer $BL_TOKEN" | jq '.[0]'
{
  "id": 41,
  "enqueuedAt": "2026-02-11T12:00:03.914Z",
  "attempts": 8,
  "lastError": "callback returned 502",
  "subscriptionId": "sub-00000003",
  "objectType": "EVENT",
  "objectId": "evt-00000117",
  "destination": "https://cpo-a.example.com/openadr/hook"
}

# once the receiver is fixed:
$ curl -sX POST localhost:3000/admin/outbox/retry -H "Authorization: Bearer $BL_TOKEN"
{"revived":4}
```

Reviving resets the attempt counters with the entries: those attempts were spent against a receiver
that was broken, and charging them to a working one would abandon the entry again on its first try.
Neither endpoint returns the notification body or the subscriber's `bearerToken`. Both are business
logic's, and both sit at the root rather than under the OpenADR base path — they are operational
rather than protocol, and should be easy to keep off a public listener.

## The transactional outbox

Delivery is not on the request path, and the reason is worth stating precisely.

An OpenADR notification is a dispatch instruction. Losing one is the worst failure this system has,
because **nothing in the protocol tells a VEN that it happened** — there is no "you missed a
message" mechanism to fall back on. So the record that a notification must be sent is written in the
**same transaction** as the object that caused it:

```text
POST /events
  ├─ snapshot subscriptions + grants        (before the transaction)
  ├─ BEGIN
  │    INSERT INTO event …
  │    INSERT INTO outbox …                 one row per entitled recipient
  │  COMMIT
  └─ 201 Created                            ← returns here

  … a dispatcher claims a batch under a lease, delivers, records the outcome
```

Three properties follow, and they are why this is worth a table rather than a background task with a
channel:

- **Latency.** A write costs one insert per entitled subscriber, not one HTTP round trip each.
- **Durability.** There is no window in which an event exists and nobody has been told. A crash
  between the two would have lost every notification the write should have produced.
- **Visibility.** Attempts, next-attempt times, the last error and abandoned entries are rows, not a
  counter inside a process that restarted.

Running **more than one dispatcher is safe**: claiming an entry sets a lease in the same transaction
as the read, so two dispatchers never take the same one, and a dispatcher that dies mid-delivery
costs a lease of delay rather than a notification. On PostgreSQL, `SKIP LOCKED` makes those batches
disjoint without contention.

**Recipients are computed at write time**, against the subscriptions that existed when the change
happened — so a subscription created a moment after an event is not told about it. That matches the
specification, where a subscription's conditions are evaluated when the operation occurs.

Each subscription carries what *kind* of client created it, recorded when the credential was still
in hand. It has to: the specification never labels a token "BL" or "VEN" — the distinction falls out
of the scopes — and a dispatcher draining the queue an hour later has no credential to ask. A
subscriber whose kind was lost reads as a VEN, and a VEN's visibility of a targeted object is its
grant; business logic holds none, so a utility subscribing to its own events would be told about
none of the targeted ones. Business logic's subscription is a standing read, and business logic
reads everything.

**A subscriber that stops answering is cut off.** `max_attempts` bounds what *one* notification
costs; nothing bounds what a *subscriber* costs, so an endpoint that has been gone for a week would
otherwise charge three to eight HTTP round trips to every write in the VTN, for ever. Giving up is
per-notification and cannot see the pattern.

So the VTN counts **consecutive** abandonments per subscription. Three in a row (by default) and new
notifications for that subscription are not queued at all; after fifteen minutes one is let through
as a probe, and a delivery that succeeds clears the record completely. Consecutive is the load-bearing
word: a subscriber that is merely flaky is never cut off, which is the failure mode that makes people
switch breakers off.

Cutting a subscriber off **loses notifications**, and OpenADR has no way to tell it so. That is why
it is deliberately loud rather than quiet:

```console
$ curl -s localhost:3000/admin/subscribers -H "Authorization: Bearer $BL_TOKEN" | jq '.[0]'
{
  "subscriptionId": "sub-00000003",
  "consecutiveFailures": 3,
  "cutOffSince": "2026-02-11T09:14:02.000000000Z",
  "retryAt": "2026-02-11T09:29:02.000000000Z",
  "lastError": "callback returned 503"
}
```

`GET /health` reports `subscribersCutOff` and `/metrics` exposes `openadr_subscribers_cut_off`,
because a cut-off subscriber **queues nothing** — without that number an empty outbox means either
"everything was delivered" or "nobody is being told any more". `POST /admin/outbox/retry` closes
every breaker *and* revives the backlog, since reviving one without the other is half a recovery.

Broker deliveries are exempt: a topic is not a subscriber the VTN can cut off, and a broker that is
down is one endpoint rather than a thousand. `--breaker-threshold 0` switches it off.

**Delivery is at least once.** A dispatcher can deliver and then die before recording that it did,
in which case the entry is delivered again. Every notification carries the object's identity and
`modificationDateTime`, and `X-OpenADR-Attempt` says which try this is, so a receiver can be
idempotent — which is its job in every at-least-once system. Exactly-once would need a two-phase
commit with the receiver, which the protocol does not offer.

## MQTT

3.1 added push over a broker for one reason: a VEN inside a residential appliance sits behind a
firewall that will never accept an inbound `POST`, so a webhook cannot reach it at all. The client
opens the connection instead, and the VTN publishes.

```console
$ openadr vtn --database ./openadr.sqlite \
      --mqtt-broker mqtts://broker.internal:8883 \
      --mqtt-advertise mqtts://broker.example.com:8883 \
      --mqtt-bl-client bl-1
```

`--mqtt-broker` is what the VTN connects to; `--mqtt-advertise` is what clients are told to connect
to, because the broker is often reachable privately from the VTN and publicly from everybody else.

A client asks what the VTN offers:

```console
$ curl …/notifiers -H "Authorization: Bearer $TOKEN"
{"WEBHOOK":true,
 "MQTT":{"URIS":["mqtts://broker.example.com:8883"],
         "serialization":"JSON",
         "authentication":{"method":"OAUTH2_BEARER_TOKEN","username":"{clientID}"}}}
```

Then asks for topic names. Twelve endpoints in the document plus two additions, and their scopes are
the load-bearing part:

| Endpoint | Scope | Topic |
|---|---|---|
| `mqtt/topics/programs` | `read_all` | `{prefix}/programs/{operation}` |
| `mqtt/topics/programs/{id}` | `read_all` | `{prefix}/programs/{id}/{operation}` |
| `mqtt/topics/programs/{id}/events` | `read_all` | `{prefix}/events/programs/{id}/{operation}` |
| `mqtt/topics/{events,reports,subscriptions,vens,resources}` | `read_bl` | `{prefix}/{collection}/{operation}` |
| `mqtt/topics/vens/{id}` | `read_ven_objects`, own VEN only | `{prefix}/vens/{id}/{operation}` |
| `mqtt/topics/vens/{id}/{events,programs,resources}` | `read_ven_objects`, own VEN only | `{prefix}/{collection}/vens/{id}/{operation}` |
| `mqtt/topics/vens/{id}/{reports,subscriptions}` † | `read_ven_objects`, own VEN only | `{prefix}/{collection}/vens/{id}/{operation}` |

† Beyond the document, and deliberately: the fan-out already publishes a VEN's own reports and
subscriptions to these topics, because they are owned objects and each copy goes to exactly one VEN.
3.1.0 defines no endpoint that names them, so a client had no way to discover a topic the VTN was
publishing to. See [Reading the specification](@/docs/spec-notes.md).

Everything that is not VEN-scoped is **collection-wide**: subscribing to it yields every object of
that type with its full target set. Handing one to a VEN would undo object privacy on the push path
however carefully the broker were configured, which is why even the programme-scoped endpoints are
business logic's. A VEN reads its own under `mqtt/topics/vens/{venID}/…`, which is what 3.1 added
them for.

Asking for another VEN's topics answers `404`, not `403`, so ids cannot be enumerated. And the scope
is checked before the deployment is: a caller that holds neither `read_all` nor `read_bl` is refused
without learning whether this VTN has a broker at all.

`read_bl` is a permission on those five rows and **not** the business-logic identity, which is a
distinction worth stating because the name invites the other reading. A credential carrying only
`read_bl` may list those topics and is an ordinary identified client everywhere else — object
privacy applies to it in full. See [Authentication](@/docs/authentication.md#scopes).

`mqtt_topic_prefix` is configurable so one broker can serve several VTNs.

### Per-VEN fan-out

For each entitled VEN the VTN publishes a **private copy** to that VEN's own topic, carrying only
that VEN's targets — the same `Access` the read path uses, so the two cannot disagree.

Which gate applies depends on the object: `program` and `event` copies are filtered by *targeting*;
`ven`, `resource`, `report` and `subscription` by *ownership*. A report carries no targets at all,
so evaluating the targeting rule on one would admit every VEN.

The gate also decides what the write has to *look up*. A targeted object can be admitted by any
VEN's grant, so the snapshot taken before the write is the whole fleet's. An owned one reaches its
owner and nobody else, so the snapshot is one indexed lookup — which is why `POST /reports`, the
highest-rate write in the system, costs the same whether the VTN has ten VENs or ten thousand.

The topic a copy is published to comes from the **same function** the discovery endpoints render, so
a VEN cannot be told to watch a name the VTN never publishes to. That is not a hypothetical: for the
VEN object itself, the two were computed separately and produced `vens/{venID}` and
`vens/vens/{venID}` respectively. Every VEN subscribed correctly and received nothing, for ever, with
no error anywhere.

### Delivery is confirmed, not fired

The publisher uses QoS 1 and **waits for the broker's `PUBACK`** before the outbox entry is
completed. An MQTT client library's `publish` returns when the packet reaches its own internal
queue, which is not the same thing at all: completing the entry on that would delete the row on the
strength of a write to an in-process channel, and a process that died a moment later would have
published nothing.

That costs one round trip per notification, and it is the whole reason the outbox exists.
`MqttConfig::qos = AtMostOnce` is available for a deployment that would rather have the throughput
and knows what it is giving up.

Retain is **off by default**. Retained messages let a reconnecting VEN see the last notification per
topic, but the specification tells clients to re-`GET` on reconnect rather than trust one, and a
retained *delete* on a per-VEN topic outlives the grant that put it there. `--mqtt-retain` turns it
on.

### The broker's half

Publishing a private copy is half of object privacy on a broker. The other half is the broker
refusing a cross-VEN subscription, and the specification is explicit that this is the VTN's problem
and equally explicit that the mechanism is not its business:

> A VTN **MUST** prevent a VEN from subscribing to topics that would expose objects the VEN is not
> authorized to access.

So the VTN answers the broker's own callbacks — the shape EMQX's HTTP backends and
`mosquitto-go-auth` already call:

| | |
|---|---|
| `POST /internal/mqtt/auth` | `{"username","password","clientid"}` → `{"result":"allow"\|"deny"}`. The password is the OpenADR access token, checked through the same `Authenticator` the REST API uses, so a revoked credential stops working on both surfaces at once. The username must be the `clientID` the token proved. |
| `POST /internal/mqtt/acl` | `{"username","clientid","topic","action"}` → the same shape. |

The authorization answer is derived from the **topic**, which is what makes it stateless — a VTN
behind a load balancer answers the two callbacks from different processes:

- `…/vens/{venID}/…` is allowed when that VEN's `clientID` is the connecting username. Same question,
  same store, as the endpoint that hands out the name — so a VEN can only subscribe to a topic it
  could have discovered.
- A wildcard where the `venID` belongs — `events/vens/+/create`, the one filter that would undo the
  design — is not a VEN-scoped topic and is refused.
- Anything else is collection-wide and needs a `clientID` named in `--mqtt-bl-client`, which is
  **empty by default**.
- `publish` is refused for everyone but one configured identity — see below. A client that could
  publish could forge a dispatch instruction, which is the worst thing these callbacks can be talked
  into.

Both callbacks answer `200` even when refusing, because EMQX treats a non-2xx as *its own* error and
applies its configured fallback rather than the decision.

Expose these two endpoints to the broker and to nothing else.

### The VTN is a client of its own broker

A broker configured this way authenticates **everyone** through the VTN — the VTN's own publisher
included. With `publish` refused for everyone, the fan-out is locked out of its own broker: it
reconnects for ever against `NotAuthorized`, every queued notification stays queued, and nothing
names the cause, because the broker's log says a client was refused and the VTN's says the
connection dropped.

So two flags go together:

```
--mqtt-username=business-logic --mqtt-password=$BL_TOKEN   # a credential /auth accepts
--mqtt-publisher-client=business-logic                     # the one identity /acl lets publish
```

That identity may publish **only** under this VTN's own `--mqtt-topic-prefix`, so the same name on a
shared broker cannot reach another deployment's topics. Leave the last flag off — the default — if
your broker authenticates the VTN by its own user database or by mutual TLS, and the blanket refusal
stands.

### Working configurations

`deploy/` in the repository carries `emqx.conf`, `mosquitto.conf`, a `Dockerfile` and a
`compose.yaml` that runs a VTN and EMQX together. Observed against EMQX 5.8: a targeted event
reaches only the entitled VEN's private topic, another VEN's topic is refused by the broker, a VEN's
forged publish reaches no subscriber, and a wrong password is refused at connect.

One line there is load-bearing and easy to leave out: `authorization.no_match = deny`. EMQX's
default is `allow`, so a VTN whose ACL endpoint is briefly unreachable would otherwise hand every
VEN every topic — object privacy on the push path, undone by a timeout.

## Two transports, one queue

A VTN can offer both. `Notifiers` holds them and routes each queued entry by its channel:

```rust
use openadr::vtn::notify::{MqttConfig, MqttNotifier, Notifiers, WebhookConfig, WebhookNotifier};

let transports = Notifiers::new()
    .with(WebhookNotifier::shared(WebhookConfig { signing_key: Some(key), ..Default::default() })?)
    .with(MqttNotifier::connect(MqttConfig::new("mqtts://broker.example.com:8883"))?);

let vtn = Vtn::builder().storage(storage).notifier(transports.shared()).build();
```

Every `Delivery` carries a `Route` — `Webhook { callback_url, bearer_token }` or `Topic { topic }`,
never both and never neither — and every route names a `Channel`. A delivery **no** transport claims
fails permanently and is counted as dead, where an operator sees it.

That is not defensive tidiness. Before it existed, a VTN advertising an MQTT binding with no
publisher behind it handed every broker notification to the webhook transport, which answered `Ok`
because the delivery genuinely was not its — and the dispatcher, which reads `Ok` as "delivered",
deleted the row. Every broker notification was lost, silently, through the machinery built to make
exactly that impossible.


## Subscribing, from the VEN

The other end of the broker. `openadr::ven::MqttPush` discovers the binding and this VEN's own
topics from the VTN, subscribes, and shortens the runtime's next sleep — see
[the VEN runtime](@/docs/ven-runtime.md#push-as-a-hint). It never deserialises a payload: the
notification's useful content is its *arrival*, and the object itself comes from the VTN over an
authenticated channel on the conditional read that follows.

```console
$ cargo add openadr --features ven,mqtt
```

`mqtt` implies no role. With `vtn` it is the publisher and the broker callbacks; with `ven` it is
the subscriber; a VEN does not compile a server to subscribe to a broker.

## Writing a transport

`Notifier` is one attempt and no state:

```rust
#[async_trait]
pub trait Notifier: Send + Sync + 'static {
    fn handles(&self, channel: Channel) -> bool;
    async fn deliver(&self, delivery: &Delivery, attempt: u32) -> Result<(), DeliveryFailure>;
    async fn verify_callback(&self, url: &str) -> Result<(), DeliveryFailure> { Ok(()) }
    fn name(&self) -> &'static str;
}
```

`handles` has no default, deliberately. A transport has to state which channels it can send on,
because the alternative — answering `Ok` to a delivery it cannot make — is indistinguishable from
success to everything downstream.

`DeliveryFailure` carries a message and one flag: `retriable`. The dispatcher makes exactly one
decision — try again, or stop — so that is what the type says. A richer error would be a transport's
private vocabulary leaking into a scheduler that cannot act on it.

Do **not** retry inside `deliver`. The durable attempt count and the schedule belong to the
dispatcher: a transport that looped internally would hold its outbox lease open for the whole backoff
sequence and would forget its count on restart, which is the durability the outbox exists to provide.

Two implementations ship for testing: `RecordingNotifier` keeps deliveries in memory, and
`NullNotifier` drops them — which an embedder installs deliberately, and which is *not* the default.
The default is an empty `Notifiers`, so a VTN with no push configured reports `WEBHOOK: false` from
`GET /notifiers` and answers `501` to `POST /subscriptions` rather than accepting a subscription it
could never deliver.
