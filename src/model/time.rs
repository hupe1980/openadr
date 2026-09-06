//! Time, durations, and the two magic values the specification hides inside them.
//!
//! OpenADR overloads ordinary RFC 3339 / ISO 8601 fields with sentinels:
//!
//! * `intervalPeriod.start` of `0001-01-01` or `0001-01-01T00:00:00` means **now** — a "do it now"
//!   event whose first interval began before the client read it (User Guide §7.3).
//! * `duration` of `P9999Y` means **forever** — "as agreed to by communicating parties" in ISO 8601
//!   parlance (User Guide §7.3).
//!
//! Both are represented as explicit enum variants and are matched *before* delegating to a
//! date-time library, so the meaning of a schedule never depends on a parser's range limits.

use crate::std_shim::{String, ToString};
use core::{fmt, str::FromStr};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Unexpected};

/// An instant on the wire, RFC 3339 with an offset.
pub type Timestamp = jiff::Timestamp;

/// The sentinel start value meaning "now", in the two forms the User Guide allows.
const NOW_SENTINELS: &[&str] = &[
    "0001-01-01",
    "0001-01-01T00:00:00",
    "0001-01-01T00:00:00Z",
    "0001-01-01T00:00:00.000Z",
    "0001-01-01T00:00:00+00:00",
];

/// Canonical rendering of the "now" sentinel when we serialize.
const NOW_CANONICAL: &str = "0001-01-01T00:00:00Z";

/// Canonical rendering of the "forever" sentinel when we serialize.
const FOREVER: &str = "P9999Y";

/// Year count at or above which a duration means "forever" rather than a span.
///
/// The specification writes the sentinel as the literal `P9999Y`, and this crate emits exactly
/// that. Recognising it is a different question from writing it: a peer that normalises durations
/// sends `P9999Y0M0DT0H0M0S`, which is the same value written differently and which a literal
/// string comparison does not recognise.
///
/// That is not hypothetical: a deployed VTN returns exactly that string, and read as an ordinary
/// span it is 9999 years — which, added to any instant after the epoch, leaves the representable
/// range. The expansion then fails, and a VEN's timeline records the event as *skipped*: the
/// perpetual price signal simply vanishes, silently, at the one implementation boundary that
/// matters.
const FOREVER_YEARS: i64 = 9999;

/// Errors from parsing a spec time value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimeError {
    /// Not a valid RFC 3339 timestamp.
    #[error("invalid RFC 3339 timestamp: {0}")]
    Timestamp(String),
    /// Not a valid ISO 8601 duration, or one the schema's pattern forbids.
    #[error("invalid ISO 8601 duration: {0}")]
    Duration(String),
    /// A duration that is syntactically fine but cannot be applied to this instant.
    #[error("duration cannot be applied at this instant: {0}")]
    OutOfRange(String),
}

/// The start of an interval or of an event's set of intervals.
///
/// `Now` carries no timestamp on purpose: resolving it requires a clock, which the wire model does
/// not have. [`crate::core::IntervalExpander`] resolves it against an injected
/// [`Clock`](crate::core::Clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StartTime {
    /// The `0001-01-01` sentinel: start is "now" from the reader's point of view.
    Now,
    /// An absolute instant.
    At(Timestamp),
}

impl StartTime {
    /// The instant, if this is an absolute start.
    pub fn absolute(&self) -> Option<Timestamp> {
        match self {
            StartTime::Now => None,
            StartTime::At(t) => Some(*t),
        }
    }

    /// Resolve against a reference instant, mapping [`StartTime::Now`] to `now`.
    pub fn resolve(&self, now: Timestamp) -> Timestamp {
        match self {
            StartTime::Now => now,
            StartTime::At(t) => *t,
        }
    }

    /// Whether this is the "do it now" sentinel.
    pub fn is_now(&self) -> bool {
        matches!(self, StartTime::Now)
    }
}

impl From<Timestamp> for StartTime {
    fn from(t: Timestamp) -> Self {
        StartTime::At(t)
    }
}

