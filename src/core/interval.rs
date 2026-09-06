//! Turning an event's declared intervals into absolute time windows.
//!
//! This is where most of the specification's subtlety lives. An event may state its timing once and
//! let every interval inherit it; an interval may override any field; a missing start means "right
//! after the previous one"; `0001-01-01` means "now"; `P9999Y` means "no end"; a scalar payload
//! carrying several values silently subdivides its interval; and `event.duration` can loop or
//! truncate the whole sequence.
//!
//! All of it is resolved here, once, against an injected [`Clock`](super::Clock) — so the rest of
//! the crate, and every test, works with plain absolute windows.

use crate::std_shim::{ToString, Vec, vec};

use crate::model::{Duration, EventRequest, Interval, Timestamp};
use crate::schema;

/// An interval resolved to absolute time.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpandedInterval {
    /// The `id` the event gave this interval. Reports quote it to correlate their data.
    pub id: i32,
    /// Position in the event's interval list.
    pub index: usize,
    /// Which repetition of the interval sequence this is (`event.duration` looping).
    pub occurrence: u32,
    /// Which sub-interval of a multi-value payload this is.
    pub subinterval: u32,
    /// Absolute start.
    pub start: Timestamp,
    /// Absolute end, or `None` for an open-ended interval.
    pub end: Option<Timestamp>,
    /// Maximum offset a client may apply to `start`, in either direction.
    pub randomize_start: Option<Duration>,
    /// The payloads in force during this window.
    pub payloads: Vec<crate::model::ValuesMap>,
}

impl ExpandedInterval {
    /// Whether an instant falls inside this window (half-open: start inclusive, end exclusive).
    pub fn contains(&self, at: Timestamp) -> bool {
        at >= self.start && self.end.is_none_or(|e| at < e)
    }

    /// Whether the window has already closed at `at`.
    pub fn has_ended(&self, at: Timestamp) -> bool {
        self.end.is_some_and(|e| e <= at)
    }
}

/// Why an event could not be expanded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandError {
    /// An interval had neither its own start nor an inherited one.
    #[error("interval {index} has no start time and none can be inherited")]
    NoStart {
        /// Position of the offending interval.
        index: usize,
    },
    /// An interval had no duration and none could be inherited, so the next start is unknown.
    #[error("interval {index} has no duration, so the following interval's start is undefined")]
    NoDuration {
        /// Position of the offending interval.
        index: usize,
    },
    /// Arithmetic left the representable range.
    #[error("interval {index} overflows the representable time range: {detail}")]
    OutOfRange {
        /// Position of the offending interval.
        index: usize,
        /// The underlying message.
        detail: crate::std_shim::String,
    },
    /// The expansion hit its safety limit.
    #[error("expansion exceeded the limit of {limit} intervals")]
    TooManyIntervals {
        /// The configured limit.
        limit: usize,
    },
}

/// Expands events into absolute intervals.
#[derive(Debug, Clone)]
pub struct IntervalExpander {
    now: Timestamp,
    subdivide_multi_value: bool,
    limit: usize,
}

impl IntervalExpander {
    /// Default ceiling on how many intervals one expansion may produce.
    ///
    /// A looping event (`duration: P9999Y`) is unbounded by construction, so every expansion is
    /// bounded either by a window or by this limit.
    pub const DEFAULT_LIMIT: usize = 10_000;

    /// An expander that resolves "now" to `now`.
    pub fn at(now: Timestamp) -> Self {
        Self {
            now,
            subdivide_multi_value: true,
            limit: Self::DEFAULT_LIMIT,
        }
    }

    /// An expander reading the current time from a clock.
    pub fn new(clock: &dyn super::Clock) -> Self {
        Self::at(clock.now())
    }

    /// Turn off multi-value sub-interval splitting.
    ///
    /// With this off, an interval carrying three prices stays one interval with three values.
    pub fn without_subdivision(mut self) -> Self {
        self.subdivide_multi_value = false;
        self
    }

    /// Change the safety limit.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// The instant "now" resolves to.
    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// Expand one pass of the event's interval list, ignoring `event.duration` looping.
    pub fn expand(&self, event: &EventRequest) -> Result<Vec<ExpandedInterval>, ExpandError> {
        self.expand_pass(event)
    }

