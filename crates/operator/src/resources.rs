//! Kubernetes objects generated for a `MinecraftServer`.

use std::collections::BTreeMap;

use anyhow::Result;
use cubeharbor_core::crd::{
    Exposure, GEYSER_PORT, MINECRAFT_PORT, MinecraftServer, ModRef, PortAllocation, RCON_PORT, Resolved,
};
use cubeharbor_core::{defaults, labels};
use cubeharbor_core::plan::{GeyserPlan, InstallPlan, VoicePlan, hash_json};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service};
use k8s_openapi::api::networking::v1::{Ingress, NetworkPolicy};
use kube::{Resource, ResourceExt};
use serde_json::{Value, json};

use crate::config::Config;

pub fn selector(name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([(labels::SERVER.to_string(), name.to_string())])
}

pub fn common_labels(name: &str) -> BTreeMap<String, String> {
    let mut l = selector(name);
    l.insert(labels::MANAGED_BY.into(), labels::MANAGER.into());
    l.insert("app.kubernetes.io/name".into(), "minecraft".into());
    l.insert("app.kubernetes.io/instance".into(), name.into());
    l
}

fn meta(mcs: &MinecraftServer, name: String) -> Value {
    json!({
        "name": name,
        "namespace": mcs.namespace(),
        "labels": common_labels(&mcs.name_any()),
        "ownerReferences": [mcs.controller_owner_ref(&())],
    })
}

pub fn secret_name(mcs: &MinecraftServer) -> String {
    format!("{}-cubeharbor", mcs.name_any())
}

pub fn secret(mcs: &MinecraftServer, rcon_password: &str, curseforge_key: Option<&str>) -> Result<Secret> {
    let mut data = BTreeMap::from([("RCON_PASSWORD".to_string(), rcon_password.to_string())]);
    if let Some(k) = curseforge_key {
        data.insert("CURSEFORGE_API_KEY".into(), k.to_string());
    }
    Ok(serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": meta(mcs, secret_name(mcs)),
        "type": "Opaque",
        "stringData": data,
    }))?)
}

/// Extra mods implied by voice/geyser settings.
fn implied_mods(mcs: &MinecraftServer, resolved: &Resolved) -> Vec<ModRef> {
    let spec = &mcs.spec;
    let mut mods = vec![];
    if let Some(v) = spec.voice.as_ref().filter(|v| v.install) {
        mods.push(ModRef { modrinth: Some(v.kind.modrinth_slug().into()), ..Default::default() });
    }
    if let Some(g) = spec.geyser.as_ref().filter(|g| g.install) {
        use cubeharbor_core::Loader::*;
        let platform = match resolved.loader {
            Paper | Purpur => Some("spigot"),
            Fabric | Quilt => Some("fabric"),
            NeoForge => Some("neoforge"),
            _ => None,
        };
        if let Some(platform) = platform {
            let url = |project: &str| {
                format!("https://download.geysermc.org/v2/projects/{project}/versions/latest/builds/latest/downloads/{platform}")
            };
            let named = |project: &str| ModRef {
                url: Some(format!("{}#{project}-{platform}.jar", url(project))),
                ..Default::default()
            };
            mods.push(named("geyser"));
            if g.floodgate {
                mods.push(named("floodgate"));
            }
            if platform == "fabric" {
                mods.push(ModRef { modrinth: Some("fabric-api".into()), ..Default::default() });
            }
        } else {
            tracing::warn!("{}: Geyser is not available for {}", mcs.name_any(), resolved.loader);
        }
    }
    mods
}

