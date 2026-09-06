+++
title = "Object privacy"
description = "How OpenADR 3.1 keeps one VEN from seeing another's dispatch: ownership, target grants, target hiding, and how the rule reaches SQL and the push transports."
weight = 50
+++

The confidentiality property that matters in demand response is not "can an attacker read the
database". It is: **can a charge point operator learn which other groups its competitor is dispatched
into?** The full target list on an event is commercially sensitive — it is a competitor's dispatch
schedule — and OpenADR 3.1 added a mechanism to conceal it.

This page is the mechanism, and how this crate implements it. It is worth reading before a pilot.

## Two gates, not one

Objects are protected by one of two rules, never both, and confusing them is the mistake to avoid.

**Ownership** protects `ven`, `resource`, `report` and `subscription`. A VEN sees only the objects
whose `clientID` is its own; business logic sees all of them. The `clientID` is stamped from the
token when the object is created and is never read from a request body.

**Targeting** protects `program` and `event`. A VEN sees one only where a three-way intersection is
non-empty, and sees only that intersection.

The Definitions are explicit that target hiding is *not* performed on the ownership-gated objects,
"as these objects are read-able only by a specific VEN". So a VEN reads its own `ven` object with
its full target list — which is the list it needs, since it is its own grant.

## How targeting works

Three steps, in order.

**1. Business logic grants targets to a VEN**, by writing them on that VEN's `ven` object and on the
`resource` objects belonging to it. Only business logic may write targets there.

```console
$ curl -X POST …/vens -H "Authorization: Bearer $BL_TOKEN" \
       -H 'Content-Type: application/json' -d '{
    "objectType": "BL_VEN_REQUEST",
    "clientID": "cpo-a",
    "venName": "cpo-a-backend",
    "targets": ["cpo-a-sites", "region-north"]
  }'
```

The VEN's *grant* is the union of the targets on its `ven` object and on all its resources.

**2. Business logic targets an event.**

```json
{ "programID": "prg-1", "targets": ["cpo-a-sites", "cpo-b-sites"], "intervals": [ … ] }
```

**3. The VEN reads, naming what it wants.**

```console
$ curl "…/events?targets=cpo-a-sites" -H "Authorization: Bearer $VEN_TOKEN"
```

It sees the event only where

```text
requested targets  ∩  granted targets  ∩  the event's targets  ≠  ∅
```

and the response carries **only that intersection**:

```json
{ "id": "evt-1", "programID": "prg-1", "targets": ["cpo-a-sites"], "intervals": [ … ] }
```

`cpo-b-sites` is not merely filtered out of the list — it is removed from the object. The reader
cannot tell it exists. That is *target hiding*, and it is the point of the whole mechanism.

## A VEN cannot grant itself anything

The request body for a `ven` object is discriminated by `objectType`, and the two flavours are
different shapes:

| Discriminator | Required | Carries targets |
|---|---|---|
| `BL_VEN_REQUEST` | `clientID`, `venName` | yes |
| `VEN_VEN_REQUEST` | `venName` | **no such field** |

A VEN sending `{"objectType": "VEN_VEN_REQUEST", "venName": "x", "targets": ["gold"]}` is not
"rejected" — the body deserialises into a type that has nowhere to put `targets`. Sending the
BL-flavoured body instead is refused by scope. The privilege is unreachable rather than merely
unauthorised.

`resource` works the same way, with the additional rule that a `VEN_RESOURCE_REQUEST` naming a
`venID` that is not the caller's own is refused rather than silently redirected.

## Where an empty target list means different things

The specification says a VEN listing objects "may only read objects with targets by providing
matching targets" — so a VEN that names nothing sees no targeted object. But that reading cannot be
universal, and this crate distinguishes three cases:

| Situation | An empty request means |
|---|---|
| `GET /events` (a list read) | **Nothing targeted.** Name your targets or see only public objects. |
| `GET /events/{id}` (by id) | **Everything I am entitled to.** The id *is* the request. |
| A push notification | **Everything I am entitled to.** A subscription with no targets asks for everything it may receive, not for nothing. |

The by-id case matters in practice: a VEN following a link from a notification would otherwise be
unable to fetch the object the notification named. The grant is still intersected, so guessing ids
reveals nothing, and target hiding still applies to the response.

## `?targets=` is also a filter

`[Def §Response Filtering]` says targeting criteria "include only those objects that include target
terms found in the query", and that filters are additive. So naming a target returns **only** objects
carrying one of the named targets. An untargeted object is not gated by targeting, but it does not
carry the term either — reach it by naming no target.

On `/vens` and `/resources` the parameter is *only* a filter, with no privacy in it, because those
collections are gated by ownership.

## A hidden object is `404`, never `403`

