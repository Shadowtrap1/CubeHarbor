//! Operator health and Prometheus metrics, served on `METRICS_ADDR`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use cubeharbor_core::MinecraftServer;
use cubeharbor_core::crd::Phase;
use cubeharbor_core::health::{Metrics, Response};
use kube::runtime::reflector::Store;

const PHASES: [Phase; 7] = [Phase::Pending, Phase::Installing, Phase::Starting, Phase::Running, Phase::Sleeping, Phase::Stopped, Phase::Error];

#[derive(Default)]
pub struct Stats {
    pub reconciles: AtomicU64,
    /// Reconciles that failed (the server's status then shows `Error` and the message).
    pub reconcile_errors: AtomicU64,
    pub reconcile_micros: AtomicU64,
    pub gateway_sync_errors: AtomicU64,
    /// This replica holds the leader Lease (always true without leader election).
    pub leader: AtomicBool,
    /// The controller's cache, set once this replica leads.
    pub servers: OnceLock<Store<MinecraftServer>>,
}

impl Stats {
    pub fn reconciled(&self, took: Duration, ok: bool) {
        self.reconciles.fetch_add(1, Ordering::Relaxed);
        self.reconcile_micros.fetch_add(took.as_micros() as u64, Ordering::Relaxed);
        if !ok {
            self.reconcile_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// `/healthz` (alive), `/readyz` (started; standbys are ready too) and `/metrics`.
pub fn endpoint(stats: &Arc<Stats>, path: &str) -> Response {
    match path {
        "/healthz" | "/livez" | "/readyz" => Response::text(200, "ok"),
        "/metrics" => Response::metrics(render(stats)),
        _ => Response::not_found(),
    }
}

fn render(s: &Stats) -> String {
    let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
    let servers = s.servers.get().map(|st| st.state()).unwrap_or_default();
    let count = |p: Phase| servers.iter().filter(|m| m.phase() == p).count() as f64;
    let by_phase: Vec<(Phase, f64)> = PHASES.iter().map(|p| (*p, count(*p))).collect();
    let labels: Vec<[(&str, &str); 1]> = by_phase.iter().map(|(p, _)| [("phase", p.as_str())]).collect();
    let players: u64 = servers.iter().filter_map(|m| m.status.as_ref()?.players.as_ref()).map(|p| u64::from(p.online)).sum();
    let ports: usize = servers.iter().map(|m| m.status.as_ref().map_or(0, |st| st.allocations.len())).sum();
    let mut m = Metrics::new();
    m.gauge("cubeharbor_operator_leader", "1 if this replica is the leader and reconciles.", if s.leader.load(Ordering::Relaxed) { 1.0 } else { 0.0 })
        .counter("cubeharbor_operator_reconciles_total", "MinecraftServer reconciles.", get(&s.reconciles))
        .counter("cubeharbor_operator_reconcile_errors_total", "Reconciles that failed (the server shows phase Error).", get(&s.reconcile_errors))
        .summary(
            "cubeharbor_operator_reconcile_duration_seconds",
            "Time spent reconciling.",
            get(&s.reconcile_micros) as f64 / 1e6,
            get(&s.reconciles),
        )
        .counter("cubeharbor_operator_gateway_sync_errors_total", "Failed updates of the gateway ports Service.", get(&s.gateway_sync_errors));
    // Only the leader has a cache of servers; standbys don't report these.
    if s.servers.get().is_some() {
        m.labeled(
            "cubeharbor_operator_servers",
            "gauge",
            "MinecraftServers by phase.",
            labels.iter().zip(&by_phase).map(|(l, (_, n))| (&l[..], *n)),
        )
        .gauge("cubeharbor_operator_players_online", "Players online across all servers.", players as f64)
        .gauge("cubeharbor_operator_allocated_ports", "Public ports allocated to servers.", ports as f64);
    }
    m.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cubeharbor_core::crd::{MinecraftServerSpec, MinecraftServerStatus, Players};
    use kube::runtime::reflector::store::Writer;
    use kube::runtime::watcher::Event;

    fn server(name: &str, phase: Phase, online: u32) -> MinecraftServer {
        let mut s = MinecraftServer::new(name, MinecraftServerSpec::default());
        s.metadata.namespace = Some("mc".into());
        s.status = Some(MinecraftServerStatus { phase, players: Some(Players { online, ..Default::default() }), ..Default::default() });
        s
    }

    #[test]
    fn metrics_by_phase() {
        let stats = Arc::new(Stats::default());
        stats.reconciled(Duration::from_millis(1500), true);
        stats.reconciled(Duration::from_millis(500), false);
        let body = endpoint(&stats, "/metrics").body;
        assert!(body.contains("\ncubeharbor_operator_reconciles_total 2\n"), "{body}");
        assert!(body.contains("\ncubeharbor_operator_reconcile_errors_total 1\n"), "{body}");
        assert!(body.contains("\ncubeharbor_operator_reconcile_duration_seconds_sum 2\n"), "{body}");
        assert!(body.contains("cubeharbor_operator_leader 0\n"), "{body}");
        assert!(!body.contains("cubeharbor_operator_servers"), "standbys have no cache: {body}");

        let mut w: Writer<MinecraftServer> = Writer::default();
        stats.servers.set(w.as_reader()).ok().unwrap();
        w.apply_watcher_event(&Event::Apply(server("a", Phase::Running, 3)));
        w.apply_watcher_event(&Event::Apply(server("b", Phase::Running, 2)));
        w.apply_watcher_event(&Event::Apply(server("c", Phase::Sleeping, 0)));
        let body = endpoint(&stats, "/metrics").body;
        assert!(body.contains("cubeharbor_operator_servers{phase=\"Running\"} 2\n"), "{body}");
        assert!(body.contains("cubeharbor_operator_servers{phase=\"Sleeping\"} 1\n"), "{body}");
        assert!(body.contains("cubeharbor_operator_servers{phase=\"Error\"} 0\n"), "{body}");
        assert!(body.contains("cubeharbor_operator_players_online 5\n"), "{body}");
        assert_eq!(endpoint(&stats, "/readyz").status, 200);
        assert_eq!(endpoint(&stats, "/x").status, 404);
    }
}
