//! Draining the outbox.
//!
//! The write path enqueues; this takes entries off the queue and hands them to a
//! [`Notifier`]. Keeping the two apart is what moves the network off
//! the request path, and it is what lets a notification survive a crash.
//!
//! A dispatcher claims entries under a lease, so running more than one is safe: two dispatchers
//! never take the same entry, and one that dies mid-delivery only costs the lease's worth of delay
//! before another picks the entry up.

use futures_util::StreamExt as _;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use crate::core::Clock;
use crate::vtn::notify::Notifier;
use crate::vtn::store::{BreakerPolicy, OutboxStats, RetryPolicy, SharedStorage};

/// How the dispatcher runs.
#[derive(Debug, Clone)]
pub struct DispatchConfig {
    /// How long to wait when the queue is empty before looking again.
    ///
    /// Only the *idle* interval: after a non-empty batch the dispatcher loops immediately, so a
    /// backlog drains at full speed rather than one batch per tick. On a backend that can announce
    /// a write — Postgres, through `LISTEN`/`NOTIFY` — this is an upper bound rather than the
    /// actual wait, so it can be generous without costing latency.
    pub idle_interval: StdDuration,
    /// Entries to claim per pass.
    pub batch: usize,
    /// How long a claim is held before another dispatcher may take the entry.
    ///
    /// Longer than the transport's own timeout, or a slow delivery would be redelivered while the
    /// first attempt is still in flight.
    pub lease: StdDuration,
    /// How many deliveries in a batch may be in flight at once.
    ///
    /// Deliveries are independent: they go to different subscribers over different sockets, and one
    /// slow receiver has nothing to do with the next. Attempting them one after another makes the
    /// time to clear a batch the *sum* of its timeouts, which is how a batch outlives its own lease
    /// and gets redelivered by a second dispatcher while the first is still working through it.
    pub concurrency: usize,
    /// When to retry and when to give up on one notification.
    pub retry: RetryPolicy,
    /// When to give up on a *subscriber*.
    ///
    /// [`RetryPolicy`] bounds what one notification costs. Without this, a permanently dead
    /// endpoint costs `retry.max_attempts` HTTP round trips on every write in the VTN, for ever:
    /// abandonment is per-notification and cannot see the pattern.
    pub breaker: BreakerPolicy,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            idle_interval: StdDuration::from_millis(250),
            batch: 32,
            lease: StdDuration::from_secs(60),
            concurrency: 8,
            retry: RetryPolicy::default(),
            breaker: BreakerPolicy::default(),
        }
    }
}

/// Drains queued deliveries.
#[derive(Clone)]
pub struct Dispatcher {
    storage: SharedStorage,
    notifier: Arc<dyn Notifier>,
    clock: Arc<dyn Clock>,
    config: DispatchConfig,
    owner: String,
    metrics: crate::vtn::Metrics,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("owner", &self.owner)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Dispatcher {
    /// Build a dispatcher.
    pub fn new(
        storage: SharedStorage,
        notifier: Arc<dyn Notifier>,
        clock: Arc<dyn Clock>,
        config: DispatchConfig,
    ) -> Self {
        // The owner identifies which dispatcher holds a lease, which is what makes a stuck lease
        // traceable to a process rather than merely visible.
        let owner = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        );
        Self {
            storage,
            notifier,
            clock,
            config,
            owner,
            metrics: crate::vtn::Metrics::new(),
        }
    }

    /// Report delivery outcomes into a registry.
    ///
    /// [`Vtn::build`](crate::vtn::VtnBuilder::build) wires this to the one `GET /metrics` serves.
    /// A dispatcher built by hand counts into its own, which is nobody's but its.
    pub fn with_metrics(mut self, metrics: crate::vtn::Metrics) -> Self {
        self.metrics = metrics;
        self
    }

    /// Deliver one batch, returning how many entries were attempted.
    ///
    /// Public because tests want to drain deterministically rather than race a background task.
    pub async fn dispatch_once(&self) -> usize {
        let now = self.clock.now();
        let claimed = match self
            .storage
            .claim_due(now, self.config.batch, self.config.lease, &self.owner)
            .await
        {
            Ok(claimed) => claimed,
            Err(e) => {
                tracing::error!(error = %e, "could not claim outbox entries");
                return 0;
            }
        };

        let attempted = claimed.len();
        let concurrency = self.config.concurrency.max(1);
        futures_util::stream::iter(claimed)
            .for_each_concurrent(concurrency, |entry| self.attempt(entry))
            .await;
        attempted
    }

