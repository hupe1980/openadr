+++
title = "The domain core"
description = "Interval expansion, timelines and report scheduling: the deterministic, I/O-free layer that turns OpenADR's declarative timing into absolute windows."
weight = 80
+++

`openadr::core` is where the specification's time semantics live. It is pure: no I/O, no clock of
its own, no `std`. Time comes from an injected `Clock`, so a schedule that misbehaves in production
can be replayed exactly.

It is also the part of the crate a VEN needs most, and it is available without the server:

```console
$ cargo add openadr --no-default-features     # model + schema + core, no_std
```

## `IntervalExpander` — declared timing to absolute windows

An event says when things happen in a compressed, inheriting form. `IntervalExpander` resolves it.

```rust
use openadr::core::IntervalExpander;

let expander = IntervalExpander::at(now);
let intervals = expander.expand(&event)?;      // one pass
```

Each `ExpandedInterval` carries an absolute `start`, an `end` (or `None` for open-ended), the
interval `id` the event gave it — so reports can quote it — and the payloads in force.

Everything the specification says about timing is resolved here, once:

**Inheritance is per field.** `intervalPeriod` on the event is the default; an interval overrides
individual fields of it. An interval stating only `duration` still inherits its start.

**A missing start means "right after the previous interval".** An interval with no duration is
closed by the next interval's explicit start — and if the next one has none either, the event is
refused, because no VEN could place it in time.

**`0001-01-01` is `StartTime::Now`.** Not a date in the year 1. Resolved against the injected clock.

**`P9999Y` is `Duration::Forever`**, and yields an interval with `end: None`.

Both sentinels are enum variants rather than values. As a timestamp and a span they are one
forgotten comparison away from a schedule that runs for nine thousand years; as variants the match
arm has to exist, and the meaning stops depending on whether a date library can represent year 9999.

**A scalar payload carrying several values subdivides its interval.** Three prices in a `PT3H`
interval are three hourly sub-intervals, each carrying one price. Whether a payload type is scalar
comes from the Alliance's own enumeration files, compiled into `openadr::schema` — which is why
`schema` sits below `core`.

Boundaries are computed from the parent (`start + total·k/n`), not by accumulating a divided
duration, so an hour split three ways still ends exactly on the hour. A property test asserts that
for every count and every duration; it is the class of bug `duration / n` arithmetic produces and
that examples rarely catch.

### Windows and repetition

```rust
let intervals = expander.expand_window(&event, from, to)?;
```

`event.duration` longer than the sum of the intervals **loops** the sequence; shorter **truncates**
it. Twenty-four hourly prices with `duration: P7D` is a week of that curve.

The window bounds the *result*, not the intervals: an interval straddling `to` comes back whole,
because only `event.duration` may shorten one. Reporting a window-clipped interval would be a lie
about the schedule.

A repeating event's relevant repetitions are found by division, not by walking from wherever the
sequence began. For an event repeating every second since 2020 that is the difference between
arithmetic and two hundred million iterations, and it is reachable from any VEN building a timeline
over a stored event.

Every expansion is bounded by `IntervalExpander::DEFAULT_LIMIT` (10 000 intervals, configurable with
`with_limit`), checked *before* allocating rather than after.

## `Timeline` — many events, one schedule

A VEN enrolled in a programme may hold several concurrent events: a day-ahead price curve and a
short emergency curtailment. `event.priority` settles the conflict — **lower wins** — and the loser
is split *around* the winner rather than dropped, so the long event resumes when the short one ends.

```text
input   |------------------ day-ahead prices (priority 10) ------------------|
                   |-- curtailment (priority 0) --|

result  |--prices--|-------- curtailment ---------|-------- prices ----------|
```

```rust
use openadr::core::{IntervalExpander, Timeline};

let timeline = Timeline::build(
    events.iter().map(|e| (&e.id, &e.content)),
    &IntervalExpander::at(now),
    now,
    now + jiff::Span::new().hours(48),
);

if let Some(segment) = timeline.at(now) {
    apply(&segment.payloads);
}
let wake_at = timeline.next_change(now);
```

`next_change` is what a VEN should sleep until: a price change at 13:00 is then acted on at 13:00
rather than up to one poll interval later.

Two diagnostics rather than failures:

**`skipped()`** lists events that could not be expanded, with the reason. One malformed event must
not blind a VEN to the others.

**`overlaps_at_equal_priority()`** lists pairs that collided at the same priority. The specification
does not say which should win, so the result is deterministic (lowest id) *and* surfaced, because it
almost certainly means a mistake upstream.

Property tests state the invariants: segments never overlap, are never empty, cover only time some
event claimed, and the highest-priority event wins at every probed instant.

## `IntervalSequence` — the intervals an event has, including the ones it does not list

Most events declare their intervals. Two kinds do not, and both are in the User Guide:

* **A looping event.** `event.duration` longer than the intervals it lists repeats them — a
  twenty-four-hour tariff with `duration: P9999Y` is the canonical case (§7.3, *Looping intervals*).
* **An implied event.** No `intervals` at all, an `intervalPeriod` carrying a start and a tick, and
  report descriptors that count intervals the event never spells out. That is how a forty-eight-hour
  capability forecast is requested without forty-eight empty intervals in the body (§7.3, §8.7,
  §8.8).

`IntervalSequence` is all three shapes behind one index:

```rust
use openadr::core::IntervalExpander;

let sequence = IntervalExpander::at(now).sequence(&event)?;

sequence.declared();       // one repetition, as written
sequence.repeats();        // does it continue past the list?
sequence.get(50_000)?;     // interval 50 000, if it exists
```

Indexing is arithmetic, not iteration: interval `n` is position `n % len` of repetition `n / len`,
shifted by that many periods. A tariff that has looped hourly since 2020 is fifty thousand
repetitions in, and reaching today costs two multiplications rather than fifty thousand clones.
Truncation by the event's own `duration` happens here too, so the timeline, the report schedule and
the VTN's `?active=` window all see the same shortened last interval.

`expand_window` is this indexing plus a filter, which is why an event that loops means one thing
everywhere.

## `ReportSchedule` — when a VEN owes a report

A `reportDescriptor` is four integers with `-1` sentinels, a boolean and an enum, and between them
they express historical reports, forecasts, rolling windows, periodic batches, ad-hoc reports and
endless repetition. `ReportSchedule` resolves one against an `IntervalSequence` over a window:

```rust
use openadr::core::ReportSchedule;

let schedule = ReportSchedule::compute(&descriptor, &sequence, from, to);

for due in schedule.overdue(now, /* skip_stale */ false) {
    submit(due.covers_from, due.covers_to, &due.interval_ids);
}
let next = schedule.next_after(now);
```

Each `ReportDue` carries the instant it becomes due, the range it covers, and the **event's own
interval ids** — so a measurement can be lined up against the price or limit it answers.

The window bounds the *result*, not the schedule. `due.sequence` counts from the event's first
interval and is the same number whichever window found it, which is what lets a VEN remember what it
has already filed across a restart, a resync and a year of a repeating tariff.

`overdue(at, skip_stale)` is how a VEN that was offline catches up. With `skip_stale` only the most
recent window is returned, which is what a "do it now" event wants.

`is_ad_hoc()` is true for `frequency: 0`, where the VEN decides for itself, and for an event that
places nothing in time at all.

### Two ambiguities, named

User Guide §7.5 works `startInterval` two ways in two examples: once as the first *covered*
interval, once as the *generation point* with `numIntervals` counted backwards from it. The
generation-point reading is implemented, because it is the one that makes both fully-specified
examples work. `ScheduleOptions::start_interval_is_first_covered` selects the other for a deployment
that has settled on it.

§7.5's rolling timeline also draws "generate report N" at the end of the range each report covers,
in both directions, while §8.7 and §8.8 — the two complete worked forecast scenarios — say a
forecast is generated "when the first interval has begun". The prose wins: a forecast delivered at
the end of the window it forecasts is not a forecast. So a report is due at its **anchor interval's
boundary** — the end of it when reporting backwards, the start of it when reporting forwards — and
`startInterval: -1` means the far end *in the direction of travel*: the last interval for a
historical report, the first for a forecast.

The reasoning is in [Reading the specification](@/docs/spec-notes.md).

## Payload typing

`openadr::schema` is **all six** of the Alliance's enumeration files, generated by
`cargo xtask codegen` with a drift check in CI. Without that check a new payload type or a tightened
bound would simply never be enforced, and nothing would fail.

Four of the files describe a `valuesMap`, and become 75 typed specifications — value kinds,
cardinalities, numeric bounds, string lengths and string enumerations — grouped by where the value
is allowed to appear:

| `PayloadGroup` | Where | Count |
|---|---|---|
| `Event` | `event.intervals[].payloads` | 38 |
| `Report` | `report.resources[].intervals[].payloads` | 24 |
| `ProgramAttribute` | `program.attributes` | 8 |
| `VenAttribute` | `ven.attributes`, `resource.attributes` | 5 |

The group is what makes `USAGE` in an event interval a violation rather than a private extension:
the name is known, and it is known to belong somewhere else.

```rust
use openadr::schema::{self, PayloadGroup};

match schema::validate(&payload, PayloadGroup::Event) {
    schema::Validity::Valid => {}
    schema::Validity::Unknown => {}                     // a private type; legal
    schema::Validity::Invalid(violations) => { /* … */ }
}
```

The other two files constrain a *descriptor field* rather than a payload, so they are string tables:
`schema::units()` (`eventPayloadDescriptor.units`) and `schema::reading_types()`
(`reportPayloadDescriptor.readingType`). `Unit` and `ReadingType` are open enums over them — a value
outside the list is preserved as `Private` rather than refused, because the Definitions permit
private strings — and a test asserts their named variants are *exactly* those tables, so a value the
Alliance adds fails CI rather than degrading quietly.

Validation is a policy, not a rule: the Definitions place content validation on the client and make
private payload types explicitly legal. An unknown type is `Unknown`, never an error.

## Determinism

Everything here takes its time from a `Clock`. Tests use `FixedClock`; the VTN uses `SystemClock`.
No test in this crate sleeps to make an assertion true — the outbox is drained with an explicit
`drain()` that runs until nothing is due — so retry counts and schedules are exact rather than
timing-dependent.

A clock that is injected should also be one a test can *move*. A pinned clock answers "what happens
at this instant" and cannot answer "what happens after one", which is the only question that reaches
anything derived, cached or aged — a timeline built over a horizon, or a `0001-01-01` start that must
not be re-resolved when it is recomputed.

That is also what makes the layer `no_std`-capable: with no clock of its own and no I/O, `core`
needs nothing but an allocator. See [Embedding](@/docs/embedding.md).