pub fn plan(mcs: &MinecraftServer, resolved: &Resolved, allocations: &[PortAllocation], cfg: &Config) -> InstallPlan {
    let spec = &mcs.spec;
    let mut mods = spec.mods.clone();
    mods.extend(implied_mods(mcs, resolved));
    let voice = spec.voice.as_ref().map(|v| VoicePlan {
        kind: v.kind,
        container_port: v.kind.container_port(),
        public_address: allocations
            .iter()
            .find(|a| a.name == "voice")
            .and_then(|a| cfg.public_host().map(|h| format!("{h}:{}", a.public_port))),
    });
    let geyser = spec.geyser.as_ref().map(|_| GeyserPlan {
        container_port: GEYSER_PORT,
        public_port: allocations.iter().find(|a| a.name == "bedrock").map(|a| a.public_port),
    });
    InstallPlan {
        server_name: mcs.name_any(),
        minecraft_version: resolved.minecraft_version.clone(),
        loader: resolved.loader,
        loader_version: resolved.loader_version.clone(),
        modpack: resolved.modpack.clone(),
        modpack_exclude: spec.modpack.as_ref().map(|m| m.exclude.clone()).unwrap_or_default(),
        modpack_include: spec.modpack.as_ref().map(|m| m.include.clone()).unwrap_or_default(),
        mods,
        accept_eula: spec.accept_eula.unwrap_or(cfg.accept_eula),
        motd: spec.motd.clone(),
        ops: spec.ops.clone(),
        whitelist: spec.whitelist.clone(),
        properties: spec.properties.clone(),
        files: spec.files.clone(),
        voice,
        geyser,
        proxy_protocol: spec.routing.proxy_protocol,
    }
}

pub fn plan_configmap(mcs: &MinecraftServer, plan: &InstallPlan) -> Result<ConfigMap> {
    Ok(serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": meta(mcs, format!("{}-plan", mcs.name_any())),
        "data": { "plan.json": serde_json::to_string_pretty(plan)? },
    }))?)
}

fn service_ports(mcs: &MinecraftServer) -> Vec<Value> {
    let mut ports = vec![
        json!({ "name": "minecraft", "port": MINECRAFT_PORT, "targetPort": MINECRAFT_PORT, "protocol": "TCP" }),
        json!({ "name": "rcon", "port": RCON_PORT, "targetPort": RCON_PORT, "protocol": "TCP" }),
    ];
    let mut add = |name: &str, port: u16, proto: &str| {
        let n: String = name.chars().take(15).collect();
        ports.push(json!({ "name": n, "port": port, "targetPort": port, "protocol": proto }));
    };
    if let Some(v) = &mcs.spec.voice {
        add("voice", v.kind.container_port(), "UDP");
    }
    if mcs.spec.geyser.is_some() {
        add("bedrock", GEYSER_PORT, "UDP");
    }
    for p in &mcs.spec.extra_ports {
        add(&p.name, p.container_port, p.protocol.as_str());
    }
    ports
}

/// mc-router (`--in-kube-cluster`) routes to Services carrying these annotations.
pub const MC_ROUTER_HOST: &str = "mc-router.itzg.me/externalServerName";
pub const MC_ROUTER_DEFAULT: &str = "mc-router.itzg.me/defaultServer";

pub fn service(mcs: &MinecraftServer, hostnames: &[String], cfg: &Config) -> Result<Service> {
    let mut metadata = meta(mcs, mcs.name_any());
    if cfg.mc_router() {
        let mut a = serde_json::Map::new();
        if !hostnames.is_empty() {
            a.insert(MC_ROUTER_HOST.into(), json!(hostnames.join(",")));
        }
        if mcs.spec.routing.default {
            a.insert(MC_ROUTER_DEFAULT.into(), json!("true"));
        }
        metadata["annotations"] = Value::Object(a);
    }
    Ok(serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": metadata,
        "spec": {
            "type": "ClusterIP",
            "selector": selector(&mcs.name_any()),
            "ports": service_ports(mcs),
            // Let the gateway connect while the server is still starting so it can show a status.
            "publishNotReadyAddresses": true,
        },
    }))?)
}

/// Parse a Kubernetes quantity like `8Gi`, `8192Mi`, `8G` into MiB.
pub fn quantity_mib(q: &str) -> Option<u64> {
    let q = q.trim();
    let idx = q.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(q.len());
    let (num, unit) = q.split_at(idx);
    let n: f64 = num.parse().ok()?;
    let bytes = match unit {
        "" => n,
        "Ki" => n * 1024.0,
        "Mi" => n * 1024.0 * 1024.0,
        "Gi" => n * 1024f64.powi(3),
        "Ti" => n * 1024f64.powi(4),
        "k" | "K" => n * 1e3,
        "M" => n * 1e6,
        "G" => n * 1e9,
        "T" => n * 1e12,
        _ => return None,
    };
    Some((bytes / 1024.0 / 1024.0) as u64)
}

