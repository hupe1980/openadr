//! Resolving overlapping events into one schedule.
//!
//! A VEN enrolled in a programme may hold several concurrent events — a day-ahead price curve and a
//! short emergency curtailment, say. The specification settles the conflict with `event.priority`:
//! a lower number wins, and the loser is *split around* the winner rather than dropped, so the
//! long-running event resumes when the short one ends (User Guide §7.1).
//!
//! ```text
//! input   |------------------ day-ahead prices (priority 10) ------------------|
//!                    |-- curtailment (priority 0) --|
//!
//! result  |--prices--|-------- curtailment ---------|-------- prices ----------|
//! ```

use crate::std_shim::{ToString, Vec, vec};

use crate::model::{Duration, EventRequest, ObjectId, Priority, Timestamp, ValuesMap};

use super::interval::{ExpandError, ExpandedInterval, IntervalExpander};

/// One resolved stretch of time and the values in force during it.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// Start of the stretch.
    pub start: Timestamp,
    /// End of the stretch, or `None` if it runs indefinitely.
    pub end: Option<Timestamp>,
    /// The event that won this stretch.
    pub event_id: ObjectId,
    /// That event's priority.
    pub priority: Priority,
    /// The interval id within the event, so a report can quote it.
    pub interval_id: i32,
    /// The randomization the winning event asked for.
    pub randomize_start: Option<Duration>,
    /// The payloads in force.
    pub payloads: Vec<ValuesMap>,
}

impl Segment {
    /// Whether an instant falls inside this stretch.
    pub fn contains(&self, at: Timestamp) -> bool {
        at >= self.start && self.end.is_none_or(|e| at < e)
    }
}

/// A conflict-free schedule for one programme.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Timeline {
    segments: Vec<Segment>,
    /// Events that were skipped, with the reason, so the caller can log rather than guess.
    skipped: Vec<(ObjectId, ExpandError)>,
    overlaps_at_equal_priority: Vec<(ObjectId, ObjectId)>,
}

