//! When a VEN must produce a report, and which intervals it must cover.
//!
//! A `reportDescriptor` is four integers with `-1` sentinels, a boolean and an enum, and between
//! them they express historical reports, forecasts, rolling windows, periodic batches, ad-hoc
//! reports and endless repetition (User Guide §7.5). This module turns that into a list of due times.
//!
//! Reports are resolved against an [`IntervalSequence`], not a slice, because two of the
//! specification's worked scenarios need intervals the event never lists: a looping tariff
//! (`event.duration = P9999Y`) reports once per repetition for ever `[UG §7.3 Looping intervals]`,
//! and a capability forecast counts forty-eight intervals out of an event that declares none at all
//! `[UG §8.7, §8.8]`. Both are ordinary indexing into the sequence.
//!
//! ## Two places the source material contradicts itself
//!
//! **Where `startInterval` points.** §7.5 "Report on a subset of intervals" (`startInterval = 1`,
//! `numIntervals = 3`) shows a report covering intervals 1–3, i.e. `startInterval` as the *first
//! covered* interval. "Report on a subset of intervals at a regular period" (`startInterval = 1`,
//! `numIntervals = 2`, `frequency = 2`, `repeat = 3`) and the rolling example both show
//! `startInterval` as the *generation point*, with `numIntervals` counted backwards from it when
//! `historical` is true. The latter reading is the one that makes all the fully-specified examples
//! work, and it is the default here.
//! [`ScheduleOptions::start_interval_is_first_covered`] selects the other.
//!
//! **When a forecast is generated.** §7.5's rolling timeline draws "generate report N" at the end
//! of the range each report covers, in both directions. §8.7 and §8.8 — the two complete worked
//! forecast scenarios, with rationale — say the opposite: "startInterval = 0 indicates that a
//! report should be generated when the first interval has *begun*", as does §7.5's own "Forecast
//! reporting" diagram ("report 1 at beginning of interval 1"). The prose wins, because a forecast
//! delivered at the end of the window it forecasts is not a forecast. So a report is due at its
//! **anchor interval's boundary**: the end of it when reporting backwards, the start of it when
//! reporting forwards.

use crate::std_shim::Vec;

use crate::model::{ReportDescriptor, ReportIntervals, Timestamp};

use super::interval::IntervalSequence;

/// One report the VEN owes, with the window it must cover.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportDue {
    /// Which repetition of the descriptor's schedule this is, counted from the event's first
    /// interval and stable whatever window it was computed over.
    ///
    /// Stability is what makes it usable as the memory of what has already been filed: the same
    /// report has the same sequence number on every poll, on a restart, and after a resync.
    pub sequence: u64,
    /// When the report becomes due — the boundary of the interval `startInterval` anchors on.
    ///
    /// `None` when that boundary is unknown because the interval is open-ended, in which case the
    /// VEN reports at its own discretion.
    pub due_at: Option<Timestamp>,
    /// Start of the covered range.
    pub covers_from: Timestamp,
    /// End of the covered range, if bounded.
    pub covers_to: Option<Timestamp>,
    /// The interval ids to quote, in order.
    ///
    /// Empty when the descriptor asks for `OPEN_INTERVALS`, where the VEN chooses its own, and for
    /// an event whose intervals are implied, where the specification leaves the ids to the VEN.
    pub interval_ids: Vec<i32>,
}

/// Knobs for the ambiguous corners of §7.5.
#[derive(Debug, Clone, Copy)]
pub struct ScheduleOptions {
    /// Read `startInterval` as the first *covered* interval rather than the generation point.
    pub start_interval_is_first_covered: bool,
    /// Ceiling on how many reports one computation may return.
    ///
    /// `repeat = -1` is unbounded by construction, so the window a schedule is computed over is
    /// what normally bounds it; this is the backstop for a descriptor asking for a report every
    /// second over a window of a week.
    pub max_reports: u32,
    /// Ceiling on how many intervals one report may cover.
    ///
    /// `numIntervals` arrives from the wire as a bare `i32`, and against a sequence that repeats
    /// there is no "last interval" to clamp it to — so without this a descriptor asking for two
    /// billion intervals is a loop of that length, a thousand times over.
    /// [`IntervalExpander::DEFAULT_LIMIT`](super::IntervalExpander::DEFAULT_LIMIT) is the same
    /// ceiling the expander applies, for the same reason.
    pub max_intervals: usize,
}

