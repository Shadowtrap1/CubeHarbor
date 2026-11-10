//! Applies a server's `spec.templateRef` before reconciling it, and paces template rollouts.
//!
//! The template is read straight from the API server on each reconcile rather than watched: every
//! server already requeues within 60 seconds, a read is cheap next to the cluster-wide server list
//! each reconcile does, and the operator needs nothing beyond `get` on templates.
//!
//! A template edit changes the merged spec of every server that inherits the edited fields, and
//! each of those would restart within a minute. Instead, a running server only picks up a new
//! template revision when one of the template's rollout slots
//! (`TEMPLATE_ROLLOUT_MAX_UNAVAILABLE`, default 1) is free. A server holds its slot from the
//! restart until its StatefulSet has rolled out and the pod is ready again; until then the
//! others keep running unchanged. A server that never comes back holds its slot, which halts the
//! rollout instead of breaking every server. Servers that aren't running (paused, asleep, still
//! installing), servers that just started using the template and servers whose own spec changed
//! apply the template right away: none of that restarts a healthy server because of a template.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use cubeharbor_core::MinecraftServer;
use cubeharbor_core::crd::{MinecraftServerStatus, Phase};
use cubeharbor_core::plan::hash_json;
use cubeharbor_core::template::{AppliedTemplate, MinecraftServerTemplate, merge_template};
use k8s_openapi::api::apps::v1::StatefulSet;
use kube::api::ListParams;
use kube::{Api, Client, ResourceExt};
use tokio::sync::Mutex;

/// What the reconcile should do with a server.
pub enum Effective {
    /// Reconcile with this (merged) server. `granted` is true when it just took a rollout slot.
    Apply { mcs: Box<MinecraftServer>, granted: bool },
    /// Leave the server's objects alone for now: it is waiting for a rollout slot.
    Wait(String),
}

/// How long a slot granted in this process counts before the status write that records it is
/// expected to be visible in the server list.
const GRANT_TTL: Duration = Duration::from_secs(300);

/// Rollout slots handed out by this operator. Reconciles run concurrently, so the list the
/// decision is based on may not yet show a slot granted moments ago; this closes that gap.
/// Only the elected leader reconciles, so process-local state is enough.
#[derive(Default)]
pub struct Rollouts {
    /// `namespace/template` -> server -> when its slot was granted.
    grants: Mutex<HashMap<String, HashMap<String, Instant>>>,
}

impl Rollouts {
    /// Take a slot for `server` if fewer than `max` servers of the template hold one. `holders`
    /// are the servers whose status says they are mid-rollout.
    async fn try_acquire(&self, key: &str, server: &str, holders: &[String], max: usize) -> bool {
        let mut grants = self.grants.lock().await;
        let g = grants.entry(key.to_string()).or_default();
        g.retain(|_, at| at.elapsed() < GRANT_TTL);
        let mut busy: Vec<&str> = holders.iter().map(String::as_str).collect();
        busy.extend(g.keys().map(String::as_str));
        busy.sort_unstable();
        busy.dedup();
        busy.retain(|s| *s != server);
        if busy.len() >= max.max(1) {
            return false;
        }
        g.insert(server.to_string(), Instant::now());
        true
    }
}

/// `mcs` with its template merged in, and the applied revision recorded in `status`. Without a
/// `templateRef` the spec is returned untouched. A missing or invalid template is an error, which
/// leaves the server's existing objects (and any running pod) as they are.
pub async fn effective(
    client: &Client,
    rollouts: &Rollouts,
    max_unavailable: usize,
    ns: &str,
    mcs: &MinecraftServer,
    status: &mut MinecraftServerStatus,
) -> Result<Effective> {
    let Some(r) = &mcs.spec.template_ref else {
        status.template = None;
        return Ok(Effective::Apply { mcs: Box::new(mcs.clone()), granted: false });
    };
    let api: Api<MinecraftServerTemplate> = Api::namespaced(client.clone(), ns);
    let template = api
        .get_opt(&r.name)
        .await
        .with_context(|| format!("reading MinecraftServerTemplate {ns}/{}", r.name))?
        .with_context(|| format!("MinecraftServerTemplate {ns}/{} not found", r.name))?;
    template.spec.check().map_err(|e| anyhow::anyhow!("MinecraftServerTemplate {ns}/{}: {e}", r.name))?;
    let mut merged = mcs.clone();
    merged.spec = merge_template(&mcs.spec, &template.spec);
    let hash = hash_json(&merged.spec);

    let prev = mcs.status.as_ref().and_then(|s| s.template.clone()).filter(|t| t.name == r.name);
    let own_edit = mcs.metadata.generation != mcs.status.as_ref().and_then(|s| s.observed_generation);
    let changed = prev.as_ref().is_some_and(|p| p.spec_hash.as_deref().is_some_and(|h| h != hash));
    let serving = !mcs.spec.paused && !status.sleeping && status.phase == Phase::Running;
    let mut granted = false;
    if changed && serving && !own_edit {
        let name = mcs.name_any();
        let holders = slot_holders(client, ns, &r.name).await?;
        if !rollouts.try_acquire(&format!("{ns}/{}", r.name), &name, &holders, max_unavailable).await {
            return Ok(Effective::Wait(format!(
                "waiting to apply MinecraftServerTemplate {} (rolling out to {} server(s) at a time)",
                r.name,
                max_unavailable.max(1)
            )));
        }
        tracing::info!("{ns}/{name}: rolling out MinecraftServerTemplate {} generation {:?}", r.name, template.metadata.generation);
        granted = true;
    }
    // A server keeps its slot until `finish_rollout` frees it, also through a further edit.
    let still_rolling = prev.as_ref().is_some_and(|p| p.rolling);
    status.template = Some(AppliedTemplate {
        name: r.name.clone(),
        generation: template.metadata.generation,
        spec_hash: Some(hash),
        rolling: granted || still_rolling,
    });
    Ok(Effective::Apply { mcs: Box::new(merged), granted })
}

