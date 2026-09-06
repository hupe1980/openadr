//! The domain core: deterministic, I/O-free, and shared by the VTN, the VEN and the tests.
//!
//! Four things live here, and they are the parts of OpenADR that carry real semantics rather than
//! transport: [`IntervalExpander`] resolves an event's declared timing into absolute windows,
//! [`Timeline`] merges concurrent events by priority, [`Access`] decides object privacy, and
//! [`ReportSchedule`] says when a VEN owes a report.
//!
//! Everything takes its time from an injected [`Clock`], so a scenario that fails in production can
//! be replayed exactly in a test and nothing here needs to sleep. That, and having no I/O, is what
//! makes the layer `no_std`-capable.
//!
//! Guide: <https://hupe1980.github.io/openadr/docs/domain-core/>.

use crate::model::Timestamp;

pub mod interval;
pub mod privacy;
pub mod report;
pub mod timeline;

pub use interval::{
    ExpandError, ExpandedInterval, IntervalExpander, IntervalSequence, active_window,
};
pub use privacy::{Access, Grant, GrantIndex, OwnerFilter, Role, TargetFilter};
pub use report::{AggregateError, ReportDue, ReportSchedule, ScheduleOptions, aggregate};
pub use timeline::{Segment, Timeline};

/// A source of the current time.
///
/// Everything time-dependent in this crate goes through this trait. Tests use [`FixedClock`]; the
/// VTN uses [`SystemClock`].
pub trait Clock: core::fmt::Debug + Send + Sync {
    /// The current instant.
    fn now(&self) -> Timestamp;
}

/// Reads the host clock.
#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

#[cfg(feature = "std")]
impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// A clock frozen at one instant.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub Timestamp);

impl FixedClock {
    /// Freeze at an instant.
    pub fn new(at: Timestamp) -> Self {
        Self(at)
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_clock_does_not_move() {
        let at: Timestamp = "2026-01-01T12:00:00Z".parse().unwrap();
        let clock = FixedClock::new(at);
        assert_eq!(clock.now(), at);
        assert_eq!(clock.now(), at);
    }
}