    /// The event's interval sequence, addressable past the end of its declared list.
    ///
    /// See [`IntervalSequence`]. This is the one place the repetition arithmetic lives; both
    /// [`IntervalExpander::expand_window`] and report scheduling read it, so a looping tariff
    /// cannot mean one thing to a timeline and another to a report.
    pub fn sequence(&self, event: &EventRequest) -> Result<IntervalSequence, ExpandError> {
        let base = self.expand_pass(event)?;

        // No declared intervals, but an `intervalPeriod` with both a start and a duration: the
        // interval structure is *implied* — one interval of that duration, repeating from that
        // start `[UG §7.3 'report-only' event with implied intervals, §8.7, §8.8]`. The count is
        // not in the event at all; a report descriptor's `numIntervals` supplies it.
        if base.is_empty() {
            let Some(period) = event.interval_period.as_ref() else {
                return Ok(IntervalSequence::empty());
            };
            let (Some(start), Some(duration)) = (period.start.as_ref(), period.duration.as_ref())
            else {
                return Ok(IntervalSequence::empty());
            };
            let start = start.resolve(self.now);
            let end = duration
                .checked_add_to(start)
                .map_err(|e| ExpandError::OutOfRange {
                    index: 0,
                    detail: e.to_string(),
                })?;
            // `P9999Y` as the tick would make every repetition nine thousand years long, which is
            // not an interval structure. Treat it as no structure at all.
            let Some(end) = end.filter(|e| *e > start) else {
                return Ok(IntervalSequence::empty());
            };
            let implied = ExpandedInterval {
                id: 0,
                index: 0,
                occurrence: 0,
                subinterval: 0,
                start,
                end: Some(end),
                randomize_start: period.randomize_start.clone(),
                payloads: Vec::new(),
            };
            return Ok(IntervalSequence {
                period: Some(end.as_nanosecond() - start.as_nanosecond()),
                start,
                lifespan_end: self.lifespan_end(event, start)?,
                implied: true,
                base: vec![implied],
            });
        }

        // Intervals may declare their own starts, so the list order is not the time order. The
        // sequence spans from the earliest start to the latest end.
        let start = base
            .iter()
            .map(|i| i.start)
            .min()
            .expect("base is not empty");
        // One open-ended interval cannot be followed by anything: there is no "after", so nothing
        // repeats however long `event.duration` is.
        let sequence_end = if base.iter().any(|i| i.end.is_none()) {
            None
        } else {
            base.iter().filter_map(|i| i.end).max()
        };
        let period = sequence_end
            .map(|e| e.as_nanosecond() - start.as_nanosecond())
            .filter(|p| *p > 0)
            .filter(|p| {
                event
                    .duration
                    .as_ref()
                    .is_some_and(|d| d.is_forever() || self.duration_exceeds(d, *p, start))
            });

        Ok(IntervalSequence {
            base,
            period,
            start,
            lifespan_end: self.lifespan_end(event, start)?,
            implied: false,
        })
    }

