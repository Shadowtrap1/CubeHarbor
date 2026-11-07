//! cubeharbor-gateway: the single public entry point for all servers.
//!
//! * Port 25565: reads the Minecraft handshake and routes by hostname to the right server.
//!   Answers pings for sleeping/starting servers itself and wakes them when someone joins.
//! * Allocated ports: plain TCP/UDP forwarding for voice chat, Bedrock (Geyser) and extras.

mod forward;
mod minecraft;
mod pipe;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use cubeharbor_core::MinecraftServer;
use cubeharbor_core::cidr;
use cubeharbor_core::health::{Metrics, Response};
use futures::StreamExt;
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client};

#[derive(Parser, Debug, Clone)]
#[command(version, about = "cubeharbor Minecraft gateway")]
pub struct Config {
    #[arg(long, env = "LISTEN_ADDR", default_value = "0.0.0.0:25565")]
    pub listen: String,
    #[arg(long, env = "HEALTH_ADDR", default_value = "0.0.0.0:8080")]
    pub health: String,
    /// Only route to servers in this namespace (all when empty).
    #[arg(long, env = "WATCH_NAMESPACE", default_value = "")]
    pub watch_namespace: String,
    /// Bind address for allocated extra ports.
    #[arg(long, env = "PORTS_BIND", default_value = "0.0.0.0")]
    pub ports_bind: String,
    /// Expect a PROXY protocol header from the load balancer in front of the gateway.
    #[arg(long, env = "ACCEPT_PROXY_PROTOCOL", default_value_t = false, value_parser = clap::builder::BoolishValueParser::new())]
    pub accept_proxy_protocol: bool,
    /// Peers (comma separated CIDRs) whose PROXY headers are believed. Headers from anyone else
    /// are ignored, so players can't spoof their address. Empty = trust every peer.
    #[arg(long, env = "TRUSTED_PROXIES", default_value = "")]
    pub trusted_proxies: String,
    /// Concurrent Minecraft connections, across all clients.
    #[arg(long, env = "MAX_CONNECTIONS", default_value_t = 4096)]
    pub max_connections: usize,
    /// Concurrent connections on forwarded TCP ports (all ports together). Separate from
    /// MAX_CONNECTIONS so a flood on e.g. a web map port can't stop players from joining.
    #[arg(long, env = "MAX_FORWARD_CONNECTIONS", default_value_t = 1024)]
    pub max_forward_connections: usize,
    /// Close TCP connections with no traffic either way for this many seconds (0 = never).
    #[arg(long, env = "IDLE_TIMEOUT_SECS", default_value_t = 600)]
    pub idle_timeout_secs: u64,
    /// Namespace the gateway runs in (where its wake ConfigMap lives).
    #[arg(long, env = "POD_NAMESPACE", default_value = cubeharbor_core::defaults::NAMESPACE)]
    pub namespace: String,
    /// ConfigMap the gateway records wake requests in (see `cubeharbor_core::wake`).
    #[arg(long, env = "WAKE_CONFIGMAP", default_value = cubeharbor_core::wake::DEFAULT_CONFIGMAP)]
    pub wake_configmap: String,
    /// Concurrent UDP sessions per forwarded port.
    #[arg(long, env = "MAX_UDP_SESSIONS", default_value_t = 4096)]
    pub max_udp_sessions: usize,
    /// On SIGTERM, keep accepting for this long while `/readyz` reports not ready, so load
    /// balancers and kube-proxy stop sending new players here first.
    #[arg(long, env = "SHUTDOWN_DELAY_SECS", default_value_t = 5)]
    pub shutdown_delay_secs: u64,
    /// Then wait up to this long for open connections to finish before exiting. Keep
    /// SHUTDOWN_DELAY_SECS + DRAIN_TIMEOUT_SECS below the pod's terminationGracePeriodSeconds (30).
    #[arg(long, env = "DRAIN_TIMEOUT_SECS", default_value_t = 20)]
    pub drain_timeout_secs: u64,
}

