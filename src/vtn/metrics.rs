//! `GET /metrics` — Prometheus exposition.
//!
//! Written by hand rather than pulled in. A metrics facade plus an exporter is two crates and a
//! global registry for four counters and one histogram, and the global registry is the part that
//! hurts: a process embedding two VTNs would have them share it silently. Everything here hangs off
//! [`AppState`], so it is per-VTN like the rest of the configuration.
//!
//! ## What is worth measuring
//!
//! Request rate and latency by route, because that is the shape of every incident; and the outbox,
//! because it is the one piece of VTN state whose degradation is invisible from the protocol. A
//! notification that is never delivered produces no error anywhere — OpenADR has no way for a VEN
//! to say "you never told me" — so `openadr_outbox_oldest_pending_seconds` is the number that turns
//! a silent failure into a page.
//!
//! Route labels come from axum's matched path (`/events/{id}`), never the request URI, so a client
//! walking ids cannot mint a new time series per request. That is the standard way to turn a
//! metrics endpoint into a memory leak.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::{
    extract::{MatchedPath, Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::AppState;

/// Upper bounds of the latency histogram, in seconds.
///
/// Chosen for what a VTN actually does: a cached `304` is sub-millisecond, a `GET /events` with a
/// year of intervals is tens of milliseconds, and anything past a second is a problem rather than a
/// measurement.
const BUCKETS: [f64; 11] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 10.0,
];

/// One route-and-status series.
#[derive(Debug, Default)]
struct Series {
    count: AtomicU64,
    /// Total latency in microseconds; divided out at scrape time.
    micros: AtomicU64,
    buckets: [AtomicU64; BUCKETS.len()],
}

