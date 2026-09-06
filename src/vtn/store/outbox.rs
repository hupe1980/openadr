//! The transactional outbox.
//!
//! A write records the object **and** the deliveries it causes in one transaction, then returns; a
//! dispatcher drains the queue afterwards.
//!
//! The atomicity is the whole point, and it is why this is a table rather than a background task
//! with a channel: an OpenADR notification is a dispatch instruction, and **nothing in the protocol
//! can say afterwards that one was never sent**. A crash between writing the event and queueing its
//! notification would be silent at both ends.
//!
//! It costs one thing. The recipients must be known before the transaction opens, because working
//! them out needs reads a backend cannot make from inside its own write — hence
//! [`Fanout`](crate::vtn::notify::Fanout), a snapshot taken just before the write and passed down,
//! so the backend's part is a pure function call. That also fixes *when* recipients are decided: a
//! subscription created a moment after an event is not told about it, which is what the
//! specification describes.
//!
//! Delivery is consequently **at least once** — a dispatcher can deliver and then die before
//! recording that it did. Every notification carries the object's identity and
//! `modificationDateTime`, and `X-OpenADR-Attempt` names the try, so a receiver can be idempotent.
//!
//! <https://hupe1980.github.io/openadr/docs/notifications/> has the long form.

use crate::model::Timestamp;
use crate::vtn::notify::Delivery;

/// Identifier of a queued delivery.
///
/// Monotonic, so draining in id order drains in enqueue order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutboxId(pub i64);

impl core::fmt::Display for OutboxId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A delivery waiting to be attempted.
#[derive(Debug, Clone, PartialEq)]
pub struct Queued {
    /// Queue position.
    pub id: OutboxId,
    /// How many attempts have already been made.
    pub attempts: u32,
    /// The delivery itself.
    pub delivery: Delivery,
}

/// What the queue looks like right now.
///
/// The numbers an operator actually needs: is anything stuck, how far behind is it, and has anything
/// been given up on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
// `camelCase` like every other body this VTN serves, `DeadLetter` and `SubscriberHealth` included.
// `GET /health` embeds this next to `subscribersCutOff`, and one object answering in two naming
// conventions is a body nobody can write a parser for without looking twice.
#[serde(rename_all = "camelCase")]
pub struct OutboxStats {
    /// Entries still to be delivered, including ones waiting for a retry.
    pub pending: u64,
    /// Entries that exhausted their attempts and were abandoned.
    pub dead: u64,
    /// Age in seconds of the oldest pending entry.
    ///
    /// The single most useful number here: a queue that is long but young is busy, and a queue that
    /// is short but old is stuck.
    pub oldest_pending_seconds: Option<i64>,
}

impl OutboxStats {
    /// Whether anything is waiting or has been abandoned.
    pub fn is_idle(&self) -> bool {
        self.pending == 0 && self.dead == 0
    }
}

/// One abandoned entry, as an operator needs to see it.
///
/// `GET /health` says `dead: 4`. That is enough to alert on and not enough to act on: *which*
/// subscriber, *which* object, and *what* went wrong are all in the row already, and reading them
/// out of the database by hand is not an operational procedure.
///
/// Deliberately without the notification body and without `bearerToken`. The body is large and
/// already reconstructible from the object it names; the token is the subscriber's secret and has
/// no business in a diagnostic listing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeadLetter {
    /// Queue position, and the handle for retrying or discarding it.
    pub id: i64,
    /// When the change that caused it was written.
    pub enqueued_at: Timestamp,
    /// How many attempts were made before it was given up on.
    pub attempts: u32,
    /// What the last attempt reported.
    pub last_error: Option<crate::std_shim::String>,
    /// The subscription that asked for it, if it came from the REST mechanism.
    pub subscription_id: Option<crate::model::ObjectId>,
    /// What kind of object the notification is about.
    pub object_type: crate::model::ObjectType,
    /// Which object.
    pub object_id: crate::model::ObjectId,
    /// Where it was going: a callback URL, or a broker topic.
    pub destination: crate::std_shim::String,
}

/// How a dispatcher decides when to try again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Attempts before an entry is abandoned.
    pub max_attempts: u32,
    /// Delay before the first retry.
    pub base_delay: core::time::Duration,
    /// Ceiling on the delay, so a long-dead endpoint is still retried occasionally.
    pub max_delay: core::time::Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 8,
            base_delay: core::time::Duration::from_secs(2),
            max_delay: core::time::Duration::from_secs(300),
        }
    }
}

/// When a subscriber that has stopped answering is cut off, and for how long.
///
/// [`RetryPolicy`] bounds what *one* notification costs; this bounds what a *subscriber* costs.
/// Without it a permanently dead endpoint costs `max_attempts` HTTP round trips on every write in
/// the VTN, for ever — the queue drains, `GET /admin/outbox` fills up, and nothing stops the
/// spending. Abandonment is per-notification by construction and cannot see the pattern.
///
/// The breaker is deliberately **not** a way to lose notifications quietly. A cut-off subscription
/// is a row an operator can see, a gauge they can alert on, and a state the subscriber recovers
/// from by itself after `cooldown` — because OpenADR has no way to tell a subscriber it was cut
/// off, and one that polls learns the truth from the VTN regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerPolicy {
    /// Consecutive abandoned notifications before the subscription is cut off.
    ///
    /// Consecutive: one delivered notification resets the count, so a subscriber that is merely
    /// flaky is never cut off.
    pub threshold: u32,
    /// How long a cut-off subscription stays cut off before one probe is let through.
    ///
    /// Half-open, not closed: the probe is an ordinary notification, and it either succeeds — which
    /// closes the breaker — or is abandoned, which opens it for another `cooldown`.
    pub cooldown: core::time::Duration,
}

impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            // Three notifications, each already retried `RetryPolicy::max_attempts` times: an
            // endpoint that has refused twenty-four deliveries in a row is not coming back within
            // the next one.
            threshold: 3,
            cooldown: core::time::Duration::from_secs(15 * 60),
        }
    }
}

impl BreakerPolicy {
    /// A breaker that never opens, for a deployment that would rather keep spending.
    pub fn disabled() -> Self {
        Self {
            threshold: u32::MAX,
            cooldown: core::time::Duration::ZERO,
        }
    }

    /// Whether this many consecutive abandonments cuts a subscriber off.
    pub fn trips(&self, consecutive_failures: u32) -> bool {
        consecutive_failures >= self.threshold
    }
}

/// What the VTN knows about one subscription's delivery health.
///
/// Only the unhealthy are recorded: a subscriber that has never failed has no row, so the common
/// case costs nothing and the table's size is the size of the problem.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriberHealth {
    /// The subscription.
    pub subscription_id: crate::model::ObjectId,
    /// Notifications abandoned in a row, reset by any delivery that succeeds.
    pub consecutive_failures: u32,
    /// When the breaker last opened, if it is open.
    pub cut_off_since: Option<Timestamp>,
    /// When one probe will be let through. `None` when the breaker is closed.
    pub retry_at: Option<Timestamp>,
    /// What the last abandoned notification reported.
    pub last_error: Option<crate::std_shim::String>,
}

impl SubscriberHealth {
    /// Whether new notifications for this subscription are being suppressed at `now`.
    pub fn is_cut_off(&self, now: Timestamp) -> bool {
        self.retry_at.is_some_and(|at| now < at)
    }
}

impl RetryPolicy {
    /// The delay before attempt number `attempts + 1`.
    ///
    /// Exponential, capped. No jitter is added here: entries are drained in batches by a single
    /// dispatcher, so they are already spread out by the time they are attempted, and a
    /// deterministic schedule is far easier to reason about in a test.
    pub fn delay_after(&self, attempts: u32) -> core::time::Duration {
        let shift = attempts.min(16);
        self.base_delay
            .saturating_mul(1u32 << shift)
            .min(self.max_delay)
    }

    /// Whether another attempt is allowed.
    pub fn may_retry(&self, attempts: u32) -> bool {
        attempts < self.max_attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delay_grows_and_then_stops_growing() {
        let policy = RetryPolicy {
            max_attempts: 8,
            base_delay: core::time::Duration::from_secs(2),
            max_delay: core::time::Duration::from_secs(300),
        };
        assert_eq!(policy.delay_after(0), core::time::Duration::from_secs(2));
        assert_eq!(policy.delay_after(1), core::time::Duration::from_secs(4));
        assert_eq!(policy.delay_after(3), core::time::Duration::from_secs(16));
        // Capped, so a long-dead endpoint is still retried occasionally rather than never.
        assert_eq!(policy.delay_after(20), core::time::Duration::from_secs(300));
    }

    #[test]
    fn attempts_are_bounded() {
        let policy = RetryPolicy::default();
        assert!(policy.may_retry(0));
        assert!(policy.may_retry(7));
        assert!(!policy.may_retry(8));
        assert!(!policy.may_retry(100));
    }

    #[test]
    fn an_empty_queue_is_idle() {
        assert!(OutboxStats::default().is_idle());
        assert!(
            !OutboxStats {
                pending: 1,
                ..Default::default()
            }
            .is_idle()
        );
        assert!(
            !OutboxStats {
                dead: 1,
                ..Default::default()
            }
            .is_idle()
        );
    }
}

/// Helpers shared by the backends.
pub(crate) mod support {
    use super::*;

    /// When an entry that has just failed should next be attempted.
    /// `attempts` is the count *including* the one that just failed, so a first failure arrives
    /// as 1 and waits `base_delay`.
    pub(crate) fn next_attempt(
        now: Timestamp,
        attempts: u32,
        policy: &RetryPolicy,
    ) -> Option<Timestamp> {
        if !policy.may_retry(attempts) {
            return None;
        }
        let delay = policy.delay_after(attempts.saturating_sub(1));
        let seconds = i64::try_from(delay.as_secs()).unwrap_or(i64::MAX);
        now.checked_add(jiff::Span::new().seconds(seconds)).ok()
    }

    /// Age in seconds of the oldest entry, for [`OutboxStats`].
    pub(crate) fn age_seconds(now: Timestamp, oldest: Option<Timestamp>) -> Option<i64> {
        oldest.map(|t| (now.as_second() - t.as_second()).max(0))
    }

    /// The object an entry refers to, for diagnostics.
    ///
    /// Only the SQL backends record it; the in-memory one keeps the whole delivery anyway.
    #[cfg(any(feature = "sqlite", feature = "postgres"))]
    pub(crate) fn subject(
        delivery: &Delivery,
    ) -> (crate::model::ObjectType, crate::model::ObjectId) {
        (
            delivery.notification.object.object_type(),
            delivery.notification.object.id().clone(),
        )
    }
}
