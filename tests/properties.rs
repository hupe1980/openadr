//! Properties that must hold for every input, not just the worked examples.
//!
//! The domain core is where a subtle error is most expensive and least visible: a timeline that
//! double-books a minute, an expansion that loses a second to rounding, a target that leaks into a
//! response. Each of those is a statement about *all* inputs, so each is stated as one here.

use openadr::{
    core::{Access, Grant, IntervalExpander, ReportSchedule, Role, Timeline, active_window},
    model::{
        ClientId, Duration, EventRequest, Interval, IntervalPeriod, ObjectId, Priority, StartTime,
        Target, Timestamp, Value, ValuesMap,
    },
};
use proptest::prelude::*;
use rust_decimal::Decimal;

const EPOCH: &str = "2026-01-01T00:00:00Z";

fn epoch() -> Timestamp {
    EPOCH.parse().unwrap()
}

fn at(offset_secs: i64) -> Timestamp {
    epoch() + jiff::Span::new().seconds(offset_secs)
}

fn price(v: i64) -> ValuesMap {
    ValuesMap::single("PRICE".parse().unwrap(), Value::Number(Decimal::from(v)))
}

/// A payload carrying `n` values, which a scalar type subdivides into `n` sub-intervals.
fn prices(n: usize) -> ValuesMap {
    ValuesMap::new(
        "PRICE".parse().unwrap(),
        (0..n)
            .map(|i| Value::Number(Decimal::from(i as i64)))
            .collect(),
    )
}

fn expander() -> IntervalExpander {
    IntervalExpander::at(epoch())
}

// ---------------------------------------------------------------------------
// Interval expansion
// ---------------------------------------------------------------------------

proptest! {
    /// Contiguous intervals abut exactly: no gap, no overlap, no drift.
    #[test]
    fn contiguous_intervals_tile_the_event(
        count in 1usize..40,
        seconds in 1i64..100_000,
    ) {
        let mut event = EventRequest::new(ObjectId::new("p").unwrap());
        event.interval_period = Some(IntervalPeriod::new(
            StartTime::At(epoch()),
            Duration::from_secs(seconds).unwrap(),
        ));
        event.intervals = Some(
            (0..count).map(|i| Interval::new(i as i32, vec![price(i as i64)])).collect(),
        );

        let out = expander().expand(&event).unwrap();
        prop_assert_eq!(out.len(), count);
        prop_assert_eq!(out[0].start, epoch());
        for w in out.windows(2) {
            prop_assert_eq!(w[0].end.unwrap(), w[1].start, "a gap or overlap opened up");
        }
        prop_assert_eq!(out.last().unwrap().end.unwrap(), at(seconds * count as i64));
    }

    /// Sub-intervals tile their parent exactly, whatever the division.
    ///
    /// This is where naive `duration / n` arithmetic loses time: three equal parts of an hour are
    /// 1200 seconds each, but seven are not a whole number of anything.
    #[test]
    fn sub_intervals_tile_their_parent(
        values in 2usize..24,
        seconds in 1i64..100_000,
    ) {
        let mut event = EventRequest::new(ObjectId::new("p").unwrap());
        event.interval_period = Some(IntervalPeriod::new(
            StartTime::At(epoch()),
            Duration::from_secs(seconds).unwrap(),
        ));
        event.intervals = Some(vec![Interval::new(0, vec![prices(values)])]);

        let out = expander().expand(&event).unwrap();
        prop_assert_eq!(out.len(), values);
        prop_assert_eq!(out[0].start, epoch());
        for w in out.windows(2) {
            prop_assert_eq!(w[0].end.unwrap(), w[1].start);
        }
        prop_assert_eq!(
            out.last().unwrap().end.unwrap(),
            at(seconds),
            "sub-intervals must end exactly where the parent ends"
        );
        // Each sub-interval carries exactly one of the parent's values.
        for piece in &out {
            prop_assert_eq!(piece.payloads[0].values.len(), 1);
        }
    }

    /// Expansion is idempotent: expanding twice gives the same answer.
    #[test]
    fn expansion_is_deterministic(count in 1usize..20, seconds in 1i64..10_000) {
        let mut event = EventRequest::new(ObjectId::new("p").unwrap());
        event.interval_period = Some(IntervalPeriod::new(
            StartTime::At(epoch()),
            Duration::from_secs(seconds).unwrap(),
        ));
        event.intervals = Some(
            (0..count).map(|i| Interval::new(i as i32, vec![price(i as i64)])).collect(),
        );
        prop_assert_eq!(expander().expand(&event).unwrap(), expander().expand(&event).unwrap());
    }

    /// The active window spans exactly the intervals, however they are ordered.
    #[test]
    fn the_active_window_covers_every_interval(
        offsets in prop::collection::vec(0i64..50_000, 1..12),
        seconds in 1i64..1_000,
    ) {
        let mut event = EventRequest::new(ObjectId::new("p").unwrap());
        // Every interval carries its own start, so the list may be out of order and may overlap.
        event.intervals = Some(
            offsets
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    Interval::new(i as i32, vec![price(1)]).with_period(IntervalPeriod::new(
                        StartTime::At(at(*o)),
                        Duration::from_secs(seconds).unwrap(),
                    ))
                })
                .collect(),
        );

        let (start, end) = active_window(&event, epoch()).unwrap().unwrap();
        let expected_start = at(*offsets.iter().min().unwrap());
        let expected_end = at(offsets.iter().max().unwrap() + seconds);
        prop_assert_eq!(start, expected_start, "the window must start at the earliest interval");
        prop_assert_eq!(end, Some(expected_end), "and end at the latest");
    }
}