A `403` confirms that the object exists, which is exactly what targeting conceals. So:

- `GET /events/{id}` for an event you may not see → `404`.
- `GET /reports/{id}` belonging to another VEN → `404`.
- `GET /notifiers/mqtt/topics/vens/{id}/…` for another VEN → `404`, so ids cannot be enumerated.

The specification does not address single-object reads; it says a list read returns an empty set.
This is the consistent extension.

## `ETag`s are computed after filtering

Two readers with different visibility of the same URL get different tags, because the tag covers the
rendered, privacy-filtered body. A shared tag would let one reader use `If-None-Match` to validate
another reader's view.

## How it is implemented, and why that matters

The rule is an intersection of three sets with an asymmetric output, and it has to hold identically
on three paths: REST reads, webhook delivery and MQTT routing. Three implementations means three
chances for one to leak, and **the leak is silent** — nothing errors when a VEN receives an event it
should not have.

So there is one type. `Access` is built once per request by one of three named constructors —
`list`, `by_id`, `push`, which are exactly the three readings of an empty target list above — and
answers two questions:

```rust
access.admits(&object.targets)          // may this be seen at all?
access.visible_targets(&object.targets) // which targets may be shown?
```

Three properties follow, and each closes a specific failure mode.

**`visible_targets` is defined in terms of `admits`.** Not checked against it by a test — derived
from it. There is one predicate, so the two cannot drift.

**The predicate reaches SQL as data.** `Access::target_filter` reduces the rule to a small enum a
`WHERE` clause renders mechanically, so a storage backend never re-derives it. That is what lets the
filter run *inside* the query rather than over a page that has already been cut — which is a
correctness property, not a performance one:

> A VEN granted `group1` that asks for `?targets=group1,group2` matches either target at the storage
> layer. A privacy filter applied afterwards drops every `group2` object from the page it was
> handed. With sixty `group2` events ahead of four `group1` ones, the first page of fifty comes back
> holding **one** of the VEN's four events — and a client that stops on a short page takes that as
> the end of the list.

**The push paths choose the gate once.** `Fanout::deliveries` resolves who owns an object before
consulting `Access`, so a caller cannot accidentally apply the targeting rule to a `report` — which
carries no targets at all, and would therefore be admitted to everybody.

The same choice decides what a write reads before it can announce itself. A targeted object may be
admitted by any VEN's grant, so the snapshot taken before the write covers the fleet; an owned one
reaches its owner and nobody else, so it is one indexed lookup.

## Verifying it

Three kinds of test, because the failure is silent and examples alone would not find it.

**Property tests** bound what `Access` can leak for *any* input: visible targets are always a subset
of the request, of the object and of the grant; a VEN never out-sees business logic; visibility is
monotone in the grant.

**A conformance suite** of 57 behaviours runs against all three storage backends, so a rule cannot
mean one thing in memory and another in SQL.

**End-to-end HTTP tests** drive the real router: a VEN in `group1` reading an event targeted at
`group1` and `group2` sees only `group1`; a VEN cannot list another's subscriptions or the tokens in
them; a partly-granted request still fills the first page.

**A test against a real broker.** An event targeted at `group1` is published, over a socket, and the
assertion is on the topics it reached: `group1`'s VEN got a copy carrying only `group1`, and
`group2`'s VEN topic is empty. A report gets the same treatment from the other gate — it reaches its
owner's topic and no other VEN's. A mock notifier proves neither half — the fan-out was computed
correctly and published nowhere for as long as this crate had topic endpoints and no publisher.

## MQTT: both halves

On a broker, object privacy has two halves.

**The VTN publishes a private per-VEN copy** of each object to that VEN's own topic, carrying only
that VEN's targets. The topic name comes from the same function the discovery endpoints render, so a
VEN cannot be told to watch a name the VTN never publishes to.

**The broker refuses a cross-VEN subscription**, and it asks the VTN whether to. The specification
requires the VTN to enforce topic access "by any means necessary" and declares the mechanism out of
scope; `POST /internal/mqtt/auth` and `/internal/mqtt/acl` are that mechanism, in the shape EMQX's
HTTP backends and `mosquitto-go-auth` already call.

The authorization answer is derived from the topic, so it is the *same question* the endpoint that
hands out the name answers: `…/vens/{venID}/…` is allowed when that VEN's `clientID` is the
connecting username, a wildcard where the `venID` belongs is not a VEN-scoped topic at all, and a
collection-wide topic needs a `clientID` the operator named. Publishing is refused for everyone; the
VTN is the only publisher.

Point the broker at those two endpoints. Without them a broker with no access control hands any
connected client any topic it names, and the per-VEN layout separates the *messages* without
separating the *subscribers*.

See [Notifications](@/docs/notifications.md) for the topic layout and the request shapes.
