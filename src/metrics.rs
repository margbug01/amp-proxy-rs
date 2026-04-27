use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

const DURATION_BUCKETS: [(&str, f64); 11] = [
    ("0.005", 0.005),
    ("0.01", 0.01),
    ("0.025", 0.025),
    ("0.05", 0.05),
    ("0.1", 0.1),
    ("0.25", 0.25),
    ("0.5", 0.5),
    ("1", 1.0),
    ("2.5", 2.5),
    ("5", 5.0),
    ("10", 10.0),
];

pub struct Metrics {
    requests_total: AtomicU64,
    request_duration_buckets: [AtomicU64; DURATION_BUCKETS.len()],
    request_duration_sum_nanos: AtomicU64,
    request_duration_count: AtomicU64,
    billable_requests_total: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            requests_total: AtomicU64::new(0),
            request_duration_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            request_duration_sum_nanos: AtomicU64::new(0),
            request_duration_count: AtomicU64::new(0),
            billable_requests_total: AtomicU64::new(0),
        }
    }

    pub fn record_request(&self, duration: Duration) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.request_duration_count.fetch_add(1, Ordering::Relaxed);
        self.request_duration_sum_nanos.fetch_add(
            duration.as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );

        let seconds = duration.as_secs_f64();
        if let Some((idx, _)) = DURATION_BUCKETS
            .iter()
            .enumerate()
            .find(|(_, (_, upper))| seconds <= *upper)
        {
            self.request_duration_buckets[idx].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn increment_billable(&self) {
        self.billable_requests_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn render_prometheus(&self) -> String {
        let requests_total = self.requests_total.load(Ordering::Relaxed);
        let duration_count = self.request_duration_count.load(Ordering::Relaxed);
        let duration_sum =
            self.request_duration_sum_nanos.load(Ordering::Relaxed) as f64 / 1_000_000_000.0;
        let billable_total = self.billable_requests_total.load(Ordering::Relaxed);

        let mut out = String::new();
        writeln!(out, "# HELP requests_total Total HTTP requests.").unwrap();
        writeln!(out, "# TYPE requests_total counter").unwrap();
        writeln!(out, "requests_total {requests_total}").unwrap();
        writeln!(
            out,
            "# HELP request_duration_seconds HTTP request duration in seconds."
        )
        .unwrap();
        writeln!(out, "# TYPE request_duration_seconds histogram").unwrap();

        let mut cumulative = 0u64;
        for (idx, (label, _)) in DURATION_BUCKETS.iter().enumerate() {
            cumulative += self.request_duration_buckets[idx].load(Ordering::Relaxed);
            writeln!(
                out,
                "request_duration_seconds_bucket{{le=\"{label}\"}} {cumulative}"
            )
            .unwrap();
        }
        writeln!(
            out,
            "request_duration_seconds_bucket{{le=\"+Inf\"}} {duration_count}"
        )
        .unwrap();
        writeln!(out, "request_duration_seconds_sum {duration_sum:.9}").unwrap();
        writeln!(out, "request_duration_seconds_count {duration_count}").unwrap();
        writeln!(
            out,
            "# HELP billable_requests_total Total requests forwarded to ampcode.com fallback."
        )
        .unwrap();
        writeln!(out, "# TYPE billable_requests_total counter").unwrap();
        writeln!(out, "billable_requests_total {billable_total}").unwrap();
        out
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn metrics_middleware(
    State(metrics): State<Arc<Metrics>>,
    req: Request,
    next: Next,
) -> Response {
    if req.uri().path() == "/metrics" {
        return next.run(req).await;
    }

    let started = Instant::now();
    let response = next.run(req).await;
    metrics.record_request(started.elapsed());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_prometheus_exports_counters_and_histogram() {
        let metrics = Metrics::new();
        metrics.record_request(Duration::from_millis(7));
        metrics.record_request(Duration::from_millis(600));
        metrics.increment_billable();

        let rendered = metrics.render_prometheus();

        assert!(rendered.contains("# TYPE requests_total counter"));
        assert!(rendered.contains("requests_total 2"));
        assert!(rendered.contains("# TYPE request_duration_seconds histogram"));
        assert!(rendered.contains("request_duration_seconds_bucket{le=\"0.005\"} 0"));
        assert!(rendered.contains("request_duration_seconds_bucket{le=\"0.01\"} 1"));
        assert!(rendered.contains("request_duration_seconds_bucket{le=\"1\"} 2"));
        assert!(rendered.contains("request_duration_seconds_bucket{le=\"+Inf\"} 2"));
        assert!(rendered.contains("request_duration_seconds_count 2"));
        assert!(rendered.contains("billable_requests_total 1"));
    }
}
