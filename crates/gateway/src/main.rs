//! cubeharbor-gateway: the single public entry point for all servers.
//!
//! * Port 25565: reads the Minecraft handshake and routes by hostname to the right server.
//!   Answers pings for sleeping/starting servers itself and wakes them when someone joins.
//! * Allocated ports: plain TCP/UDP forwarding for voice chat, Bedrock (Geyser) and extras.

mod cidr;
mod forward;
mod minecraft;
mod pipe;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use cubeharbor_core::MinecraftServer;
use futures::StreamExt;
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client};
use tokio::io::AsyncWriteExt;

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
    /// Concurrent UDP sessions per forwarded port.
    #[arg(long, env = "MAX_UDP_SESSIONS", default_value_t = 4096)]
    pub max_udp_sessions: usize,
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
}

impl State {
    pub fn idle_timeout(&self) -> Option<std::time::Duration> {
        (self.cfg.idle_timeout_secs > 0).then(|| std::time::Duration::from_secs(self.cfg.idle_timeout_secs))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,kube=warn".into()))
        .init();
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
        cfg: cfg.clone(),
    });
    tokio::spawn(forward::run(state.clone()));
    tokio::spawn(health(cfg.health.clone()));
    minecraft::listen(state).await
}

async fn health(addr: String) {
    let Ok(listener) = tokio::net::TcpListener::bind(&addr).await else {
        tracing::error!("cannot bind health endpoint {addr}");
        return;
    };
    while let Ok((mut s, _)) = listener.accept().await {
        tokio::spawn(async move {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").await;
        });
    }
}