// ---------------------------------------------------------------------------
// Timeline
// ---------------------------------------------------------------------------

/// A set of events at varying priorities, all inside a bounded window.
fn overlapping_events() -> impl Strategy<Value = Vec<(u32, i64, i64)>> {
    prop::collection::vec(
        (0u32..5, 0i64..20, 1i64..20).prop_map(|(p, start, len)| (p, start, len)),
        1..8,
    )
}

proptest! {
    /// Whatever the inputs, the resolved timeline never double-books an instant.
    #[test]
    fn timeline_segments_never_overlap(specs in overlapping_events()) {
        let events: Vec<(ObjectId, EventRequest)> = specs
            .iter()
            .enumerate()
            .map(|(i, (priority, start, len))| {
                let mut e = EventRequest::new(ObjectId::new("p").unwrap());
                e.priority = Priority::new(*priority);
                e.interval_period = Some(IntervalPeriod::new(
                    StartTime::At(at(start * 3600)),
                    Duration::from_secs(len * 3600).unwrap(),
                ));
                e.intervals = Some(vec![Interval::new(0, vec![price(i as i64)])]);
                (ObjectId::new(format!("e{i:03}")).unwrap(), e)
            })
            .collect();

        let timeline = Timeline::build(
            events.iter().map(|(id, e)| (id, e)),
            &expander(),
            epoch(),
            at(1_000 * 3600),
        );

        let segments = timeline.segments();
        for w in segments.windows(2) {
            let end = w[0].end.unwrap_or(Timestamp::MAX);
            prop_assert!(
                end <= w[1].start,
                "segments {:?}..{:?} and {:?}.. overlap",
                w[0].start, w[0].end, w[1].start
            );
        }
        // And every segment is non-empty.
        for s in segments {
            prop_assert!(s.end.is_none_or(|e| e > s.start), "empty segment at {:?}", s.start);
        }
    }

    /// The timeline invents no time: every resolved instant was covered by some input event.
    #[test]
    fn timeline_covers_only_time_some_event_claimed(specs in overlapping_events()) {
        let events: Vec<(ObjectId, EventRequest)> = specs
            .iter()
            .enumerate()
            .map(|(i, (priority, start, len))| {
                let mut e = EventRequest::new(ObjectId::new("p").unwrap());
                e.priority = Priority::new(*priority);
                e.interval_period = Some(IntervalPeriod::new(
                    StartTime::At(at(start * 3600)),
                    Duration::from_secs(len * 3600).unwrap(),
                ));
                e.intervals = Some(vec![Interval::new(0, vec![price(i as i64)])]);
                (ObjectId::new(format!("e{i:03}")).unwrap(), e)
            })
            .collect();

        let timeline = Timeline::build(
            events.iter().map(|(id, e)| (id, e)),
            &expander(),
            epoch(),
            at(1_000 * 3600),
        );

        for segment in timeline.segments() {
            let covered = specs.iter().any(|(_, start, len)| {
                let s = at(start * 3600);
                let e = at((start + len) * 3600);
                segment.start >= s && segment.end.is_none_or(|x| x <= e)
            });
            prop_assert!(covered, "segment at {:?} was not claimed by any event", segment.start);
        }
    }

    /// At any instant, the winner is the highest-priority event covering it.
    #[test]
    fn the_highest_priority_event_always_wins(specs in overlapping_events(), probe in 0i64..40) {
        let events: Vec<(ObjectId, EventRequest)> = specs
            .iter()
            .enumerate()
            .map(|(i, (priority, start, len))| {
                let mut e = EventRequest::new(ObjectId::new("p").unwrap());
                e.priority = Priority::new(*priority);
                e.interval_period = Some(IntervalPeriod::new(
                    StartTime::At(at(start * 3600)),
                    Duration::from_secs(len * 3600).unwrap(),
                ));
                e.intervals = Some(vec![Interval::new(0, vec![price(i as i64)])]);
                (ObjectId::new(format!("e{i:03}")).unwrap(), e)
            })
            .collect();

        let timeline = Timeline::build(
            events.iter().map(|(id, e)| (id, e)),
            &expander(),
            epoch(),
            at(1_000 * 3600),
        );

        let instant = at(probe * 3600);
        let best = specs
            .iter()
            .filter(|(_, start, len)| probe >= *start && probe < start + len)
            .map(|(p, _, _)| *p)
            .min();

        match (timeline.at(instant), best) {
            (Some(segment), Some(expected)) => {
                prop_assert_eq!(segment.priority.value(), Some(expected));
            }
            (None, None) => {}
            (found, expected) => prop_assert!(
                false,
                "timeline says {:?} at {probe}h, events say priority {:?}",
                found.map(|s| s.priority.value()), expected
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Object privacy
// ---------------------------------------------------------------------------

fn targets(names: &[&str]) -> Vec<Target> {
    names.iter().map(|n| Target::new(*n).unwrap()).collect()
}

fn label(i: usize) -> Target {
    Target::new(format!("g{i}")).unwrap()
}

proptest! {
    /// A reader never sees a target it did not ask for, nor one the object does not carry.
    ///
    /// This is target hiding stated as an invariant rather than as three examples: the visible set
    /// is always a subset of both the request and the object.
    #[test]
    fn visible_targets_are_a_subset_of_both_the_request_and_the_object(
        granted in prop::collection::vec(0usize..8, 0..5),
        requested in prop::collection::vec(0usize..8, 0..5),
        on_object in prop::collection::vec(0usize..8, 0..5),
    ) {
        let role = Role::Ven {
            client_id: ClientId::new("c").unwrap(),
            grant: Grant::from_targets(granted.iter().map(|i| label(*i))),
        };
        let requested: Vec<Target> = requested.iter().map(|i| label(*i)).collect();
        let object: Vec<Target> = on_object.iter().map(|i| label(*i)).collect();

        let access = Access::list(role, requested.clone());
        if let Some(visible) = access.visible_targets(&object) {
            for t in &visible {
                prop_assert!(object.contains(t), "leaked a target the object does not carry");
                if !object.is_empty() {
                    prop_assert!(requested.contains(t), "leaked a target nobody asked for");
                    prop_assert!(
                        granted.iter().any(|i| &label(*i) == t),
                        "leaked a target that was never granted"
                    );
                }
            }
        }
    }

    /// A VEN can never see more than business logic can.
    #[test]
    fn a_ven_never_out_sees_business_logic(
        granted in prop::collection::vec(0usize..8, 0..5),
        requested in prop::collection::vec(0usize..8, 1..5),
        on_object in prop::collection::vec(0usize..8, 0..5),
    ) {
        let requested: Vec<Target> = requested.iter().map(|i| label(*i)).collect();
        let object: Vec<Target> = on_object.iter().map(|i| label(*i)).collect();

        let ven = Access::list(
            Role::Ven {
                client_id: ClientId::new("c").unwrap(),
                grant: Grant::from_targets(granted.iter().map(|i| label(*i))),
            },
            requested.clone(),
        );
        let bl = Access::list(Role::BusinessLogic, requested);

        if ven.admits(&object) {
            prop_assert!(bl.admits(&object), "a VEN saw an object business logic could not");
        }
    }

    /// Adding a grant never takes visibility away, and removing one never adds it.
    #[test]
    fn visibility_is_monotone_in_the_grant(
        base in prop::collection::vec(0usize..6, 0..4),
        extra in 0usize..6,
        requested in prop::collection::vec(0usize..6, 1..4),
        on_object in prop::collection::vec(0usize..6, 1..4),
    ) {
        let requested: Vec<Target> = requested.iter().map(|i| label(*i)).collect();
        let object: Vec<Target> = on_object.iter().map(|i| label(*i)).collect();

        let with = |grant: Vec<Target>| {
            Access::list(
                Role::Ven { client_id: ClientId::new("c").unwrap(), grant: Grant::from_targets(grant) },
                requested.clone(),
            )
        };
        let small: Vec<Target> = base.iter().map(|i| label(*i)).collect();
        let mut large = small.clone();
        large.push(label(extra));

        if with(small).admits(&object) {
            prop_assert!(with(large).admits(&object), "a wider grant lost visibility");
        }
    }
}

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

proptest! {
    /// Every duration this crate can build renders as ISO 8601 and parses back to itself.
    #[test]
    fn durations_round_trip(secs in -100_000_000i64..100_000_000) {
        let d = Duration::from_secs(secs).unwrap();
        let text = d.to_string();
        prop_assert!(
            text.starts_with('P') || text.starts_with("-P"),
            "{text} is not an ISO 8601 duration"
        );
        prop_assert_eq!(text.parse::<Duration>().unwrap(), d);
    }

    /// Decimal payload values survive JSON in both directions.
    #[test]
    fn decimal_values_round_trip(units in -1_000_000i64..1_000_000, scale in 0u32..6) {
        let value = Value::Number(Decimal::new(units, scale));
        let json = serde_json::to_string(&value).unwrap();
        prop_assert_eq!(serde_json::from_str::<Value>(&json).unwrap(), value);
    }

    /// An event survives serialization unchanged, whatever it holds.
    #[test]
    fn events_round_trip(count in 0usize..12, secs in 1i64..100_000, priority in 0u32..100) {
        let mut event = EventRequest::new(ObjectId::new("p").unwrap());
        event.priority = Priority::new(priority);
        event.targets = targets(&["a", "b"]);
        event.interval_period = Some(IntervalPeriod::new(
            StartTime::At(epoch()),
            Duration::from_secs(secs).unwrap(),
        ));
        event.intervals = Some(
            (0..count).map(|i| Interval::new(i as i32, vec![price(i as i64)])).collect(),
        );

        let json = serde_json::to_string(&event).unwrap();
        prop_assert_eq!(serde_json::from_str::<EventRequest>(&json).unwrap(), event);
    }
}

// ---------------------------------------------------------------------------
// The repetition arithmetic
// ---------------------------------------------------------------------------

/// A looping event: `count` intervals of `secs` each, repeating for `event.duration`.
///
/// `event.duration` longer than one pass makes the sequence repeat `[UG §7.3 Looping intervals]`.
fn looping_event(count: usize, secs: i64, total_secs: i64) -> EventRequest {
    let mut event = EventRequest::new(ObjectId::new("prg-1").unwrap());
    event.interval_period = Some(IntervalPeriod::new(
        StartTime::At(epoch()),
        Duration::from_secs(secs).unwrap(),
    ));
    event.intervals = Some(
        (0..count)
            .map(|i| Interval::new(i as i32, vec![price(i as i64)]))
            .collect(),
    );
    event.duration = Some(Duration::from_secs(total_secs).unwrap());
    event
}

proptest! {
    // The arithmetic these two check replaced a walk, so a brute-force walk is the only reference
    // that is not the implementation restated. Both were written after the audit found that
    // `expand_window` and `ReportSchedule` share `IntervalSequence` precisely so a looping tariff
    // cannot mean one thing to a timeline and another to a report — a claim nothing was testing
    // across a *window*.
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// `expand_window` returns exactly the intervals a brute-force walk would.
    ///
    /// `IntervalSequence::indices_overlapping` skips straight to the first repetition that can
    /// reach the window rather than counting there, and an off-by-one in that arithmetic drops a
    /// whole repetition — silently, because the result is still a well-formed list of intervals.
    #[test]
    fn a_window_holds_exactly_what_a_walk_would_find(
        count in 1usize..5,
        secs in 1i64..2_000,
        repeats in 1i64..20,
        from_secs in 0i64..40_000,
        span in 1i64..40_000,
    ) {
        let total = secs * count as i64 * repeats;
        let event = looping_event(count, secs, total);
        let expander = IntervalExpander::at(epoch());

        let from = at(from_secs);
        let to = at(from_secs + span);
        let windowed = expander.expand_window(&event, from, to).unwrap();

        // The reference: every index the sequence has, filtered by the same overlap rule.
        let sequence = expander.sequence(&event).unwrap();
        let mut walked = Vec::new();
        for index in 0..(count as u64 * repeats as u64 + count as u64) {
            match sequence.get(index).unwrap() {
                Some(interval) => {
                    let ends_after = interval.end.is_none_or(|e| e > from);
                    if ends_after && interval.start < to {
                        walked.push(interval);
                    }
                }
                None => break,
            }
        }

        prop_assert_eq!(
            windowed.len(),
            walked.len(),
            "window {}..{} over {} repetitions of {}×{}s",
            from, to, repeats, count, secs
        );
        for (a, b) in windowed.iter().zip(walked.iter()) {
            prop_assert_eq!(a.start, b.start);
            prop_assert_eq!(a.end, b.end);
            prop_assert_eq!(a.occurrence, b.occurrence);
            prop_assert_eq!(a.id, b.id);
        }
    }

    /// A repetition's intervals abut exactly, across the seam between repetitions too.
    ///
    /// The seam is the interesting part: within one pass the intervals inherit a duration, and
    /// between passes the whole sequence is shifted by a period computed separately.
    #[test]
    fn repetitions_tile_without_a_seam(
        count in 1usize..6,
        secs in 1i64..5_000,
        repeats in 2i64..12,
    ) {
        let total = secs * count as i64 * repeats;
        let event = looping_event(count, secs, total);
        let sequence = IntervalExpander::at(epoch()).sequence(&event).unwrap();
        prop_assert!(sequence.repeats(), "an event longer than its intervals must loop");

        let mut previous_end: Option<Timestamp> = None;
        for index in 0..(count as u64 * repeats as u64) {
            let interval = sequence.get(index).unwrap().expect("inside the event's own duration");
            if let Some(end) = previous_end {
                prop_assert_eq!(
                    interval.start,
                    end,
                    "gap or overlap at index {} of {}×{}s × {}",
                    index, count, secs, repeats
                );
            }
            previous_end = interval.end;
        }
        // And the event stops exactly when its duration says.
        prop_assert_eq!(previous_end, Some(at(total)));
        prop_assert!(
            sequence.get(count as u64 * repeats as u64).unwrap().is_none(),
            "the sequence ran past its own duration"
        );
    }
}

// ---------------------------------------------------------------------------
// Report scheduling
// ---------------------------------------------------------------------------

fn descriptor(
    start_interval: i32,
    num_intervals: i32,
    historical: bool,
    frequency: i32,
    repeat: i32,
) -> openadr::model::ReportDescriptor {
    openadr::model::ReportDescriptor {
        payload_type: "USAGE".parse().unwrap(),
        reading_type: None,
        units: None,
        targets: Vec::new(),
        aggregate: false,
        start_interval,
        num_intervals,
        historical,
        frequency,
        repeat,
        report_intervals: openadr::model::ReportIntervals::default(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// A report keeps its sequence number whatever window found it.
    ///
    /// The type's own documentation says so — "the same report has the same sequence number on
    /// every poll, on a restart, and after a resync" — and that is what makes the number usable as
    /// the memory of what a VEN has already filed. A number that shifted with the polling window
    /// would refile a report on every restart, or skip one, and neither end would notice.
    #[test]
    fn a_report_keeps_its_number_whatever_window_found_it(
        count in 1usize..6,
        secs in 60i64..3_600,
        repeats in 1i64..8,
        num in 1i32..4,
        frequency in 1i32..4,
        historical in any::<bool>(),
        cut in 1i64..20_000,
    ) {
        let total = secs * count as i64 * repeats;
        let event = looping_event(count, secs, total);
        let sequence = IntervalExpander::at(epoch()).sequence(&event).unwrap();
        let d = descriptor(-1, num, historical, frequency, -1);

        let start = epoch();
        let end = at(total + secs);
        let whole = ReportSchedule::compute(&d, &sequence, start, end);

        // The same schedule, computed over two halves of the same range.
        let split = at(cut.min(total));
        let first = ReportSchedule::compute(&d, &sequence, start, split);
        let second = ReportSchedule::compute(&d, &sequence, split, end);

        for part in [&first, &second] {
            for due in part.due() {
                let same = whole
                    .due()
                    .iter()
                    .find(|w| w.sequence == due.sequence)
                    .unwrap_or_else(|| panic!("report {} exists in a window and not in the whole range", due.sequence));
                prop_assert_eq!(same.due_at, due.due_at, "report {} moved", due.sequence);
                prop_assert_eq!(same.covers_from, due.covers_from);
                prop_assert_eq!(same.covers_to, due.covers_to);
                prop_assert_eq!(&same.interval_ids, &due.interval_ids);
            }
        }
    }

    /// Every scheduled report covers intervals that exist, in order, and lands inside the event.
    ///
    /// `numIntervals` arrives as a bare `i32` and `startInterval` as another, so the covered range
    /// is arithmetic over two attacker-supplied numbers against a sequence that may not end.
    #[test]
    fn a_scheduled_report_covers_real_intervals(
        count in 1usize..6,
        secs in 60i64..3_600,
        repeats in 1i64..6,
        start_interval in -1i32..8,
        num in -1i32..8,
        historical in any::<bool>(),
        frequency in -1i32..5,
        repeat in -1i32..6,
    ) {
        let total = secs * count as i64 * repeats;
        let event = looping_event(count, secs, total);
        let sequence = IntervalExpander::at(epoch()).sequence(&event).unwrap();
        let d = descriptor(start_interval, num, historical, frequency, repeat);

        let schedule = ReportSchedule::compute(&d, &sequence, epoch(), at(total * 2));
        for due in schedule.due() {
            prop_assert!(
                due.covers_to.is_none_or(|to| to > due.covers_from),
                "a report covering nothing: {:?}..{:?}", due.covers_from, due.covers_to
            );
            // Never outside the event's own lifespan, which its `duration` bounds.
            prop_assert!(due.covers_from >= epoch());
            prop_assert!(due.covers_to.is_none_or(|to| to <= at(total)));
            if let Some(at_) = due.due_at {
                prop_assert!(at_ >= epoch() && at_ <= at(total));
            }
            // The ids are the event's, and there is at least one.
            prop_assert!(!due.interval_ids.is_empty());
            for id in &due.interval_ids {
                prop_assert!((0..count as i32).contains(id), "unknown interval id {}", id);
            }
        }
        // Sequence numbers are strictly increasing, which is what makes "the last one filed" a
        // number a VEN can store.
        let numbers: Vec<u64> = schedule.due().iter().map(|d| d.sequence).collect();
        prop_assert!(numbers.windows(2).all(|w| w[0] < w[1]), "{:?}", numbers);
    }
}