pub struct PodInputs<'a> {
    pub replicas: i32,
    pub image: String,
    pub plan_hash: String,
    pub existing_claims: Option<Value>,
    pub cfg: &'a Config,
}

pub fn statefulset(mcs: &MinecraftServer, inp: PodInputs<'_>) -> Result<StatefulSet> {
    let name = mcs.name_any();
    let spec = &mcs.spec;
    let cfg = inp.cfg;
    let mem = &spec.resources.memory;

    // Shared by both containers. Secrets and the cache path are added for the installer only:
    // the server container runs third-party mod code, which must not see the CurseForge key.
    let mut env = vec![
        json!({ "name": "CUBEHARBOR_DATA", "value": defaults::DATA_DIR }),
        json!({ "name": "CUBEHARBOR_DOWNLOAD_CONCURRENCY", "value": cfg.download_concurrency.to_string() }),
        json!({ "name": "STOP_TIMEOUT_SECONDS", "value": defaults::STOP_TIMEOUT_SECONDS.to_string() }),
    ];
    if let Some(mib) = quantity_mib(mem) {
        env.push(json!({ "name": "MEMORY_LIMIT_MIB", "value": mib.to_string() }));
    }
    if let Some(p) = spec.jvm.heap_percent {
        env.push(json!({ "name": "HEAP_PERCENT", "value": p.to_string() }));
    }
    if spec.jvm.aikar_flags == Some(false) {
        env.push(json!({ "name": "AIKAR_FLAGS", "value": "false" }));
    }
    if !spec.jvm.extra_args.is_empty() {
        env.push(json!({ "name": "JVM_EXTRA_ARGS", "value": serde_json::to_string(&spec.jvm.extra_args)? }));
    }
    let use_cache = cfg.cache_pvc.is_some() && mcs.namespace().as_deref() == Some(cfg.cache_namespace.as_str());
    for (k, v) in &spec.env {
        env.push(json!({ "name": k, "value": v }));
    }
    let server_env = env.clone();
    let mut install_env = env;
    install_env.push(json!({ "name": "RCON_PASSWORD", "valueFrom": { "secretKeyRef": { "name": secret_name(mcs), "key": "RCON_PASSWORD" } } }));
    install_env.push(json!({ "name": "CURSEFORGE_API_KEY", "valueFrom": { "secretKeyRef": { "name": secret_name(mcs), "key": "CURSEFORGE_API_KEY", "optional": true } } }));
    if use_cache {
        install_env.push(json!({ "name": "CUBEHARBOR_CACHE", "value": "/cache" }));
    }

    let mut requests = json!({ "memory": mem, "cpu": spec.resources.cpu.clone().unwrap_or_else(|| cfg.default_cpu_request.clone()) });
    let mut limits = json!({ "memory": mem });
    if let Some(c) = &spec.resources.cpu_limit {
        limits["cpu"] = json!(c);
    }
    if requests["cpu"] == "0" {
        requests.as_object_mut().unwrap().remove("cpu");
    }
    let resources = json!({ "requests": requests, "limits": limits });

    let server_mounts = vec![json!({ "name": "data", "mountPath": defaults::DATA_DIR }), json!({ "name": "plan", "mountPath": "/etc/cubeharbor", "readOnly": true })];
    let mut install_mounts = server_mounts.clone();
    let mut volumes = vec![json!({ "name": "plan", "configMap": { "name": format!("{name}-plan") } })];
    if use_cache {
        // The cache is shared between servers: only the installer may write to it.
        install_mounts.push(json!({ "name": "cache", "mountPath": "/cache" }));
        volumes.push(json!({ "name": "cache", "persistentVolumeClaim": { "claimName": cfg.cache_pvc } }));
    }
    let container_security = json!({
        "allowPrivilegeEscalation": false,
        "capabilities": { "drop": ["ALL"] },
    });

    let mut container_ports = vec![
        json!({ "name": "minecraft", "containerPort": MINECRAFT_PORT, "protocol": "TCP" }),
        json!({ "name": "rcon", "containerPort": RCON_PORT, "protocol": "TCP" }),
    ];
    for p in service_ports(mcs).into_iter().skip(2) {
        container_ports.push(json!({ "name": p["name"], "containerPort": p["port"], "protocol": p["protocol"] }));
    }

    let tcp = json!({ "tcpSocket": { "port": MINECRAFT_PORT } });
    let mut startup = tcp.clone();
    // Big modpacks can take a long time on first boot (world gen, mixins, datapacks).
    let startup_period = 10;
    let startup_failures = defaults::STARTUP_PROBE_MINUTES * 60 / startup_period;
    startup.as_object_mut().unwrap().extend([("periodSeconds".into(), json!(startup_period)), ("failureThreshold".into(), json!(startup_failures))]);
    // Ready = answers a status ping. Vanilla binds the port before the world is loaded, so a TCP
    // check would mark it Running (and the gateway would forward players) while it's still starting.
    let readiness = json!({
        "exec": { "command": ["cubeharbor-runner", "probe"] },
        "periodSeconds": 10, "timeoutSeconds": 8, "failureThreshold": 3,
    });
    let mut liveness = tcp;
    liveness.as_object_mut().unwrap().extend([("periodSeconds".into(), json!(30)), ("failureThreshold".into(), json!(10)), ("timeoutSeconds".into(), json!(10))]);

    let mut annotations = BTreeMap::from([(labels::PLAN_HASH_ANNOTATION.to_string(), inp.plan_hash)]);
    if let Some(r) = mcs.annotations().get(labels::RESTART_ANNOTATION) {
        annotations.insert(labels::RESTART_ANNOTATION.into(), r.clone());
    }

    let claims = inp.existing_claims.unwrap_or_else(|| {
        let mut pvc_spec = json!({
            "accessModes": ["ReadWriteOnce"],
            "resources": { "requests": { "storage": spec.resources.storage } },
        });
        if let Some(sc) = spec.resources.storage_class.clone().or(cfg.default_storage_class.clone()) {
            pvc_spec["storageClassName"] = json!(sc);
        }
        json!([{ "metadata": { "name": "data", "labels": selector(&name) }, "spec": pvc_spec }])
    });

    let pod_defaults = &cfg.server_pod;
    // Operator-wide labels first, so cubeharbor's own labels (used by selectors) always win.
    let mut labels_pod = pod_defaults.labels.clone();
    labels_pod.extend(common_labels(&name));
    labels_pod.insert(labels::COMPONENT.into(), "server".into());
    for (k, v) in &pod_defaults.annotations {
        annotations.entry(k.clone()).or_insert_with(|| v.clone());
    }
    let mut node_selector = pod_defaults.node_selector.clone();
    node_selector.extend(spec.node_selector.clone());
    let security_context = pod_defaults.security_context.clone().unwrap_or_else(|| {
        json!({
            "runAsUser": defaults::SERVER_UID, "runAsGroup": defaults::SERVER_UID, "runAsNonRoot": true,
            "fsGroup": defaults::SERVER_UID, "fsGroupChangePolicy": "OnRootMismatch",
            "seccompProfile": { "type": "RuntimeDefault" },
        })
    });

    let mut sts = json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "metadata": meta(mcs, name.clone()),
        "spec": {
            "replicas": inp.replicas,
            "serviceName": name,
            "selector": { "matchLabels": selector(&name) },
            "persistentVolumeClaimRetentionPolicy": { "whenDeleted": "Retain", "whenScaled": "Retain" },
            "template": {
                "metadata": { "labels": labels_pod, "annotations": annotations },
                "spec": {
                    "terminationGracePeriodSeconds": defaults::TERMINATION_GRACE_SECONDS,
                    "enableServiceLinks": false,
                    // Mods are third-party code: no API token, no privilege escalation, default seccomp.
                    "automountServiceAccountToken": false,
                    "securityContext": security_context,
                    "nodeSelector": node_selector,
                    "initContainers": [{
                        "name": "install",
                        "image": inp.image,
                        "imagePullPolicy": cfg.image_pull_policy,
                        "command": ["cubeharbor-installer"],
                        "env": install_env,
                        "resources": resources,
                        "volumeMounts": install_mounts,
                        "securityContext": container_security,
                    }],
                    "containers": [{
                        "name": "server",
                        "image": inp.image,
                        "imagePullPolicy": cfg.image_pull_policy,
                        "command": ["cubeharbor-runner"],
                        "stdin": true,
                        "tty": false,
                        "env": server_env,
                        "ports": container_ports,
                        "resources": resources,
                        "volumeMounts": server_mounts,
                        "securityContext": container_security,
                        "startupProbe": startup,
                        "readinessProbe": readiness,
                        "livenessProbe": liveness,
                    }],
                    "volumes": volumes,
                },
            },
            "volumeClaimTemplates": claims,
        },
    });
    let pod = &mut sts["spec"]["template"]["spec"];
    if !pod_defaults.image_pull_secrets.is_empty() {
        pod["imagePullSecrets"] = json!(pod_defaults.image_pull_secrets);
    }
    if !pod_defaults.tolerations.is_empty() {
        pod["tolerations"] = json!(pod_defaults.tolerations);
    }
    if let Some(a) = &pod_defaults.affinity {
        pod["affinity"] = a.clone();
    }
    if !pod_defaults.topology_spread_constraints.is_empty() {
        pod["topologySpreadConstraints"] = json!(pod_defaults.topology_spread_constraints);
    }
    if let Some(p) = &pod_defaults.priority_class_name {
        pod["priorityClassName"] = json!(p);
    }
    if let Some(r) = &pod_defaults.runtime_class_name {
        pod["runtimeClassName"] = json!(r);
    }
    Ok(serde_json::from_value(sts)?)
}