impl Timeline {
    /// Build a timeline from a programme's events over a window, expanding them all against one
    /// [`IntervalExpander`].
    ///
    /// Events are laid down most-important-first; each one claims only the time not already taken.
    /// An event that cannot be expanded is skipped and recorded in [`Timeline::skipped`] rather than
    /// failing the whole build — one malformed event must not blind a VEN to the others.
    pub fn build<'a>(
        events: impl IntoIterator<Item = (&'a ObjectId, &'a EventRequest)>,
        expander: &IntervalExpander,
        from: Timestamp,
        to: Timestamp,
    ) -> Self {
        Self::build_with(
            events
                .into_iter()
                .map(|(id, event)| (id, event, expander.clone())),
            from,
            to,
        )
    }

    /// The same, with each event expanded against **its own** [`IntervalExpander`].
    ///
    /// Which matters for exactly one thing, and it is not a nicety. `0001-01-01` means "now from
    /// the reader's point of view" `[UG §7.3]`, and the reader's point of view is the moment it
    /// first read the event — not whenever a timeline happens to be rebuilt. One expander for the
    /// whole set re-resolves the sentinel on every rebuild, so a "do it now" curtailment slides
    /// forward each time and never ends. A caller that holds events over time
    /// ([`VenRuntime`](crate::ven::VenRuntime)) therefore anchors each event when it arrives and
    /// passes that instant back in here.
    pub fn build_with<'a>(
        events: impl IntoIterator<Item = (&'a ObjectId, &'a EventRequest, IntervalExpander)>,
        from: Timestamp,
        to: Timestamp,
    ) -> Self {
        let mut expanded: Vec<(&ObjectId, Priority, Vec<ExpandedInterval>)> = Vec::new();
        let mut skipped = Vec::new();

        for (id, event, expander) in events {
            match expander.expand_window(event, from, to) {
                Ok(intervals) => {
                    // An interval with no payloads carries no instruction. Report-only events are
                    // written exactly that way — `payloads: []`, or no `intervals` at all with the
                    // structure implied by `intervalPeriod` `[UG §7.3]` — and they exist to anchor
                    // reports, not to dispatch. Letting them claim the timeline would let a
                    // priority-0 report request silently mask a real curtailment.
                    let intervals: Vec<ExpandedInterval> = intervals
                        .into_iter()
                        .filter(|i| !i.payloads.is_empty())
                        .collect();
                    if !intervals.is_empty() {
                        expanded.push((id, event.priority, intervals));
                    }
                }
                Err(e) => skipped.push((id.clone(), e)),
            }
        }

        // Most important first. Ties are broken by event id so the result is deterministic.
        expanded.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.as_str().cmp(b.0.as_str())));

        let mut overlaps = Vec::new();
        let mut segments: Vec<Segment> = Vec::new();

        for (id, priority, intervals) in &expanded {
            for iv in intervals {
                let pieces = subtract_covered(iv.start, iv.end, &segments);
                if pieces.len() != 1 || pieces[0].0 != iv.start || pieces[0].1 != iv.end {
                    // Something already occupied part of this window. Note an equal-priority clash,
                    // which the specification leaves undefined and which usually means a mistake.
                    if let Some(other) = segments.iter().find(|s| {
                        overlaps_window(s, iv.start, iv.end)
                            && s.priority == *priority
                            // Two intervals of one event overlapping each other is a malformed
                            // event, not a clash between two of them.
                            && &s.event_id != *id
                    }) {
                        let pair = (other.event_id.clone(), (*id).clone());
                        if !overlaps.contains(&pair) {
                            overlaps.push(pair);
                        }
                    }
                }
                for (start, end) in pieces {
                    segments.push(Segment {
                        start,
                        end,
                        event_id: (*id).clone(),
                        priority: *priority,
                        interval_id: iv.id,
                        randomize_start: iv.randomize_start.clone(),
                        payloads: iv.payloads.clone(),
                    });
                }
            }
        }

        segments.sort_by_key(|s| s.start);
        Self {
            segments,
            skipped,
            overlaps_at_equal_priority: overlaps,
        }
    }

    /// The resolved stretches, in time order.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Whether the timeline holds nothing.
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Events that could not be expanded, with the reason.
    pub fn skipped(&self) -> &[(ObjectId, ExpandError)] {
        &self.skipped
    }

    /// Pairs of events that overlapped at the same priority.
    ///
    /// The specification does not say which should win, so this is surfaced as a diagnostic: the
    /// result is deterministic (lowest event id wins) but almost certainly not what was intended.
    pub fn overlaps_at_equal_priority(&self) -> &[(ObjectId, ObjectId)] {
        &self.overlaps_at_equal_priority
    }

    /// What is in force at an instant.
    pub fn at(&self, when: Timestamp) -> Option<&Segment> {
        self.segments.iter().find(|s| s.contains(when))
    }

    /// The next instant at which the schedule changes.
    pub fn next_change(&self, after: Timestamp) -> Option<Timestamp> {
        self.segments
            .iter()
            .flat_map(|s| [Some(s.start), s.end])
            .flatten()
            .filter(|t| *t > after)
            .min()
    }
}

fn overlaps_window(segment: &Segment, start: Timestamp, end: Option<Timestamp>) -> bool {
    let a_end = segment.end.unwrap_or(Timestamp::MAX);
    let b_end = end.unwrap_or(Timestamp::MAX);
    segment.start < b_end && start < a_end
}

/// Remove the parts of `[start, end)` already claimed by higher-priority segments.
fn subtract_covered(
    start: Timestamp,
    end: Option<Timestamp>,
    taken: &[Segment],
) -> Vec<(Timestamp, Option<Timestamp>)> {
    let mut pieces = vec![(start, end.unwrap_or(Timestamp::MAX))];

    for seg in taken {
        let seg_end = seg.end.unwrap_or(Timestamp::MAX);
        let mut next = Vec::with_capacity(pieces.len() + 1);
        for (s, e) in pieces {
            if seg_end <= s || seg.start >= e {
                next.push((s, e)); // no overlap
                continue;
            }
            if seg.start > s {
                next.push((s, seg.start)); // piece before the taken stretch
            }
            if seg_end < e {
                next.push((seg_end, e)); // piece after it
            }
        }
        pieces = next;
        if pieces.is_empty() {
            break;
        }
    }

    pieces
        .into_iter()
        .filter(|(s, e)| s < e)
        .map(|(s, e)| (s, if e == Timestamp::MAX { None } else { Some(e) }))
        .collect()
}

impl core::fmt::Display for Timeline {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for s in &self.segments {
            writeln!(
                f,
                "{} .. {}  {} (priority {})",
                s.start,
                s.end
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "∞".to_string()),
                s.event_id,
                s.priority
                    .value()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".to_string())
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Interval, IntervalPeriod, StartTime, Value};
    use rust_decimal::Decimal;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn oid(s: &str) -> ObjectId {
        ObjectId::new(s).unwrap()
    }

