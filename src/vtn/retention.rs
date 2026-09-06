//! Ageing reports out.
//!
//! `report` is the only object OpenADR grows without bound. Every other one is created by business
//! logic and deleted by it; a report arrives from a *fleet*, on a schedule the VTN itself asked for,
//! and nothing in the protocol ever removes one — a thousand resources on a quarter-hourly
//! descriptor is about thirty-five million rows a year.
//!
//! Three properties, each of which the storage conformance suite states as a behaviour every
//! backend has to have:
//!
//! * **Off by default.** Reports are settlement data, so deleting them is a decision an operator
//!   makes rather than one this crate makes for them.
//! * **Bounded and oldest-first.** A first sweep over a year of data is many small transactions
//!   rather than one long lock; a bound that took an arbitrary `limit` of the expired rows would
//!   make progress that never reaches the far end.
//! * **It announces nothing.** A `DELETE /reports/{id}` is business logic removing an object and
//!   subscribers may hear about it. Expiry is housekeeping, and telling ten thousand subscribers
//!   that ten thousand old reports have aged out is a fan-out nobody asked for.
//!
//! Not here: archival (getting data out before it expires is `GET /reports`), and per-programme
//! periods (one number an operator can state and check, rather than a column and a resolution rule
//! for a case nobody has yet).

use std::sync::Arc;
use std::time::Duration as StdDuration;

use crate::core::Clock;
use crate::vtn::store::SharedStorage;

/// How reports are aged out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionConfig {
    /// How long a report is kept after it is filed. `None` keeps it for ever.
    pub reports: Option<StdDuration>,
    /// How long to wait between sweeps once there is nothing left to delete.
    pub interval: StdDuration,
    /// Rows deleted per statement.
    pub batch: usize,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            // Settlement data. Keeping it is the safe default and deleting it is a decision.
            reports: None,
            interval: StdDuration::from_secs(3600),
            batch: 1000,
        }
    }
}

impl RetentionConfig {
    /// Keep reports for this long.
    pub fn keeping_reports_for(duration: StdDuration) -> Self {
        Self {
            reports: Some(duration),
            ..Self::default()
        }
    }

    /// Whether anything is configured to expire.
    pub fn is_enabled(&self) -> bool {
        self.reports.is_some()
    }
}

/// Deletes what has expired.
#[derive(Clone)]
pub struct Retention {
    storage: SharedStorage,
    clock: Arc<dyn Clock>,
    config: RetentionConfig,
    metrics: crate::vtn::Metrics,
}