/// Ingresses for HTTP extra ports (web maps, dashboards of plugins).
pub fn ingresses(mcs: &MinecraftServer, cfg: &Config) -> Result<Vec<Ingress>> {
    let mut out = vec![];
    for p in mcs.spec.extra_ports.iter().filter(|p| p.expose == Exposure::Ingress) {
        let Some(host) = &p.host else {
            tracing::warn!("{}: port {} has expose=ingress but no host", mcs.name_any(), p.name);
            continue;
        };
        cfg.ingress_host_allowed(host).map_err(|e| anyhow::anyhow!("port {}: {e}", p.name))?;
        let mut spec = json!({
            "rules": [{
                "host": host,
                "http": { "paths": [{
                    "path": "/",
                    "pathType": "Prefix",
                    "backend": { "service": { "name": mcs.name_any(), "port": { "number": p.container_port } } },
                }]},
            }],
        });
        if let Some(c) = &cfg.ingress_class {
            spec["ingressClassName"] = json!(c);
        }
        out.push(serde_json::from_value(json!({
            "apiVersion": "networking.k8s.io/v1",
            "kind": "Ingress",
            "metadata": meta(mcs, format!("{}-{}", mcs.name_any(), p.name)),
            "spec": spec,
        }))?);
    }
    Ok(out)
}