    /// One entry: deliver it, then record what happened.
    async fn attempt(&self, entry: crate::vtn::store::Queued) {
        // The attempt the transport announces is the durable count plus this try, so a receiver
        // reading `X-OpenADR-Attempt` sees 1 on the first delivery and 2 on the first retry.
        let attempt = entry.attempts + 1;
        let channel = entry.delivery.channel();
        let outcome = self.notifier.deliver(&entry.delivery, attempt).await;
        let now = self.clock.now();
        match outcome {
            Ok(()) => {
                self.metrics.record_delivery(&channel, "delivered");
                if let Err(e) = self.storage.complete(entry.id).await {
                    // The delivery happened; only the record of it failed. The entry will be
                    // redelivered when its lease expires, which is why delivery is at-least-once.
                    tracing::error!(entry = %entry.id, error = %e, "delivered but not recorded");
                }
                // A subscriber that answers is a subscriber that works: this is what closes a
                // half-open breaker, and it is why a merely flaky endpoint is never cut off.
                self.note(&entry, false, now, None).await;
            }
            Err(failure) => {
                match self
                    .storage
                    .record_failure(entry.id, &failure, now, &self.config.retry)
                    .await
                {
                    Ok(true) => {
                        self.metrics.record_delivery(&channel, "abandoned");
                        tracing::warn!(
                        entry = %entry.id,
                        subscription = ?entry.delivery.subscription_id,
                        error = %failure,
                        retriable = failure.retriable,
                            "giving up on a notification"
                        );
                        // Only an *abandoned* notification counts against the subscriber. A retry
                        // is still in flight, and one that eventually succeeds proves nothing was
                        // wrong with the endpoint.
                        self.note(&entry, true, now, Some(failure.message.as_str()))
                            .await;
                    }
                    Ok(false) => {
                        self.metrics.record_delivery(&channel, "retrying");
                        tracing::debug!(
                        entry = %entry.id,
                        attempts = attempt,
                        error = %failure,
                            "delivery failed; will retry"
                        )
                    }
                    Err(e) => {
                        tracing::error!(entry = %entry.id, error = %e, "could not record failure")
                    }
                }
            }
        }
    }

    /// Move the subscriber's circuit breaker, if this delivery belongs to a subscription.
    ///
    /// Broker deliveries have no subscription — a topic is not a subscriber the VTN can cut off,
    /// and a broker that is down is one endpoint rather than a thousand — so they are exempt.
    async fn note(
        &self,
        entry: &crate::vtn::store::Queued,
        abandoned: bool,
        now: crate::model::Timestamp,
        error: Option<&str>,
    ) {
        let Some(subscription) = entry.delivery.subscription_id.as_ref() else {
            return;
        };
        match self
            .storage
            .note_delivery(subscription, abandoned, now, &self.config.breaker, error)
            .await
        {
            Ok(true) => tracing::warn!(
                %subscription,
                threshold = self.config.breaker.threshold,
                cooldown = ?self.config.breaker.cooldown,
                error = error.unwrap_or("-"),
                "cutting a subscriber off: it has abandoned this many notifications in a row. \
                 New notifications for it are suppressed until one probe is let through; \
                 GET /admin/subscribers lists it, and POST /admin/outbox/retry closes it early."
            ),
            Ok(false) => {}
            Err(e) => {
                tracing::error!(%subscription, error = %e, "could not record delivery health")
            }
        }
    }

    /// Drain until nothing is due, returning how many entries were attempted.
    ///
    /// For tests and for a shutdown that wants to flush. In production the loop in [`Dispatcher::run`]
    /// does this continuously.
    pub async fn drain(&self) -> usize {
        let mut total = 0;
        loop {
            let attempted = self.dispatch_once().await;
            total += attempted;
            if attempted == 0 {
                return total;
            }
        }
    }

    /// Run until the process ends.
    pub async fn run(self) {
        tracing::info!(
            notifier = self.notifier.name(),
            owner = %self.owner,
            "outbox dispatcher started"
        );
        loop {
            // A non-empty batch means there may be more waiting, so loop straight round; only an
            // empty one waits. A backlog therefore drains at full speed.
            if self.dispatch_once().await == 0 {
                // Not a sleep: a backend that can tell us a write happened does, and the interval
                // becomes an upper bound on the wait rather than the wait itself.
                self.storage.await_outbox(self.config.idle_interval).await;
            }
        }
    }

