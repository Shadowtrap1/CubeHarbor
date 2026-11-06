//! cubeharbor-operator: reconciles `MinecraftServer` resources into running servers.

mod config;
mod dns;
mod ports;
mod reconcile;
mod resources;
mod wake;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use cubeharbor_core::MinecraftServer;
use cubeharbor_core::sources::Http;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service};
use kube::runtime::{Controller, watcher};
use kube::{Api, Client};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Notify};

use crate::reconcile::{Ctx, error_policy, reconcile, sync_gateway_ports};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,kube=warn".into()))
        .init();
    let cfg = config::Config::load()?;
    let client = Client::try_default().await?;
    let http = Http::new(cfg.curseforge_api_key.clone())?;
    tracing::info!("cubeharbor-operator {} starting (ports {})", env!("CARGO_PKG_VERSION"), cfg.port_range);

    let health = cfg.health_addr.clone();
    tokio::spawn(async move {
        if let Err(e) = health_server(&health).await {
            tracing::error!("health server: {e}");
        }
    });

    let ctx = Arc::new(Ctx { client: client.clone(), http, cfg: cfg.clone(), alloc_lock: Mutex::new(()), gateway_sync: Notify::new() });

    // Gateway ports: debounce reconcile notifications, plus a periodic sweep that catches deletions.
    let sync_ctx = ctx.clone();
    tokio::spawn(async move {
        loop {
            let _ = tokio::time::timeout(Duration::from_secs(30), sync_ctx.gateway_sync.notified()).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Err(e) = sync_gateway_ports(&sync_ctx).await {
                tracing::warn!("syncing gateway ports: {e:#}");
            }
        }
    });

    tokio::spawn(wake::run(client.clone(), cfg.clone()));

    match dns::Settings::from_config(&cfg) {
        Ok(Some(settings)) => {
            tokio::spawn(dns::run(client.clone(), settings, cfg.watch_namespace.clone()));
        }
        Ok(None) => {}
        // A settings mistake shouldn't take server management down with it.
        Err(e) => tracing::error!("Cloudflare DNS updates are off: {e}"),
    }

    let (servers, sts, svc, cm, sec) = if cfg.watch_namespace.is_empty() {
        (Api::<MinecraftServer>::all(client.clone()), Api::<StatefulSet>::all(client.clone()), Api::<Service>::all(client.clone()), Api::<ConfigMap>::all(client.clone()), Api::<Secret>::all(client.clone()))
    } else {
        let ns = cfg.watch_namespace.as_str();
        (Api::namespaced(client.clone(), ns), Api::namespaced(client.clone(), ns), Api::namespaced(client.clone(), ns), Api::namespaced(client.clone(), ns), Api::namespaced(client.clone(), ns))
    };
    let owned = watcher::Config::default().labels(&format!("{}={}", cubeharbor_core::labels::MANAGED_BY, cubeharbor_core::labels::MANAGER));

    Controller::new(servers, watcher::Config::default())
        .owns(sts, owned.clone())
        .owns(svc, owned.clone())
        .owns(cm, owned.clone())
        .owns(sec, owned)
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::debug!("reconcile: {e}");
            }
        })
        .await;
    Ok(())
}

/// Minimal liveness endpoint (`GET /healthz`).
async fn health_server(addr: &str) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    loop {
        let (mut s, _) = listener.accept().await?;
        tokio::spawn(async move {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").await;
        });
    }
}
