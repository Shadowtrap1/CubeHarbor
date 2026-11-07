use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use cubeharbor_core::crd::{
    MINECRAFT_PORT, MinecraftServer, MinecraftServerStatus, Phase, Players, PortAllocation, effective_hostnames,
};
use cubeharbor_core::labels;
use cubeharbor_core::plan::hash_json;
use cubeharbor_core::protocol;
use cubeharbor_core::sources::{Http, resolve};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Secret, Service};
use k8s_openapi::api::networking::v1::{Ingress, NetworkPolicy};
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{Client, Resource, ResourceExt};
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::sync::{Mutex, Notify};

use crate::config::Config;
use crate::ports::{self, Used};
use crate::resources::{self, PodInputs};

pub struct Ctx {
    pub client: Client,
    pub http: Http,
    pub cfg: Config,
    pub alloc_lock: Mutex<()>,
    pub gateway_sync: Notify,
    pub stats: Arc<crate::metrics::Stats>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0:#}")]
pub struct Error(#[from] anyhow::Error);

/// Minute precision: status patches with an unchanged timestamp are no-ops and don't re-trigger reconciles.
fn now() -> String {
    let secs = jiff::Timestamp::now().as_second() / 60 * 60;
    jiff::Timestamp::from_second(secs).map(|t| t.to_string()).unwrap_or_default()
}

fn minutes_since(ts: &str) -> Option<i64> {
    let t: jiff::Timestamp = ts.parse().ok()?;
    Some((jiff::Timestamp::now().as_second() - t.as_second()) / 60)
}

pub async fn reconcile(mcs: Arc<MinecraftServer>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = mcs.namespace().context("MinecraftServer must be namespaced")?;
    let name = mcs.name_any();
    let api: Api<MinecraftServer> = Api::namespaced(ctx.client.clone(), &ns);
    let mut status = mcs.status.clone().unwrap_or_default();
    status.observed_generation = mcs.metadata.generation;
    status.service = Some(format!("{name}.{ns}.svc"));
    status.hostnames = effective_hostnames(&name, &mcs.spec, ctx.cfg.base_domain());

    let started = std::time::Instant::now();
    let result = reconcile_inner(&mcs, &ctx, &ns, &name, &mut status).await;
    ctx.stats.reconciled(started.elapsed(), result.is_ok());
    match result {
        Ok(action) => {
            patch_status(&api, &name, &status).await?;
            ctx.gateway_sync.notify_one();
            Ok(action)
        }
        Err(e) => {
            tracing::warn!("{ns}/{name}: {e:#}");
            status.phase = Phase::Error;
            status.message = Some(format!("{e:#}"));
            patch_status(&api, &name, &status).await?;
            Ok(Action::requeue(Duration::from_secs(60)))
        }
    }
}

async fn patch_status(api: &Api<MinecraftServer>, name: &str, status: &MinecraftServerStatus) -> Result<(), Error> {
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(json!({ "status": status_patch(status)? })))
        .await
        .context("patching status")?;
    Ok(())
}

/// Status fields that serialization leaves out when empty.
const CLEARABLE_STATUS_FIELDS: &[&str] =
    &["message", "observedGeneration", "resolved", "resolvedHash", "hostnames", "service", "allocations", "players", "lastActive"];

/// The status as a merge patch. A key left out of a merge patch keeps its old value, so empty
/// fields are sent as explicit nulls; otherwise e.g. a fixed error's message would never clear.
fn status_patch(status: &MinecraftServerStatus) -> Result<serde_json::Value> {
    let mut v = serde_json::to_value(status)?;
    if let Some(o) = v.as_object_mut() {
        for field in CLEARABLE_STATUS_FIELDS {
            o.entry(*field).or_insert(serde_json::Value::Null);
        }
    }
    Ok(v)
}

/// Refuse to take over an object this MinecraftServer does not own. Server-side apply with
/// `force` would otherwise silently adopt e.g. an unrelated Service or Secret that happens to
/// share the name.
async fn ensure_ours<K>(api: &Api<K>, name: &str, mcs: &MinecraftServer) -> Result<Option<K>>
where
    K: Resource<DynamicType = ()> + Clone + DeserializeOwned + std::fmt::Debug,
{
    let Some(obj) = api.get_opt(name).await? else { return Ok(None) };
    let uid = mcs.uid().unwrap_or_default();
    if !obj.owner_references().iter().any(|o| o.uid == uid) {
        anyhow::bail!("{} {name} already exists and is not owned by this server; refusing to take it over", K::kind(&()));
    }
    Ok(Some(obj))
}

/// Delete objects of kind `K` this server created that are no longer wanted. Owner references
/// only clean up when the whole MinecraftServer is deleted.
async fn prune<K>(api: &Api<K>, mcs: &MinecraftServer, wanted: &[String]) -> Result<()>
where
    K: Resource<DynamicType = ()> + Clone + DeserializeOwned + std::fmt::Debug,
{
    let uid = mcs.uid().unwrap_or_default();
    let selector = format!("{}={},{}={}", labels::SERVER, mcs.name_any(), labels::MANAGED_BY, labels::MANAGER);
    for obj in api.list(&ListParams::default().labels(&selector)).await? {
        let name = obj.name_any();
        if stale(&obj, &uid, wanted) {
            tracing::info!("{}/{}: deleting stale {} {name}", mcs.namespace().unwrap_or_default(), mcs.name_any(), K::kind(&()));
            api.delete(&name, &Default::default()).await?;
        }
    }
    Ok(())
}

/// Owned by this server (by UID, so a same-named server's leftovers are never touched) and not wanted.
fn stale<K: Resource>(obj: &K, uid: &str, wanted: &[String]) -> bool {
    let owned = !uid.is_empty() && obj.owner_references().iter().any(|o| o.uid == uid);
    owned && !wanted.iter().any(|w| Some(w.as_str()) == obj.meta().name.as_deref())
}

async fn reconcile_inner(mcs: &MinecraftServer, ctx: &Ctx, ns: &str, name: &str, status: &mut MinecraftServerStatus) -> Result<Action> {
    let client = &ctx.client;
    let cfg = &ctx.cfg;
    let pp = PatchParams::apply(labels::FIELD_MANAGER).force();
    cubeharbor_core::crd::check_server_name(name).map_err(anyhow::Error::msg)?;
    mcs.spec.check_ports().map_err(anyhow::Error::msg)?;

    // 1. Resolve versions (network) only when the relevant spec changed.
    let spec = &mcs.spec;
    let resolve_hash = hash_json(&(&spec.version, spec.loader, &spec.loader_version, &spec.modpack, spec.jvm.java_version));
    if status.resolved.is_none() || status.resolved_hash.as_deref() != Some(resolve_hash.as_str()) {
        tracing::info!("{ns}/{name}: resolving versions");
        let resolved = resolve::resolve(&ctx.http, spec).await.context("resolving server version")?;
        tracing::info!(
            "{ns}/{name}: Minecraft {} / {} / Java {}{}",
            resolved.minecraft_version,
            resolved.loader,
            resolved.java_version,
            resolved.modpack.as_ref().map(|m| format!(" / {} {}", m.name, m.version)).unwrap_or_default()
        );
        status.resolved = Some(resolved);
        status.resolved_hash = Some(resolve_hash);
    }
    let resolved = status.resolved.clone().unwrap();
    if let Some(img) = &spec.image {
        cfg.image_allowed(img).map_err(anyhow::Error::msg)?;
    }

    // 2. Wake requests from the gateway.
    let mcs_api: Api<MinecraftServer> = Api::namespaced(client.clone(), ns);
    if mcs.annotations().contains_key(labels::WAKE_ANNOTATION) {
        if status.sleeping {
            tracing::info!("{ns}/{name}: waking up (player joined)");
        }
        status.sleeping = false;
        status.last_active = Some(now());
        let patch = json!({ "metadata": { "annotations": { labels::WAKE_ANNOTATION: null } } });
        mcs_api.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
    }

    // 3. Public ports, and routing conflicts (same cluster-wide list).
    let mut routing_conflict = None;
    let mut is_default = spec.routing.default;
    {
        let _guard = ctx.alloc_lock.lock().await;
        let all: Api<MinecraftServer> = Api::all(client.clone());
        let servers = all.list(&ListParams::default()).await?;
        // Two servers on one hostname (or two defaults) would be routed to at random: the
        // newer one gives up the contested hostnames and reports why.
        // The server keeps running (pause, sleep and wake still apply); the conflict is reported
        // in the status message, not as an Error phase, which the gateway would treat as down.
        let conflicts = cubeharbor_core::crd::routing_conflicts(mcs, &status.hostnames, &servers.items);
        if !conflicts.is_empty() {
            status.hostnames.retain(|h| !conflicts.hostnames.iter().any(|(c, _)| c == h));
            is_default &= conflicts.default.is_none();
            tracing::warn!("{ns}/{name}: {}", conflicts.message());
            routing_conflict = Some(conflicts.message());
        }
        let mut used = Used::new();
        for other in servers {
            if other.namespace().as_deref() == Some(ns) && other.name_any() == name {
                continue;
            }
            let owner = format!("{}/{}", other.namespace().unwrap_or_default(), other.name_any());
            for a in other.status.iter().flat_map(|s| &s.allocations) {
                used.insert((a.public_port, a.protocol), owner.clone());
            }
        }
        status.allocations = ports::allocate(name, &spec.port_requests(), &status.allocations, &used, cfg.port_range())?;
        // Persist right away so concurrent reconciles see the allocation.
        mcs_api
            .patch_status(name, &PatchParams::default(), &Patch::Merge(json!({ "status": { "allocations": status.allocations } })))
            .await?;
    }

    // 4. Secret (RCON password survives re-applies).
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let existing_pw = ensure_ours(&secrets, &resources::secret_name(mcs), mcs)
        .await?
        .and_then(|s| s.data)
        .and_then(|d| d.get("RCON_PASSWORD").map(|b| String::from_utf8_lossy(&b.0).to_string()));
    let pw = match existing_pw {
        Some(pw) => pw,
        None => random_password()?,
    };
    let secret = resources::secret(mcs, &pw, cfg.curseforge_api_key.as_deref())?;
    secrets.patch(&resources::secret_name(mcs), &pp, &Patch::Apply(&secret)).await?;

    // 5. Install plan.
    let plan = resources::plan(mcs, &resolved, &status.allocations, cfg);
    let image = spec.image.clone().unwrap_or_else(|| cfg.image_for(resolved.java_version));
    let plan_hash = resources::hash_plan(&plan, &image);
    let cm = resources::plan_configmap(mcs, &plan)?;
    let cm_api = Api::<ConfigMap>::namespaced(client.clone(), ns);
    ensure_ours(&cm_api, &format!("{name}-plan"), mcs).await?;
    cm_api.patch(&format!("{name}-plan"), &pp, &Patch::Apply(&cm)).await?;

    // 6. Service.
    let svc = resources::service(mcs, &status.hostnames, is_default, cfg)?;
    let svc_api = Api::<Service>::namespaced(client.clone(), ns);
    ensure_ours(&svc_api, name, mcs).await?;
    svc_api.patch(name, &pp, &Patch::Apply(&svc)).await?;
    let np_api = Api::<NetworkPolicy>::namespaced(client.clone(), ns);
    let mut wanted_policies = vec![];
    if cfg.rcon_network_policy {
        let np_name = resources::network_policy_name(mcs);
        ensure_ours(&np_api, &np_name, mcs).await?;
        np_api.patch(&np_name, &pp, &Patch::Apply(&resources::network_policy(mcs, cfg)?)).await?;
        wanted_policies.push(np_name);
    }
    prune(&np_api, mcs, &wanted_policies).await?;

    // 7. mc-router's auto-scale-up wakes a sleeping server by scaling its StatefulSet to 1
    //    (we applied 0 before marking it asleep, so anything above 0 is a join attempt).
    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
    let existing_sts = ensure_ours(&sts_api, name, mcs).await?;
    if cfg.mc_router() && status.sleeping && !spec.paused {
        let replicas = existing_sts.as_ref().and_then(|s| s.spec.replicas).unwrap_or(0);
        if replicas > 0 {
            tracing::info!("{ns}/{name}: waking up (mc-router scaled it up for a joining player)");
            status.sleeping = false;
            status.last_active = Some(now());
        }
    }

    // 8. Observe the current pod, players and idle time, then decide whether it should run.
    let pod = Api::<Pod>::namespaced(client.clone(), ns).get_opt(&format!("{name}-0")).await?;
    let (phase, message) = pod_phase(pod.as_ref());
    if phase == Phase::Running {
        match protocol::ping(&format!("{name}.{ns}.svc:{MINECRAFT_PORT}"), &format!("{name}.{ns}.svc"), MINECRAFT_PORT).await {
            Ok(s) => {
                let p = s.players.unwrap_or_default();
                if p.online > 0 || status.last_active.is_none() {
                    status.last_active = Some(now());
                }
                status.players = Some(Players { online: p.online, max: p.max, sample: p.sample.into_iter().map(|s| s.name).collect() });
            }
            Err(e) => tracing::debug!("{ns}/{name}: ping failed: {e:#}"),
        }
        if let (Some(sleep), Some(last)) = (&spec.auto_sleep, &status.last_active) {
            let idle = minutes_since(last).unwrap_or(0);
            let online = status.players.as_ref().map_or(0, |p| p.online);
            if online == 0 && idle >= sleep.idle_minutes as i64 {
                tracing::info!("{ns}/{name}: idle for {idle} min, going to sleep");
                status.sleeping = true;
            }
        }
    } else if phase != Phase::Starting && phase != Phase::Installing {
        status.players = None;
    }
    if spec.paused || spec.auto_sleep.is_none() {
        status.sleeping = false;
    }
    let running = !spec.paused && !status.sleeping;

    // 9. StatefulSet (volume claim templates are immutable, so keep whatever exists).
    let existing_claims = existing_sts
        .and_then(|s| s.spec.volume_claim_templates)
        .map(|c| {
            // Strip server-populated fields so the apply stays identical.
            let mut v = serde_json::to_value(c).unwrap_or_default();
            for claim in v.as_array_mut().into_iter().flatten() {
                if let Some(o) = claim.as_object_mut() {
                    o.remove("status");
                    o.remove("apiVersion");
                    o.remove("kind");
                }
            }
            v
        });
    let sts = resources::statefulset(
        mcs,
        PodInputs { replicas: if running { 1 } else { 0 }, image, plan_hash, existing_claims, cfg },
    )?;
    sts_api.patch(name, &pp, &Patch::Apply(&sts)).await?;

    // 10. Ingresses for web ports.
    let ing_api: Api<Ingress> = Api::namespaced(client.clone(), ns);
    let mut wanted_ingresses = vec![];
    for ing in resources::ingresses(mcs, cfg)? {
        ensure_ours(&ing_api, &ing.name_any(), mcs).await?;
        ing_api.patch(&ing.name_any(), &pp, &Patch::Apply(&ing)).await?;
        wanted_ingresses.push(ing.name_any());
    }
    // Ports that were removed, renamed or moved to another host leave their Ingress behind.
    prune(&ing_api, mcs, &wanted_ingresses).await?;

    // 11. Status.
    (status.phase, status.message) = if spec.paused {
        (Phase::Stopped, None)
    } else if status.sleeping {
        (Phase::Sleeping, Some("sleeping until a player joins".into()))
    } else {
        (phase, message)
    };
    if let Some(c) = routing_conflict {
        status.message = Some(match status.message.take() {
            Some(m) => format!("{c}; {m}"),
            None => c,
        });
    }
    let requeue = match status.phase {
        Phase::Running => 30,
        Phase::Installing | Phase::Starting | Phase::Pending => 10,
        _ => 60,
    };
    Ok(Action::requeue(Duration::from_secs(requeue)))
}

/// Derive a phase from the server pod.
fn pod_phase(pod: Option<&Pod>) -> (Phase, Option<String>) {
    let Some(pod) = pod else { return (Phase::Pending, Some("waiting for pod".into())) };
    if pod.metadata.deletion_timestamp.is_some() {
        return (Phase::Pending, Some("restarting".into()));
    }
    let Some(st) = &pod.status else { return (Phase::Pending, None) };
    let waiting_error = |cs: &k8s_openapi::api::core::v1::ContainerStatus| -> Option<String> {
        let w = cs.state.as_ref()?.waiting.as_ref()?;
        let reason = w.reason.clone().unwrap_or_default();
        let msg = w.message.clone().unwrap_or_default();
        match reason.as_str() {
            "ErrImagePull" | "ImagePullBackOff" | "InvalidImageName" => Some(format!("cannot pull image {}: {msg}", cs.image)),
            "CreateContainerConfigError" => Some(format!("{}: {msg}", cs.name)),
            "CrashLoopBackOff" if cs.name == "install" => Some("install failed (see the install container logs)".into()),
            "CrashLoopBackOff" => Some("server keeps crashing (see the logs)".into()),
            _ => None,
        }
    };
    for cs in st.init_container_statuses.iter().flatten() {
        if let Some(e) = waiting_error(cs) {
            return (Phase::Error, Some(e));
        }
        let state = cs.state.as_ref();
        if state.and_then(|s| s.running.as_ref()).is_some() {
            return (Phase::Installing, Some("installing server and mods".into()));
        }
        if let Some(t) = state.and_then(|s| s.terminated.as_ref()) {
            if t.exit_code != 0 {
                return (Phase::Error, Some("install failed (see the install container logs)".into()));
            }
        } else {
            return (Phase::Installing, None);
        }
    }
    for cs in st.container_statuses.iter().flatten() {
        if let Some(e) = waiting_error(cs) {
            return (Phase::Error, Some(e));
        }
        if cs.ready {
            return (Phase::Running, None);
        }
        if cs.state.as_ref().and_then(|s| s.running.as_ref()).is_some() {
            return (Phase::Starting, Some("server is starting".into()));
        }
    }
    (Phase::Pending, st.phase.clone())
}

/// 144 random bits from the kernel. There is no fallback: a guessable RCON password is worse
/// than a failed reconcile.
fn random_password() -> Result<String> {
    use std::io::Read;
    let mut buf = [0u8; 18];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).context("reading /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn error_policy(_mcs: Arc<MinecraftServer>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    tracing::warn!("reconcile error: {err}");
    Action::requeue(Duration::from_secs(30))
}

/// One Service port per allocated (port, protocol).
fn gateway_service_ports<'a>(allocations: impl Iterator<Item = &'a PortAllocation>) -> Vec<serde_json::Value> {
    let mut seen = std::collections::BTreeSet::new();
    let mut ports = vec![];
    for a in allocations {
        if seen.insert((a.public_port, a.protocol)) {
            let proto = a.protocol.as_str();
            ports.push(json!({
                "name": format!("p{}-{}", a.public_port, &proto[..1].to_ascii_lowercase()),
                "port": a.public_port,
                "targetPort": a.public_port,
                "protocol": proto,
            }));
        }
    }
    ports
}