impl Default for ScheduleOptions {
    fn default() -> Self {
        Self {
            start_interval_is_first_covered: false,
            max_reports: 1_000,
            max_intervals: super::IntervalExpander::DEFAULT_LIMIT,
        }
    }
}

/// The reports one descriptor asks for over a window.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportSchedule {
    due: Vec<ReportDue>,
    ad_hoc: bool,
    ven_chooses_intervals: bool,
}

impl ReportSchedule {
    /// Compute the reports due in `[from, to]` for one descriptor.
    ///
    /// The window is a bound on the *result*, not on the schedule: sequence numbers count from the
    /// event's first interval, so the same report keeps its number whatever window found it.
    pub fn compute(
        descriptor: &ReportDescriptor,
        sequence: &IntervalSequence,
        from: Timestamp,
        to: Timestamp,
    ) -> Self {
        Self::compute_with(descriptor, sequence, from, to, ScheduleOptions::default())
    }

    /// The same, with explicit options.
    ///
    /// Infallible by construction: every sentinel and every out-of-range integer the wire can carry
    /// has a defined reading here, and `repeat = -1` is bounded by the window and by
    /// [`ScheduleOptions::max_reports`] rather than refused.
    pub fn compute_with(
        descriptor: &ReportDescriptor,
        sequence: &IntervalSequence,
        from: Timestamp,
        to: Timestamp,
        options: ScheduleOptions,
    ) -> Self {
        let ven_chooses =
            descriptor.report_intervals == ReportIntervals::OpenIntervals || sequence.is_implied();

        // `frequency == 0`: the VEN reports whenever it sees fit. Nothing to schedule.
        // An event that places nothing in time is the same case: the VEN supplies its own
        // intervals `[UG §7.3 'report-only' event with VEN-determined intervals]`.
        if descriptor.is_ad_hoc() || sequence.is_empty() {
            return Self {
                due: Vec::new(),
                ad_hoc: true,
                ven_chooses_intervals: ven_chooses || sequence.is_empty(),
            };
        }

        let declared = sequence.declared_len() as i64;
        let bounded = !sequence.repeats();

        // `-1` means "include all intervals" — all the event *declares*, which is one repetition.
        let mut num = if descriptor.num_intervals < 0 {
            declared
        } else {
            i64::from(descriptor.num_intervals)
        };
        num = if bounded {
            // A bounded sequence clamps itself: there are only so many intervals.
            num.min(declared)
        } else {
            // A repeating one does not, so the ceiling has to be explicit.
            num.min(options.max_intervals as i64)
        };
        let num = num.max(1);

        // Where the schedule anchors. `-1` means "the far end, in the direction the report runs":
        // the last interval for a historical report, the first for a forecast.
        let anchor0 = if descriptor.start_interval < 0 {
            if options.start_interval_is_first_covered {
                (declared - num).max(0)
            } else if descriptor.historical {
                declared - 1
            } else {
                0
            }
        } else {
            let requested = i64::from(descriptor.start_interval);
            if bounded {
                requested.min(declared - 1)
            } else {
                requested
            }
        }
        .max(0);

        // `-1` means "the same as numIntervals", giving back-to-back batches.
        let step = if descriptor.frequency < 0 {
            num
        } else {
            i64::from(descriptor.frequency)
        }
        .max(1);

        // `repeat = -1` repeats until the event ends; the window and `max_reports` bound it.
        let repeat = if descriptor.repeats_forever() {
            u64::MAX
        } else {
            descriptor.repeat.max(0) as u64
        };

        // Skip straight to the first repetition that can reach the window rather than walking
        // there. A tariff that has looped hourly since 2020 is fifty thousand repetitions in.
        let first_k = Self::first_k_reaching(sequence, anchor0, step, from);

        let mut due = Vec::new();
        let mut k = first_k;
        while k < repeat && due.len() < options.max_reports as usize {
            let anchor = anchor0.saturating_add((k as i64).saturating_mul(step));

            // Which intervals this report covers, and which of them it anchors on.
            let (first, last) = if options.start_interval_is_first_covered || !descriptor.historical
            {
                (anchor, anchor.saturating_add(num).saturating_sub(1))
            } else {
                (anchor.saturating_sub(num).saturating_add(1), anchor)
            };
            let first = first.max(0);
            if bounded && first >= declared {
                break;
            }
            let last = if bounded {
                last.min(declared - 1)
            } else {
                last
            };
            if last < first {
                k += 1;
                continue;
            }

            let Some(covered) = Self::collect(sequence, first, last) else {
                // The anchor itself is past the event's end. Later anchors are later still, save at
                // the one boundary where a repetition's declared intervals are out of time order —
                // stopping there costs at most the tail of a truncated event, and walking on would
                // cost `max_reports` iterations of nothing on every ordinary one.
                break;
            };
            let (covers_from, covers_to, ids) = covered;

            // A report is due at the boundary of the interval it anchors on: the end of it when
            // reporting backwards, the start of it when reporting forwards.
            let due_at = if descriptor.historical && !options.start_interval_is_first_covered {
                covers_to
            } else {
                Some(covers_from)
            };

            if due_at.is_some_and(|t| t > to) {
                break;
            }
            if due_at.is_none_or(|t| t >= from) {
                due.push(ReportDue {
                    sequence: k,
                    due_at,
                    covers_from,
                    covers_to,
                    interval_ids: if ven_chooses { Vec::new() } else { ids },
                });
            }
            k += 1;
        }

        Self {
            due,
            ad_hoc: false,
            ven_chooses_intervals: ven_chooses,
        }
    }