pub fn network_policy_name(mcs: &MinecraftServer) -> String {
    format!("{}-cubeharbor-rcon", mcs.name_any())
}

/// Lets anyone reach the game and extra ports but only the dashboard reach RCON, so a
/// compromised pod elsewhere in the cluster cannot brute-force the console.
pub fn network_policy(mcs: &MinecraftServer, cfg: &Config) -> Result<NetworkPolicy> {
    let rcon = i32::from(RCON_PORT);
    Ok(serde_json::from_value(json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": meta(mcs, network_policy_name(mcs)),
        "spec": {
            "podSelector": { "matchLabels": selector(&mcs.name_any()) },
            "policyTypes": ["Ingress"],
            "ingress": [
                {
                    "ports": [
                        { "protocol": "TCP", "port": 1, "endPort": rcon - 1 },
                        { "protocol": "TCP", "port": rcon + 1, "endPort": 65535 },
                        { "protocol": "UDP", "port": 1, "endPort": 65535 },
                    ],
                },
                {
                    "from": [{
                        "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": cfg.dashboard_namespace } },
                        "podSelector": { "matchLabels": cfg.dashboard_selector() },
                    }],
                    "ports": [{ "protocol": "TCP", "port": rcon }],
                },
            ],
        },
    }))?)
}