    fn payload(kind: &str, v: i64) -> ValuesMap {
        ValuesMap::single(kind.parse().unwrap(), Value::Number(Decimal::from(v)))
    }

    fn event(start: &str, duration: &str, priority: Priority, value: i64) -> EventRequest {
        let mut e = EventRequest::new(oid("prog"));
        e.priority = priority;
        e.interval_period = Some(IntervalPeriod::new(
            StartTime::At(ts(start)),
            duration.parse().unwrap(),
        ));
        e.intervals = Some(vec![Interval::new(0, vec![payload("PRICE", value)])]);
        e
    }

    fn expander() -> IntervalExpander {
        IntervalExpander::at(ts("2026-01-01T00:00:00Z"))
    }

    #[test]
    fn a_high_priority_event_splits_a_low_priority_one() {
        let long = event("2026-01-01T00:00:00Z", "PT8H", Priority::new(10), 1);
        let short = event("2026-01-01T02:00:00Z", "PT2H", Priority::new(0), 2);
        let long_id = oid("long");
        let short_id = oid("short");

        let tl = Timeline::build(
            [(&long_id, &long), (&short_id, &short)],
            &expander(),
            ts("2026-01-01T00:00:00Z"),
            ts("2026-01-02T00:00:00Z"),
        );

        let s = tl.segments();
        assert_eq!(
            s.len(),
            3,
            "the long event is split in two around the short one"
        );
        assert_eq!(s[0].event_id, long_id);
        assert_eq!(s[0].start, ts("2026-01-01T00:00:00Z"));
        assert_eq!(s[0].end, Some(ts("2026-01-01T02:00:00Z")));

        assert_eq!(s[1].event_id, short_id);
        assert_eq!(s[1].end, Some(ts("2026-01-01T04:00:00Z")));

        assert_eq!(s[2].event_id, long_id, "the long event resumes afterwards");
        assert_eq!(s[2].start, ts("2026-01-01T04:00:00Z"));
        assert_eq!(s[2].end, Some(ts("2026-01-01T08:00:00Z")));
    }

    #[test]
    fn lookup_returns_the_winning_event() {
        let long = event("2026-01-01T00:00:00Z", "PT8H", Priority::new(10), 1);
        let short = event("2026-01-01T02:00:00Z", "PT2H", Priority::new(0), 2);
        let (l, s) = (oid("long"), oid("short"));
        let tl = Timeline::build(
            [(&l, &long), (&s, &short)],
            &expander(),
            ts("2026-01-01T00:00:00Z"),
            ts("2026-01-02T00:00:00Z"),
        );
        assert_eq!(tl.at(ts("2026-01-01T01:00:00Z")).unwrap().event_id, l);
        assert_eq!(tl.at(ts("2026-01-01T03:00:00Z")).unwrap().event_id, s);
        assert_eq!(tl.at(ts("2026-01-01T05:00:00Z")).unwrap().event_id, l);
        assert!(tl.at(ts("2026-01-01T09:00:00Z")).is_none());
    }

    #[test]
    fn segments_never_overlap() {
        let a = event("2026-01-01T00:00:00Z", "PT8H", Priority::new(10), 1);
        let b = event("2026-01-01T02:00:00Z", "PT2H", Priority::new(5), 2);
        let c = event("2026-01-01T03:00:00Z", "PT1H", Priority::new(0), 3);
        let (ia, ib, ic) = (oid("a"), oid("b"), oid("c"));
        let tl = Timeline::build(
            [(&ia, &a), (&ib, &b), (&ic, &c)],
            &expander(),
            ts("2026-01-01T00:00:00Z"),
            ts("2026-01-02T00:00:00Z"),
        );
        let s = tl.segments();
        for w in s.windows(2) {
            assert!(
                w[0].end.unwrap() <= w[1].start,
                "segments {:?} and {:?} overlap",
                w[0].start,
                w[1].start
            );
        }
    }

    #[test]
    fn an_unprioritised_event_loses_to_a_prioritised_one() {
        let plain = event("2026-01-01T00:00:00Z", "PT4H", Priority::UNSPECIFIED, 1);
        let ranked = event("2026-01-01T00:00:00Z", "PT4H", Priority::new(99), 2);
        let (p, r) = (oid("plain"), oid("ranked"));
        let tl = Timeline::build(
            [(&p, &plain), (&r, &ranked)],
            &expander(),
            ts("2026-01-01T00:00:00Z"),
            ts("2026-01-02T00:00:00Z"),
        );
        assert_eq!(tl.at(ts("2026-01-01T01:00:00Z")).unwrap().event_id, r);
    }