/// Servers of this template that hold a rollout slot according to their status.
async fn slot_holders(client: &Client, ns: &str, template: &str) -> Result<Vec<String>> {
    let api: Api<MinecraftServer> = Api::namespaced(client.clone(), ns);
    let servers = api.list(&ListParams::default()).await.context("listing servers for the template rollout")?;
    Ok(servers
        .items
        .iter()
        .filter(|s| s.spec.template_ref.as_ref().is_some_and(|t| t.name == template))
        .filter(|s| s.status.as_ref().and_then(|st| st.template.as_ref()).is_some_and(|t| t.name == template && t.rolling))
        .map(|s| s.name_any())
        .collect())
}

/// Free the server's slot once the restart is over: the StatefulSet (as read before this
/// reconcile's apply) has rolled out every replica and the pod reports ready, or the server
/// isn't supposed to run at all. Not in the reconcile that took the slot, whose StatefulSet read
/// predates the change.
pub fn finish_rollout(status: &mut MinecraftServerStatus, granted: bool, running: bool, phase: Phase, sts: Option<&StatefulSet>) {
    let Some(t) = status.template.as_mut().filter(|t| t.rolling) else { return };
    if granted {
        return;
    }
    if !running || (phase == Phase::Running && sts.is_some_and(rolled_out)) {
        t.rolling = false;
    }
}

fn rolled_out(sts: &StatefulSet) -> bool {
    let Some(st) = &sts.status else { return false };
    let want = sts.spec.replicas.unwrap_or(1);
    st.observed_generation >= sts.metadata.generation
        && st.updated_replicas.unwrap_or(0) >= want
        && st.ready_replicas.unwrap_or(0) >= want
        && st.update_revision.is_some()
        && st.current_revision == st.update_revision
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::apps::v1::{StatefulSetSpec, StatefulSetStatus};
    use kube::api::ObjectMeta;

    use super::*;

    #[tokio::test]
    async fn slots_are_bounded_and_shared_with_status_holders() {
        let r = Rollouts::default();
        assert!(r.try_acquire("ns/t", "a", &[], 1).await);
        // Granted moments ago but not yet visible in any status: still counts.
        assert!(!r.try_acquire("ns/t", "b", &[], 1).await);
        // The holder itself may re-acquire, and other templates are independent.
        assert!(r.try_acquire("ns/t", "a", &[], 1).await);
        assert!(r.try_acquire("ns/other", "b", &[], 1).await);
        // Status holders count too, and a larger budget admits more.
        assert!(!r.try_acquire("ns/u", "c", &["x".into()], 1).await);
        assert!(r.try_acquire("ns/u", "c", &["x".into()], 2).await);
        assert!(!r.try_acquire("ns/u", "d", &["x".into()], 2).await);
    }

    fn sts(generation: i64, observed: i64, updated: i32, ready: i32, cur: &str, upd: &str) -> StatefulSet {
        StatefulSet {
            metadata: ObjectMeta { generation: Some(generation), ..Default::default() },
            spec: StatefulSetSpec { replicas: Some(1), ..Default::default() },
            status: Some(StatefulSetStatus {
                observed_generation: Some(observed),
                updated_replicas: Some(updated),
                ready_replicas: Some(ready),
                current_revision: Some(cur.into()),
                update_revision: Some(upd.into()),
                ..Default::default()
            }),
        }
    }

    fn rolling() -> MinecraftServerStatus {
        MinecraftServerStatus {
            template: Some(AppliedTemplate { name: "t".into(), rolling: true, ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn a_slot_is_freed_only_once_the_restart_is_over() {
        let done = sts(2, 2, 1, 1, "r2", "r2");
        for (granted, running, phase, s, freed) in [
            (true, true, Phase::Running, Some(&done), false),
            (false, true, Phase::Starting, Some(&done), false),
            (false, true, Phase::Running, Some(&sts(2, 1, 1, 1, "r2", "r2")), false),
            (false, true, Phase::Running, Some(&sts(2, 2, 1, 1, "r1", "r2")), false),
            (false, true, Phase::Running, Some(&sts(2, 2, 0, 1, "r2", "r2")), false),
            (false, true, Phase::Running, None, false),
            (false, true, Phase::Running, Some(&done), true),
            (false, false, Phase::Stopped, None, true),
        ] {
            let mut st = rolling();
            finish_rollout(&mut st, granted, running, phase, s);
            assert_eq!(!st.template.unwrap().rolling, freed, "{granted} {running} {phase:?}");
        }
    }
}
