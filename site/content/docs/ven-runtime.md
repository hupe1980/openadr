+++
title = "The VEN runtime"
description = "Registration, conditional event sync, a maintained timeline, report scheduling and MQTT push hints — the loop a real VEN runs, with only the meter to write."
weight = 95
+++

```console
$ cargo add openadr --features ven
```

[The client](@/docs/client.md) is the transport. This is the loop on top of it: everything a VEN has
to do that is not business logic, and nothing that is.

```rust
use openadr::client::{Client, VirtualEndNode};
use openadr::ven::{VenConfig, VenRuntime};

let client = Client::<VirtualEndNode>::builder("https://vtn.example.com/openadr3/3.1.0")?
    .bearer_token(&token)
    .build()?;

let config = VenConfig::new("water-heater-7".parse()?)
    .with_resources(["water-heater".parse()?])
    .with_targets(["group1".parse()?])
    .with_poll_interval(Duration::from_secs(60));

let ven = VenRuntime::with_meter(client, config, HouseMeter);
ven.register().await?;

loop {
    ven.sync().await?;
    if let Some(segment) = ven.active_at(ven.now()) {
        act_on(&segment.payloads);
    }
    ven.submit_due_reports().await?;
    ven.wait_for_work().await;
}
```

That is the whole shape. The rest of this page is what each line is doing and why.

## Registration is idempotent

A VEN restarts. Re-registering must not create a second object, and must not fail because the first
one exists — `venName` is unique per VTN, so both would be errors a VEN cannot recover from on its
own.

`register()` looks the VEN up by name, creates one only if absent, and then reconciles the
configured resources: missing ones are created, existing ones are left alone, and **extra ones are
not deleted**. Another operator may have created them, and a runtime that removes what it did not
put there is a runtime nobody can share a VEN with.

It writes no targets, and could not: only business logic may grant them, and `VEN_VEN_REQUEST` has
no `targets` member to write them with. See [object privacy](@/docs/object-privacy.md).

`register()` also checks the clock — see below — so a VEN with a wrong one refuses before it does
anything else.

## Sync is conditional

```rust
let outcome = ven.sync().await?;
if outcome.changed {
    println!("+{} ~{} -{}", outcome.added.len(), outcome.updated.len(), outcome.removed.len());
}
```

One `GET /events` carrying the previous `ETag`. A cycle in which nothing changed is a **`304` with
no body**, which is the whole reason a VEN can poll a 48-hour window every minute without anybody
minding.

`SyncOutcome` reports what moved rather than making you diff it: `added`, `updated` (by
`modificationDateTime`) and `removed`. The last one matters most — **deleting an event is how
OpenADR cancels one**, and a VEN that missed that keeps curtailing for an instruction that has been
withdrawn.

Timelines are rebuilt when something changed — and when they are about to run out. They reach 48
hours ahead of the moment they were built, and a tariff that loops for ever never changes: every poll
after the first is a `304`. Rebuilding only on change meant that after two quiet days `active_at`
returned `None` and the VEN silently stopped following a signal the VTN was still publishing. The
runtime now rebuilds once half the horizon has elapsed, on the `304` path as well, which is
arithmetic over events it already holds.

**A `0001-01-01` start is anchored where it was read.** "Now" in the User Guide means "now from the
reader's point of view", and the reader's point of view is the moment the event arrived — not
whenever a timeline happens to be recomputed. The runtime records that instant per event, so a
one-hour "do it now" curtailment ends an hour after it arrived rather than an hour after the last
unrelated write anywhere in the VTN.

`sync_programs()` is separate because programmes change far less often than events do, and a VEN
polling its schedule every minute has no reason to re-read the tariff with it.

## The timeline answers the question a VEN actually asks

```rust
match ven.active_at(ven.now()) {
    Some(segment) => act_on(&segment.payloads),   // priority already resolved
    None => idle(),
}
```

`Timeline` merges a programme's concurrent events by priority and splits the loser *around* the
winner, so a long price curve resumes when a short curtailment ends — see
[the domain core](@/docs/domain-core.md).

A VEN enrolled in two programmes at once holds two timelines, and choosing between them is not
something the specification defines. `active_at` returns the highest-priority segment across all of
them; `active_segments` returns them all, for a VEN that would rather decide for itself.

### The wake-up is driven by the schedule

```rust
ven.wait_for_work().await;
```

The next transition, measured to the nanosecond and bounded by the poll interval —
`time_to_next_wakeup()` is that duration if nothing interrupts. Both halves matter: sleeping until
the poll interval would act on a 13:00 price change at 13:00:59, and sleeping until the next
transition alone would never learn about an event created in the meantime.

