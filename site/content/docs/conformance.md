+++
title = "Measuring a VTN"
description = "A black-box conformance suite that runs against any OpenADR 3.1 VTN, cites the clause behind every check, and never counts a skip as a pass."
weight = 105
+++

```console
$ openadr conformance --url https://vtn.example.com/openadr3/3.1.0 \
      --bl-token $BL --ven-token $VEN --ven-client-id ven-client-1
```

Every other test in this project proves that this implementation agrees with itself. Two systems can
each be perfectly self-consistent and refuse to talk to each other, so self-agreement is not
interoperability — it is the thing people mistake for it.

This is the measurement. It speaks nothing but HTTP, imports nothing from the server module, and
runs against any of them: the Alliance's reference VTN, another Rust one, or something written this
afternoon in Go
exactly as it runs against this one.

## What a result means

```text
pass  create-stamps-metadata        [API objectMetadata]      POST answers 201 and the VTN stamps id, …
FAIL  target-hiding-on-reads        [Def §Object Privacy]     a granted VEN sees only its own target …
      → the response carried "group2", a target this VEN was not granted. The full target list on
        an event is a competitor's dispatch schedule, and target hiding exists to conceal it
skip  mqtt-binding-shape            [Notifiers §7.2]          the MQTT binding names URIS, …
      ~ this VTN offers no MQTT notifier binding, which is optional

required 26 passed, 1 failed, 4 skipped
recommended 2 passed, 0 failed, 0 skipped
extension 3 passed, 0 failed, 0 skipped

NOT CONFORMANT: 1 required check(s) failed.
```

Three outcomes, and the third is what makes the other two worth reading.

| | |
|---|---|
| **pass** | The VTN did what the cited clause requires. |
| **FAIL** | It did something else. The message says what was expected and what arrived. |
| **skip** | The check could not be *attempted*, and says why. |

**A skip is never counted as a pass.** That is not pedantry. The usual headline for a conformance
run is "166 of 168 *applicable* cases", and the load-bearing word is the one in italics: a suite that
reports what it could not attempt as success, or quietly drops it from the denominator, produces a
figure nobody can act on — and the checks it drops are exactly the ones a peer has not implemented.

`--strict` makes any failure non-zero. Without it, only a **required** failure does.

## Three severities

| | |
|---|---|
| **required** | The specification says **SHALL** or **MUST**. A failure is non-conformance. |
| **recommended** | It says **SHOULD**, or leaves the choice open while naming a preference. A failure is a disagreement worth having. |
| **extension** | Not in the specification at all — something this project adds, checked so a report can say whether a peer happens to support it. A failure is *information*. |

Collapsing the three would make the report either uselessly harsh or uselessly forgiving. A VTN
without `ETag` support is not broken; it is a VTN whose pollers pay for every cycle.

## Every check cites a sentence

`[Def §Object Privacy]`, `[API eventRequest]`, `[UG §7.3]`, `[Notifiers §9.3]`.

This is the difference between an accusation and a bug report. "Your VTN failed
`target-hiding-on-reads`" invites an argument about the suite; "*a VTN will only include requested
targets in a response*, and yours returned `group2` to a VEN granted only `group1`" invites a fix.

A check that cannot name a sentence is asserting this project's opinion, and those are marked
`extension` — which a test enforces, so an opinion cannot be filed as a requirement by accident.

## It writes

Conformance is not observable from reads alone. Whether `id` in a request body is ignored, whether a
duplicate `programName` is refused, whether a cascade removes an event, whether a VEN can grant
itself a target — none of it is visible from a `GET`. A read-only suite is a suite that checks the
shape of a list.

So it creates programmes, events and VEN objects under names prefixed `oadr-conformance-`, and
deletes them afterwards in reverse order, **including when a check failed**.

> **Do not point it at a VTN whose data matters.**

Two tests hold the cleanup: one asserts nothing with the prefix survives a run, and the other runs
the suite twice against the same VTN — which is the property cleanup actually exists for, because
the second run trips over the first's uniqueness constraints otherwise.

## What it needs

