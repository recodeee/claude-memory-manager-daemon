//! Minimal Prometheus exposition over a Unix-domain TCP listener.
//!
//! Why hand-roll instead of `prometheus` crate: keeps the dep tree small and
//! the metrics surface obvious. If we ever want histograms or labels beyond
//! what's here, swap in the proper crate.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tracing::warn;

#[derive(Default, Debug, Serialize)]
pub struct Metrics {
    pub ticks_total: AtomicU64,
    pub ticks_ran_total: AtomicU64,
    pub ticks_skipped_total: AtomicU64,
    pub tick_duration_sum_seconds: AtomicU64,
    pub tick_failures_total: AtomicU64,
    pub audit_issues_last: AtomicU64,
    pub history_appends_total: AtomicU64,
    pub last_tick_unix: AtomicU64,
}

impl Metrics {
    pub fn record_tick(
        &self,
        ran: bool,
        duration_sec: u64,
        audit_issues: u64,
        exit_code: Option<i32>,
    ) {
        self.ticks_total.fetch_add(1, Ordering::Relaxed);
        if ran {
            self.ticks_ran_total.fetch_add(1, Ordering::Relaxed);
        } else {
            self.ticks_skipped_total.fetch_add(1, Ordering::Relaxed);
        }
        self.tick_duration_sum_seconds
            .fetch_add(duration_sec, Ordering::Relaxed);
        self.audit_issues_last
            .store(audit_issues, Ordering::Relaxed);
        if let Some(code) = exit_code {
            if code != 0 {
                self.tick_failures_total.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.last_tick_unix
            .store(crate::history::now_unix(), Ordering::Relaxed);
    }

    pub fn record_history_append(&self) {
        self.history_appends_total.fetch_add(1, Ordering::Relaxed);
    }

    /// `tick_interval_sec` lets the exporter publish a derived gauge for
    /// "is the daemon stuck?" — staleness > 2× interval is unhealthy.
    pub fn render_prometheus(&self, tick_interval_sec: u64) -> String {
        let now = crate::history::now_unix();
        let last_tick = self.last_tick_unix.load(Ordering::Relaxed);
        let staleness = if last_tick == 0 {
            0
        } else {
            now.saturating_sub(last_tick)
        };
        let mut out = String::new();
        // Build info — published once. CARGO_PKG_VERSION is always available;
        // GIT_SHA is set by build.rs (empty when not in a git checkout).
        let version = env!("CARGO_PKG_VERSION");
        let git_sha = option_env!("CMMD_GIT_SHA").unwrap_or("unknown");
        out.push_str("# HELP cmmd_build_info Static build information.\n");
        out.push_str("# TYPE cmmd_build_info gauge\n");
        out.push_str(&format!(
            "cmmd_build_info{{version=\"{version}\",git_sha=\"{git_sha}\"}} 1\n"
        ));
        out.push_str("# HELP cmmd_tick_interval_sec Configured TICK_INTERVAL_SEC.\n");
        out.push_str("# TYPE cmmd_tick_interval_sec gauge\n");
        out.push_str(&format!("cmmd_tick_interval_sec {}\n", tick_interval_sec));
        out.push_str("# HELP cmmd_ticks_total Total tick attempts (ran + skipped).\n");
        out.push_str("# TYPE cmmd_ticks_total counter\n");
        out.push_str(&format!(
            "cmmd_ticks_total {}\n",
            self.ticks_total.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP cmmd_ticks_ran_total Ticks where the agent actually spawned.\n");
        out.push_str("# TYPE cmmd_ticks_ran_total counter\n");
        out.push_str(&format!(
            "cmmd_ticks_ran_total {}\n",
            self.ticks_ran_total.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP cmmd_ticks_skipped_total Ticks that aborted before spawn (clean audit, lsof guard, min idle).\n");
        out.push_str("# TYPE cmmd_ticks_skipped_total counter\n");
        out.push_str(&format!(
            "cmmd_ticks_skipped_total {}\n",
            self.ticks_skipped_total.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP cmmd_tick_duration_sum_seconds Sum of tick durations (compute average as ratio over ticks_total).\n");
        out.push_str("# TYPE cmmd_tick_duration_sum_seconds counter\n");
        out.push_str(&format!(
            "cmmd_tick_duration_sum_seconds {}\n",
            self.tick_duration_sum_seconds.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP cmmd_tick_failures_total Ticks where claude exited non-zero.\n");
        out.push_str("# TYPE cmmd_tick_failures_total counter\n");
        out.push_str(&format!(
            "cmmd_tick_failures_total {}\n",
            self.tick_failures_total.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP cmmd_audit_issues_last Issues found by the most recent Rust audit.\n");
        out.push_str("# TYPE cmmd_audit_issues_last gauge\n");
        out.push_str(&format!(
            "cmmd_audit_issues_last {}\n",
            self.audit_issues_last.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP cmmd_history_appends_total Tick records written to history.jsonl.\n");
        out.push_str("# TYPE cmmd_history_appends_total counter\n");
        out.push_str(&format!(
            "cmmd_history_appends_total {}\n",
            self.history_appends_total.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP cmmd_last_tick_unix Unix timestamp of the most recent tick.\n");
        out.push_str("# TYPE cmmd_last_tick_unix gauge\n");
        out.push_str(&format!("cmmd_last_tick_unix {}\n", last_tick));
        out.push_str("# HELP cmmd_tick_staleness_seconds Seconds since the last tick (helps spot a stuck daemon).\n");
        out.push_str("# TYPE cmmd_tick_staleness_seconds gauge\n");
        out.push_str(&format!("cmmd_tick_staleness_seconds {}\n", staleness));
        out
    }
}

/// Start a minimal HTTP server on `bind`. Supported endpoints:
///   GET /metrics  — Prometheus exposition
///   GET /healthz  — 200 if last-tick is within 2× tick_interval, else 503
/// Anything else returns 404. Bind failure is logged and the task exits.
pub async fn serve(bind: String, metrics: std::sync::Arc<Metrics>, tick_interval_sec: u64) {
    let listener = match TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            warn!("metrics: could not bind {bind}: {e}");
            return;
        }
    };
    tracing::info!(addr = %bind, "metrics endpoint up");
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                warn!("metrics: accept err: {e}");
                continue;
            }
        };
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            // We only need to read until the first \r\n to know if it's GET /metrics.
            use tokio::io::AsyncReadExt;
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let first_line = req.lines().next().unwrap_or("");
            let (body, status_line, content_type) = if first_line.starts_with("GET /metrics") {
                (
                    metrics.render_prometheus(tick_interval_sec),
                    "HTTP/1.1 200 OK",
                    "text/plain; version=0.0.4",
                )
            } else if first_line.starts_with("GET /healthz") {
                let now = crate::history::now_unix();
                let last = metrics.last_tick_unix.load(Ordering::Relaxed);
                let staleness = if last == 0 {
                    0
                } else {
                    now.saturating_sub(last)
                };
                let limit = tick_interval_sec.saturating_mul(2);
                if last == 0 || staleness <= limit {
                    (
                        format!("ok staleness={staleness}s limit={limit}s\n"),
                        "HTTP/1.1 200 OK",
                        "text/plain",
                    )
                } else {
                    (
                        format!("stuck staleness={staleness}s > limit={limit}s\n"),
                        "HTTP/1.1 503 Service Unavailable",
                        "text/plain",
                    )
                }
            } else {
                (String::new(), "HTTP/1.1 404 Not Found", "text/plain")
            };
            let resp = format!(
                "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_all_metric_names() {
        let m = Metrics::default();
        m.record_tick(true, 5, 3, Some(0));
        m.record_tick(false, 0, 0, None);
        let out = m.render_prometheus(900);
        for name in [
            "cmmd_build_info",
            "cmmd_tick_interval_sec",
            "cmmd_ticks_total",
            "cmmd_ticks_ran_total",
            "cmmd_ticks_skipped_total",
            "cmmd_tick_duration_sum_seconds",
            "cmmd_tick_failures_total",
            "cmmd_audit_issues_last",
            "cmmd_history_appends_total",
            "cmmd_last_tick_unix",
            "cmmd_tick_staleness_seconds",
        ] {
            assert!(out.contains(name), "missing metric {name} in:\n{out}");
        }
    }

    #[test]
    fn ran_tick_increments_ran_counter_not_skipped() {
        let m = Metrics::default();
        m.record_tick(true, 3, 1, Some(0));
        assert_eq!(m.ticks_total.load(Ordering::Relaxed), 1);
        assert_eq!(m.ticks_ran_total.load(Ordering::Relaxed), 1);
        assert_eq!(m.ticks_skipped_total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn nonzero_exit_increments_failures() {
        let m = Metrics::default();
        m.record_tick(true, 3, 0, Some(1));
        assert_eq!(m.tick_failures_total.load(Ordering::Relaxed), 1);
    }
}
