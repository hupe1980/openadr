+++
title = "Security model"
description = "The threat model behind an OpenADR 3.1 VTN: what a VEN structurally cannot do, how secrets are held, and the webhook and MQTT attack surfaces."
weight = 55
+++

An OpenADR VTN dispatches load. The worst outcomes are not "an attacker reads the database" — they
are *an attacker curtails a fleet*, and *one participant learns another's dispatch schedule*. This
page is what the implementation does about each, and what it deliberately leaves to you.

## What a VEN structurally cannot do

The strongest guarantees here are the ones that are unreachable rather than merely refused.

**A VEN cannot grant itself a target.** Targeting is how a VTN decides which VENs may see an event,
so a VEN that could write its own targets could read any event aimed at anyone. The request body it
is allowed to send — `VEN_VEN_REQUEST` — has **no `targets` member at all**. There is no field to
set, so there is no check to forget. Only business logic may send `BL_VEN_REQUEST`, and the VTN
refuses that body from a non-BL caller.

**A VEN cannot claim someone else's identity.** `clientID` is stamped by the VTN from the
credential, never read from the body.

**The typed client makes the same split a compile error.** `Client<VirtualEndNode>` has no
`events().create()` — the method does not exist, rather than existing and returning `403`.

## Object privacy is the confidentiality boundary

The full target list on an event is commercially sensitive: it is a competitor's dispatch schedule.
One `Access` value decides visibility for REST reads, webhook delivery *and* MQTT topic routing, so
the three cannot disagree, and it reaches the database as a `WHERE` clause rather than as a second
implementation of the rule.

A hidden object answers **`404`, never `403`** — a `403` confirms the object exists, which is
exactly what targeting conceals.

[Object privacy](@/docs/object-privacy.md) is the whole mechanism, and is worth reading before a
pilot.

## Authentication

Three backends, chosen because deployments genuinely disagree: the VTN's own OAuth2
client-credentials grant, JWT validation against somebody else's JWKS, or a pre-shared token behind
a gateway. [Authentication](@/docs/authentication.md) covers choosing between them.

Five refusals in the JWKS path carry the weight, each a documented way of getting JWT validation
wrong: the algorithm comes from the **key**, never the token; a symmetric key in a published key set
is refused outright; a key its publisher marked for *encryption* is not used to verify signatures;
`nbf` is validated; and a configured `aud` or `iss` is **required** rather than merely matched.