impl FromStr for StartTime {
    type Err = TimeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if NOW_SENTINELS
            .iter()
            .any(|n| n.eq_ignore_ascii_case(trimmed))
        {
            return Ok(StartTime::Now);
        }
        // A bare date is legal in the sentinel forms only; anything else must be a full timestamp.
        trimmed
            .parse::<Timestamp>()
            .map(StartTime::At)
            .map_err(|_| TimeError::Timestamp(trimmed.to_string()))
    }
}

impl fmt::Display for StartTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartTime::Now => f.write_str(NOW_CANONICAL),
            StartTime::At(t) => write!(f, "{t}"),
        }
    }
}

impl Serialize for StartTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for StartTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(|_| {
            serde::de::Error::invalid_value(Unexpected::Str(&s), &"an RFC 3339 timestamp")
        })
    }
}

/// An ISO 8601 duration, with the `P9999Y` "forever" sentinel lifted into the type.
///
/// Equality is *fieldwise*: `PT60M` and `PT1H` are different durations here, because they render
/// differently on the wire and a round trip must be lossless. Use [`Duration::as_secs`] to compare
/// lengths.
#[derive(Debug, Clone)]
pub enum Duration {
    /// `P9999Y` — unbounded.
    Forever,
    /// A bounded span.
    Finite(jiff::Span),
}

impl Duration {
    /// The schema default, `PT0S`.
    pub fn zero() -> Self {
        Duration::Finite(jiff::Span::new())
    }

    /// A span of whole seconds.
    pub fn from_secs(secs: i64) -> Result<Self, TimeError> {
        jiff::Span::new()
            .try_seconds(secs)
            .map(Duration::Finite)
            .map_err(|e| TimeError::Duration(e.to_string()))
    }

    /// A span of nanoseconds, balanced into seconds plus a sub-second remainder.
    ///
    /// Balancing matters for the wire format: an unbalanced nanosecond span renders as an absurd
    /// `PT3600000000000S` rather than `PT1H`-scale text, and [`Duration::as_secs`] would have to
    /// know to look at the nanosecond field.
    pub fn from_nanos(nanos: i128) -> Result<Self, TimeError> {
        let secs = i64::try_from(nanos / 1_000_000_000)
            .map_err(|_| TimeError::OutOfRange("duration overflows i64 seconds".to_string()))?;
        let rem = (nanos % 1_000_000_000) as i64;
        let mut span = jiff::Span::new()
            .try_seconds(secs)
            .map_err(|e| TimeError::Duration(e.to_string()))?;
        if rem != 0 {
            span = span
                .try_nanoseconds(rem)
                .map_err(|e| TimeError::Duration(e.to_string()))?;
        }
        Ok(Duration::Finite(span))
    }

    /// Whether this is the unbounded sentinel.
    pub fn is_forever(&self) -> bool {
        matches!(self, Duration::Forever)
    }

    /// Whether this duration runs backwards.
    ///
    /// The schema's pattern begins `^(-?)P`, so a negative duration is *well formed* and meaningless
    /// everywhere OpenADR puts one — the sign is an artefact of a generic ISO 8601 regex. The wire
    /// model therefore parses one, so a client can say what it received, and the VTN refuses it at
    /// the boundary, where a rule about meaning belongs (D-126).
    pub fn is_negative(&self) -> bool {
        match self {
            Duration::Forever => false,
            Duration::Finite(s) => s.is_negative(),
        }
    }

    /// Whether this is a zero-length span (used to cancel an event, User Guide §7.9).
    pub fn is_zero(&self) -> bool {
        match self {
            Duration::Forever => false,
            Duration::Finite(s) => s.is_zero(),
        }
    }

    /// The underlying span, if bounded.
    pub fn span(&self) -> Option<&jiff::Span> {
        match self {
            Duration::Forever => None,
            Duration::Finite(s) => Some(s),
        }
    }