impl Series {
    fn record(&self, elapsed: std::time::Duration) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.micros
            .fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
        let seconds = elapsed.as_secs_f64();
        for (bucket, bound) in self.buckets.iter().zip(BUCKETS) {
            if seconds <= bound {
                bucket.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Counters and histograms for one VTN.
///
/// Cheap to clone: everything is behind one `Arc`.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Keyed by `(method, matched path, status)`. Bounded by the router, not by traffic.
    requests: std::sync::Mutex<std::collections::BTreeMap<(String, String, u16), Arc<Series>>>,
    /// Keyed by `(channel, outcome)`.
    deliveries: std::sync::Mutex<std::collections::BTreeMap<(&'static str, &'static str), u64>>,
    /// Reports deleted by retention, since this process started.
    reports_purged: std::sync::atomic::AtomicU64,
}

/// The gauges that are *state* rather than a rate, read from the store at scrape time.
///
/// A struct rather than an argument list, because every one of these is read from the same store in
/// the same place and a second VTN instance sharing the database sees the same numbers: a counter
/// maintained per process would report only its own share. It also means adding a gauge is a field
/// rather than a change to every caller's signature.
#[derive(Debug, Clone, Default)]
pub struct Gauges {
    /// The notification queue.
    pub outbox: super::store::OutboxStats,
    /// Subscriptions the circuit breaker is suppressing.
    pub subscribers_cut_off: u64,
    /// How much report data there is, and how far back it goes.
    pub reports: super::store::ReportStats,
}

impl Metrics {
    /// A fresh registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one finished request.
    pub fn record_request(
        &self,
        method: &str,
        route: &str,
        status: u16,
        elapsed: std::time::Duration,
    ) {
        let key = (method.to_string(), route.to_string(), status);
        let series = {
            let mut requests = self
                .inner
                .requests
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            requests.entry(key).or_default().clone()
        };
        series.record(elapsed);
    }

    /// Record the outcome of one delivery attempt.
    ///
    /// `outcome` is `delivered`, `retrying` or `abandoned` — the three the dispatcher distinguishes
    /// and the three an operator alerts on differently.
    pub fn record_delivery(&self, channel: &super::notify::Channel, outcome: &'static str) {
        let channel = match channel {
            super::notify::Channel::Webhook => "webhook",
            super::notify::Channel::Mqtt => "mqtt",
        };
        *self
            .inner
            .deliveries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry((channel, outcome))
            .or_default() += 1;
    }

    /// Record reports deleted by a retention sweep.
    pub fn record_purged_reports(&self, count: u64) {
        self.inner
            .reports_purged
            .fetch_add(count, std::sync::atomic::Ordering::Relaxed);
    }

    /// Render the exposition format.
    ///
    /// [`Gauges`] comes from the store at scrape time rather than from counters, because it is
    /// state rather than a rate: a second VTN instance sharing the database sees the same queue,
    /// and a counter maintained per process would report only its own share of it.
    pub fn render(&self, gauges: &Gauges) -> String {
        use std::fmt::Write as _;
        let outbox = &gauges.outbox;
        let cut_off_subscribers = gauges.subscribers_cut_off;
        let mut out = String::with_capacity(4096);

        out.push_str(
            "# HELP openadr_http_requests_total Requests handled, by method, route and status.\n\
             # TYPE openadr_http_requests_total counter\n",
        );
        let requests = self
            .inner
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for ((method, route, status), series) in &requests {
            let _ = writeln!(
                out,
                "openadr_http_requests_total{{method=\"{}\",route=\"{}\",status=\"{status}\"}} {}",
                escape(method),
                escape(route),
                series.count.load(Ordering::Relaxed)
            );
        }

        out.push_str(
            "# HELP openadr_http_request_duration_seconds Request latency, by method and route.\n\
             # TYPE openadr_http_request_duration_seconds histogram\n",
        );
        // Summed across statuses: latency by route is the question, and a 404's latency is not a
        // different distribution worth its own series.
        let mut by_route: std::collections::BTreeMap<
            (&str, &str),
            (u64, u64, [u64; BUCKETS.len()]),
        > = std::collections::BTreeMap::new();
        for ((method, route, _), series) in &requests {
            let entry = by_route
                .entry((method.as_str(), route.as_str()))
                .or_insert((0, 0, [0; BUCKETS.len()]));
            entry.0 += series.count.load(Ordering::Relaxed);
            entry.1 += series.micros.load(Ordering::Relaxed);
            for (total, bucket) in entry.2.iter_mut().zip(&series.buckets) {
                *total += bucket.load(Ordering::Relaxed);
            }
        }
        for ((method, route), (count, micros, buckets)) in &by_route {
            let labels = format!("method=\"{}\",route=\"{}\"", escape(method), escape(route));
            for (bucket, bound) in buckets.iter().zip(BUCKETS) {
                let _ = writeln!(
                    out,
                    "openadr_http_request_duration_seconds_bucket{{{labels},le=\"{bound}\"}} {bucket}"
                );
            }
            let _ = writeln!(
                out,
                "openadr_http_request_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {count}"
            );
            let _ = writeln!(
                out,
                "openadr_http_request_duration_seconds_sum{{{labels}}} {}",
                *micros as f64 / 1_000_000.0
            );
            let _ = writeln!(
                out,
                "openadr_http_request_duration_seconds_count{{{labels}}} {count}"
            );
        }

        out.push_str(
            "# HELP openadr_notification_attempts_total Delivery attempts, by channel and outcome.\n\
             # TYPE openadr_notification_attempts_total counter\n",
        );
        for ((channel, outcome), count) in self
            .inner
            .deliveries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            let _ = writeln!(
                out,
                "openadr_notification_attempts_total{{channel=\"{channel}\",outcome=\"{outcome}\"}} {count}"
            );
        }

        out.push_str(
            "# HELP openadr_outbox_pending Notifications queued, including ones awaiting a retry.\n\
             # TYPE openadr_outbox_pending gauge\n",
        );
        let _ = writeln!(out, "openadr_outbox_pending {}", outbox.pending);
        out.push_str(
            "# HELP openadr_outbox_dead Notifications abandoned after exhausting their attempts.\n\
             # TYPE openadr_outbox_dead gauge\n",
        );
        let _ = writeln!(out, "openadr_outbox_dead {}", outbox.dead);
        out.push_str(
            "# HELP openadr_outbox_oldest_pending_seconds Age of the oldest queued notification.\n\
             # TYPE openadr_outbox_oldest_pending_seconds gauge\n",
        );
        let _ = writeln!(
            out,
            "openadr_outbox_oldest_pending_seconds {}",
            outbox.oldest_pending_seconds.unwrap_or(0)
        );
        out.push_str(
            "# HELP openadr_subscribers_cut_off Subscriptions the circuit breaker is suppressing.\n\
             # TYPE openadr_subscribers_cut_off gauge\n",
        );
        // The one number the outbox gauges cannot show. A cut-off subscriber queues nothing, so an
        // empty queue means either "everything was delivered" or "nobody is being told any more",
        // and only this distinguishes them.
        let _ = writeln!(out, "openadr_subscribers_cut_off {cut_off_subscribers}");

        // Reports are the only object that grows without bound, so its size is the one storage
        // number worth a gauge — and the age of the oldest is how an operator tells "retention is
        // configured and working" from "retention is configured and the sweeper is not running".
        out.push_str(
            "# HELP openadr_reports_stored Reports currently held.\n\
             # TYPE openadr_reports_stored gauge\n",
        );
        let _ = writeln!(out, "openadr_reports_stored {}", gauges.reports.count);
        out.push_str(
            "# HELP openadr_reports_oldest_seconds Age of the oldest stored report.\n\
             # TYPE openadr_reports_oldest_seconds gauge\n",
        );
        let _ = writeln!(
            out,
            "openadr_reports_oldest_seconds {}",
            gauges.reports.oldest_seconds.unwrap_or(0)
        );
        out.push_str(
            "# HELP openadr_reports_purged_total Reports deleted by retention since start-up.\n\
             # TYPE openadr_reports_purged_total counter\n",
        );
        let _ = writeln!(
            out,
            "openadr_reports_purged_total {}",
            self.inner
                .reports_purged
                .load(std::sync::atomic::Ordering::Relaxed)
        );

        out
    }
}

/// Escape a label value per the exposition format.
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Time every request and file it under its *matched* route.
pub async fn track(State(state): State<AppState>, request: Request, next: Next) -> Response {
    // The matched path, not the URI: `/events/{id}` is one series, `/events/evt-1` … `/events/evt-n`
    // is a series per event and an unbounded one, which is how a metrics endpoint becomes the
    // memory leak it was installed to detect.
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "<unmatched>".to_string());
    let method = request.method().as_str().to_string();