That last one is worth knowing if you rely on audience checking elsewhere: the usual JWT libraries
check a claim only when the token carries it, so an audience-*less* token passes an audience check.
[Authentication](@/docs/authentication.md#an-external-authorization-server) has the consequence for
your authorization server. Key rotation is a cache miss rather than a timer, rate limited so a
stream of nonsense tokens cannot be aimed at it.

**Secrets.** Client secrets are stored Argon2id-hashed, and an unknown `client_id` costs the same
verification as a known one so response time does not enumerate clients. Use `openadr hash-secret`
and `--client-hashed` rather than `--client`, which puts a plaintext secret in the process table.

That timing defence has a cost worth naming: it means an *unauthenticated* request to
`/auth/token` always pays a full Argon2id verification — 19 MiB and tens of milliseconds. So the
verification runs on a blocking thread, behind a semaphore whose permit count is the memory ceiling
(the machine's parallelism, capped at eight; `max_concurrent_verifications` changes it). Beyond it a
caller queues until the request timeout and gets a `503`, which says the VTN could not look rather
than that the secret was wrong. Run inline, a handful of concurrent token requests would occupy
every worker thread the VTN has — including the ones delivering dispatch instructions.

**Anonymous mode is read-only by construction**, not by a flag: an anonymous principal holds no
scopes and every write requires one.

## Webhooks: the threat model is the VTN itself

A subscriber names a URL, and the VTN then makes requests to it from inside your network. That is
server-side request forgery with the specification's blessing, so most of the webhook chapter is
refusals — all implemented:

- **The echo challenge runs before the subscription exists**, so one aimed at a third party is never
  created rather than created and found undeliverable.
- **HTTPS only, public addresses only.** Loopback, RFC 1918, link-local, carrier-grade NAT,
  multicast, reserved, IPv6 unique-local and IPv4-mapped-IPv6 are refused — at subscription time and
  again before every delivery.
- **What a name resolves to is checked inside the HTTP client's own resolver**, so the answer that
  is inspected is the answer the socket is opened against. Checking separately and letting the
  client resolve again is DNS rebinding with a check in front of it.
- **Redirects are not followed** — the same attack with an extra hop.
- **Payloads are signed over an instant as well as a body** — `v1.<unix seconds>.<body>`, HMAC-SHA256,
  with the instant in `X-OpenADR-Timestamp` — so a receiver can tell a genuine notification both
  from a forged one and from a *replayed* one. A signature over the body alone would be valid for
  ever. The verifier ships too, as `openadr::webhook` behind the `webhook-signature` feature: a
  subscriber should not compile a server to check a signature. See
  [Notifications](@/docs/notifications.md#verifying-a-delivery).

## MQTT: the broker enforces half of it

Per-VEN topics separate the *messages*. They do not separate the *subscribers* — a broker with no
access control hands any connected client any topic it names.

The specification requires the VTN to prevent that and declines to say how. This one answers the
broker's own authorization callbacks at `POST /internal/mqtt/auth` and `/internal/mqtt/acl`, in the
shape EMQX and `mosquitto-go-auth` already call. Authentication is the OpenADR token as the MQTT
password, checked through the same authenticator the REST API uses; authorization is derived from
the topic, so a VEN can only subscribe to a topic it could have discovered. Publishing is refused
for every client — the VTN is the only publisher.

> **Point your broker at those two endpoints, and expose them to the broker only.** Without them the
> topic layout is a convention rather than a boundary. [Notifications](@/docs/notifications.md) has
> the request shapes.

## Transport security

`--tls-cert` and `--tls-key` serve the API over TLS 1.2+ with ALPN offering h2 and http/1.1, so a
site controller needs no reverse proxy to be reachable over anything but plaintext. The certificate
is validated before the port opens.

`--tls-client-ca` refuses any connection whose client certificate that CA did not issue, during the
handshake and before a byte of HTTP is parsed. It is a **network gate, not an identity**: who the
caller *is* stays with the credential, because a VTN with two identity sources is a VTN with two
answers. The peer's chain reaches your application as a `PeerCertificates` request extension in DER;
parsing a subject is yours, with an X.509 library of your choosing.

That pair is what Fluvius' NetFlex profile expects: mutual TLS at the edge, a pre-shared token behind
it, no OAuth2.

```console
$ openadr vtn \
      --tls-cert /etc/openadr/fullchain.pem \
      --tls-key /etc/openadr/privkey.pem \
      --tls-client-ca /etc/openadr/clients-ca.pem \
      --database /var/lib/openadr/openadr.sqlite \
      --bl-token "$BL_TOKEN"
```

With TLS on, `GET /auth/server` and the mDNS record advertise `https://` rather than `http://`.

## Caching is a confidentiality question here

Object privacy makes a response a function of the *reader*: two VENs are entitled to different
targets on the same event at the same URL. So every read carries `Cache-Control: private, no-cache`
and `Vary: Authorization`, and every `POST /auth/token` response `Cache-Control: no-store`, which
RFC 6749 §5.1 requires of anything that may carry a token. `no-cache` means "revalidate", not "do not
store", so the `ETag` still saves the poll.

## Denial of service

A request body limit (8 MB by default) and a request timeout answering `504` are attached to the
router. Two limits sit below HTTP, both found by looking for work that scales with attacker input:
interval expansion refuses before allocating rather than after, and a repeating event's relevant
repetitions are found by arithmetic rather than by walking the sequence from wherever it began.

Rate limiting is an API gateway's job in the specification's security chapter, and it is not
implemented here.

## Left to the deployment

Stated plainly, because a reader who assumes otherwise will not configure it:

- **Certificate issuance and renewal.** The listener reads PEM files; ACME is not built in. A
  reverse proxy is still a perfectly good answer if you already have one.
- **Mapping a client certificate to a `clientID`.** The connection gate exists; the identity half
  needs an X.509 parser and is deliberately yours.
- **Rate limiting**, per the above.
- **Broker ACLs** must be pointed at the callback endpoints; the VTN answers them but cannot install
  them.
- **Encryption at rest** for the secrets the VTN holds on behalf of clients — subscription
  `bearerToken`s and webhook signing keys — is not implemented. Acceptable in memory; not acceptable
  for a durable backend holding third-party credentials.
- **Certification.** Nothing here has been through the Alliance test tool.
  [Status](@/docs/status.md) is the full list.

## Reporting a vulnerability

Please report privately through
[GitHub security advisories](https://github.com/hupe1980/openadr/security/advisories/new) rather
than in a public issue. Reports about object privacy or the notification outbox are the most
valuable, because both fail silently.