    /// Expand the event across a time window, applying `event.duration` looping and truncation.
    ///
    /// The window bounds the *result*, not the intervals: an interval that straddles `to` is
    /// returned whole. What does shorten an interval is the event's own `duration`, which truncates
    /// the sequence `[UG §7.3]` — that is a property of the event, not of who is looking at it.
    ///
    /// A repeating event is not walked repetition by repetition from wherever the sequence began.
    /// An event that has repeated every second since 2020 has two hundred million repetitions
    /// before today, and counting them is arithmetic rather than a search.
    pub fn expand_window(
        &self,
        event: &EventRequest,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<ExpandedInterval>, ExpandError> {
        let sequence = self.sequence(event)?;
        if sequence.is_empty() {
            return Ok(Vec::new());
        }
        let Some(range) = sequence.indices_overlapping(from, to) else {
            return Ok(Vec::new());
        };
        let count = range.end.saturating_sub(range.start);
        if count > self.limit as u64 {
            return Err(ExpandError::TooManyIntervals { limit: self.limit });
        }

        let mut out = Vec::with_capacity(count as usize);
        for index in range {
            match sequence.get(index) {
                Ok(Some(iv)) => out.push(iv),
                // Past the event's own end. Intervals may declare their own starts, so the list
                // order is not the time order and a later index may still be inside — skip rather
                // than stop.
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(select(out, from, to))
    }

    /// When the event stops, per `event.duration` (User Guide §7.3).
    ///
    /// `None` means it never stops.
    pub fn lifespan_end(
        &self,
        event: &EventRequest,
        sequence_start: Timestamp,
    ) -> Result<Option<Timestamp>, ExpandError> {
        match &event.duration {
            None => Ok(None),
            Some(d) if d.is_forever() => Ok(None),
            Some(d) => d
                .checked_add_to(sequence_start)
                .map_err(|e| ExpandError::OutOfRange {
                    index: 0,
                    detail: e.to_string(),
                }),
        }
    }

    fn duration_exceeds(&self, d: &Duration, period_nanos: i128, anchor: Timestamp) -> bool {
        match d.checked_add_to(anchor) {
            Ok(Some(end)) => (end.as_nanosecond() - anchor.as_nanosecond()) > period_nanos,
            _ => false,
        }
    }

    fn expand_pass(&self, event: &EventRequest) -> Result<Vec<ExpandedInterval>, ExpandError> {
        let default = event.interval_period.as_ref();
        let Some(intervals) = event.intervals.as_ref() else {
            return Ok(Vec::new());
        };

        let mut out: Vec<ExpandedInterval> = Vec::new();
        // The start the next interval inherits if it declares none.
        let mut cursor: Option<Timestamp> = default
            .and_then(|p| p.start.as_ref())
            .map(|s| s.resolve(self.now));

        for (index, interval) in intervals.iter().enumerate() {
            let own = interval.interval_period.as_ref();
            let start = match own.and_then(|p| p.start.as_ref()) {
                Some(s) => s.resolve(self.now),
                None => cursor.ok_or(ExpandError::NoStart { index })?,
            };
            let duration = own
                .and_then(|p| p.duration.clone())
                .or_else(|| default.and_then(|p| p.duration.clone()));
            let randomize = own
                .and_then(|p| p.randomize_start.clone())
                .or_else(|| default.and_then(|p| p.randomize_start.clone()));

            let end = match &duration {
                Some(d) => d
                    .checked_add_to(start)
                    .map_err(|e| ExpandError::OutOfRange {
                        index,
                        detail: e.to_string(),
                    })?,
                // No duration anywhere. The interval runs until the next one starts, so the next
                // one has to say when that is. Nothing following means it simply runs on.
                None => match intervals.get(index + 1) {
                    None => None,
                    Some(next) => Some(
                        next.interval_period
                            .as_ref()
                            .and_then(|p| p.start.as_ref())
                            .ok_or(ExpandError::NoDuration { index })?
                            .resolve(self.now),
                    ),
                },
            };

            cursor = end;

            let pieces = self.split(interval, index, start, end, randomize)?;
            out.extend(pieces);

            if out.len() > self.limit {
                return Err(ExpandError::TooManyIntervals { limit: self.limit });
            }
        }

        Ok(out)
    }

    /// Split an interval into equal sub-intervals when a scalar payload carries several values.
    fn split(
        &self,
        interval: &Interval,
        index: usize,
        start: Timestamp,
        end: Option<Timestamp>,
        randomize: Option<Duration>,
    ) -> Result<Vec<ExpandedInterval>, ExpandError> {
        let count = if self.subdivide_multi_value {
            interval
                .payloads
                .iter()
                .map(schema::subinterval_count)
                .max()
                .unwrap_or(1)
        } else {
            1
        };

        let one = |payloads: Vec<crate::model::ValuesMap>,
                   subinterval: u32,
                   start: Timestamp,
                   end: Option<Timestamp>| ExpandedInterval {
            id: interval.id,
            index,
            occurrence: 0,
            subinterval,
            start,
            end,
            randomize_start: randomize.clone(),
            payloads,
        };

        if count <= 1 {
            return Ok(vec![one(interval.payloads.clone(), 0, start, end)]);
        }
        // Refuse before allocating. One payload in an 8 MB body can name a million values, and
        // checking the length of the result afterwards is a check that runs after the damage.
        if count > self.limit {
            return Err(ExpandError::TooManyIntervals { limit: self.limit });
        }

        // Sub-intervals need a bounded parent: without an end there is nothing to divide.
        let Some(parent_end) = end else {
            return Ok(vec![one(interval.payloads.clone(), 0, start, end)]);
        };
        let total = parent_end.as_nanosecond() - start.as_nanosecond();
        let n = count as i128;

        let mut out = Vec::with_capacity(count);
        for k in 0..count {
            // Compute boundaries from the parent so rounding never accumulates.
            let s = shift(start, total * k as i128 / n)
                .map_err(|detail| ExpandError::OutOfRange { index, detail })?;
            let e = shift(start, total * (k as i128 + 1) / n)
                .map_err(|detail| ExpandError::OutOfRange { index, detail })?;
            let payloads = interval
                .payloads
                .iter()
                .map(|p| {
                    let mut p = p.clone();
                    if schema::subinterval_count(&p) == count {
                        p.values = vec![p.values[k].clone()];
                    }
                    p
                })
                .collect();
            out.push(one(payloads, k as u32, s, Some(e)));
        }
        Ok(out)
    }
}

/// An event's interval sequence, addressable past the end of its declared list.
///
/// Three shapes of event share one representation:
///
/// * **A plain event.** The declared intervals, once. Index `n` past the last one does not exist.
/// * **A looping event.** `event.duration` longer than the interval sequence repeats it
///   `[UG §7.3 Looping intervals]`. Index `n` is position `n % len` of repetition `n / len`,
///   shifted by that many periods.
/// * **An implied event.** No `intervals` at all, an `intervalPeriod` carrying a start and a
///   duration, and report descriptors that count intervals the event never lists
///   `[UG §7.3, §8.7, §8.8]`. The sequence is one interval of that duration, repeating.
///
/// Addressing it arithmetically rather than materialising it is what lets a `repeat = -1`
/// descriptor sit on a tariff that has looped hourly since 2020: report number fifty thousand is
/// two multiplications away, not fifty thousand clones.
#[derive(Debug, Clone, PartialEq)]
pub struct IntervalSequence {
    base: Vec<ExpandedInterval>,
    /// Nanoseconds from one repetition to the next, when the sequence repeats at all.
    period: Option<i128>,
    start: Timestamp,
    lifespan_end: Option<Timestamp>,
    implied: bool,
}

impl IntervalSequence {
    fn empty() -> Self {
        Self {
            base: Vec::new(),
            period: None,
            start: Timestamp::UNIX_EPOCH,
            lifespan_end: None,
            implied: false,
        }
    }

    /// Whether the event places nothing in time at all — a report-only event whose intervals the
    /// VEN chooses `[UG §7.3 'report-only' event with VEN-determined intervals]`.
    pub fn is_empty(&self) -> bool {
        self.base.is_empty()
    }

    /// The declared intervals, once, before any repetition.
    ///
    /// This is what a descriptor's `numIntervals = -1` ("all intervals") counts.
    pub fn declared(&self) -> &[ExpandedInterval] {
        &self.base
    }

    /// How many intervals one repetition holds.
    pub fn declared_len(&self) -> usize {
        self.base.len()
    }

    /// Whether the sequence continues past its declared list.
    pub fn repeats(&self) -> bool {
        self.period.is_some()
    }

    /// Whether the interval structure was implied by the event's `intervalPeriod` rather than
    /// listed. Such intervals carry no payloads: they exist to anchor reports.
    pub fn is_implied(&self) -> bool {
        self.implied
    }

    /// Nanoseconds from one repetition to the next, if it repeats.
    pub fn period(&self) -> Option<i128> {
        self.period
    }

    /// The earliest start in the sequence.
    pub fn start(&self) -> Timestamp {
        self.start
    }

    /// When the event stops, per its own `duration`. `None` means it never does.
    pub fn lifespan_end(&self) -> Option<Timestamp> {
        self.lifespan_end
    }

    /// The interval at an absolute index, or `None` if it is past the event's end.
    ///
    /// Truncation by `event.duration` happens here, so every caller — timeline, report schedule,
    /// conformance check — sees the same shortened last interval.
    pub fn get(&self, index: u64) -> Result<Option<ExpandedInterval>, ExpandError> {
        let len = self.base.len() as u64;
        if len == 0 {
            return Ok(None);
        }
        let (occurrence, position) = match self.period {
            Some(_) => (index / len, (index % len) as usize),
            None if index < len => (0, index as usize),
            None => return Ok(None),
        };

        let mut interval = self.base[position].clone();
        interval.occurrence = u32::try_from(occurrence).unwrap_or(u32::MAX);

        let offset = self.period.unwrap_or(0) * occurrence as i128;
        if offset != 0 {
            let index = interval.index;
            interval.start = shift(interval.start, offset)
                .map_err(|detail| ExpandError::OutOfRange { index, detail })?;
            interval.end = match interval.end {
                Some(end) => Some(
                    shift(end, offset)
                        .map_err(|detail| ExpandError::OutOfRange { index, detail })?,
                ),
                None => None,
            };
        }

        if let Some(end) = self.lifespan_end {
            if interval.start >= end {
                return Ok(None);
            }
            if interval.end.is_none_or(|e| e > end) {
                interval.end = Some(end);
            }
        }
        Ok(Some(interval))
    }

    /// The indices whose intervals can overlap `[from, to)`.
    ///
    /// A conservative bound rather than an exact set: within one repetition the declared intervals
    /// need not be in time order, so the range covers whole repetitions and the caller filters.
    pub fn indices_overlapping(
        &self,
        from: Timestamp,
        to: Timestamp,
    ) -> Option<core::ops::Range<u64>> {
        let len = self.base.len() as u64;
        if len == 0 {
            return None;
        }
        let Some(period) = self.period else {
            return Some(0..len);
        };

        // The last instant any repetition may start at.
        let stop = match self.lifespan_end {
            Some(end) => end.min(to),
            None => to,
        };
        if stop <= self.start {
            return None;
        }
        let sequence_end = self.start.as_nanosecond().checked_add(period)?;

        // First repetition that can still be in the window: the smallest k with
        // `sequence_end + k·period > from`.
        let first = if sequence_end > from.as_nanosecond() {
            0
        } else {
            (from.as_nanosecond() - sequence_end) / period + 1
        };
        // One past the last: the smallest k with `start + k·period >= stop`.
        let span = stop.as_nanosecond() - self.start.as_nanosecond();
        let last = span / period + i128::from(span % period != 0);
        if first >= last {
            return None;
        }
        let first = u64::try_from(first).ok()?;
        let last = u64::try_from(last).ok()?;
        Some(first.saturating_mul(len)..last.saturating_mul(len))
    }
}

fn shift(t: Timestamp, nanos: i128) -> Result<Timestamp, crate::std_shim::String> {
    let target = t
        .as_nanosecond()
        .checked_add(nanos)
        .ok_or_else(|| "timestamp overflow".to_string())?;
    Timestamp::from_nanosecond(target).map_err(|e| e.to_string())
}

/// Keep the intervals that overlap `[from, to)`.
///
/// The window changes only which intervals are worth returning — an interval that runs past `to`
/// comes back with its real end, because reporting a shortened one would be a lie about the
/// schedule. Truncation by the event's own `duration` is a different thing and happens where the
/// interval is produced, in [`IntervalSequence::get`].
fn select(
    intervals: Vec<ExpandedInterval>,
    from: Timestamp,
    to: Timestamp,
) -> Vec<ExpandedInterval> {
    intervals
        .into_iter()
        .filter(|i| {
            let ends_after_window_start = i.end.is_none_or(|e| e > from);
            let starts_before_window_end = i.start < to;
            ends_after_window_start && starts_before_window_end
        })
        .collect()
}

/// The window over which an event is live, resolved once at write time.
///
/// `[UG §7.3]`: an event whose intervals have all elapsed is inactive. Computing that on every read
/// would mean expanding every event on every request; computing it once, when the event is written,
/// turns `?active=true` into a comparison of two timestamps — which is also exactly the pair of
/// columns a SQL backend needs.
///
/// Returns `None` when the event places nothing in time — a report-only event whose intervals the
/// VEN chooses, which is always live. The end is `None` for an event that never ends: one that
/// loops, one whose structure is implied, or one carrying an open-ended interval.
pub fn active_window(
    event: &EventRequest,
    now: Timestamp,
) -> Result<Option<(Timestamp, Option<Timestamp>)>, ExpandError> {
    let sequence = IntervalExpander::at(now).sequence(event)?;
    if sequence.is_empty() {
        return Ok(None);
    }
    let start = sequence.start();
    let end = if event.duration.is_some() || sequence.repeats() {
        // The event's own duration wins: it truncates the intervals, or extends them by looping.
        // A sequence that repeats and is never cut off runs for ever.
        sequence.lifespan_end()
    } else if sequence.declared().iter().any(|i| i.end.is_none()) {
        // One open-ended interval makes the whole event open-ended.
        None
    } else {
        sequence.declared().iter().filter_map(|i| i.end).max()
    };
    Ok(Some((start, end)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IntervalPeriod, ObjectId, StartTime, Value, ValuesMap};
    use rust_decimal::Decimal;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn price(v: i64) -> ValuesMap {
        ValuesMap::single("PRICE".parse().unwrap(), Value::Number(Decimal::from(v)))
    }

    fn event(period: Option<IntervalPeriod>, intervals: Vec<Interval>) -> EventRequest {
        let mut e = EventRequest::new(ObjectId::new("p1").unwrap());
        e.interval_period = period;
        e.intervals = Some(intervals);
        e
    }

    fn expander() -> IntervalExpander {
        IntervalExpander::at(ts("2026-02-11T06:00:00Z"))
    }

    #[test]
    fn contiguous_intervals_inherit_the_event_period() {
        let e = event(
            Some(IntervalPeriod::new(
                StartTime::At(ts("2026-02-11T00:00:00Z")),
                "PT1H".parse().unwrap(),
            )),
            vec![
                Interval::new(0, vec![price(1)]),
                Interval::new(1, vec![price(2)]),
                Interval::new(2, vec![price(3)]),
            ],
        );
        let out = expander().expand(&e).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].start, ts("2026-02-11T00:00:00Z"));
        assert_eq!(out[0].end, Some(ts("2026-02-11T01:00:00Z")));
        assert_eq!(out[1].start, ts("2026-02-11T01:00:00Z"));
        assert_eq!(out[2].end, Some(ts("2026-02-11T03:00:00Z")));
    }

    #[test]
    fn an_interval_overrides_only_the_fields_it_states() {
        // User Guide example 7.4-1: the second interval overrides duration but not start.
        let e = event(
            Some(IntervalPeriod::new(
                StartTime::At(ts("2023-02-10T00:00:00Z")),
                "PT1H".parse().unwrap(),
            )),
            vec![
                Interval::new(0, vec![price(1)]),
                Interval::new(1, vec![price(2)]).with_period(IntervalPeriod {
                    start: None,
                    duration: Some("PT2H".parse().unwrap()),
                    randomize_start: None,
                }),
                Interval::new(2, vec![price(3)]),
            ],
        );
        let out = expander().expand(&e).unwrap();
        assert_eq!(out[1].start, ts("2023-02-10T01:00:00Z"));
        assert_eq!(out[1].end, Some(ts("2023-02-10T03:00:00Z")));
        // The third interval follows the lengthened second one.
        assert_eq!(out[2].start, ts("2023-02-10T03:00:00Z"));
    }

    #[test]
    fn the_now_sentinel_resolves_against_the_clock() {
        let e = event(
            Some(IntervalPeriod::new(StartTime::Now, "PT1H".parse().unwrap())),
            vec![Interval::new(0, vec![price(1)])],
        );
        let out = expander().expand(&e).unwrap();
        assert_eq!(out[0].start, ts("2026-02-11T06:00:00Z"));
        assert_eq!(out[0].end, Some(ts("2026-02-11T07:00:00Z")));
    }

    #[test]
    fn forever_yields_an_open_ended_interval() {
        let e = event(
            Some(IntervalPeriod::new(
                StartTime::Now,
                "P9999Y".parse().unwrap(),
            )),
            vec![Interval::new(0, vec![price(1)])],
        );
        let out = expander().expand(&e).unwrap();
        assert_eq!(out[0].end, None);
        assert!(out[0].contains(ts("2099-01-01T00:00:00Z")));
    }

    #[test]
    fn multi_value_payloads_split_into_equal_sub_intervals() {
        // User Guide example 7.3-1: three prices in a PT3H interval.
        let payload = ValuesMap::new(
            "PRICE".parse().unwrap(),
            vec![
                Value::Number(Decimal::from(17)),
                Value::Number(Decimal::from(3)),
                Value::Number(Decimal::from(11)),
            ],
        );
        let e = event(
            None,
            vec![
                Interval::new(0, vec![payload]).with_period(IntervalPeriod::new(
                    StartTime::At(ts("2025-06-25T00:00:00Z")),
                    "PT3H".parse().unwrap(),
                )),
            ],
        );
        let out = expander().expand(&e).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].start, ts("2025-06-25T00:00:00Z"));
        assert_eq!(out[0].end, Some(ts("2025-06-25T01:00:00Z")));
        assert_eq!(out[2].start, ts("2025-06-25T02:00:00Z"));
        assert_eq!(out[2].end, Some(ts("2025-06-25T03:00:00Z")));
        // Each sub-interval carries exactly its own value.
        assert_eq!(
            out[1].payloads[0].values,
            vec![Value::Number(Decimal::from(3))]
        );
        // And they all keep the parent interval's id, so reports can correlate.
        assert!(out.iter().all(|i| i.id == 0));
    }