| | |
|---|---|
| `--url` | The base URL, including the base path. Everything below is optional; without any credentials only the two unauthenticated checks run. |
| `--bl-token` / `--bl-client id:secret` | Business logic. Runs most of the suite. |
| `--ven-token` / `--ven-client id:secret`, and `--ven-client-id` | A VEN, **and** the `clientID` that credential authenticates as. |

The `clientID` cannot be discovered: a VTN maps a token to one "by means not specified here"
`[Def §VEN created object privacy]`. Without it the object-privacy checks skip — and object privacy
is the half of OpenADR 3.1 most worth measuring, so a run without VEN credentials is a much weaker
run, and the report says so on every line.

## What it checks

Forty-eight checks, in the order they run — discovery first, because a VTN that fails those will
fail everything else for the same reason.

- **Discovery** — `/auth/server` answers without credentials; `/notifiers` carries the `WEBHOOK`
  key; an unauthenticated write is refused.
- **Object lifecycle** — the VTN stamps `id` and the timestamps and ignores a client's; `PUT`
  advances `modificationDateTime` and leaves `createdDateTime` alone; a missing object is a `404`
  with a `problem` body; a duplicate `programName` is a `409`; an event naming a programme that does
  not exist is refused; deleting a programme leaves no event naming it.
- **Collections** — `limit` above 50 is refused rather than clamped; `skip`/`limit` walks the
  collection once with no gap or repeat; filters narrow together; both `?targets=` forms work.
- **Time and values** — `P9999Y` and `0001-01-01` survive a round trip *as sentinels*; intervals and
  payloads come back byte-identical; a price of `0.1` comes back as `0.1` rather than as
  `0.09999999999999999`.
- **Object privacy** — a VEN cannot write programmes or grant itself targets; untargeted objects are
  visible; ungranted ones are not, *including when the VEN names the target*; a granted VEN sees
  only its own target on the object; a hidden object is a `404` rather than a `403`; a VEN reads
  only its own VENs, its own subscriptions, the reports it filed, and the resources under the
  VENs it owns; and it cannot write targets onto a resource either, which is the half of the
  discriminator rule an implementation adds `/resources` without carrying across.
- **Reports** — a report a VEN files comes back with its resources, its interval ids, its decimal
  values and its `payloadDescriptors` intact; its `clientID` is stamped from the credential rather
  than copied from the body; `?eventID=` returns that event's reports and no others; and a report
  naming an event that does not exist is refused. Reports carry a customer's meter data and are the
  object a settlement process reads back months later, so a round trip that loses an interval id or
  rounds a number is a billing dispute rather than a warning.
- **The MQTT binding** — the shape of the binding, that collection-wide topics are business logic's,
  that a VEN gets its own, and that it cannot get another VEN's. All four skip when the VTN offers
  no broker, because MQTT is optional.
- **Query semantics** — `?active=true` drops an event whose intervals have all elapsed and
  *keeps* one scheduled for next week, because "active" is "has not transpired".
- **The HTTP contract** — a `401` carries `WWW-Authenticate`, so a refused client knows what to
  present (RFC 9110 §11.6.1); the token endpoint answers `Cache-Control: no-store`, which RFC 6749
  §5.1 requires of anything that may carry a token; and a body labelled with a media type the
  endpoint does not declare answers `415`. The last is *recommended* rather than required: the
  OpenADR documents say nothing about media-type negotiation, so a lenient VTN is interoperable
  rather than wrong.
- **Extensions** — `ETag`/`304`, `problem.instance` matching the request-id header, whether error
  bodies use RFC 9457's `application/problem+json` media type or the document's plain
  `application/json`, whether a read says how it may be cached (which matters because object privacy
  makes the body depend on who asked), and `?programName=`.

## A suite that cannot fail is not a suite