/// Counters for `/metrics`.
#[derive(Default)]
pub struct Stats {
    /// Minecraft connections accepted on the routing port.
    pub accepted: AtomicU64,
    /// Dropped because MAX_CONNECTIONS was reached.
    pub rejected: AtomicU64,
    /// Relayed to a running server.
    pub routed: AtomicU64,
    /// Answered by the gateway because the server wasn't running (sleeping, starting, ...).
    pub offline: AtomicU64,
    /// No server (and no default server) for the requested hostname.
    pub unknown_host: AtomicU64,
    /// Wake requests written for sleeping servers, and those that failed.
    pub wakes: AtomicU64,
    pub wake_errors: AtomicU64,
    /// Forwarded-port TCP connections dropped because MAX_FORWARD_CONNECTIONS was reached.
    pub forward_rejected: AtomicU64,
    /// Allocated ports currently being forwarded.
    pub forwarded_ports: AtomicU64,
    /// Set on SIGTERM: `/readyz` fails and the listeners shut down.
    pub draining: AtomicBool,
}

pub fn inc(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone)]
pub struct State {
    pub servers: Store<MinecraftServer>,
    pub client: Client,
    pub cfg: Config,
    pub trusted_proxies: Vec<cidr::Cidr>,
    pub connections: Arc<tokio::sync::Semaphore>,
    pub forward_connections: Arc<tokio::sync::Semaphore>,
    /// Last wake request per server, so join spam doesn't hammer the API server.
    pub wakes: Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>>,
    pub stats: Arc<Stats>,
}