fn placeholder_port(port: u16) -> serde_json::Value {
    json!({ "name": "placeholder", "port": port, "targetPort": port, "protocol": "TCP" })
}

/// Keep the gateway's extra-ports Service in sync with all allocations.
pub async fn sync_gateway_ports(ctx: &Ctx) -> Result<()> {
    let cfg = &ctx.cfg;
    let all: Api<MinecraftServer> = Api::all(ctx.client.clone());
    let servers = all.list(&ListParams::default()).await?;
    let allocations = servers.iter().flat_map(|m| m.status.iter().flat_map(|s| &s.allocations));
    let mut ports = gateway_service_ports(allocations);
    let api: Api<Service> = Api::namespaced(ctx.client.clone(), &cfg.gateway_namespace);
    if ports.is_empty() {
        // A Service needs at least one port. Rather than deleting it (and losing the load
        // balancer address players and DNS records point at), keep it with a placeholder port
        // nothing listens on until ports are allocated again.
        if api.get_opt(&cfg.gateway_ports_service).await?.is_none() {
            return Ok(());
        }
        ports = vec![placeholder_port(cfg.port_range().0)];
    }
    let annotations: serde_json::Value = serde_json::from_str(&cfg.gateway_service_annotations).unwrap_or(json!({}));
    let mut spec = json!({
        "type": cfg.gateway_service_type,
        "selector": cfg.gateway_selector(),
        "ports": ports,
    });
    if cfg.gateway_service_type != "ClusterIP" {
        spec["externalTrafficPolicy"] = json!(cfg.gateway_external_traffic_policy);
    }
    // Without a load balancer the public port *is* the node port, so players reach the port
    // that was configured into the voice chat mod. The port range must then lie inside the
    // cluster's node port range.
    if cfg.gateway_service_type == "NodePort" {
        for p in spec["ports"].as_array_mut().into_iter().flatten() {
            p["nodePort"] = p["port"].clone();
        }
    }
    if !cfg.gateway_ip_family_policy.is_empty() {
        spec["ipFamilyPolicy"] = json!(cfg.gateway_ip_family_policy);
    }
    let families = cfg.gateway_ip_families();
    if !families.is_empty() {
        spec["ipFamilies"] = json!(families);
    }
    let svc: Service = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {
            "name": cfg.gateway_ports_service,
            "namespace": cfg.gateway_namespace,
            "labels": { labels::MANAGED_BY: labels::MANAGER, labels::COMPONENT: "gateway" },
            "annotations": annotations,
        },
        "spec": spec,
    }))?;
    api.patch(&cfg.gateway_ports_service, &PatchParams::apply(labels::FIELD_MANAGER).force(), &Patch::Apply(&svc)).await?;
    Ok(())
}

