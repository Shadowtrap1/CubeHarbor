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
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(cm) => {
                for (key, ts) in cm.data.unwrap_or_default() {
                    handle(&client, &cms, &cfg, &key, &ts).await;
                }
            }
            Err(e) => {
                tracing::warn!("watching wake requests: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
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
        // Keep the entry so the next watch event retries.
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