Forty-eight checks that all returned `pass` unconditionally would also pass against this VTN. So the
suite is run against **deliberately weakened** ones, and each test names which checks must catch that
weakening and asserts that nothing else fires. The weakenings: caching off; the programme-name lookup
off; the broker binding removed; a layer that accepts `?eventID=` and drops it; a proxy that strips
`WWW-Authenticate`, or `Cache-Control` from the token endpoint, or `Cache-Control` and `Vary` from a
read; and one that rewrites every inbound `Content-Type` to `application/json`.

There is a quieter version of the same mistake, and it took longer to find: a check that *can* fail,
against a state the suite never creates. `ven-reads-only-its-own-reports` asserted that a VEN's
report list held nothing belonging to another client — and nothing in the suite had ever written a
report, so against a fresh VTN it asserted that an empty list was empty. It had been added *because*
reports were the thinnest area. Reading a check tells you whether it is correct; it does not tell you
whether it has any data.

Self-agreement is the mistake this page exists to prevent, and building the instrument is not an
exemption from it.

## As a library

```rust
use openadr::conformance::{Credential, Runner, Target};

let target = Target::new("https://vtn.example.com/openadr3/3.1.0")
    .with_business_logic(Credential::Token(bl_token))
    .with_ven(Credential::Token(ven_token), "ven-client-1");

let report = Runner::new(target)?.run().await;
if !report.is_conformant() {
    for finding in report.failures() {
        eprintln!("{}: {}", finding.check.clause, finding.check.title);
    }
}
println!("{}", report.to_json());
```

`--json`, or `Report::to_json`, is what a published interoperability matrix is built from.

## One reading, and what it cost

On 2026-09-06 this was run against another OpenADR 3 VTN, in production at several DSOs.
`cargo test --all-features --test interop -- --ignored --nocapture` reproduces it: the harness starts
that VTN and a database in containers, gives it a first client, and points the suite at it.

**30 of 36 attempted required checks passed**, including object privacy in full: ownership
filtering, target grants, target hiding, `404`-rather-than-`403`, and a VEN reading only its own
VENs and subscriptions. Four MQTT checks skipped, because that VTN advertises no broker.

Six required failures, each a concrete interoperability hazard:

| | |
|---|---|
| `GET /notifiers` carries **no `WEBHOOK` key** | The key exists so a client can *tell* whether webhooks are available. A client asking gets no answer. |
| **`P9999Y` is normalised to `P9999Y0M0DT0H0M0S`** | The sentinel means "no end". Rewritten, it is an ordinary 9999-year span that a literal comparison does not recognise. |
| **An `intervalPeriod` without `start` is rejected** | `intervalPeriod` has no required fields in either 3.1.0 or 3.1.1, and the User Guide's own §7.4 example is an interval that overrides `duration` and inherits `start`. |
| **An event's intervals do not round-trip unchanged** | What a report quotes to correlate its data is the interval it was written against. |
| **`POST` does not stamp all of `id`, the timestamps and `objectType`** | A client cannot tell what it created from what it sent. |
| **The token endpoint answers no `Cache-Control: no-store`** | RFC 6749 §5.1 makes it a **MUST** of any response that may carry a token. |

### It found two things here, too

That is the half of the exercise that is easy to forget to expect, and it is the better argument for
doing it.

**A sentinel matched by spelling.** This crate recognised `P9999Y` as "forever" by comparing the
literal string. Given the normalised form it parsed an ordinary 9999-year span, which overflows the
representable range when added to any modern instant — so the event failed to expand and the VEN's
timeline recorded it as *skipped*. A perpetual price signal from that peer simply disappeared. And
because the value renders back as `P9999Y`, a debug print showed the right string; only the schedule
knew. The sentinel is now recognised by value and still written as the literal.

**A check that was a preference.** `programme-delete-cascades` asserted that deleting a programme
removes its events, cited a Definitions section that says nothing of the sort, and reported the peer
as non-conformant for refusing the delete instead. Cascading and refusing are both defensible; what
no reading permits is deleting the programme and leaving orphaned events. The check now tests
*that*, and both implementations pass it.

A conformance suite run only against its author never has to survive its own citations being read
back.

## If you run it

The disagreements are the interesting output. Some of them will be this suite's fault, and those are
the most useful of all — please send them either way.
