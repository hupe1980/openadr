+++
title = "What OpenADR is"
description = "What OpenADR 3.1 is and how it works: VTN and VEN, the six objects, targets, intervals and reports, and what changed from OpenADR 2.0b and 3.0."
weight = 20
+++

Enough of the protocol to read the rest of this documentation, and to know what you are looking at
in a packet capture. No Rust in this page.

## The problem it solves

Electricity has to be generated at the instant it is used. Historically the grid solved that by
moving generation to follow demand. That is getting harder — wind and solar are not dispatchable,
and the peaks are getting sharper — so the other half of the equation has become interesting:
moving *demand* to follow generation.

Doing so means a utility, grid operator or aggregator has to tell a heat pump, a battery, a
building, or ten thousand car chargers something like *charge later*, *stay under 4 kW between five
and eight*, or *here is what electricity costs each hour tomorrow*. OpenADR is the standard message
format and protocol for saying it.

It is published by the [OpenADR Alliance](https://www.openadr.org/), and it is deployed at scale:
Dutch DSOs use it for grid-aware EV charging, Belgian Fluvius for large DER connections, California
for public tariff distribution.

## The two roles

**VTN** — *Virtual Top Node.* The server. A utility, DSO or aggregator runs one. It publishes
programmes and events, and stores the reports that come back. Everything in this documentation
about "the server" means a VTN.

**VEN** — *Virtual End Node.* The client. A charge point operator's backend, a building management
system, a battery inverter, a home gateway. It reads the events that apply to it, acts, and reports
what it did.

The word for the third party is **business logic** (often *BL*): whatever inside the utility decides
which events to publish. OpenADR does not define it — forecasting, optimisation and settlement are
somebody else's problem. It defines only how the resulting instruction reaches the VEN.

A VEN is a *client* in the HTTP sense: it makes the requests. Push notifications exist, but the
notification only says "something changed" — the VEN still fetches the object.

## The six objects

Everything on the wire is one of six things, each addressable at `/{collection}` and
`/{collection}/{id}`.

| Object | What it is |
|---|---|
| **program** | The container. A tariff, a flexibility product, a curtailment scheme. Has a name, unique per VTN. |
| **event** | An instruction or a price curve, belonging to exactly one programme. This is the payload of the protocol. |
| **report** | What a VEN did, or measured. Answers a specific event. |
| **subscription** | "Call this URL when something I care about changes." The webhook registration. |
| **ven** | A VEN's own record on the VTN: its name, and the targets business logic has granted it. |
| **resource** | A device behind a VEN — one charge point of a hundred, one battery of a fleet. |

Every one carries VTN-assigned metadata: `id`, `createdDateTime`, `modificationDateTime`,
`objectType`. A client never sets those, which is why this crate models requests and responses as
different types.

Pagination is `?skip=` and `?limit=`, with `limit` capped at 50. Filters are additive.

## An event, in detail

An event is where the specification's subtlety lives. A minimal one:

```json
{
  "programID": "prg-1",
  "eventName": "evening peak",
  "priority": 0,
  "targets": ["feeder-north"],
  "intervalPeriod": { "start": "2026-02-11T17:00:00Z", "duration": "PT1H" },
  "intervals": [
    { "id": 0, "payloads": [{ "type": "IMPORT_CAPACITY_LIMIT", "values": [4.0] }] },
    { "id": 1, "payloads": [{ "type": "IMPORT_CAPACITY_LIMIT", "values": [2.5] }] },
    { "id": 2, "payloads": [{ "type": "IMPORT_CAPACITY_LIMIT", "values": [4.0] }] }
  ]
}
```

Read as: on the northern feeder, from 17:00, hold import under 4 kW for an hour, 2.5 kW for the
next, 4 kW for the next.

Five rules govern how that becomes absolute time.

**Timing is inherited per field.** `intervalPeriod` on the event is the default; an interval may
override any field of it. An interval that states only `duration` still inherits its start.

**A missing start means "right after the previous interval".** That is how the three intervals above
land at 17:00, 18:00 and 19:00 without saying so.

**Two magic values.** `start` of `0001-01-01` means *now*, resolved against the reader's clock —
a "do it now" event whose first interval began before anyone read it. `duration` of `P9999Y` means
*forever*. Both look like ordinary values and are not.

**`event.duration` loops or truncates.** Longer than the sum of the intervals, and the sequence
repeats until it is used up — twenty-four hourly prices with `duration: P7D` is a week of that
curve. Shorter, and the sequence is cut off.

**A scalar payload carrying several values subdivides its interval.** Three prices in a `PT3H`
interval are three hourly sub-intervals. Whether a payload type is scalar is not a guess: the
Alliance publishes an enumeration file saying so for every payload type.

`priority` resolves overlaps — **lower wins** — and the loser is split *around* the winner rather
than replaced, so a long price curve resumes after a short curtailment ends.

## Targets, and why they are a privacy feature

A `target` is a plain string: `feeder-north`, `871685900000000000` (a Dutch EAN18 connection code),
`BATTERY-*`. Programmes and events may carry them; so may `ven` and `resource` objects.

Targeting is how a VTN addresses a subset of its VENs — but its more important job is
**concealment**. Consider a DSO whose VTN serves eight competing charge point operators. An event
targeted at `cpo-a-sites` and `cpo-b-sites` must be visible to both, and neither may learn that the
other exists on that event. The full target list *is* commercially sensitive: it is a competitor's
dispatch schedule.

So the specification defines a three-way intersection. Business logic *grants* targets to a VEN by
writing them on that VEN's `ven` and `resource` objects — only business logic may write them there.
A VEN reading `/events` must then name the targets it wants, and it sees an event only where

```text
requested targets  ∩  granted targets  ∩  the event's targets  ≠  ∅
```

and the response shows **only that intersection**, never the event's full set. That last part is
called *target hiding*, and it is the whole point.

Objects with no targets are ungated: a public tariff is visible to everybody.

This crate implements the rule in exactly one place, consulted by REST reads, webhook delivery and
MQTT routing alike — see [Object privacy](@/docs/object-privacy.md), which is worth reading before a
pilot.

## Reports

A `reportDescriptor` on an event says what the VTN wants back and when. Four integers with
sentinels — `startInterval`, `numIntervals`, `frequency`, `repeat` — a boolean, `historical`, and
`reportIntervals`, which says whether the VEN may subdivide the event's intervals or choose its own.
Between them they express historical reports, forecasts, rolling windows, periodic batches, ad-hoc
reports and endless repetition. `-1` means "the default for this field"; `frequency: 0` means the
VEN decides.

The VEN then `POST`s a `report` naming the `eventID` and reusing the event's interval `id`s, so a
measurement can be lined up against the price or limit it answers.

This crate resolves a descriptor into a list of due times and covered intervals
(`ReportSchedule`); see [The domain core](@/docs/domain-core.md).

## Push, when polling is not enough

Two mechanisms, both optional, both of which only *announce* — the VEN still fetches.

**Webhooks.** A VEN creates a `subscription` naming a callback URL and which object types and
operations it cares about. The VTN `POST`s a `notification` there. Because the callback URL is
supplied by the client and fetched by the server, the specification's own security chapter is mostly
about server-side request forgery.

**MQTT**, added in 3.1. `GET /notifiers` describes the broker; a family of
`/notifiers/mqtt/topics/…` endpoints hand out topic names. The interesting ones are *VEN-scoped*:
each VEN gets a private topic carrying only what it is entitled to, with only its own targets on it.

In practice **polling dominates**. Every large deployment surveyed for this project polls — the
Netherlands re-reads a rolling 48-hour window, Belgium polls its schedule, California polls hourly —
which is why HTTP caching is treated here as a first-class feature rather than an optimisation.

## Authentication

OAuth2 client credentials, with a scope model that is also the role model — the specification never
labels a token "BL" or "VEN"; the distinction falls out of which scopes it carries.

| Scope | Grants |
|---|---|
| `read_all`, `read_bl` | Business logic's unrestricted read |
| `read_targets` | A VEN reading targeted programmes and events |
| `read_ven_objects` | A VEN reading its own `ven`, `resource`, `report`, `subscription` objects |
| `write_programs`, `write_events` | Business logic |
| `write_reports`, `write_subscriptions`, `write_vens` | A VEN, on its own objects |

A VTN publishing only public information may serve **anonymous** readers; the specification says so
explicitly, and it is what Californian price servers do.

## What changed, and when

| Version | |
|---|---|
| **2.0b** | SOAP and XML. Still deployed, entirely different on the wire, and out of scope for this crate. |
| **3.0** (2024) | The REST/JSON redesign. Introduced `program`. |
| **3.1** (2025) | MQTT notifiers, object privacy, string targets (3.0 used key/value pairs), `/resources` promoted to a top-level collection, `/auth/server`, compact multi-value payloads. |

**3.1 is not backwards compatible with 3.0**, despite the specification's own claim of semantic
versioning — endpoints, request bodies and query parameters all changed, and the 3.1.0 changelog
says so. This crate implements 3.1 only; peers that bend the schema are handled by explicit adapters
at the edge rather than by loosening the model.

## Next

- [Getting started](@/docs/getting-started.md) — a VTN running, with a programme and an event on it.
- [Object privacy](@/docs/object-privacy.md) — the targeting rule in full, including where the
  specification is ambiguous.
- [Reading the specification](@/docs/spec-notes.md) — every place this implementation had to choose
  between two defensible readings.
