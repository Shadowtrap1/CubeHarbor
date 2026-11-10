//! cubeharbor-operator: reconciles `MinecraftServer` resources into running servers.

mod conditions;
mod config;
mod dns;
mod leader;
mod metrics;
mod ports;
mod reconcile;
mod resources;
mod server_conditions;
mod wake;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use cubeharbor_core::MinecraftServer;
use cubeharbor_core::sources::Http;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service};
use kube::runtime::events::{Recorder, Reporter};
use kube::runtime::controller::Config as ControllerConfig;
use kube::runtime::{Controller, WatchStreamExt, reflector, watcher};
use kube::{Api, Client};
use std::collections::HashMap;
use tokio::sync::{Mutex, Notify};

use crate::reconcile::{Ctx, error_policy, reconcile, sync_gateway_ports};

#[tokio::main]
async fn main() -> Result<()> {
    cubeharbor_core::logging::init("info,kube=warn", true);
    let cfg = config::Config::load()?;
    let client = Client::try_default().await?;
    let http = Http::new(cfg.curseforge_api_key.clone())?;
    tracing::info!("cubeharbor-operator {} starting (ports {})", env!("CARGO_PKG_VERSION"), cfg.port_range);

    let stats = Arc::new(metrics::Stats::default());
    let probe_stats = stats.clone();
    cubeharbor_core::health::spawn(cfg.health_addr.clone(), move |path| metrics::endpoint(&probe_stats, path));

    // Only one replica may reconcile: port allocation is serialised inside one process.
    let elector = cfg.leader_election.then(|| {
        let ns = if cfg.pod_namespace.is_empty() { cfg.gateway_namespace.clone() } else { cfg.pod_namespace.clone() };
        let identity = identity(&cfg.pod_name);
        tracing::info!("leader election on, Lease {ns}/{} as {identity}", cfg.leader_election_lease);
        leader::Elector::new(client.clone(), &ns, &cfg.leader_election_lease, identity)
    });
    if let Some(e) = &elector {
        tokio::select! {
            _ = e.acquire() => tracing::info!("became the leader"),
            _ = cubeharbor_core::health::shutdown_signal() => {
                tracing::info!("shutting down before becoming the leader");
                return Ok(());
            }
        }
    }
    stats.leader.store(true, Ordering::Relaxed);

    let (servers, sts, svc, cm, sec) = if cfg.watch_namespace.is_empty() {
        (Api::<MinecraftServer>::all(client.clone()), Api::<StatefulSet>::all(client.clone()), Api::<Service>::all(client.clone()), Api::<ConfigMap>::all(client.clone()), Api::<Secret>::all(client.clone()))
    } else {
        let ns = cfg.watch_namespace.as_str();
        (Api::namespaced(client.clone(), ns), Api::namespaced(client.clone(), ns), Api::namespaced(client.clone(), ns), Api::namespaced(client.clone(), ns), Api::namespaced(client.clone(), ns))
    };
    let owned = watcher::Config::default().labels(&format!("{}={}", cubeharbor_core::labels::MANAGED_BY, cubeharbor_core::labels::MANAGER));

    let controller = Controller::new(servers, watcher::Config::default());
    let _ = stats.servers.set(controller.store());
    // Port allocation needs every server even when only one namespace is reconciled.
    let fleet = if cfg.watch_namespace.is_empty() {
        controller.store()
    } else {
        let (store, writer) = reflector::store();
        let stream = reflector(writer, watcher(Api::<MinecraftServer>::all(client.clone()), watcher::Config::default()).default_backoff());
        tokio::spawn(stream.for_each(|_| futures::future::ready(())));
        store
    };

    let reporter = Reporter { controller: "cubeharbor-operator".into(), instance: (!cfg.pod_name.is_empty()).then(|| cfg.pod_name.clone()) };
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        http,
        cfg: cfg.clone(),
        allocations: Mutex::new(HashMap::new()),
        fleet,
        gateway_applied: Mutex::new(None),
        gateway_sync: Notify::new(),
        stats: stats.clone(),
        recorder: Recorder::new(client.clone(), reporter),
        failure_writes: Default::default(),
    });

    // Gateway ports: debounce reconcile notifications, plus a periodic sweep that catches deletions.
    let sync_ctx = ctx.clone();
    tokio::spawn(async move {
        loop {
            let sweep = tokio::time::timeout(Duration::from_secs(30), sync_ctx.gateway_sync.notified()).await.is_err();
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Err(e) = sync_gateway_ports(&sync_ctx, sweep).await {
                sync_ctx.stats.gateway_sync_errors.fetch_add(1, Ordering::Relaxed);
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

    // Stops on SIGTERM/SIGINT after the reconciles in flight finish.
    // The debounce folds the watch echo of a status write into the next run.
    let limits = ControllerConfig::default().concurrency(cfg.max_concurrent_reconciles).debounce(Duration::from_secs(1));
    let run = controller
        .with_config(limits)
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
        });

    match &elector {
        None => run.await,
        Some(e) => {
            tokio::select! {
                _ = run => {}
                _ = e.hold() => {
                    // Another replica may be reconciling already: stop at once rather than race it.
                    tracing::error!("lost the leader Lease, exiting");
                    std::process::exit(1);
                }
            }
            e.release().await;
        }
    }
    tracing::info!("stopped");
    Ok(())
}

/// The pod name, or the hostname (which is the pod name in Kubernetes), plus a random suffix so a
/// restarted container never mistakes its predecessor's Lease for its own.
fn identity(pod_name: &str) -> String {
    let base = if pod_name.is_empty() { std::env::var("HOSTNAME").unwrap_or_else(|_| "cubeharbor-operator".into()) } else { pod_name.to_string() };
    let suffix: String = {
        use std::io::Read;
        let mut b = [0u8; 4];
        let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
        b.iter().map(|x| format!("{x:02x}")).collect()
    };
    format!("{base}_{suffix}")
}