    /// The covered range: its start, its end, and the interval ids in order.
    ///
    /// `None` once an index is past the event's own end.
    fn collect(
        sequence: &IntervalSequence,
        first: i64,
        last: i64,
    ) -> Option<(Timestamp, Option<Timestamp>, Vec<i32>)> {
        let start = sequence.get(u64::try_from(first).ok()?).ok()??;
        let mut ids = Vec::with_capacity((last - first + 1).max(1) as usize);
        ids.push(start.id);
        let mut end = start.end;
        for index in (first + 1)..=last {
            let Some(interval) = sequence.get(u64::try_from(index).ok()?).ok()? else {
                // The event ended part-way through the range. The report covers what exists.
                break;
            };
            ids.push(interval.id);
            end = interval.end;
        }
        Some((start.start, end, ids))
    }

    /// The first `k` whose anchor can fall at or after `from`.
    ///
    /// Arithmetic, not a search: the interval at index `i` starts inside repetition `i / len`, so
    /// the repetition that reaches `from` bounds `i`, and `i = anchor0 + k·step` bounds `k`. One
    /// repetition of slack, because within a repetition the declared intervals need not be in time
    /// order.
    fn first_k_reaching(
        sequence: &IntervalSequence,
        anchor0: i64,
        step: i64,
        from: Timestamp,
    ) -> u64 {
        let (Some(period), len) = (sequence.period(), sequence.declared_len() as i64) else {
            return 0;
        };
        if period <= 0 || len <= 0 {
            return 0;
        }
        let delta = from.as_nanosecond() - sequence.start().as_nanosecond();
        if delta <= 0 {
            return 0;
        }
        let occurrence = (delta / period).saturating_sub(1).max(0);
        let Ok(occurrence) = i64::try_from(occurrence) else {
            return 0;
        };
        let index = occurrence.saturating_mul(len);
        let ahead = index.saturating_sub(anchor0);
        if ahead <= 0 {
            return 0;
        }
        // Round up: the first k whose anchor is at or past `index`.
        (ahead as u64).div_ceil(step as u64)
    }

    /// The scheduled reports, in sequence order.
    pub fn due(&self) -> &[ReportDue] {
        &self.due
    }

    /// Whether the VEN decides when to report.
    pub fn is_ad_hoc(&self) -> bool {
        self.ad_hoc
    }

    /// Whether the VEN supplies its own intervals rather than the event's.
    pub fn ven_chooses_intervals(&self) -> bool {
        self.ven_chooses_intervals
    }