impl std::fmt::Debug for Retention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Retention")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Retention {
    /// Build a sweeper.
    pub fn new(storage: SharedStorage, clock: Arc<dyn Clock>, config: RetentionConfig) -> Self {
        Self {
            storage,
            clock,
            config,
            metrics: crate::vtn::Metrics::new(),
        }
    }

    /// Count deletions into a registry.
    pub fn with_metrics(mut self, metrics: crate::vtn::Metrics) -> Self {
        self.metrics = metrics;
        self
    }

    /// The instant before which a report has expired, or `None` when retention is off.
    ///
    /// `None` also when the subtraction leaves the representable range, which needs a clock at the
    /// beginning of time — and there the safe answer is "nothing has expired yet" rather than a
    /// cutoff that saturates to the epoch and deletes everything.
    fn cutoff(&self) -> Option<crate::model::Timestamp> {
        let keep = self.config.reports?;
        let seconds = i64::try_from(keep.as_secs()).ok()?;
        self.clock
            .now()
            .checked_sub(jiff::Span::new().try_seconds(seconds).ok()?)
            .ok()
    }

    /// Delete one batch. Returns how many rows went.
    ///
    /// Public so a test can sweep deterministically rather than race a background task.
    pub async fn sweep_once(&self) -> u64 {
        let Some(before) = self.cutoff() else {
            return 0;
        };
        match self.storage.purge_reports(before, self.config.batch).await {
            Ok(0) => 0,
            Ok(n) => {
                self.metrics.record_purged_reports(n);
                tracing::info!(
                    reports = n,
                    %before,
                    "retention: deleted reports older than the retention period"
                );
                n
            }
            Err(e) => {
                tracing::error!(error = %e, "retention sweep failed");
                0
            }
        }
    }

    /// Sweep until nothing more has expired. Returns the total deleted.
    pub async fn sweep(&self) -> u64 {
        let mut total = 0;
        loop {
            let deleted = self.sweep_once().await;
            total += deleted;
            // A short batch means the table is caught up; a full one means there is more.
            if deleted < self.config.batch as u64 {
                return total;
            }
        }
    }

    /// Run until the process ends.
    pub async fn run(self) {
        let Some(keep) = self.config.reports else {
            return;
        };
        tracing::info!(
            keep_seconds = keep.as_secs(),
            interval_seconds = self.config.interval.as_secs(),
            batch = self.config.batch,
            "report retention enabled: reports older than this are deleted, permanently and \
             without notifying anyone"
        );
        loop {
            self.sweep().await;
            tokio::time::sleep(self.config.interval).await;
        }
    }

    /// Spawn [`Retention::run`] on the current runtime.
    ///
    /// `None` when retention is off, so a caller cannot mistake a handle to a task that returns
    /// immediately for a sweeper that is working.
    pub fn spawn(self) -> Option<tokio::task::JoinHandle<()>> {
        self.config
            .is_enabled()
            .then(|| tokio::spawn(self.clone().run()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{FixedClock, SystemClock};
    use crate::model::{ClientName, ProgramRequest, ReportRequest, Timestamp};
    use crate::vtn::notify::Fanout;
    use crate::vtn::store::MemoryStorage;

    fn now() -> Timestamp {
        "2026-06-01T00:00:00Z".parse().unwrap()
    }

    /// A store holding `count` reports, the *n*th filed `n` hours before `now()`.
    async fn store_with_reports(count: i64) -> SharedStorage {
        let storage = MemoryStorage::shared();
        let program = storage
            .create_program(
                ProgramRequest::new("retention".parse().unwrap()),
                now(),
                &Fanout::none(),
            )
            .await
            .unwrap();
        let event = storage
            .create_event(
                crate::model::EventRequest::new(program.id.clone()),
                now(),
                &Fanout::none(),
            )
            .await
            .unwrap();
        for hour in 0..count {
            let at = now()
                .checked_sub(jiff::Span::new().hours(hour))
                .expect("inside the representable range");
            storage
                .create_report(
                    ReportRequest::new(
                        event.id.clone(),
                        ClientName::new("ven-1").unwrap(),
                        Vec::new(),
                    ),
                    None,
                    at,
                    &Fanout::none(),
                )
                .await
                .unwrap();
        }
        storage
    }

    fn sweeper(storage: SharedStorage, config: RetentionConfig) -> Retention {
        Retention::new(storage, Arc::new(FixedClock::new(now())), config)
    }

    #[tokio::test]
    async fn nothing_is_deleted_unless_retention_is_configured() {
        // The default is not "delete after a sensible period". Reports are settlement data, and a
        // VTN that quietly forgot them would be a billing dispute.
        let storage = store_with_reports(10).await;
        let sweeper = sweeper(storage.clone(), RetentionConfig::default());
        assert!(!RetentionConfig::default().is_enabled());
        assert_eq!(sweeper.sweep().await, 0);
        assert_eq!(storage.report_stats(now()).await.unwrap().count, 10);
        assert!(
            sweeper.spawn().is_none(),
            "an idle sweeper must not be spawned"
        );
    }

    #[tokio::test]
    async fn only_what_is_older_than_the_period_goes() {
        let storage = store_with_reports(10).await;
        let sweeper = sweeper(
            storage.clone(),
            RetentionConfig::keeping_reports_for(StdDuration::from_secs(5 * 3600)),
        );
        // Filed 0..9 hours ago. The boundary is exclusive — "older *than* five hours" — so the
        // report filed exactly five hours ago stays, and 6h..9h go.
        assert_eq!(sweeper.sweep().await, 4);
        assert_eq!(storage.report_stats(now()).await.unwrap().count, 6);
        // And a second sweep is a no-op rather than a second five.
        assert_eq!(sweeper.sweep().await, 0);
    }

    #[tokio::test]
    async fn a_backlog_is_cleared_in_batches() {
        // The property the bound exists for: a first sweep over a year of accumulated data is many
        // small transactions, and it still reaches the far end.
        let storage = store_with_reports(20).await;
        let sweeper = sweeper(
            storage.clone(),
            RetentionConfig {
                reports: Some(StdDuration::from_secs(3600)),
                batch: 3,
                ..Default::default()
            },
        );
        // Filed 0..19 hours ago; one hour's retention leaves the 0h and 1h reports.
        assert_eq!(sweeper.sweep_once().await, 3, "one batch, not the backlog");
        assert_eq!(sweeper.sweep().await, 15, "the rest, in batches");
        assert_eq!(storage.report_stats(now()).await.unwrap().count, 2);
    }

    #[tokio::test]
    async fn a_sweep_is_counted() {
        let storage = store_with_reports(4).await;
        let metrics = crate::vtn::Metrics::new();
        let sweeper = sweeper(
            storage,
            RetentionConfig::keeping_reports_for(StdDuration::from_secs(1)),
        )
        .with_metrics(metrics.clone());
        // Filed 0..3 hours ago; a one-second period leaves only the one filed at `now`.
        sweeper.sweep().await;
        assert!(
            metrics
                .render(&crate::vtn::metrics::Gauges::default())
                .contains("openadr_reports_purged_total 3")
        );
    }

    #[test]
    fn a_clock_at_the_beginning_of_time_deletes_nothing() {
        // `now - keep` can leave the representable range, and a cutoff that saturated to the epoch
        // would be a cutoff in the *future* of every stored report.
        let sweeper = Retention::new(
            MemoryStorage::shared(),
            Arc::new(FixedClock::new(Timestamp::MIN)),
            RetentionConfig::keeping_reports_for(StdDuration::from_secs(86_400)),
        );
        assert_eq!(sweeper.cutoff(), None);

        // And an ordinary clock does produce one.
        let ordinary = Retention::new(
            MemoryStorage::shared(),
            Arc::new(SystemClock),
            RetentionConfig::keeping_reports_for(StdDuration::from_secs(86_400)),
        );
        assert!(ordinary.cutoff().is_some());
    }
}