pub fn hash_plan(plan: &InstallPlan, image: &str) -> String {
    hash_json(&(plan, image))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (MinecraftServer, Config) {
        use clap::Parser;
        let out = cubeharbor_core::compose::parse(
            "servers:\n  atm:\n    modpack: modrinth:cobblemon-fabric\n    memory: 12Gi\n    voice: true\n    geyser: true\n    ports: [\"bluemap:8100@map.mc.example.com\", \"query:25565/udp!internal\"]\n",
        )
        .unwrap();
        let mut mcs = out.manifests(Some("default")).remove(0);
        mcs.metadata.uid = Some("00000000-0000-0000-0000-000000000001".into());
        (mcs, Config::parse_from(["op", "--base-domain", "mc.example.com"]))
    }

    /// Builds every object for a feature-heavy server. With CUBEHARBOR_DUMP=<file> the
    /// objects are written as YAML for `kubectl apply --dry-run=server`.
    #[test]
    fn objects_build() {
        let (mcs, cfg) = sample();
        let resolved = Resolved {
            minecraft_version: "1.21.1".into(),
            loader: cubeharbor_core::Loader::Fabric,
            java_version: 21,
            ..Default::default()
        };
        let allocs = vec![PortAllocation { name: "voice".into(), protocol: cubeharbor_core::crd::Protocol::Udp, container_port: 24454, public_port: 25600 }];
        let plan = plan(&mcs, &resolved, &allocs, &cfg);
        assert_eq!(plan.voice.as_ref().unwrap().public_address.as_deref(), Some("mc.example.com:25600"));
        assert!(plan.mods.iter().any(|m| m.modrinth.as_deref() == Some("simple-voice-chat")));
        assert!(plan.mods.iter().any(|m| m.url.as_deref().is_some_and(|u| u.contains("floodgate"))));
        let objs: Vec<Value> = vec![
            serde_json::to_value(secret(&mcs, "pw", None).unwrap()).unwrap(),
            serde_json::to_value(plan_configmap(&mcs, &plan).unwrap()).unwrap(),
            serde_json::to_value(service(&mcs, &[], &cfg).unwrap()).unwrap(),
            serde_json::to_value(
                statefulset(&mcs, PodInputs { replicas: 1, image: cfg.image_for(21), plan_hash: "x".into(), existing_claims: None, cfg: &cfg }).unwrap(),
            )
            .unwrap(),
        ]
        .into_iter()
        .chain(ingresses(&mcs, &cfg).unwrap().into_iter().map(|i| serde_json::to_value(i).unwrap()))
        .collect();
        assert_eq!(objs.len(), 5);
        if let Ok(path) = std::env::var("CUBEHARBOR_DUMP") {
            let yaml: String = objs.iter().map(|o| format!("---\n{}", serde_json::to_string(o).unwrap())).collect::<Vec<_>>().join("\n");
            std::fs::write(path, yaml).unwrap();
        }
    }

    #[test]
    fn server_pod_defaults_apply() {
        let (mcs, mut cfg) = sample();
        cfg.server_pod = serde_json::from_str(
            r#"{"imagePullSecrets":[{"name":"regcred"}],"nodeSelector":{"pool":"games"},
                "tolerations":[{"key":"games","operator":"Exists","effect":"NoSchedule"}],
                "priorityClassName":"game-servers","labels":{"team":"mc","app.kubernetes.io/name":"nope"},
                "securityContext":{}}"#,
        )
        .unwrap();
        cfg.default_cpu_request = "100m".into();
        let sts = statefulset(&mcs, PodInputs { replicas: 1, image: cfg.image_for(21), plan_hash: "x".into(), existing_claims: None, cfg: &cfg }).unwrap();
        let v = serde_json::to_value(&sts).unwrap();
        let pod = &v["spec"]["template"]["spec"];
        assert_eq!(pod["imagePullSecrets"][0]["name"], "regcred");
        assert_eq!(pod["nodeSelector"]["pool"], "games");
        assert_eq!(pod["tolerations"][0]["key"], "games");
        assert_eq!(pod["priorityClassName"], "game-servers");
        assert_eq!(pod["securityContext"], json!({}));
        assert_eq!(pod["containers"][0]["resources"]["requests"]["cpu"], "100m");
        let labels = &v["spec"]["template"]["metadata"]["labels"];
        assert_eq!(labels["team"], "mc");
        assert_eq!(labels["app.kubernetes.io/name"], "minecraft", "built-in labels win");
    }

    #[test]
    fn mc_router_annotations() {
        use clap::Parser;
        let (mcs, _) = sample();
        let hosts = vec!["atm.cy.example.com".to_string(), "play.example.com".to_string()];
        let gw = Config::parse_from(["op"]);
        assert!(service(&mcs, &hosts, &gw).unwrap().metadata.annotations.is_none());
        let mr = Config::parse_from(["op", "--router", "mc-router"]);
        let a = service(&mcs, &hosts, &mr).unwrap().metadata.annotations.unwrap();
        assert_eq!(a[MC_ROUTER_HOST], "atm.cy.example.com,play.example.com");
        assert!(!a.contains_key(MC_ROUTER_DEFAULT));
    }

    #[test]
    fn server_container_gets_no_secrets() {
        let (mcs, cfg) = sample();
        let sts = statefulset(&mcs, PodInputs { replicas: 1, image: cfg.image_for(21), plan_hash: "x".into(), existing_claims: None, cfg: &cfg }).unwrap();
        let pod = sts.spec.template.spec.unwrap();
        assert_eq!(pod.automount_service_account_token, Some(false));
        let names = |env: &Option<Vec<k8s_openapi::api::core::v1::EnvVar>>| env.iter().flatten().map(|e| e.name.clone()).collect::<Vec<_>>();
        let install = names(&pod.init_containers.unwrap()[0].env);
        let server = names(&pod.containers[0].env);
        assert!(install.contains(&"CURSEFORGE_API_KEY".to_string()) && install.contains(&"RCON_PASSWORD".to_string()));
        assert!(!server.contains(&"CURSEFORGE_API_KEY".to_string()) && !server.contains(&"RCON_PASSWORD".to_string()));
        let sc = pod.containers[0].security_context.clone().unwrap();
        assert_eq!(sc.allow_privilege_escalation, Some(false));
    }

    #[test]
    fn ingress_hosts_are_restricted() {
        use clap::Parser;
        let (mut mcs, cfg) = sample();
        assert_eq!(ingresses(&mcs, &cfg).unwrap().len(), 1);
        mcs.spec.extra_ports[0].host = Some("cubeharbor.example.com".into());
        assert!(ingresses(&mcs, &cfg).is_err());
        let reserved = Config::parse_from(["op", "--base-domain", "example.com", "--reserved-hosts", "cubeharbor.example.com"]);
        assert!(ingresses(&mcs, &reserved).is_err());
        let open = Config::parse_from(["op"]);
        assert!(ingresses(&mcs, &open).is_ok());
    }

    #[test]
    fn rcon_policy() {
        let (mcs, cfg) = sample();
        let v = serde_json::to_value(network_policy(&mcs, &cfg).unwrap()).unwrap();
        assert_eq!(v["spec"]["ingress"][1]["ports"][0]["port"], RCON_PORT);
        assert_eq!(v["spec"]["ingress"][0]["ports"][0]["endPort"], RCON_PORT - 1);
    }

    #[test]
    fn image_overrides() {
        use clap::Parser;
        assert!(Config::parse_from(["op"]).image_allowed("evil/miner").is_err());
        let any = Config::parse_from(["op", "--allow-image-override", "true"]);
        assert!(any.image_allowed("evil/miner").is_ok());
        let some = Config::parse_from(["op", "--allow-image-override", "true", "--allowed-image-prefixes", "ghcr.io/me/"]);
        assert!(some.image_allowed("ghcr.io/me/server:1").is_ok());
        assert!(some.image_allowed("evil/miner").is_err());
    }

    #[test]
    fn quantities() {
        assert_eq!(quantity_mib("8Gi"), Some(8192));
        assert_eq!(quantity_mib("10240Mi"), Some(10240));
        assert_eq!(quantity_mib("1G"), Some(953));
        assert_eq!(quantity_mib("lots"), None);
    }
}