    /// Add this duration to an instant.
    ///
    /// Returns `Ok(None)` for [`Duration::Forever`] — the caller decides what an unbounded end
    /// means in its context. Calendar units (`Y`, `M`, `W`, `D`) are resolved against UTC, so a
    /// month added to 31 January lands on 28/29 February rather than overflowing.
    pub fn checked_add_to(&self, at: Timestamp) -> Result<Option<Timestamp>, TimeError> {
        let Some(span) = self.span() else {
            return Ok(None);
        };
        at.to_zoned(jiff::tz::TimeZone::UTC)
            .checked_add(*span)
            .map(|z| Some(z.timestamp()))
            .map_err(|e| TimeError::OutOfRange(e.to_string()))
    }

    /// Length in seconds, if bounded and free of calendar units.
    ///
    /// Calendar units have no fixed length, so this returns `None` for them; use
    /// [`Duration::checked_add_to`] when an anchor instant is available. Sub-second components are
    /// included and truncated towards zero.
    pub fn as_secs(&self) -> Option<i64> {
        i64::try_from(self.as_nanos()? / 1_000_000_000).ok()
    }

    /// Length in nanoseconds, if bounded and free of calendar units.
    pub fn as_nanos(&self) -> Option<i128> {
        let span = self.span()?;
        if span.get_years() != 0 || span.get_months() != 0 {
            return None;
        }
        let whole_secs = i128::from(span.get_weeks()) * 7 * 86_400
            + i128::from(span.get_days()) * 86_400
            + i128::from(span.get_hours()) * 3_600
            + i128::from(span.get_minutes()) * 60
            + i128::from(span.get_seconds());
        Some(
            whole_secs * 1_000_000_000
                + i128::from(span.get_milliseconds()) * 1_000_000
                + i128::from(span.get_microseconds()) * 1_000
                + i128::from(span.get_nanoseconds()),
        )
    }
}

impl PartialEq for Duration {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Duration::Forever, Duration::Forever) => true,
            // `jiff::Span` has no `PartialEq` on purpose: two spans can denote the same length with
            // different units. Comparing fieldwise is what a wire-format round trip needs.
            (Duration::Finite(a), Duration::Finite(b)) => a.fieldwise() == b.fieldwise(),
            _ => false,
        }
    }
}

impl Eq for Duration {}

impl Default for Duration {
    fn default() -> Self {
        Duration::zero()
    }
}

impl FromStr for Duration {
    type Err = TimeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case(FOREVER) {
            return Ok(Duration::Forever);
        }
        if !is_schema_duration(trimmed) {
            return Err(TimeError::Duration(trimmed.to_string()));
        }
        let span = trimmed
            .parse::<jiff::Span>()
            .map_err(|_| TimeError::Duration(trimmed.to_string()))?;
        // The sentinel by *value*, not only by spelling. See `FOREVER_YEARS`: a peer that
        // normalises `P9999Y` sends the same duration written out in full, and reading that as an
        // ordinary span makes a perpetual event disappear rather than never end.
        if i64::from(span.get_years()) >= FOREVER_YEARS {
            return Ok(Duration::Forever);
        }
        Ok(Duration::Finite(span))
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Duration::Forever => f.write_str(FOREVER),
            Duration::Finite(span) => {
                if span.is_zero() {
                    // The schema's default is exactly this string.
                    f.write_str("PT0S")
                } else {
                    // `{}` is jiff's ISO 8601 rendering; `{:#}` would emit its "friendly" format
                    // ("3600s"), which is not a legal `duration` on the wire.
                    write!(f, "{span}")
                }
            }
        }
    }
}

impl Serialize for Duration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(|_| {
            serde::de::Error::invalid_value(Unexpected::Str(&s), &"an ISO 8601 duration")
        })
    }
}

/// The `duration` pattern, verbatim from `components/schemas/duration` in `openadr3.yaml`.
///
/// `is_schema_duration` — the private byte-level state machine [`Duration`]'s parser uses — implements
/// exactly this, and the two are held together from both ends: `cargo xtask check-model` fails if
/// the Alliance changes the pattern and this constant does not follow, and a property test asserts
/// that the state machine and this regex agree on every input it can generate.
///
/// It is deliberately stricter than ISO 8601 proper — no fractional hours or minutes, and days and
/// weeks mutually exclusive — which is why the parser checks it directly rather than accepting
/// whatever `jiff` allows.
pub const SCHEMA_DURATION_PATTERN: &str = r"^(-?)P(?=\d|T\d)(?:(\d+)Y)?(?:(\d+)M)?(?:(\d+)([DW]))?(?:T(?:(\d+)H)?(?:(\d+)M)?(?:(\d+(?:\.\d+)?)S)?)?$";