    #[test]
    fn sub_interval_boundaries_do_not_drift() {
        // An hour split three ways must still end exactly on the hour.
        let payload = ValuesMap::new(
            "PRICE".parse().unwrap(),
            (1..=3).map(|v| Value::Number(Decimal::from(v))).collect(),
        );
        let e = event(
            Some(IntervalPeriod::new(
                StartTime::At(ts("2026-01-01T00:00:00Z")),
                "PT1H".parse().unwrap(),
            )),
            vec![Interval::new(0, vec![payload])],
        );
        let out = expander().expand(&e).unwrap();
        assert_eq!(out.last().unwrap().end, Some(ts("2026-01-01T01:00:00Z")));
    }

    #[test]
    fn curves_are_not_subdivided() {
        let curve = ValuesMap::new(
            "CURVE".parse().unwrap(),
            vec![
                Value::Point(crate::model::Point::new(Decimal::from(1), Decimal::from(2))),
                Value::Point(crate::model::Point::new(Decimal::from(3), Decimal::from(4))),
            ],
        );
        let e = event(
            Some(IntervalPeriod::new(
                StartTime::At(ts("2026-01-01T00:00:00Z")),
                "PT1H".parse().unwrap(),
            )),
            vec![Interval::new(0, vec![curve])],
        );
        assert_eq!(expander().expand(&e).unwrap().len(), 1);
    }