    let started = Instant::now();
    let response = next.run(request).await;
    state.metrics.record_request(
        &method,
        &route,
        response.status().as_u16(),
        started.elapsed(),
    );
    response
}

/// `GET /metrics`
pub async fn scrape(State(state): State<AppState>) -> Response {
    let gauges = gauges(&state).await;
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(&gauges),
    )
        .into_response()
}

/// Read every state gauge from the store.
///
/// Shared by `GET /metrics` and `GET /health`, so the two cannot report different numbers for the
/// same question — which is the whole reason `GET /health` embeds `OutboxStats` rather than
/// counting for itself.
pub async fn gauges(state: &AppState) -> Gauges {
    let now = state.clock.now();
    Gauges {
        outbox: state.storage.outbox_stats(now).await.unwrap_or_default(),
        subscribers_cut_off: state
            .storage
            .subscriber_health()
            .await
            .map(|health| health.iter().filter(|h| h.is_cut_off(now)).count() as u64)
            .unwrap_or(0),
        reports: state.storage.report_stats(now).await.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_request_lands_in_every_bucket_at_or_above_its_latency() {
        let metrics = Metrics::new();
        metrics.record_request("GET", "/events", 200, Duration::from_millis(30));
        let rendered = metrics.render(&Gauges::default());

        assert!(rendered.contains(
            "openadr_http_requests_total{method=\"GET\",route=\"/events\",status=\"200\"} 1"
        ));
        // 30 ms is above the 25 ms bound and at or below the 50 ms one. Cumulative buckets mean
        // every bound from there up counts it.
        assert!(rendered.contains("le=\"0.025\"} 0"));
        assert!(rendered.contains("le=\"0.05\"} 1"));
        assert!(rendered.contains("le=\"+Inf\"} 1"));
        assert!(rendered.contains(
            "openadr_http_request_duration_seconds_count{method=\"GET\",route=\"/events\"} 1"
        ));
    }

    #[test]
    fn latency_is_summed_across_statuses_but_counted_apart() {
        let metrics = Metrics::new();
        metrics.record_request("GET", "/events", 200, Duration::from_millis(1));
        metrics.record_request("GET", "/events", 404, Duration::from_millis(1));
        let rendered = metrics.render(&Gauges::default());
        assert!(rendered.contains("status=\"200\"} 1"));
        assert!(rendered.contains("status=\"404\"} 1"));
        assert!(rendered.contains(
            "openadr_http_request_duration_seconds_count{method=\"GET\",route=\"/events\"} 2"
        ));
    }

    #[test]
    fn the_outbox_gauges_come_from_the_store_not_from_a_counter() {
        // Two VTN instances share one queue. A per-process counter would report each one's share of
        // it, which is a number nobody can alert on.
        let metrics = Metrics::new();
        let rendered = metrics.render(&Gauges {
            outbox: super::super::store::OutboxStats {
                pending: 7,
                dead: 2,
                oldest_pending_seconds: Some(310),
            },
            ..Default::default()
        });
        assert!(rendered.contains("openadr_outbox_pending 7"));
        assert!(rendered.contains("openadr_outbox_dead 2"));
        assert!(rendered.contains("openadr_outbox_oldest_pending_seconds 310"));
    }

    #[test]
    fn the_report_gauges_are_a_state_and_a_counter() {
        // The size is state — a second instance sharing the database sees the same table — and the
        // number purged is this process's own work. Reporting one as the other is how "retention is
        // running" and "retention has nothing to do" become indistinguishable.
        let metrics = Metrics::new();
        metrics.record_purged_reports(4);
        metrics.record_purged_reports(3);
        let rendered = metrics.render(&Gauges {
            reports: super::super::store::ReportStats {
                count: 1_234,
                oldest_seconds: Some(86_400),
            },
            ..Default::default()
        });
        assert!(rendered.contains("openadr_reports_stored 1234"));
        assert!(rendered.contains("openadr_reports_oldest_seconds 86400"));
        assert!(rendered.contains("openadr_reports_purged_total 7"));
    }

    #[test]
    fn delivery_outcomes_are_counted_per_channel() {
        let metrics = Metrics::new();
        metrics.record_delivery(&super::super::notify::Channel::Webhook, "delivered");
        metrics.record_delivery(&super::super::notify::Channel::Mqtt, "abandoned");
        let rendered = metrics.render(&Gauges::default());
        assert!(rendered.contains(
            "openadr_notification_attempts_total{channel=\"webhook\",outcome=\"delivered\"} 1"
        ));
        assert!(rendered.contains(
            "openadr_notification_attempts_total{channel=\"mqtt\",outcome=\"abandoned\"} 1"
        ));
    }

    #[test]
    fn label_values_are_escaped() {
        let metrics = Metrics::new();
        metrics.record_request("GET", "/a\"b\\c", 200, Duration::from_millis(1));
        assert!(metrics.render(&Gauges::default()).contains("/a\\\"b\\\\c"));
    }
}