    /// The next report due strictly after an instant.
    pub fn next_after(&self, at: Timestamp) -> Option<&ReportDue> {
        self.due
            .iter()
            .filter(|d| d.due_at.is_some_and(|t| t > at))
            .min_by_key(|d| d.due_at)
    }

    /// Reports that became due at or before an instant.
    ///
    /// A VEN that was offline catches up with this; for a "do it now" event only the most recent
    /// window is worth sending, which is what `skip_stale` gives.
    pub fn overdue(&self, at: Timestamp, skip_stale: bool) -> Vec<&ReportDue> {
        let mut overdue: Vec<&ReportDue> = self
            .due
            .iter()
            .filter(|d| d.due_at.is_some_and(|t| t <= at))
            .collect();
        if skip_stale {
            overdue = overdue.into_iter().next_back().into_iter().collect();
        }
        overdue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::interval::IntervalExpander;
    use crate::model::{
        Duration, EventRequest, Interval, IntervalPeriod, PayloadType, StartTime, ValuesMap,
    };
    use crate::std_shim::vec;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    const MIDNIGHT: &str = "2026-01-01T00:00:00Z";

    /// An event of `count` hourly intervals starting at midnight, ids 0..count.
    fn hourly_event(count: i32) -> EventRequest {
        let mut event = EventRequest::new("prg-1".parse().unwrap());
        event.interval_period = Some(IntervalPeriod::new(
            StartTime::At(ts(MIDNIGHT)),
            "PT1H".parse::<Duration>().unwrap(),
        ));
        event.intervals = Some(
            (0..count)
                .map(|i| {
                    Interval::new(
                        i,
                        vec![ValuesMap::new(
                            PayloadType::new("USAGE").unwrap(),
                            Vec::new(),
                        )],
                    )
                })
                .collect(),
        );
        event
    }

    /// The sequence for `count` hourly intervals.
    fn hourly(count: i32) -> IntervalSequence {
        IntervalExpander::at(ts(MIDNIGHT))
            .sequence(&hourly_event(count))
            .unwrap()
    }

    /// A window wide enough that nothing is cut off by it.
    fn everything() -> (Timestamp, Timestamp) {
        (ts("2000-01-01T00:00:00Z"), ts("2100-01-01T00:00:00Z"))
    }

    fn schedule(rd: &ReportDescriptor, sequence: &IntervalSequence) -> ReportSchedule {
        let (from, to) = everything();
        ReportSchedule::compute(rd, sequence, from, to)
    }

    fn descriptor() -> ReportDescriptor {
        ReportDescriptor::new(PayloadType::new("USAGE").unwrap())
    }

    #[test]
    fn defaults_produce_one_report_over_every_interval() {
        // User Guide §7.5 "Report on all intervals": all defaults, 4 intervals.
        let s = schedule(&descriptor(), &hourly(4));
        assert_eq!(s.due().len(), 1);
        let d = &s.due()[0];
        assert_eq!(d.interval_ids, vec![0, 1, 2, 3]);
        assert_eq!(d.due_at, Some(ts("2026-01-01T04:00:00Z")));
    }

    #[test]
    fn a_forecast_with_defaults_covers_every_interval_and_is_due_before_them() {
        // §7.5 "Forecast reporting": every default except `historical = false`. The diagram puts
        // the report at the *beginning* of the first interval, and it covers all four — so the
        // `startInterval = -1` sentinel points at the far end in the direction of travel, which
        // for a forecast is the first interval, not the last.
        let mut rd = descriptor();
        rd.historical = false;
        let s = schedule(&rd, &hourly(4));
        assert_eq!(s.due().len(), 1);
        let d = &s.due()[0];
        assert_eq!(d.interval_ids, vec![0, 1, 2, 3]);
        assert_eq!(d.covers_from, ts(MIDNIGHT));
        assert_eq!(d.due_at, Some(ts(MIDNIGHT)));
    }

    #[test]
    fn a_forecast_reports_before_the_intervals_it_covers() {
        let mut rd = descriptor();
        rd.historical = false;
        rd.start_interval = 0;
        rd.num_intervals = 4;
        let s = schedule(&rd, &hourly(4));
        let d = &s.due()[0];
        assert_eq!(d.covers_from, ts(MIDNIGHT));
        assert_eq!(d.due_at, Some(ts(MIDNIGHT)));
        assert_eq!(d.interval_ids, vec![0, 1, 2, 3]);
    }

    #[test]
    fn periodic_batches_match_the_user_guide_example() {
        // §7.5 "Report on a subset of intervals at a regular period":
        // startInterval=1, numIntervals=2, frequency=2, repeat=3.
        let mut rd = descriptor();
        rd.start_interval = 1;
        rd.num_intervals = 2;
        rd.frequency = 2;
        rd.repeat = 3;
        let s = schedule(&rd, &hourly(6));
        assert_eq!(s.due().len(), 3);
        assert_eq!(s.due()[0].interval_ids, vec![0, 1]);
        assert_eq!(s.due()[0].due_at, Some(ts("2026-01-01T02:00:00Z")));
        assert_eq!(s.due()[1].interval_ids, vec![2, 3]);
        assert_eq!(s.due()[2].interval_ids, vec![4, 5]);
    }

    #[test]
    fn rolling_forecasts_match_the_user_guide_example() {
        // §7.5 "Rolling reports": start=0, num=3, frequency=1, repeat=3, historical=false.
        let mut rd = descriptor();
        rd.start_interval = 0;
        rd.num_intervals = 3;
        rd.frequency = 1;
        rd.repeat = 3;
        rd.historical = false;
        let s = schedule(&rd, &hourly(5));
        assert_eq!(s.due().len(), 3);
        assert_eq!(s.due()[0].interval_ids, vec![0, 1, 2]);
        assert_eq!(s.due()[1].interval_ids, vec![1, 2, 3]);
        assert_eq!(s.due()[2].interval_ids, vec![2, 3, 4]);
        // Each forecast is due when the first interval it covers begins.
        assert_eq!(s.due()[0].due_at, Some(ts(MIDNIGHT)));
        assert_eq!(s.due()[1].due_at, Some(ts("2026-01-01T01:00:00Z")));
    }

    #[test]
    fn the_alternative_reading_of_start_interval_is_available() {
        // §7.5 "Report on a subset of intervals": start=1, num=3, read as "first covered".
        let mut rd = descriptor();
        rd.start_interval = 1;
        rd.num_intervals = 3;
        let (from, to) = everything();
        let s = ReportSchedule::compute_with(
            &rd,
            &hourly(5),
            from,
            to,
            ScheduleOptions {
                start_interval_is_first_covered: true,
                ..Default::default()
            },
        );
        assert_eq!(s.due()[0].interval_ids, vec![1, 2, 3]);
    }

    #[test]
    fn frequency_zero_means_the_ven_decides() {
        let mut rd = descriptor();
        rd.frequency = 0;
        let s = schedule(&rd, &hourly(4));
        assert!(s.is_ad_hoc());
        assert!(s.due().is_empty());
    }

    #[test]
    fn a_report_only_event_leaves_timing_to_the_ven() {
        // No intervals and no `intervalPeriod`: nothing places the event in time, so the VEN
        // supplies both the timing and the intervals `[UG §7.3]`.
        let event = EventRequest::new("prg-1".parse().unwrap());
        let sequence = IntervalExpander::at(ts(MIDNIGHT)).sequence(&event).unwrap();
        assert!(sequence.is_empty());
        let s = schedule(&descriptor(), &sequence);
        assert!(s.is_ad_hoc());
        assert!(s.ven_chooses_intervals());
    }

    #[test]
    fn indefinite_repetition_is_bounded_by_the_event() {
        let mut rd = descriptor();
        rd.repeat = -1;
        rd.num_intervals = 1;
        rd.frequency = 1;
        rd.start_interval = 0;
        let s = schedule(&rd, &hourly(6));
        // It stops at the end of the event, not at the 1000-report ceiling.
        assert_eq!(s.due().len(), 6);
    }

    #[test]
    fn open_intervals_leave_the_ids_to_the_ven() {
        let mut rd = descriptor();
        rd.report_intervals = ReportIntervals::OpenIntervals;
        let s = schedule(&rd, &hourly(4));
        assert!(s.ven_chooses_intervals());
        assert!(s.due()[0].interval_ids.is_empty());
    }

    #[test]
    fn catch_up_after_downtime_can_skip_stale_windows() {
        let mut rd = descriptor();
        rd.start_interval = 0;
        rd.num_intervals = 1;
        rd.frequency = 1;
        rd.repeat = 6;
        let s = schedule(&rd, &hourly(6));
        let now = ts("2026-01-01T04:30:00Z");
        assert_eq!(s.overdue(now, false).len(), 4);
        let latest = s.overdue(now, true);
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].interval_ids, vec![3]);
        assert_eq!(
            s.next_after(now).unwrap().due_at,
            Some(ts("2026-01-01T05:00:00Z"))
        );
    }

    #[test]
    fn a_schedule_never_runs_past_the_last_interval() {
        let mut rd = descriptor();
        rd.start_interval = 0;
        rd.num_intervals = 2;
        rd.frequency = 2;
        rd.repeat = 100;
        let s = schedule(&rd, &hourly(5));
        assert!(s.due().len() <= 3);
        for d in s.due() {
            assert!(d.interval_ids.iter().all(|id| *id < 5));
        }
    }

    // -- intervals the event never lists ------------------------------------

    #[test]
    fn a_looping_event_reports_once_per_repetition() {
        // §7.5 "Looping intervals": 24 hourly intervals, `event.duration = P9999Y`. The schedule
        // continues past the declared list, and a report's sequence number counts from the first
        // interval — so the report the VEN owes on the third day is number 2 whichever window
        // found it.
        let mut event = hourly_event(24);
        event.duration = Some("P9999Y".parse().unwrap());
        let mut rd = descriptor();
        rd.repeat = -1;
        let sequence = IntervalExpander::at(ts(MIDNIGHT)).sequence(&event).unwrap();
        assert!(sequence.repeats());

        let s = ReportSchedule::compute(
            &rd,
            &sequence,
            ts("2026-01-03T00:00:00Z"),
            ts("2026-01-04T00:00:00Z"),
        );
        assert_eq!(s.due().len(), 2);
        assert_eq!(s.due()[0].sequence, 1);
        assert_eq!(s.due()[0].due_at, Some(ts("2026-01-03T00:00:00Z")));
        assert_eq!(s.due()[0].covers_from, ts("2026-01-02T00:00:00Z"));
        assert_eq!(s.due()[1].sequence, 2);
        assert_eq!(s.due()[1].due_at, Some(ts("2026-01-04T00:00:00Z")));
    }

    #[test]
    fn a_repetition_far_from_the_first_is_reached_by_arithmetic() {
        // The same tariff, looked at a year in. Walking there would be 365 repetitions; the
        // schedule is computed from the window, so the answer costs the same as the first day's.
        let mut event = hourly_event(24);
        event.duration = Some("P9999Y".parse().unwrap());
        let mut rd = descriptor();
        rd.repeat = -1;
        let sequence = IntervalExpander::at(ts(MIDNIGHT)).sequence(&event).unwrap();

        let s = ReportSchedule::compute(
            &rd,
            &sequence,
            ts("2027-01-01T00:00:00Z"),
            ts("2027-01-02T00:00:00Z"),
        );
        // Report `k` covers repetition `k` and falls due at its end, so the one due on the first
        // of 2027 is number 364 — 2026 is not a leap year.
        assert_eq!(s.due().len(), 2);
        assert_eq!(s.due()[0].sequence, 364);
        assert_eq!(s.due()[0].due_at, Some(ts("2027-01-01T00:00:00Z")));
        assert_eq!(s.due()[1].sequence, 365);
    }

    #[test]
    fn implied_intervals_carry_a_capability_forecast() {
        // §8.7: an event with an `intervalPeriod` and no `intervals` at all. Forty-eight hourly
        // intervals are implied by the period; the descriptor counts them.
        let mut event = EventRequest::new("prg-1".parse().unwrap());
        event.interval_period = Some(IntervalPeriod::new(
            StartTime::At(ts("2023-02-10T00:00:00Z")),
            "PT1H".parse::<Duration>().unwrap(),
        ));
        let mut rd = ReportDescriptor::new(PayloadType::new("LOAD_SHED_DELTA_AVAILABLE").unwrap());
        rd.start_interval = 0;
        rd.num_intervals = 48;
        rd.historical = false;
        rd.frequency = 1;
        rd.repeat = -1;

        let sequence = IntervalExpander::at(ts("2023-02-10T00:00:00Z"))
            .sequence(&event)
            .unwrap();
        assert!(sequence.is_implied());
        assert_eq!(sequence.declared_len(), 1);

        let s = ReportSchedule::compute(
            &rd,
            &sequence,
            ts("2023-02-10T00:00:00Z"),
            ts("2023-02-10T03:00:00Z"),
        );
        // Hourly, one per hour, each covering the following 48 hours.
        assert_eq!(s.due().len(), 4);
        assert_eq!(s.due()[0].due_at, Some(ts("2023-02-10T00:00:00Z")));
        assert_eq!(s.due()[0].covers_from, ts("2023-02-10T00:00:00Z"));
        assert_eq!(s.due()[0].covers_to, Some(ts("2023-02-12T00:00:00Z")));
        assert_eq!(s.due()[3].sequence, 3);
        assert_eq!(s.due()[3].due_at, Some(ts("2023-02-10T03:00:00Z")));
        // The specification leaves the ids of implied intervals to the VEN.
        assert!(s.ven_chooses_intervals());
        assert!(s.due()[0].interval_ids.is_empty());
    }

    #[test]
    fn an_implied_sequence_stops_when_the_event_does() {
        let mut event = EventRequest::new("prg-1".parse().unwrap());
        event.interval_period = Some(IntervalPeriod::new(
            StartTime::At(ts(MIDNIGHT)),
            "PT1H".parse::<Duration>().unwrap(),
        ));
        event.duration = Some("PT3H".parse().unwrap());
        let mut rd = descriptor();
        rd.start_interval = 0;
        rd.num_intervals = 1;
        rd.frequency = 1;
        rd.repeat = -1;
        rd.historical = false;

        let sequence = IntervalExpander::at(ts(MIDNIGHT)).sequence(&event).unwrap();
        assert_eq!(sequence.lifespan_end(), Some(ts("2026-01-01T03:00:00Z")));
        let s = schedule(&rd, &sequence);
        assert_eq!(s.due().len(), 3);
    }

    #[test]
    fn a_hostile_num_intervals_is_clamped_rather_than_walked() {
        // `numIntervals` is a bare `i32` on the wire, and a repeating sequence has no last interval
        // to clamp it against. Two billion intervals a thousand times over is not a schedule.
        let mut event = hourly_event(24);
        event.duration = Some("P9999Y".parse().unwrap());
        let mut rd = descriptor();
        rd.num_intervals = i32::MAX;
        rd.start_interval = 0;
        rd.historical = false;
        let sequence = IntervalExpander::at(ts(MIDNIGHT)).sequence(&event).unwrap();

        let (from, to) = everything();
        let s = ReportSchedule::compute_with(
            &rd,
            &sequence,
            from,
            to,
            ScheduleOptions {
                max_intervals: 100,
                ..Default::default()
            },
        );
        assert_eq!(s.due()[0].interval_ids.len(), 100);
    }

    #[test]
    fn max_reports_bounds_an_unbounded_descriptor() {
        let mut event = EventRequest::new("prg-1".parse().unwrap());
        event.interval_period = Some(IntervalPeriod::new(
            StartTime::At(ts(MIDNIGHT)),
            "PT1S".parse::<Duration>().unwrap(),
        ));
        let mut rd = descriptor();
        rd.start_interval = 0;
        rd.num_intervals = 1;
        rd.frequency = 1;
        rd.repeat = -1;
        rd.historical = false;
        let sequence = IntervalExpander::at(ts(MIDNIGHT)).sequence(&event).unwrap();

        let (from, to) = everything();
        let s = ReportSchedule::compute_with(
            &rd,
            &sequence,
            from,
            to,
            ScheduleOptions {
                max_reports: 10,
                ..Default::default()
            },
        );
        assert_eq!(s.due().len(), 10);
    }
}
