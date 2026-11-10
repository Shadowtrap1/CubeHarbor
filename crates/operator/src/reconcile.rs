use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use cubeharbor_core::crd::{
    Exposure, MINECRAFT_PORT, MinecraftServer, MinecraftServerSpec, MinecraftServerStatus, Phase, Players, PortAllocation, Resolved, effective_hostnames,
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
use kube::runtime::events::Recorder;
use kube::runtime::reflector::Store;
use kube::{Client, Resource, ResourceExt};
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::sync::{Mutex, Notify};

use crate::conditions;
use crate::config::Config;
use crate::ports::{self, Used};
use crate::resources::{self, PodInputs};
use crate::server_conditions::{self, Observed};

pub struct Ctx {
    pub client: Client,
    pub http: Http,
    pub cfg: Config,
    /// Serialises allocation; holds what this process wrote until the cache has caught up.
    pub allocations: Mutex<ports::Ledger>,
    /// Every MinecraftServer in the cluster, also with a watch namespace: ports are cluster-wide.
    pub fleet: Store<MinecraftServer>,
    /// The gateway ports Service spec last applied, to skip identical writes.
    pub gateway_applied: Mutex<Option<serde_json::Value>>,
    pub gateway_sync: Notify,
    pub stats: Arc<crate::metrics::Stats>,
    pub recorder: Recorder,
    /// When a failed reconcile last wrote each server's status, by UID.
    pub failure_writes: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

/// Minimum spacing of status writes that only reword a failure message (the failure requeue).
const FAILURE_REWRITE_INTERVAL: Duration = Duration::from_secs(55);

impl Ctx {
    fn recently_wrote_failure(&self, uid: &str) -> bool {
        let writes = self.failure_writes.lock().unwrap_or_else(|e| e.into_inner());
        writes.get(uid).is_some_and(|t| t.elapsed() < FAILURE_REWRITE_INTERVAL)
    }

    fn wrote_failure(&self, uid: String) {
        let mut writes = self.failure_writes.lock().unwrap_or_else(|e| e.into_inner());
        writes.retain(|_, t| t.elapsed() < FAILURE_REWRITE_INTERVAL);
        writes.insert(uid, std::time::Instant::now());
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0:#}")]
pub struct Error(#[from] anyhow::Error);

/// A spec the operator refuses. Retrying can't help until the object (or the operator's own
/// settings, which means a restart and so a fresh reconcile) changes.
#[derive(Debug, thiserror::Error)]
#[error("invalid spec: {0}")]
struct InvalidSpec(String);

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

    let previous = mcs.status.clone();
    let started = std::time::Instant::now();
    let result = reconcile_inner(&mcs, &ctx, &ns, &name, &mut status).await;
    ctx.stats.reconciled(started.elapsed(), result.is_ok());
    let action = match result {
        Ok(action) => {
            write_status(&api, &name, previous.as_ref(), &status).await?;
            ctx.gateway_sync.notify_one();
            action
        }
        Err(e) => {
            tracing::warn!("{ns}/{name}: {e:#}");
            let message = format!("{e:#}");
            status.phase = phase_after_failure(status.phase);
            status.message = Some(message.clone());
            // A status write comes back as a watch event, so an unchanged failure must not write.
            let (reason, action) = match e.downcast_ref::<InvalidSpec>() {
                Some(_) => (server_conditions::REASON_INVALID_SPEC, Action::await_change()),
                None => (server_conditions::REASON_RECONCILE_FAILED, Action::requeue(Duration::from_secs(60))),
            };
            server_conditions::on_failure(&mut status, mcs.metadata.generation, &conditions::now(), reason, &message);
            // A reworded message alone is written at most once per retry interval; otherwise a
            // message that changes per attempt would retry on every echo of its own write.
            let uid = mcs.uid().unwrap_or_default();
            let reworded = server_conditions::only_messages_differ(previous.as_ref(), &status);
            let quiet = reworded && ctx.recently_wrote_failure(&uid);
            if !quiet {
                write_status(&api, &name, previous.as_ref(), &status).await?;
                ctx.wrote_failure(uid);
            }
            action
        }
    };
    let before = previous.map(|s| s.conditions).unwrap_or_default();
    conditions::publish(&ctx.recorder, &mcs.object_ref(&()), &server_conditions::transitions(&before, &status.conditions)).await;
    Ok(action)
}

/// Everything about a spec that can be refused without talking to anything. Each check here was
/// already a failure further down; finding it first stops the retries and the network calls.
fn validate(name: &str, spec: &MinecraftServerSpec, cfg: &Config) -> Result<(), String> {
    cubeharbor_core::crd::check_server_name(name)?;
    spec.check_ports()?;
    if let Some(img) = &spec.image {
        cfg.image_allowed(img)?;
    }
    if let Some(m) = &spec.modpack
        && m.modrinth.is_none()
        && m.curseforge.is_none()
        && m.url.is_none()
    {
        return Err("spec.modpack needs one of: modrinth, curseforge, url".into());
    }
    let requests = spec.port_requests();
    let mut pinned = std::collections::BTreeSet::new();
    for (port, protocol) in requests.iter().filter_map(|r| Some((r.public_port?, r.protocol))) {
        if !pinned.insert((port, protocol)) {
            return Err(format!("public port {port}/{} is requested twice", protocol.as_str()));
        }
    }
    for p in spec.extra_ports.iter().filter(|p| p.expose == Exposure::Ingress) {
        if let Some(host) = &p.host {
            cfg.ingress_host_allowed(host).map_err(|e| format!("port {}: {e}", p.name))?;
        }
    }
    Ok(())
}

/// The phase to report when a reconcile fails. A failed reconcile (an API server hiccup, Modrinth
/// down while re-resolving a changed spec, a port conflict) doesn't stop a pod that is already
/// running, sleeping or stopped, so that phase is kept and the error goes into the message.
/// Reporting `Error` instead made the gateway turn players away from a healthy server (and stop
/// waking a sleeping one) until the next successful reconcile.
fn phase_after_failure(previous: Phase) -> Phase {
    match previous {
        Phase::Running | Phase::Sleeping | Phase::Stopped => previous,
        _ => Phase::Error,
    }
}

/// Upper bound for resolving versions (several Modrinth/Mojang/loader requests). The HTTP
/// client's own timeout is sized for multi-GB downloads, far too long to hold up a reconcile.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(120);

/// This server's port allocations as last persisted. The object handed to the reconciler can be
/// older than the allocation this operator just wrote; allocating from that stale copy could move
/// the server to a different port.
fn own_allocations(servers: &[Arc<MinecraftServer>], ledger: &ports::Ledger, uid: Option<&str>, ns: &str, name: &str, cached: &[PortAllocation]) -> Vec<PortAllocation> {
    if let Some(w) = uid.and_then(|u| ledger.get(u)) {
        return w.allocations.clone();
    }
    servers
        .iter()
        .find(|s| s.namespace().as_deref() == Some(ns) && s.name_any() == name)
        .map(|s| ports::allocations_of(ledger, s).to_vec())
        .unwrap_or_else(|| cached.to_vec())
}

/// Write the status unless it is what the object already has.
async fn write_status(api: &Api<MinecraftServer>, name: &str, previous: Option<&MinecraftServerStatus>, status: &MinecraftServerStatus) -> Result<(), Error> {
    if !server_conditions::status_changed(previous, status) {
        return Ok(());
    }
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(json!({ "status": status_patch(status)? })))
        .await
        .context("patching status")?;
    Ok(())
}

/// Status fields that serialization leaves out when empty.
const CLEARABLE_STATUS_FIELDS: &[&str] =
    &["message", "observedGeneration", "resolved", "resolvedHash", "hostnames", "service", "allocations", "players", "lastActive", "conditions"];

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

/// One reconcile of one server. Each step is a method, so adding behaviour means adding a step to
/// [`reconcile_inner`] rather than growing it.
struct Run<'a> {
    mcs: &'a MinecraftServer,
    ctx: &'a Ctx,
    ns: &'a str,
    name: &'a str,
    pp: PatchParams,
}

/// The outcome of allocating ports and hostnames.
struct Routing {
    conflict: Option<String>,
    is_default: bool,
}

/// What the StatefulSet is built from.
struct Install {
    image: String,
    plan_hash: String,
}

async fn reconcile_inner(mcs: &MinecraftServer, ctx: &Ctx, ns: &str, name: &str, status: &mut MinecraftServerStatus) -> Result<Action> {
    validate(name, &mcs.spec, &ctx.cfg).map_err(InvalidSpec)?;
    let run = Run { mcs, ctx, ns, name, pp: PatchParams::apply(labels::FIELD_MANAGER).force() };

    let resolved = run.resolve_versions(status).await?;
    run.handle_wake_request(status).await?;
    let routing = run.assign_ports(status).await?;
    run.apply_secret().await?;
    let install = run.apply_plan(status, &resolved).await?;
    run.apply_network(status, routing.is_default).await?;
    let existing_sts = run.existing_statefulset(status).await?;
    let pod = run.get_pod().await?;
    let (phase, message) = run.observe(pod.as_ref(), status).await;
    run.apply_statefulset(status, install, existing_sts).await?;
    run.apply_ingresses().await?;
    Ok(run.finish(status, phase, message, pod.as_ref(), routing))
}

impl Run<'_> {
    fn api<K>(&self) -> Api<K>
    where
        K: Resource<Scope = k8s_openapi::NamespaceResourceScope, DynamicType = ()>,
    {
        Api::namespaced(self.ctx.client.clone(), self.ns)
    }

    /// Resolve versions (network), only when the relevant spec changed.
    async fn resolve_versions(&self, status: &mut MinecraftServerStatus) -> Result<Resolved> {
        let (spec, ns, name) = (&self.mcs.spec, self.ns, self.name);
        let resolve_hash = hash_json(&(&spec.version, spec.loader, &spec.loader_version, &spec.modpack, spec.jvm.java_version));
        if status.resolved.is_none() || status.resolved_hash.as_deref() != Some(resolve_hash.as_str()) {
            tracing::info!("{ns}/{name}: resolving versions");
            let resolved = tokio::time::timeout(RESOLVE_TIMEOUT, resolve::resolve(&self.ctx.http, spec))
                .await
                .map_err(|_| anyhow::anyhow!("timed out after {}s", RESOLVE_TIMEOUT.as_secs()))
                .and_then(|r| r)
                .context("resolving server version")?;
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
        Ok(status.resolved.clone().unwrap())
    }

    /// Wake requests from the gateway.
    async fn handle_wake_request(&self, status: &mut MinecraftServerStatus) -> Result<()> {
        if self.mcs.annotations().contains_key(labels::WAKE_ANNOTATION) {
            if status.sleeping {
                tracing::info!("{}/{}: waking up (player joined)", self.ns, self.name);
            }
            status.sleeping = false;
            status.last_active = Some(now());
            let patch = json!({ "metadata": { "annotations": { labels::WAKE_ANNOTATION: null } } });
            self.api::<MinecraftServer>().patch(self.name, &PatchParams::default(), &Patch::Merge(&patch)).await?;
        }
        Ok(())
    }

    /// Public ports, and routing conflicts (from the same view of every server).
    async fn assign_ports(&self, status: &mut MinecraftServerStatus) -> Result<Routing> {
        let (mcs, ctx, ns, name) = (self.mcs, self.ctx, self.ns, self.name);
        // An unfilled cache would make every port look free.
        ctx.fleet.wait_until_ready().await.context("waiting for the MinecraftServer cache")?;
        let mut ledger = ctx.allocations.lock().await;
        let servers = ctx.fleet.state();
        ports::settle(&mut ledger, &servers);

        // Two servers on one hostname (or two defaults) would be routed to at random: the
        // newer one gives up the contested hostnames and reports why.
        // The server keeps running (pause, sleep and wake still apply); the conflict is reported
        // in the status message, not as an Error phase, which the gateway would treat as down.
        let mut routing = Routing { conflict: None, is_default: mcs.spec.routing.default };
        let conflicts = cubeharbor_core::crd::routing_conflicts(mcs, &status.hostnames, servers.iter().map(|s| s.as_ref()));
        if !conflicts.is_empty() {
            status.hostnames.retain(|h| !conflicts.hostnames.iter().any(|(c, _)| c == h));
            routing.is_default &= conflicts.default.is_none();
            tracing::warn!("{ns}/{name}: {}", conflicts.message());
            routing.conflict = Some(conflicts.message());
        }

        let uid = mcs.uid();
        let current = own_allocations(&servers, &ledger, uid.as_deref(), ns, name, &status.allocations);
        let mut used = Used::new();
        for other in &servers {
            if other.namespace().as_deref() == Some(ns) && other.name_any() == name {
                continue;
            }
            let owner = format!("{}/{}", other.namespace().unwrap_or_default(), other.name_any());
            for a in ports::allocations_of(&ledger, other) {
                used.insert((a.public_port, a.protocol), owner.clone());
            }
        }
        // Written by this process for servers the cache doesn't show yet (or any more).
        for (owner, a) in ports::absent_from_cache(&ledger, &servers, uid.as_deref()) {
            used.entry((a.public_port, a.protocol)).or_insert_with(|| format!("server {owner}"));
        }
        status.allocations = ports::allocate(name, &mcs.spec.port_requests(), &current, &used, ctx.cfg.port_range())?;
        if status.allocations != current {
            // Persist right away, under the lock, so concurrent reconciles see the allocation.
            self.api::<MinecraftServer>()
                .patch_status(name, &PatchParams::default(), &Patch::Merge(json!({ "status": { "allocations": status.allocations } })))
                .await?;
            if let Some(uid) = uid {
                ledger.insert(uid, ports::Written::now(status.allocations.clone()));
            }
        }
        Ok(routing)
    }

    /// The Secret (the RCON password survives re-applies).
    async fn apply_secret(&self) -> Result<()> {
        let secrets = self.api::<Secret>();
        let name = resources::secret_name(self.mcs);
        let existing_pw = ensure_ours(&secrets, &name, self.mcs)
            .await?
            .and_then(|s| s.data)
            .and_then(|d| d.get("RCON_PASSWORD").map(|b| String::from_utf8_lossy(&b.0).to_string()));
        let pw = match existing_pw {
            Some(pw) => pw,
            None => random_password()?,
        };
        let secret = resources::secret(self.mcs, &pw, self.ctx.cfg.curseforge_api_key.as_deref())?;
        secrets.patch(&name, &self.pp, &Patch::Apply(&secret)).await?;
        Ok(())
    }

    /// The install plan.
    async fn apply_plan(&self, status: &MinecraftServerStatus, resolved: &Resolved) -> Result<Install> {
        let (mcs, cfg, name) = (self.mcs, &self.ctx.cfg, self.name);
        let plan = resources::plan(mcs, resolved, &status.allocations, cfg);
        let image = mcs.spec.image.clone().unwrap_or_else(|| cfg.image_for(resolved.java_version));
        let plan_hash = resources::hash_plan(&plan, &image);
        let cm = resources::plan_configmap(mcs, &plan)?;
        let cm_api = self.api::<ConfigMap>();
        ensure_ours(&cm_api, &format!("{name}-plan"), mcs).await?;
        cm_api.patch(&format!("{name}-plan"), &self.pp, &Patch::Apply(&cm)).await?;
        Ok(Install { image, plan_hash })
    }

    /// The Service and the RCON NetworkPolicy.
    async fn apply_network(&self, status: &MinecraftServerStatus, is_default: bool) -> Result<()> {
        let (mcs, cfg, name) = (self.mcs, &self.ctx.cfg, self.name);
        let svc = resources::service(mcs, &status.hostnames, is_default, cfg)?;
        let svc_api = self.api::<Service>();
        ensure_ours(&svc_api, name, mcs).await?;
        svc_api.patch(name, &self.pp, &Patch::Apply(&svc)).await?;
        let np_api = self.api::<NetworkPolicy>();
        let mut wanted_policies = vec![];
        if cfg.rcon_network_policy {
            let np_name = resources::network_policy_name(mcs);
            ensure_ours(&np_api, &np_name, mcs).await?;
            np_api.patch(&np_name, &self.pp, &Patch::Apply(&resources::network_policy(mcs, cfg)?)).await?;
            wanted_policies.push(np_name);
        }
        prune(&np_api, mcs, &wanted_policies).await
    }

    /// mc-router's auto-scale-up wakes a sleeping server by scaling its StatefulSet to 1 (we
    /// applied 0 before marking it asleep, so anything above 0 is a join attempt).
    async fn existing_statefulset(&self, status: &mut MinecraftServerStatus) -> Result<Option<StatefulSet>> {
        let existing = ensure_ours(&self.api::<StatefulSet>(), self.name, self.mcs).await?;
        if self.ctx.cfg.mc_router() && status.sleeping && !self.mcs.spec.paused {
            let replicas = existing.as_ref().and_then(|s| s.spec.replicas).unwrap_or(0);
            if replicas > 0 {
                tracing::info!("{}/{}: waking up (mc-router scaled it up for a joining player)", self.ns, self.name);
                status.sleeping = false;
                status.last_active = Some(now());
            }
        }
        Ok(existing)
    }

    async fn get_pod(&self) -> Result<Option<Pod>> {
        Ok(self.api::<Pod>().get_opt(&format!("{}-0", self.name)).await?)
    }

    /// Players and idle time from the current pod; decides whether the server should sleep.
    async fn observe(&self, pod: Option<&Pod>, status: &mut MinecraftServerStatus) -> (Phase, Option<String>) {
        let (spec, ns, name) = (&self.mcs.spec, self.ns, self.name);
        let (phase, message) = pod_phase(pod);
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
        (phase, message)
    }

    /// The StatefulSet (volume claim templates are immutable, so keep whatever exists).
    async fn apply_statefulset(&self, status: &MinecraftServerStatus, install: Install, existing: Option<StatefulSet>) -> Result<()> {
        let running = !self.mcs.spec.paused && !status.sleeping;
        let existing_claims = existing.and_then(|s| s.spec.volume_claim_templates).map(|c| {
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
        let inputs = PodInputs { replicas: if running { 1 } else { 0 }, image: install.image, plan_hash: install.plan_hash, existing_claims, cfg: &self.ctx.cfg };
        let sts = resources::statefulset(self.mcs, inputs)?;
        self.api::<StatefulSet>().patch(self.name, &self.pp, &Patch::Apply(&sts)).await?;
        Ok(())
    }

    /// Ingresses for web ports. Ports removed, renamed or moved to another host leave one behind.
    async fn apply_ingresses(&self) -> Result<()> {
        let ing_api = self.api::<Ingress>();
        let mut wanted = vec![];
        for ing in resources::ingresses(self.mcs, &self.ctx.cfg)? {
            ensure_ours(&ing_api, &ing.name_any(), self.mcs).await?;
            ing_api.patch(&ing.name_any(), &self.pp, &Patch::Apply(&ing)).await?;
            wanted.push(ing.name_any());
        }
        prune(&ing_api, self.mcs, &wanted).await
    }

    /// The final phase, message and conditions, and when to look again.
    fn finish(&self, status: &mut MinecraftServerStatus, phase: Phase, message: Option<String>, pod: Option<&Pod>, routing: Routing) -> Action {
        let spec = &self.mcs.spec;
        (status.phase, status.message) = if spec.paused {
            (Phase::Stopped, None)
        } else if status.sleeping {
            (Phase::Sleeping, Some("sleeping until a player joins".into()))
        } else {
            (phase, message)
        };
        let observed = Observed { install: server_conditions::install_state(pod), is_default: routing.is_default, routing_conflict: routing.conflict.as_deref() };
        server_conditions::on_success(status, self.mcs.metadata.generation, &conditions::now(), observed);
        if let Some(c) = routing.conflict {
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
        Action::requeue(Duration::from_secs(requeue))
    }
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

/// Keep the gateway's extra-ports Service in sync with all allocations. Unless `force` is set (the
/// periodic repair of outside edits), an unchanged Service is not written.
pub async fn sync_gateway_ports(ctx: &Ctx, force: bool) -> Result<()> {
    let cfg = &ctx.cfg;
    // An empty cache would look like "no ports" and drop every forwarded port.
    ctx.fleet.wait_until_ready().await.context("waiting for the MinecraftServer cache")?;
    let servers = ctx.fleet.state();
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
    let mut applied = ctx.gateway_applied.lock().await;
    if !force && applied.as_ref() == Some(&spec) {
        return Ok(());
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
        "spec": &spec,
    }))?;
    api.patch(&cfg.gateway_ports_service, &PatchParams::apply(labels::FIELD_MANAGER).force(), &Patch::Apply(&svc)).await?;
    *applied = Some(spec);
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
    fn failures_keep_a_running_server_routable() {
        for p in [Phase::Running, Phase::Sleeping, Phase::Stopped] {
            assert_eq!(phase_after_failure(p), p);
        }
        for p in [Phase::Pending, Phase::Installing, Phase::Starting, Phase::Error] {
            assert_eq!(phase_after_failure(p), Phase::Error);
        }
    }

    #[test]
    fn invalid_spec_survives_the_error_chain() {
        let e = anyhow::Error::new(InvalidSpec("nope".into())).context("outer");
        assert!(e.downcast_ref::<InvalidSpec>().is_some());
    }

    #[test]
    fn allocations_come_from_the_fleet_and_the_ledger() {
        use cubeharbor_core::crd::MinecraftServerSpec;
        let alloc = |port| PortAllocation { name: "voice".into(), protocol: Protocol::Udp, container_port: 24454, public_port: port };
        let server = |ns: &str, name: &str, port: u16| {
            let mut s = MinecraftServer::new(name, MinecraftServerSpec::default());
            s.metadata.namespace = Some(ns.into());
            s.metadata.uid = Some(format!("{ns}-{name}"));
            s.status = Some(MinecraftServerStatus { allocations: vec![alloc(port)], ..Default::default() });
            Arc::new(s)
        };
        let listed = vec![server("mc", "lobby", 25601), server("mc", "atm", 25602), server("other", "atm", 25603)];
        let mut ledger = ports::Ledger::new();
        // The object handed to the reconcile has no allocation yet; the fleet has 25602.
        assert_eq!(own_allocations(&listed, &ledger, Some("mc-atm"), "mc", "atm", &[]), vec![alloc(25602)]);
        assert_eq!(own_allocations(&listed, &ledger, Some("other-atm"), "other", "atm", &[]), vec![alloc(25603)]);
        // Just written, not yet seen by the cache.
        ledger.insert("mc-atm".into(), ports::Written::now(vec![alloc(25610)]));
        assert_eq!(own_allocations(&listed, &ledger, Some("mc-atm"), "mc", "atm", &[]), vec![alloc(25610)]);
        // Not in the fleet (yet): fall back to the cached status...
        assert_eq!(own_allocations(&listed, &ledger, Some("mc-new"), "mc", "new", &[alloc(25604)]), vec![alloc(25604)]);
        // ...unless this process already wrote its allocation.
        ledger.insert("mc-new".into(), ports::Written::now(vec![alloc(25611)]));
        assert_eq!(own_allocations(&listed, &ledger, Some("mc-new"), "mc", "new", &[alloc(25604)]), vec![alloc(25611)]);
    }

    #[test]
    fn validate_refuses_bad_specs_and_passes_good_ones() {
        use clap::Parser;
        use cubeharbor_core::crd::{ExtraPort, ModpackSource};
        let cfg = Config::parse_from(["op"]);
        assert_eq!(validate("lobby", &MinecraftServerSpec::default(), &cfg), Ok(()));
        assert!(validate("Bad_Name", &MinecraftServerSpec::default(), &cfg).is_err());

        let spec = MinecraftServerSpec { modpack: Some(ModpackSource::default()), ..Default::default() };
        assert!(validate("lobby", &spec, &cfg).unwrap_err().contains("spec.modpack"));

        let port = |name: &str, container_port, public_port| ExtraPort {
            name: name.into(),
            container_port,
            protocol: Protocol::Udp,
            expose: Exposure::Gateway,
            public_port,
            host: None,
        };
        let spec = MinecraftServerSpec { extra_ports: vec![port("a", 8100, Some(25600)), port("b", 8101, Some(25600))], ..Default::default() };
        assert!(validate("lobby", &spec, &cfg).unwrap_err().contains("25600"));
        let spec = MinecraftServerSpec { extra_ports: vec![port("a", 8100, Some(25600)), port("b", 8101, Some(25601))], ..Default::default() };
        assert_eq!(validate("lobby", &spec, &cfg), Ok(()));

        let spec = MinecraftServerSpec { image: Some("evil/image".into()), ..Default::default() };
        assert!(validate("lobby", &spec, &cfg).is_err(), "image overrides are off by default");
    }

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
            conditions: vec![k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition {
                last_transition_time: conditions::now(),
                message: String::new(),
                observed_generation: None,
                reason: "R".into(),
                status: "True".into(),
                type_: "T".into(),
            }],
            ..Default::default()
        };
        let full = serde_json::to_value(&full).unwrap();
        // A populated status passes through unchanged.
        assert_eq!(status_patch(&serde_json::from_value(full.clone()).unwrap()).unwrap(), full);
    }
}