/// Does this string match the specification's `duration` pattern?
///
/// The pattern is [`SCHEMA_DURATION_PATTERN`]. This is that regex as a state machine, because the
/// crate is `no_std` and a regex engine is not.
pub(crate) fn is_schema_duration(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    if i < bytes.len() && bytes[i] == b'-' {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'P' {
        return false;
    }
    i += 1;
    // Lookahead: `(?=\d|T\d)` — a bare "P" or "PT" is not a duration.
    match bytes.get(i) {
        Some(c) if c.is_ascii_digit() => {}
        Some(b'T') if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {}
        _ => return false,
    }

    let mut date_units = ['Y', 'M', 'D'].iter().peekable();
    // Date part: Y, then M, then exactly one of D or W.
    let mut seen_day_or_week = false;
    while i < bytes.len() && bytes[i] != b'T' {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start || i >= bytes.len() {
            return false;
        }
        match bytes[i] {
            b'Y' | b'M' => {
                // Y must precede M; both must precede D/W.
                if seen_day_or_week {
                    return false;
                }
                let want = if bytes[i] == b'Y' { 'Y' } else { 'M' };
                // Advance the expected-order cursor; out-of-order or repeated units fail.
                loop {
                    match date_units.next() {
                        Some(u) if *u == want => break,
                        Some(_) => continue,
                        None => return false,
                    }
                }
            }
            b'D' | b'W' => {
                if seen_day_or_week {
                    return false;
                }
                seen_day_or_week = true;
            }
            _ => return false,
        }
        i += 1;
    }

    if i == bytes.len() {
        return true;
    }
    // Time part: T, then H, M, S in order; only S may be fractional.
    //
    // A trailing `T` with nothing after it is *accepted*, because the pattern accepts it: every
    // component of the time group is optional, and the lookahead — which already rejected a bare
    // `P` and a bare `PT` — is the only thing that constrains emptiness. Rejecting it here was the
    // function being stricter than the rule it exists to state, which a property test against the
    // pattern found within two thousand cases (D-101). `jiff` refuses `P1YT` on the next line
    // anyway, so nothing downstream changes; what changes is that this function answers the
    // question it is named for.
    debug_assert_eq!(bytes[i], b'T');
    i += 1;
    let mut time_units = ['H', 'M', 'S'].iter().peekable();
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
        let mut fractional = false;
        if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            let frac_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i == frac_start {
                return false;
            }
            fractional = true;
        }
        if i >= bytes.len() {
            return false;
        }
        let unit = bytes[i] as char;
        if fractional && unit != 'S' {
            return false;
        }
        loop {
            match time_units.next() {
                Some(u) if *u == unit => break,
                Some(_) => continue,
                None => return false,
            }
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_normalised_forever_is_still_forever() {
        // `P9999Y` is a magic string, and a peer that normalises durations sends the same value
        // written out in full. Read as an ordinary span it is 9999 years, which leaves the
        // representable range when added to any instant after the epoch — so the event does not
        // last for ever, it fails to expand and vanishes from the timeline.
        //
        // Found by running this crate's own conformance suite against another implementation,
        // which returns exactly this string.
        for spelling in [
            "P9999Y",
            "p9999y",
            "P9999Y0M0DT0H0M0S",
            "P9999Y0M",
            "P10000Y",
        ] {
            assert!(
                spelling.parse::<Duration>().unwrap().is_forever(),
                "{spelling} was not recognised as the forever sentinel"
            );
        }

        // And a duration that is merely long is still a duration.
        assert!(!"P9998Y".parse::<Duration>().unwrap().is_forever());
        assert!(!"P100Y".parse::<Duration>().unwrap().is_forever());
    }

    #[test]
    fn forever_is_always_written_as_the_sentinel() {
        // Liberal in what we accept, conservative in what we send: whichever spelling arrived, the
        // value that leaves is the one the specification names, because a peer that only knows the
        // literal has to recognise it.
        assert_eq!(
            "P9999Y0M0DT0H0M0S".parse::<Duration>().unwrap().to_string(),
            "P9999Y"
        );
    }

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn now_sentinels_are_recognised_in_every_documented_form() {
        for form in ["0001-01-01", "0001-01-01T00:00:00", "0001-01-01T00:00:00Z"] {
            assert_eq!(form.parse::<StartTime>().unwrap(), StartTime::Now, "{form}");
        }
    }

    #[test]
    fn ordinary_timestamps_are_not_sentinels() {
        let t = "2026-02-11T12:00:00Z".parse::<StartTime>().unwrap();
        assert_eq!(t, StartTime::At(ts("2026-02-11T12:00:00Z")));
        assert!(!t.is_now());
    }

    #[test]
    fn now_round_trips_through_json_as_the_canonical_form() {
        let json = serde_json::to_string(&StartTime::Now).unwrap();
        assert_eq!(json, "\"0001-01-01T00:00:00Z\"");
        assert_eq!(
            serde_json::from_str::<StartTime>(&json).unwrap(),
            StartTime::Now
        );
    }

    #[test]
    fn forever_is_lifted_out_of_the_span_type() {
        let d: Duration = "P9999Y".parse().unwrap();
        assert!(d.is_forever());
        assert_eq!(d.to_string(), "P9999Y");
        assert_eq!(d.checked_add_to(ts("2026-01-01T00:00:00Z")).unwrap(), None);
    }

    #[test]
    fn durations_are_calendar_aware() {
        // One month from 31 January is 28 February, not 3 March.
        let d: Duration = "P1M".parse().unwrap();
        let end = d
            .checked_add_to(ts("2026-01-31T00:00:00Z"))
            .unwrap()
            .unwrap();
        assert_eq!(end.to_string(), "2026-02-28T00:00:00Z");
    }

    /// The hand-written state machine against the regex it claims to implement.
    ///
    /// `is_schema_duration` is the most intricate hand-written code in the crate — a byte-level
    /// state machine over an ordering rule with two mutually exclusive units and one optional
    /// fraction — and it exists only because the model is `no_std` and a regex engine is not. The
    /// examples below cover the cases somebody thought of. This covers the ones nobody did.
    ///
    /// The regex is [`SCHEMA_DURATION_PATTERN`], which `cargo xtask check-model` holds equal to the
    /// document's own `duration.pattern`, so a change upstream reaches this test rather than
    /// slipping past it. Rust's `regex` has no lookahead, so `(?=\d|T\d)` — "a bare `P` or `PT` is
    /// not a duration" — is applied separately and stated once here.
    fn reference(pattern: &regex::Regex, s: &str) -> bool {
        let lookahead = match s.strip_prefix('-').unwrap_or(s).strip_prefix('P') {
            Some(rest) => {
                let mut chars = rest.chars();
                match chars.next() {
                    Some(c) if c.is_ascii_digit() => true,
                    Some('T') => chars.next().is_some_and(|c| c.is_ascii_digit()),
                    _ => false,
                }
            }
            None => false,
        };
        lookahead && pattern.is_match(s)
    }

    fn schema_regex() -> regex::Regex {
        // The lookahead is applied by `reference`; everything else is the pattern verbatim.
        regex::Regex::new(&SCHEMA_DURATION_PATTERN.replace(r"(?=\d|T\d)", "")).expect("valid")
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(4096))]

        /// Duration-shaped strings: the interesting half, because a generator of arbitrary bytes
        /// almost never produces one.
        #[test]
        fn the_state_machine_agrees_with_the_pattern_on_duration_shaped_input(
            s in r"-?P?[0-9]{0,3}[YMWDTHS.]?[0-9]{0,3}[YMWDTHS.]?[0-9]{0,4}[YMWDTHS.]?[0-9]{0,2}[YMWDTHS]?"
        ) {
            let pattern = schema_regex();
            proptest::prop_assert_eq!(
                is_schema_duration(&s),
                reference(&pattern, &s),
                "the state machine and the schema's own pattern disagree about {:?}",
                s
            );
        }

        /// And arbitrary text, because "what does it do with nonsense" is the other half.
        #[test]
        fn the_state_machine_agrees_with_the_pattern_on_anything(s in ".{0,24}") {
            let pattern = schema_regex();
            proptest::prop_assert_eq!(
                is_schema_duration(&s),
                reference(&pattern, &s),
                "the state machine and the schema's own pattern disagree about {:?}",
                s
            );
        }
    }

    #[test]
    fn a_trailing_t_matches_the_pattern_and_still_does_not_parse() {
        // The pattern accepts `P1YT` — every component of the time group is optional — so
        // `is_schema_duration` does too. `jiff` is what refuses it, which is the right division:
        // the pre-filter states the schema's rule and the parser states ISO 8601's.
        assert!(is_schema_duration("P1YT"));
        assert!(is_schema_duration("P0WT"));
        assert!("P1YT".parse::<Duration>().is_err());
        // And the lookahead still refuses the two the pattern refuses.
        assert!(!is_schema_duration("P"));
        assert!(!is_schema_duration("PT"));
    }

    #[test]
    fn schema_pattern_rejects_what_iso_8601_would_allow() {
        assert!("PT1H".parse::<Duration>().is_ok());
        assert!("PT15M".parse::<Duration>().is_ok());
        assert!("P1DT2H3M4.5S".parse::<Duration>().is_ok());
        assert!("P2W".parse::<Duration>().is_ok());
        // The pattern begins `^(-?)P`, so this is well formed and parses. What it is not is
        // *meaningful*, which is a question for whoever is being asked to act on it: the VTN
        // refuses an event carrying one (D-126).
        let backwards: Duration = "-PT1H".parse().unwrap();
        assert!(backwards.is_negative());
        assert!(!"PT1H".parse::<Duration>().unwrap().is_negative());
        assert!(!"PT0S".parse::<Duration>().unwrap().is_negative());
        assert!(!"P9999Y".parse::<Duration>().unwrap().is_negative());

        // Fractional hours/minutes are outside the schema pattern.
        assert!("PT1.5H".parse::<Duration>().is_err());
        // Bare designators.
        assert!("P".parse::<Duration>().is_err());
        assert!("PT".parse::<Duration>().is_err());
        // Out-of-order units.
        assert!("PT1M1H".parse::<Duration>().is_err());
        // Days and weeks together.
        assert!("P1W1D".parse::<Duration>().is_err());
        // Not a duration at all.
        assert!("1h".parse::<Duration>().is_err());
    }

    #[test]
    fn every_duration_renders_as_iso_8601_not_a_friendly_string() {
        // Regression: jiff's alternate format produces "3600s", which no peer would accept.
        for text in [
            "PT1H",
            "PT15M",
            "P1D",
            "P1DT2H3M4S",
            "P2W",
            "PT0S",
            "P9999Y",
        ] {
            let d: Duration = text.parse().unwrap();
            let rendered = d.to_string();
            assert!(
                rendered.starts_with('P') || rendered.starts_with("-P"),
                "{text} rendered as {rendered}, which is not an ISO 8601 duration"
            );
            assert_eq!(
                rendered.parse::<Duration>().unwrap(),
                d,
                "round trip of {text}"
            );
        }
    }

    #[test]
    fn sub_second_components_count_towards_the_length() {
        let d: Duration = "PT1.5S".parse().unwrap();
        assert_eq!(d.as_nanos(), Some(1_500_000_000));
        assert_eq!(d.as_secs(), Some(1));
    }

    #[test]
    fn zero_duration_renders_as_the_schema_default() {
        assert_eq!(Duration::zero().to_string(), "PT0S");
        assert!(Duration::zero().is_zero());
    }
}
