//! Turn the gateway's wake requests into wake annotations.
//!
//! The gateway is internet-facing, so it may not patch MinecraftServers. It writes
//! `<namespace>_<name>` into one ConfigMap instead (see [`cubeharbor_core::wake`]), and this task
//! sets the server's wake annotation (which the reconciler already handles) and removes the entry.

use std::time::Duration;

use cubeharbor_core::{MinecraftServer, labels, wake};
use futures::StreamExt;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Client, ResourceExt};
use serde_json::json;

use crate::config::Config;

pub async fn run(client: Client, cfg: Config) {
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), &cfg.gateway_namespace);
    let wc = watcher::Config::default().fields(&format!("metadata.name={}", cfg.wake_configmap));
    let mut stream = watcher(cms.clone(), wc).default_backoff().applied_objects().boxed();
    // A request whose patch failed stays in the ConfigMap, but nothing changes it until the next
    // join attempt. Re-reading it now and then retries such requests on its own.
    let mut retry = tokio::time::interval(RETRY_EVERY);
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    retry.tick().await;
    loop {
        tokio::select! {
            ev = stream.next() => match ev {
                Some(Ok(cm)) => process(&client, &cms, &cfg, cm).await,
                Some(Err(e)) => {
                    tracing::warn!("watching wake requests: {e}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                None => break,
            },
            _ = retry.tick() => match cms.get_opt(&cfg.wake_configmap).await {
                Ok(Some(cm)) => process(&client, &cms, &cfg, cm).await,
                Ok(None) => {}
                Err(e) => tracing::warn!("re-reading wake requests: {e}"),
            },
        }
    }
}

/// How often pending wake requests are retried without a watch event.
const RETRY_EVERY: Duration = Duration::from_secs(60);

async fn process(client: &Client, cms: &Api<ConfigMap>, cfg: &Config, cm: ConfigMap) {
    for (key, ts) in cm.data.unwrap_or_default() {
        handle(client, cms, cfg, &key, &ts).await;
    }
}

async fn handle(client: &Client, cms: &Api<ConfigMap>, cfg: &Config, key: &str, ts: &str) {
    let Some((ns, name)) = wake::parse_key(key) else {
        tracing::warn!("ignoring malformed wake request `{key}`");
        return forget(cms, cfg, key).await;
    };
    if !cfg.watch_namespace.is_empty() && ns != cfg.watch_namespace {
        return forget(cms, cfg, key).await;
    }
    let servers: Api<MinecraftServer> = Api::namespaced(client.clone(), ns);
    let patch = json!({ "metadata": { "annotations": { labels::WAKE_ANNOTATION: ts } } });
    match servers.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await {
        Ok(s) => tracing::info!("{ns}/{}: wake requested by the gateway", s.name_any()),
        // The server is gone: nothing to wake.
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        // Keep the entry: the next watch event or the periodic re-read retries it.
        Err(e) => return tracing::warn!("{ns}/{name}: could not request wake: {e}"),
    }
    forget(cms, cfg, key).await;
}

async fn forget(cms: &Api<ConfigMap>, cfg: &Config, key: &str) {
    let patch = json!({ "data": { key: null } });
    if let Err(e) = cms.patch(&cfg.wake_configmap, &PatchParams::default(), &Patch::Merge(&patch)).await {
        tracing::warn!("could not clear wake request `{key}`: {e}");
    }
}