impl State {
    pub fn idle_timeout(&self) -> Option<std::time::Duration> {
        (self.cfg.idle_timeout_secs > 0).then(|| std::time::Duration::from_secs(self.cfg.idle_timeout_secs))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    cubeharbor_core::logging::init("info,kube=warn", true);
    let cfg = Config::parse();
    let trusted_proxies = cidr::parse_list(&cfg.trusted_proxies)?;
    if cfg.accept_proxy_protocol && trusted_proxies.is_empty() {
        tracing::warn!("ACCEPT_PROXY_PROTOCOL is on without TRUSTED_PROXIES: any client can claim any source address");
    }
    let client = Client::try_default().await?;
    let api: Api<MinecraftServer> =
        if cfg.watch_namespace.is_empty() { Api::all(client.clone()) } else { Api::namespaced(client.clone(), &cfg.watch_namespace) };

    let (store, writer) = reflector::store();
    tokio::spawn(async move {
        reflector::reflector(writer, watcher(api, watcher::Config::default()))
            .default_backoff()
            .touched_objects()
            .for_each(|r| async move {
                if let Err(e) = r {
                    tracing::warn!("watch error: {e}");
                }
            })
            .await;
    });
    store.wait_until_ready().await?;
    tracing::info!("cubeharbor-gateway {} ready, {} servers known", env!("CARGO_PKG_VERSION"), store.state().len());

    let state = Arc::new(State {
        servers: store,
        client,
        trusted_proxies,
        connections: Arc::new(tokio::sync::Semaphore::new(cfg.max_connections.max(1))),
        forward_connections: Arc::new(tokio::sync::Semaphore::new(cfg.max_forward_connections.max(1))),
        wakes: Default::default(),
        stats: Default::default(),
        cfg: cfg.clone(),
    });
    let forwarding = tokio::spawn(forward::run(state.clone()));
    let probe_state = state.clone();
    cubeharbor_core::health::spawn(cfg.health.clone(), move |path| endpoint(&probe_state, path));

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let listening = tokio::spawn(minecraft::listen(state.clone(), stop_rx));
    tokio::select! {
        r = listening => return r?,
        _ = cubeharbor_core::health::shutdown_signal() => {}
    }

    // Graceful shutdown: report not ready and keep serving while the pod is removed from the
    // Service endpoints, then stop accepting and let open connections finish (up to the drain
    // timeout). Players still connected after that are disconnected when the process exits.
    tracing::info!("shutting down: not ready, still accepting for {}s", cfg.shutdown_delay_secs);
    state.stats.draining.store(true, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_secs(cfg.shutdown_delay_secs)).await;
    let _ = stop_tx.send(true);
    forwarding.abort();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(cfg.drain_timeout_secs);
    loop {
        let (mc, fwd) = open_connections(&state);
        if mc + fwd == 0 {
            tracing::info!("all connections closed, exiting");
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::info!("drain timeout reached, closing {} connection(s)", mc + fwd);
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}

/// Open (Minecraft, forwarded TCP) connections.
fn open_connections(state: &State) -> (usize, usize) {
    let mc = state.cfg.max_connections.max(1).saturating_sub(state.connections.available_permits());
    let fwd = state.cfg.max_forward_connections.max(1).saturating_sub(state.forward_connections.available_permits());
    (mc, fwd)
}

/// HEALTH_ADDR: `/healthz` (alive), `/readyz` (alive and not shutting down), `/metrics`.
fn endpoint(state: &State, path: &str) -> Response {
    match path {
        "/healthz" | "/livez" => Response::text(200, "ok"),
        "/readyz" if state.stats.draining.load(Ordering::Relaxed) => Response::text(503, "shutting down"),
        "/readyz" => Response::text(200, "ok"),
        "/metrics" => Response::metrics(metrics(state)),
        _ => Response::not_found(),
    }
}

fn metrics(state: &State) -> String {
    let s = &state.stats;
    let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
    let servers = state.servers.state();
    let routes: usize = servers.iter().map(|m| m.status.as_ref().map_or(0, |st| st.hostnames.len())).sum();
    let (mc_open, fwd_open) = open_connections(state);
    let mut m = Metrics::new();
    m.gauge("cubeharbor_gateway_connections_open", "Minecraft connections currently open.", mc_open as f64)
        .gauge("cubeharbor_gateway_connections_max", "MAX_CONNECTIONS.", state.cfg.max_connections as f64)
        .counter("cubeharbor_gateway_connections_accepted_total", "Minecraft connections accepted.", get(&s.accepted))
        .counter("cubeharbor_gateway_connections_rejected_total", "Minecraft connections dropped at MAX_CONNECTIONS.", get(&s.rejected))
        .counter("cubeharbor_gateway_connections_routed_total", "Connections relayed to a running server.", get(&s.routed))
        .counter(
            "cubeharbor_gateway_connections_offline_total",
            "Connections answered by the gateway because the server wasn't running.",
            get(&s.offline),
        )
        .counter("cubeharbor_gateway_connections_unknown_host_total", "Connections for a hostname no server claims.", get(&s.unknown_host))
        .counter("cubeharbor_gateway_wakes_total", "Wake requests written for sleeping servers.", get(&s.wakes))
        .counter("cubeharbor_gateway_wake_errors_total", "Wake requests that could not be written.", get(&s.wake_errors))
        .gauge("cubeharbor_gateway_forward_connections_open", "TCP connections open on forwarded ports.", fwd_open as f64)
        .counter(
            "cubeharbor_gateway_forward_connections_rejected_total",
            "Forwarded-port connections dropped at MAX_FORWARD_CONNECTIONS.",
            get(&s.forward_rejected),
        )
        .gauge("cubeharbor_gateway_forwarded_ports", "Allocated ports being forwarded.", get(&s.forwarded_ports) as f64)
        .gauge("cubeharbor_gateway_servers", "MinecraftServers the gateway knows.", servers.len() as f64)
        .gauge("cubeharbor_gateway_routes", "Hostnames routed to a server.", routes as f64)
        .gauge("cubeharbor_gateway_draining", "1 while shutting down.", if s.draining.load(Ordering::Relaxed) { 1.0 } else { 0.0 });
    m.finish()
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[tokio::test]
    async fn readiness_and_metrics() {
        let client = Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().unwrap())).unwrap();
        let (store, _writer) = reflector::store();
        let cfg = Config::parse_from(["gw", "--max-connections", "10", "--max-forward-connections", "5"]);
        let state = State {
            servers: store,
            client,
            trusted_proxies: vec![],
            connections: Arc::new(tokio::sync::Semaphore::new(10)),
            forward_connections: Arc::new(tokio::sync::Semaphore::new(5)),
            wakes: Default::default(),
            stats: Default::default(),
            cfg,
        };
        let _held = state.connections.clone().try_acquire_many_owned(3).unwrap();
        inc(&state.stats.wakes);
        assert_eq!(endpoint(&state, "/readyz").status, 200);
        let body = endpoint(&state, "/metrics").body;
        assert!(body.contains("\ncubeharbor_gateway_connections_open 3\n"), "{body}");
        assert!(body.contains("\ncubeharbor_gateway_wakes_total 1\n"), "{body}");
        assert_eq!(open_connections(&state), (3, 0));
        state.stats.draining.store(true, Ordering::Relaxed);
        assert_eq!(endpoint(&state, "/readyz").status, 503);
        assert_eq!(endpoint(&state, "/healthz").status, 200, "liveness stays up while draining");
        assert_eq!(endpoint(&state, "/nope").status, 404);
    }
}