A [push hint](#push-as-a-hint) is the third thing that can end the wait, and a loop that awaits the
bare sleep instead simply never notices one.

### `randomizeStart` is applied here, and applied deterministically

The specification puts `randomizeStart` on the VEN, because the point of it is that a fleet does not
all switch on the same second — a VTN that applied it would defeat that.

The offset is derived from `VenConfig::randomization_seed` and the interval's identity, **not drawn
fresh**. A VEN that restarts mid-event and re-randomizes jumps, which is worse for the grid than not
randomizing at all.

Give every unit in a fleet a different seed. Deploying one image with one seed randomizes
identically, which is a fleet that has not randomized.

## Reporting

The runtime knows *when* a report is due and *which* intervals it covers. What the meter reads is
the deployment's, and it arrives through one trait:

```rust
#[async_trait]
impl Meter for HouseMeter {
    async fn read(&self, due: &DueReport) -> Result<Vec<ReportResource>, VenError> {
        Ok(vec![ReportResource {
            resource_name: "water-heater".parse().unwrap(),
            interval_period: None,
            intervals: due.interval_ids.iter()
                .map(|id| Interval::new(*id, vec![
                    ValuesMap::new(due.payload_type.clone(), vec![read_register(*id)])
                ]))
                .collect(),
        }])
    }
}
```

`DueReport` carries the event, the payload type and reading type the descriptor asked for, the
interval ids to quote — report intervals quote the *event's* ids so the VTN can correlate them — and
the window covered.

**Returning an empty vector leaves the report due.** A meter that is briefly unavailable costs a
cycle rather than a window, because skipping without marking the report sent is the difference
between a late report and a missing one.

`NoMeter` is the default and reports nothing, which is right for a VEN that only follows prices.

### Reports the event never spelled out

Two of the User Guide's worked scenarios ask for reports against intervals no event lists, and both
work here:

- **A looping tariff** — twenty-four hourly intervals with `duration: P9999Y` and `repeat: -1` —
  produces a report every day, for ever. `due.sequence` counts from the event's first interval, so
  the report owed on day 365 is number 364 whichever cycle finds it.
- **A capability forecast** (§8.7) — an `intervalPeriod` with a start and a tick, no `intervals` at
  all, and a descriptor asking for `numIntervals: 48` — produces an hourly rolling forty-eight-hour
  forecast. The interval structure is *implied* by the period; the descriptor supplies the count.
  The specification leaves the ids of implied intervals to the VEN, so `interval_ids` is empty.

A forecast becomes due when the window it covers **opens**, not when it closes: a forty-eight-hour
forecast delivered forty-eight hours late is a historical record. See
[the domain core](@/docs/domain-core.md#two-ambiguities-named) for which sentence that comes from.

`VenConfig::report_catchup` is how far back a cycle looks — 24 hours by default. It only has to
cover downtime; a report stays on offer until it is filed, and what stops a *refile* is the memory
below.

### Each window is filed once, across restarts

Filing the same report twice is a duplicate in somebody's settlement data, and nothing on the VTN
side deduplicates. Remembering what has been sent is the VEN's job:

```rust
// after each cycle
std::fs::write("ven-state.json", serde_json::to_vec(&ven.exported_state())?)?;

// on the next start, before register()
ven.restore(serde_json::from_str(&std::fs::read_to_string("ven-state.json")?)?);
```

`VenState` is deliberately small: the VEN object's id, and which reports have been filed. Everything
else — the events, the timelines, the schedule — is re-derived from the VTN on the first sync, and
losing it costs one request.

## Push, as a hint

3.1.0 added notifications over a broker so a VEN behind a residential firewall can be told about a
change at all. This runtime subscribes — and treats what arrives as a *hint*, never as data:

```rust
let push = MqttPush::connect(&ven, Default::default()).await?;   // None if the VTN has no broker
loop {
    ven.sync().await?;
    ven.submit_due_reports().await?;
    ven.wait_for_work().await;    // returns early when a notification arrives
}
```

`MqttPush` reads `GET /notifiers` for the broker and `/notifiers/mqtt/topics/vens/{venID}/…` for the
topic names — discovered from the VTN, never constructed — connects with the OpenADR access token as
the broker password, and calls `Waker::wake` on every message. **The payload is never
deserialised.** Waking shortens the sleep; the conditional sync above re-reads from the VTN.

That is worth being deliberate about:

- **The broker is a third party.** Acting on a payload would mean taking a dispatch instruction from
  whoever can write to a topic. Waking and re-reading takes it from the VTN over an authenticated
  channel, so the worst a compromised or misconfigured broker achieves is a VEN that polls too
  often.
- **A missed message is not a missed event.** The poll loop still runs. A broker that is down,
  wrong, or absent costs latency and never correctness — which is why `connect` returns `Ok(None)`
  rather than an error when the VTN offers no broker.
- **The `GET` happens anyway.** The sync is `ETag`-conditional, so a spurious hint costs a `304` and
  a real one costs the read that was going to happen at the next poll regardless.

`Waker` is a plain handle. Anything that learns about a change sooner than the poll interval would —
a webhook endpoint of your own, a message from a building controller, a button on the front panel —
can call `wake()`.

Needs `features = ["ven", "mqtt"]`. `mqtt` does not pull in the server.

## The clock

Every interval in OpenADR is an **absolute instant**. A VEN whose clock is wrong curtails at the
wrong time and reports compliance it did not achieve, confidently. Fluvius' NetFlex profile requires
NTP drift within five seconds for exactly this reason.

`check_clock()` compares the VTN's `Date` header with the local clock and refuses beyond
`VenConfig::max_clock_skew`. `register()` calls it first, so a VEN with a wrong clock stops before it
does anything.

A VTN behind a proxy that strips `Date` cannot be checked, and that reads as "unknown" rather than
"agreed": refusing to run against one would be refusing to run against a correct deployment.

## What is deliberately not in it

**A webhook receiver.** The MQTT subscriber is [here](#push-as-a-hint); an inbound HTTP endpoint is
not, because a VEN that can accept one is a VEN that could have been reached without a broker in the
first place. `Waker` is public, so wiring your own handler to it is three lines.

**Anything that acts on a notification's payload.** Deliberately, and permanently: see
[Push, as a hint](#push-as-a-hint).

**A customer-override state machine.** AHRI 1380 wants one, reporting `OPERATING_STATE`. It is
arguably the appliance's rather than the runtime's, and it can be reported through `Meter` today.

**Anything that decides what the load does.** That is the whole point of the split.