#[cfg(test)]
mod gateway_ports_tests {
    use super::*;
    use cubeharbor_core::crd::Protocol;

    fn alloc(port: u16, protocol: Protocol) -> PortAllocation {
        PortAllocation { name: "x".into(), protocol, container_port: 1, public_port: port }
    }

    #[test]
    fn one_service_port_per_port_and_protocol() {
        let allocs = [alloc(25600, Protocol::Udp), alloc(25600, Protocol::Tcp), alloc(25600, Protocol::Udp)];
        let ports = gateway_service_ports(allocs.iter());
        let names: Vec<&str> = ports.iter().map(|p| p["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["p25600-u", "p25600-t"]);
        assert!(gateway_service_ports([].iter()).is_empty());
        assert_eq!(placeholder_port(25600)["protocol"], "TCP");
    }
}

#[cfg(test)]
mod prune_tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

    fn ingress(name: &str, owner_uid: Option<&str>) -> Ingress {
        let mut i = Ingress::default();
        i.metadata.name = Some(name.into());
        i.metadata.owner_references = owner_uid.map(|uid| vec![OwnerReference { uid: uid.into(), ..Default::default() }]);
        i
    }

    #[test]
    fn only_unwanted_owned_objects_are_stale() {
        let wanted = vec!["atm-bluemap".to_string()];
        assert!(!stale(&ingress("atm-bluemap", Some("u1")), "u1", &wanted));
        assert!(stale(&ingress("atm-dynmap", Some("u1")), "u1", &wanted), "removed port");
        assert!(!stale(&ingress("atm-dynmap", Some("u2")), "u1", &wanted), "owned by another server");
        assert!(!stale(&ingress("atm-dynmap", None), "u1", &wanted), "not ours");
        assert!(!stale(&ingress("atm-dynmap", Some("")), "", &wanted), "no UID yet");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cubeharbor_core::crd::{PortAllocation, Protocol, Resolved};

    #[test]
    fn status_patch_clears_empty_fields() {
        let v = status_patch(&MinecraftServerStatus::default()).unwrap();
        for field in CLEARABLE_STATUS_FIELDS {
            assert_eq!(v[field], serde_json::Value::Null, "{field} should be sent as null");
        }
        assert_eq!(v["phase"], "Pending");
    }

    /// A new optional status field must be added to CLEARABLE_STATUS_FIELDS, or it can never be
    /// cleared. The field list comes from the CRD schema, so a field nobody remembered still counts.
    #[test]
    fn every_optional_status_field_is_clearable() {
        use kube::CustomResourceExt;
        let crd = serde_json::to_value(MinecraftServer::crd()).unwrap();
        let fields = crd
            .pointer("/spec/versions/0/schema/openAPIV3Schema/properties/status/properties")
            .and_then(|p| p.as_object())
            .expect("status properties in the CRD schema");
        let always = serde_json::to_value(MinecraftServerStatus::default()).unwrap();
        for key in fields.keys() {
            assert!(
                always.get(key).is_some() || CLEARABLE_STATUS_FIELDS.contains(&key.as_str()),
                "status field {key} is skipped when empty but not in CLEARABLE_STATUS_FIELDS"
            );
        }

        let full = MinecraftServerStatus {
            message: Some("m".into()),
            observed_generation: Some(1),
            resolved: Some(Resolved::default()),
            resolved_hash: Some("h".into()),
            hostnames: vec!["a.test".into()],
            service: Some("s".into()),
            allocations: vec![PortAllocation { name: "voice".into(), protocol: Protocol::Udp, container_port: 1, public_port: 2 }],
            players: Some(Players::default()),
            last_active: Some("t".into()),
            ..Default::default()
        };
        let full = serde_json::to_value(&full).unwrap();
        // A populated status passes through unchanged.
        assert_eq!(status_patch(&serde_json::from_value(full.clone()).unwrap()).unwrap(), full);
    }
}