    #[test]
    fn a_payload_with_more_values_than_the_limit_is_refused_not_allocated() {
        // Regression: the sub-interval split used to build the whole vector and check the limit
        // afterwards, so one payload in an 8 MB body could ask for millions of allocations.
        let payload = ValuesMap::new(
            "PRICE".parse().unwrap(),
            (0..5_000)
                .map(|v| Value::Number(Decimal::from(v)))
                .collect(),
        );
        let e = event(
            Some(IntervalPeriod::new(
                StartTime::At(ts("2026-01-01T00:00:00Z")),
                "PT1H".parse().unwrap(),
            )),
            vec![Interval::new(0, vec![payload])],
        );
        assert!(matches!(
            expander().with_limit(100).expand(&e),
            Err(ExpandError::TooManyIntervals { limit: 100 })
        ));
    }

    #[test]
    fn event_duration_loops_the_interval_sequence() {
        // 24 hourly prices with duration P7D repeat for a week (User Guide §7.3).
        let mut e = event(
            Some(IntervalPeriod::new(
                StartTime::At(ts("2026-01-01T00:00:00Z")),
                "PT12H".parse().unwrap(),
            )),
            vec![
                Interval::new(0, vec![price(1)]),
                Interval::new(1, vec![price(2)]),
            ],
        );
        e.duration = Some("P3D".parse().unwrap());
        let out = expander()
            .expand_window(&e, ts("2026-01-01T00:00:00Z"), ts("2026-01-05T00:00:00Z"))
            .unwrap();
        // One day per pass, three passes.
        assert_eq!(out.len(), 6);
        assert_eq!(out[0].occurrence, 0);
        assert_eq!(out[2].occurrence, 1);
        assert_eq!(out[2].start, ts("2026-01-02T00:00:00Z"));
        // Nothing past the event's own end.
        assert!(out.iter().all(|i| i.start < ts("2026-01-04T00:00:00Z")));
    }