    /// Spawn [`Dispatcher::run`] on the current runtime.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run())
    }

    /// What the queue looks like right now.
    pub async fn stats(&self) -> OutboxStats {
        self.storage
            .outbox_stats(self.clock.now())
            .await
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::FixedClock;
    use crate::model::{
        Operation, Program, ProgramRequest,
        notification::{AnyObject, Notification},
    };
    use crate::vtn::notify::{Delivery, DeliveryFailure};
    use crate::vtn::store::MemoryStorage;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn now() -> crate::model::Timestamp {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }

    fn delivery() -> Delivery {
        Delivery {
            subscription_id: None,
            route: crate::vtn::notify::Route::Webhook {
                callback_url: "https://example.com/hook".into(),
                bearer_token: None,
            },
            notification: Notification::new(
                Operation::Create,
                AnyObject::Program(Program {
                    id: "prg-1".parse().unwrap(),
                    created_date_time: now(),
                    modification_date_time: now(),
                    object_type: crate::model::ObjectType::Program,
                    content: ProgramRequest::new("p".parse().unwrap()),
                }),
            ),
        }
    }

    /// Fails the first `fail_first` attempts, then succeeds. Records the attempt numbers it saw.
    struct Flaky {
        attempts: AtomicUsize,
        fail_first: usize,
        retriable: bool,
        seen: std::sync::Mutex<Vec<u32>>,
    }

    impl Flaky {
        fn new(fail_first: usize, retriable: bool) -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                fail_first,
                retriable,
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Notifier for Flaky {
        fn handles(&self, _channel: crate::vtn::notify::Channel) -> bool {
            true
        }

        async fn deliver(&self, _delivery: &Delivery, attempt: u32) -> Result<(), DeliveryFailure> {
            self.seen.lock().unwrap().push(attempt);
            let n = self.attempts.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_first {
                Err(if self.retriable {
                    DeliveryFailure::retriable("nope")
                } else {
                    DeliveryFailure::permanent("nope")
                })
            } else {
                Ok(())
            }
        }
        fn name(&self) -> &'static str {
            "flaky"
        }
    }

    /// A dispatcher over a fixed clock, with retries due immediately so `drain` is deterministic.
    fn dispatcher(notifier: Arc<dyn Notifier>, max_attempts: u32) -> (Dispatcher, SharedStorage) {
        let storage = MemoryStorage::shared();
        let d = Dispatcher::new(
            storage.clone(),
            notifier,
            Arc::new(FixedClock::new(now())),
            DispatchConfig {
                retry: RetryPolicy {
                    max_attempts,
                    base_delay: StdDuration::ZERO,
                    max_delay: StdDuration::ZERO,
                },
                ..Default::default()
            },
        );
        (d, storage)
    }

    #[tokio::test]
    async fn a_queued_delivery_is_handed_to_the_notifier_and_then_gone() {
        let notifier = Arc::new(Flaky::new(0, true));
        let (d, storage) = dispatcher(notifier.clone(), 8);
        storage.enqueue(vec![delivery()], now()).await.unwrap();

        assert_eq!(d.drain().await, 1);
        assert_eq!(notifier.attempts.load(Ordering::SeqCst), 1);
        assert!(
            d.stats().await.is_idle(),
            "a delivered entry must not linger"
        );
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_until_it_succeeds() {
        let notifier = Arc::new(Flaky::new(2, true));
        let (d, storage) = dispatcher(notifier.clone(), 8);
        storage.enqueue(vec![delivery()], now()).await.unwrap();

        assert_eq!(d.drain().await, 3, "two failures then a success");
        assert!(d.stats().await.is_idle());
        // The transport is told which try this is, so `X-OpenADR-Attempt` means something.
        assert_eq!(*notifier.seen.lock().unwrap(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn attempts_are_bounded_and_the_remains_are_visible() {
        let notifier = Arc::new(Flaky::new(usize::MAX, true));
        let (d, storage) = dispatcher(notifier.clone(), 3);
        storage.enqueue(vec![delivery()], now()).await.unwrap();

        assert_eq!(d.drain().await, 3);
        // Abandoned rather than retried for ever, and counted rather than silently dropped.
        let stats = d.stats().await;
        assert_eq!((stats.pending, stats.dead), (0, 1));
        assert_eq!(d.drain().await, 0);
    }

    #[tokio::test]
    async fn a_permanent_failure_costs_exactly_one_attempt() {
        let notifier = Arc::new(Flaky::new(usize::MAX, false));
        let (d, storage) = dispatcher(notifier.clone(), 8);
        storage.enqueue(vec![delivery()], now()).await.unwrap();

        assert_eq!(
            d.drain().await,
            1,
            "a permanent failure must not be retried"
        );
        assert_eq!(d.stats().await.dead, 1);
    }

    #[tokio::test]
    async fn a_batch_is_a_ceiling_on_one_pass_not_on_the_drain() {
        let notifier = Arc::new(Flaky::new(0, true));
        let storage = MemoryStorage::shared();
        let d = Dispatcher::new(
            storage.clone(),
            notifier.clone(),
            Arc::new(FixedClock::new(now())),
            DispatchConfig {
                batch: 2,
                ..Default::default()
            },
        );
        storage
            .enqueue(
                vec![delivery(), delivery(), delivery(), delivery(), delivery()],
                now(),
            )
            .await
            .unwrap();

        assert_eq!(d.dispatch_once().await, 2, "one pass takes one batch");
        assert_eq!(d.drain().await, 3, "the rest still drains");
        assert!(d.stats().await.is_idle());
    }
}