    #[test]
    fn equal_priority_overlaps_are_reported_not_hidden() {
        let a = event("2026-01-01T00:00:00Z", "PT4H", Priority::new(1), 1);
        let b = event("2026-01-01T02:00:00Z", "PT4H", Priority::new(1), 2);
        let (ia, ib) = (oid("a"), oid("b"));
        let tl = Timeline::build(
            [(&ia, &a), (&ib, &b)],
            &expander(),
            ts("2026-01-01T00:00:00Z"),
            ts("2026-01-02T00:00:00Z"),
        );
        assert_eq!(tl.overlaps_at_equal_priority().len(), 1);
        // Still deterministic and still non-overlapping.
        for w in tl.segments().windows(2) {
            assert!(w[0].end.unwrap() <= w[1].start);
        }
    }

    #[test]
    fn a_malformed_event_is_skipped_not_fatal() {
        let good = event("2026-01-01T00:00:00Z", "PT1H", Priority::new(1), 1);
        let mut bad = EventRequest::new(oid("prog"));
        bad.intervals = Some(vec![Interval::new(0, vec![payload("PRICE", 9)])]); // no start anywhere
        let (g, b) = (oid("good"), oid("bad"));
        let tl = Timeline::build(
            [(&g, &good), (&b, &bad)],
            &expander(),
            ts("2026-01-01T00:00:00Z"),
            ts("2026-01-02T00:00:00Z"),
        );
        assert_eq!(tl.segments().len(), 1);
        assert_eq!(tl.skipped().len(), 1);
        assert_eq!(tl.skipped()[0].0, b);
    }

    #[test]
    fn next_change_finds_the_following_boundary() {
        let e = event("2026-01-01T00:00:00Z", "PT1H", Priority::new(1), 1);
        let id = oid("e");
        let tl = Timeline::build(
            [(&id, &e)],
            &expander(),
            ts("2026-01-01T00:00:00Z"),
            ts("2026-01-02T00:00:00Z"),
        );
        assert_eq!(
            tl.next_change(ts("2026-01-01T00:30:00Z")),
            Some(ts("2026-01-01T01:00:00Z"))
        );
        assert_eq!(tl.next_change(ts("2026-01-01T02:00:00Z")), None);
    }

    #[test]
    fn an_open_ended_event_is_interrupted_and_resumes() {
        let mut forever = event("2026-01-01T00:00:00Z", "P9999Y", Priority::new(10), 1);
        forever.duration = Some("P9999Y".parse().unwrap());
        let short = event("2026-01-01T02:00:00Z", "PT1H", Priority::new(0), 2);
        let (f, s) = (oid("forever"), oid("short"));
        let tl = Timeline::build(
            [(&f, &forever), (&s, &short)],
            &expander(),
            ts("2026-01-01T00:00:00Z"),
            ts("2026-01-02T00:00:00Z"),
        );
        assert_eq!(tl.at(ts("2026-01-01T01:00:00Z")).unwrap().event_id, f);
        assert_eq!(tl.at(ts("2026-01-01T02:30:00Z")).unwrap().event_id, s);
        assert_eq!(tl.at(ts("2026-01-01T04:00:00Z")).unwrap().event_id, f);
    }

    #[test]
    fn a_report_only_event_does_not_claim_the_schedule() {
        // A report-only event is written with `payloads: []` `[UG §7.3]`. It exists to anchor
        // reports; letting it win the timeline on priority would let a report request silently
        // mask the curtailment it is asking about.
        let mut anchor = event("2026-01-01T00:00:00Z", "PT4H", Priority::new(0), 0);
        anchor.intervals = Some(vec![Interval::new(0, Vec::new())]);
        let dispatch = event("2026-01-01T01:00:00Z", "PT1H", Priority::new(10), 5);

        let a = oid("evt-anchor");
        let d = oid("evt-dispatch");
        let tl = Timeline::build(
            [(&a, &anchor), (&d, &dispatch)],
            &expander(),
            ts("2026-01-01T00:00:00Z"),
            ts("2026-01-01T06:00:00Z"),
        );
        let segment = tl
            .at(ts("2026-01-01T01:30:00Z"))
            .expect("the dispatch is in force");
        assert_eq!(segment.event_id, d);
        // And the anchor contributes nothing at all.
        assert!(tl.at(ts("2026-01-01T00:30:00Z")).is_none());
    }
}