    #[test]
    fn event_duration_truncates_a_longer_sequence() {
        let mut e = event(
            Some(IntervalPeriod::new(
                StartTime::At(ts("2026-01-01T00:00:00Z")),
                "PT1H".parse().unwrap(),
            )),
            (0..24)
                .map(|i| Interval::new(i, vec![price(i as i64)]))
                .collect(),
        );
        e.duration = Some("PT12H".parse().unwrap());
        let out = expander()
            .expand_window(&e, ts("2026-01-01T00:00:00Z"), ts("2026-01-02T00:00:00Z"))
            .unwrap();
        assert_eq!(
            out.len(),
            12,
            "the last 12 intervals fall outside the event"
        );
        assert_eq!(out.last().unwrap().end, Some(ts("2026-01-01T12:00:00Z")));
    }

    #[test]
    fn an_infinite_event_is_bounded_by_the_window_not_by_memory() {
        let mut e = event(
            Some(IntervalPeriod::new(
                StartTime::At(ts("2026-01-01T00:00:00Z")),
                "PT1H".parse().unwrap(),
            )),
            vec![Interval::new(0, vec![price(1)])],
        );
        e.duration = Some("P9999Y".parse().unwrap());
        let out = expander()
            .expand_window(&e, ts("2026-01-01T00:00:00Z"), ts("2026-01-01T06:00:00Z"))
            .unwrap();
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn a_long_running_repetition_is_reached_by_arithmetic_not_by_walking() {
        // Regression: this used to iterate once per second since 2020 — about two hundred million
        // times — before producing sixty intervals.
        let mut e = event(
            Some(IntervalPeriod::new(
                StartTime::At(ts("2020-01-01T00:00:00Z")),
                "PT1S".parse().unwrap(),
            )),
            vec![Interval::new(0, vec![price(1)])],
        );
        e.duration = Some("P9999Y".parse().unwrap());
        let out = expander()
            .expand_window(&e, ts("2026-01-01T00:00:00Z"), ts("2026-01-01T00:01:00Z"))
            .unwrap();
        assert_eq!(out.len(), 60);
        assert_eq!(out[0].start, ts("2026-01-01T00:00:00Z"));
        assert_eq!(out[59].end, Some(ts("2026-01-01T00:01:00Z")));
    }

    #[test]
    fn a_repeating_sequence_spans_from_its_earliest_start_to_its_latest_end() {
        // Intervals may carry their own starts, so the declaration order is not the time order.
        // Taking the first and last of the list as the sequence bounds gets the period wrong, and
        // every repetition after the first lands in the wrong place.
        let mut e = event(
            None,
            vec![
                Interval::new(0, vec![price(2)]).with_period(IntervalPeriod::new(
                    StartTime::At(ts("2026-01-01T01:00:00Z")),
                    "PT1H".parse().unwrap(),
                )),
                Interval::new(1, vec![price(1)]).with_period(IntervalPeriod::new(
                    StartTime::At(ts("2026-01-01T00:00:00Z")),
                    "PT1H".parse().unwrap(),
                )),
            ],
        );
        e.duration = Some("PT6H".parse().unwrap());
        let out = expander()
            .expand_window(&e, ts("2026-01-01T00:00:00Z"), ts("2026-01-02T00:00:00Z"))
            .unwrap();

        // Two hours per repetition, three repetitions inside PT6H — not one hour and six.
        assert_eq!(out.len(), 6);
        let mut starts: Vec<_> = out.iter().map(|i| i.start).collect();
        starts.sort();
        assert_eq!(starts.first(), Some(&ts("2026-01-01T00:00:00Z")));
        assert_eq!(starts.last(), Some(&ts("2026-01-01T05:00:00Z")));
    }

    #[test]
    fn a_window_selects_intervals_without_shortening_them() {
        // The window says which intervals are worth returning; it does not rewrite their ends.
        // Only `event.duration` does that.
        let e = event(
            Some(IntervalPeriod::new(
                StartTime::At(ts("2026-01-01T00:00:00Z")),
                "PT6H".parse().unwrap(),
            )),
            vec![Interval::new(0, vec![price(1)])],
        );
        let out = expander()
            .expand_window(&e, ts("2026-01-01T00:00:00Z"), ts("2026-01-01T01:00:00Z"))
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].end, Some(ts("2026-01-01T06:00:00Z")));
    }

    #[test]
    fn a_missing_start_with_no_inheritance_is_an_error() {
        let e = event(None, vec![Interval::new(0, vec![price(1)])]);
        assert!(matches!(
            expander().expand(&e),
            Err(ExpandError::NoStart { index: 0 })
        ));
    }

    #[test]
    fn an_interval_that_cannot_be_closed_before_the_next_one_is_an_error() {
        // Two intervals, neither with a duration and neither with a start of its own: the second
        // has nowhere to begin, and silently stacking them at the same instant would be worse than
        // saying so.
        let e = event(
            Some(IntervalPeriod {
                start: Some(StartTime::At(ts("2026-01-01T00:00:00Z"))),
                duration: None,
                randomize_start: None,
            }),
            vec![
                Interval::new(0, vec![price(1)]),
                Interval::new(1, vec![price(2)]),
            ],
        );
        assert!(matches!(
            expander().expand(&e),
            Err(ExpandError::NoDuration { index: 0 })
        ));
    }

    #[test]
    fn a_report_only_event_expands_to_nothing() {
        let mut e = EventRequest::new(ObjectId::new("p1").unwrap());
        e.intervals = None;
        assert!(expander().expand(&e).unwrap().is_empty());
    }

    // -- the interval sequence ---------------------------------------------

    #[test]
    fn an_event_with_no_intervals_implies_them_from_its_interval_period() {
        // `[UG §7.3 'report-only' event with implied intervals]`: no `intervals`, but an
        // `intervalPeriod` carrying a start and a tick. The structure is one interval of that
        // length, repeating, and it carries no payloads because there is nothing to dispatch.
        let mut e = EventRequest::new(ObjectId::new("p1").unwrap());
        e.interval_period = Some(IntervalPeriod::new(
            StartTime::At(ts("2026-02-11T00:00:00Z")),
            "PT1H".parse().unwrap(),
        ));
        let seq = expander().sequence(&e).unwrap();
        assert!(seq.is_implied());
        assert!(seq.repeats());
        assert_eq!(seq.declared_len(), 1);
        assert_eq!(seq.lifespan_end(), None);

        let fifth = seq.get(5).unwrap().unwrap();
        assert_eq!(fifth.start, ts("2026-02-11T05:00:00Z"));
        assert_eq!(fifth.end, Some(ts("2026-02-11T06:00:00Z")));
        assert!(fifth.payloads.is_empty());
    }

    #[test]
    fn an_implied_sequence_needs_both_a_start_and_a_tick() {
        let mut e = EventRequest::new(ObjectId::new("p1").unwrap());
        e.interval_period = Some(IntervalPeriod {
            start: Some(StartTime::At(ts("2026-02-11T00:00:00Z"))),
            duration: None,
            randomize_start: None,
        });
        assert!(expander().sequence(&e).unwrap().is_empty());

        // `P9999Y` as the tick is "no end", not a nine-thousand-year interval structure.
        e.interval_period = Some(IntervalPeriod::new(
            StartTime::At(ts("2026-02-11T00:00:00Z")),
            "P9999Y".parse().unwrap(),
        ));
        assert!(expander().sequence(&e).unwrap().is_empty());
    }

    #[test]
    fn a_declared_sequence_repeats_only_when_the_event_duration_says_so() {
        let mut e = event(
            Some(IntervalPeriod::new(
                StartTime::At(ts("2026-02-11T00:00:00Z")),
                "PT1H".parse().unwrap(),
            )),
            vec![
                Interval::new(0, vec![price(1)]),
                Interval::new(1, vec![price(2)]),
            ],
        );
        assert!(!expander().sequence(&e).unwrap().repeats());

        e.duration = Some("PT2H".parse().unwrap());
        assert!(!expander().sequence(&e).unwrap().repeats());

        e.duration = Some("PT6H".parse().unwrap());
        let seq = expander().sequence(&e).unwrap();
        assert!(seq.repeats());
        assert_eq!(seq.period(), Some(2 * 3_600 * 1_000_000_000));
        // Index 4 is position 0 of the third repetition.
        let third = seq.get(4).unwrap().unwrap();
        assert_eq!(third.start, ts("2026-02-11T04:00:00Z"));
        assert_eq!(third.occurrence, 2);
        assert_eq!(third.id, 0);
        // The event's own duration cuts the sequence off; nothing exists past six hours.
        assert!(seq.get(6).unwrap().is_none());
    }

    #[test]
    fn the_last_interval_is_truncated_by_the_events_own_duration() {
        let mut e = event(
            Some(IntervalPeriod::new(
                StartTime::At(ts("2026-02-11T00:00:00Z")),
                "PT1H".parse().unwrap(),
            )),
            vec![
                Interval::new(0, vec![price(1)]),
                Interval::new(1, vec![price(2)]),
            ],
        );
        e.duration = Some("PT90M".parse().unwrap());
        let seq = expander().sequence(&e).unwrap();
        assert_eq!(
            seq.get(1).unwrap().unwrap().end,
            Some(ts("2026-02-11T01:30:00Z"))
        );
    }

    #[test]
    fn an_implied_event_is_active_from_its_start_and_never_ends() {
        let mut e = EventRequest::new(ObjectId::new("p1").unwrap());
        e.interval_period = Some(IntervalPeriod::new(
            StartTime::At(ts("2026-02-11T00:00:00Z")),
            "PT1H".parse().unwrap(),
        ));
        let window = active_window(&e, ts("2026-02-11T06:00:00Z")).unwrap();
        assert_eq!(window, Some((ts("2026-02-11T00:00:00Z"), None)));

        // An event with nothing to place in time is always live, which is what `None` says.
        let bare = EventRequest::new(ObjectId::new("p1").unwrap());
        assert_eq!(
            active_window(&bare, ts("2026-02-11T06:00:00Z")).unwrap(),
            None
        );
    }

    #[test]
    fn a_window_far_from_the_first_repetition_costs_no_more_than_the_first() {
        // A tariff looping hourly since 2020 has forty thousand repetitions behind it. Asking for
        // one hour of it must not walk them.
        let mut e = event(
            Some(IntervalPeriod::new(
                StartTime::At(ts("2020-01-01T00:00:00Z")),
                "PT1H".parse().unwrap(),
            )),
            vec![Interval::new(0, vec![price(1)])],
        );
        e.duration = Some("P9999Y".parse().unwrap());
        let out = expander()
            .expand_window(&e, ts("2026-02-11T06:00:00Z"), ts("2026-02-11T08:00:00Z"))
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].start, ts("2026-02-11T06:00:00Z"));
        assert_eq!(out[1].start, ts("2026-02-11T07:00:00Z"));
    }
}
