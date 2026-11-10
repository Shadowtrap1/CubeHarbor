//! Operations endpoint (`HEALTH_ADDR`): `/healthz`, `/readyz` and Prometheus `/metrics`. Counts
//! only: no user names, IPs or server names.

use std::sync::atomic::{AtomicU64, Ordering};

use cubeharbor_core::health::{Metrics, Response};

use crate::{AppState, MAX_LOG_STREAMS};

#[derive(Default)]
pub struct Stats {
    pub logins: AtomicU64,
    pub password_failures: AtomicU64,
    pub token_failures: AtomicU64,
    pub throttled: AtomicU64,
    pub forbidden: AtomicU64,
    pub csrf_refused: AtomicU64,
}

pub fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// HEALTH_ADDR: `/healthz` (alive), `/readyz` (alive and not shutting down), `/metrics`.
pub fn endpoint(st: &AppState, path: &str) -> Response {
    match path {
        "/healthz" | "/livez" => Response::text(200, "ok"),
        "/readyz" if st.draining.load(Ordering::Relaxed) => Response::text(503, "shutting down"),
        "/readyz" => Response::text(200, "ok"),
        "/metrics" => Response::metrics(metrics(st)),
        _ => Response::not_found(),
    }
}

fn metrics(st: &AppState) -> String {
    let s = &st.stats;
    let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
    let streams = MAX_LOG_STREAMS.saturating_sub(st.log_streams.available_permits());
    let mut m = Metrics::new();
    m.counter("cubeharbor_dashboard_logins_total", "Successful password logins.", get(&s.logins))
        .counter("cubeharbor_dashboard_password_failures_total", "Failed password checks.", get(&s.password_failures))
        .counter("cubeharbor_dashboard_token_failures_total", "Rejected API tokens.", get(&s.token_failures))
        .counter("cubeharbor_dashboard_throttled_total", "Attempts refused by the failed-login limits.", get(&s.throttled))
        .counter("cubeharbor_dashboard_forbidden_total", "Requests refused for their role or namespace.", get(&s.forbidden))
        .counter("cubeharbor_dashboard_csrf_refused_total", "Cross-site requests refused.", get(&s.csrf_refused))
        .gauge("cubeharbor_dashboard_sessions", "Browser sessions that haven't expired.", st.users.session_count() as f64)
        .gauge("cubeharbor_dashboard_users", "Dashboard users.", st.users.snapshot().users.len() as f64)
        .gauge("cubeharbor_dashboard_log_streams_open", "Log streams currently open.", streams as f64)
        .gauge("cubeharbor_dashboard_draining", "1 while shutting down.", if st.draining.load(Ordering::Relaxed) { 1.0 } else { 0.0 });
    m.finish()
}
