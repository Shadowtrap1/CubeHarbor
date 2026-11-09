//! Applies a server's `spec.templateRef` before reconciling it.
//!
//! The template is read straight from the API server on each reconcile rather than watched: every
//! server already requeues within 60 seconds, a read is cheap next to the cluster-wide server list
//! each reconcile does, and the operator needs nothing beyond `get` on templates.

use anyhow::{Context as _, Result};
use cubeharbor_core::MinecraftServer;
use cubeharbor_core::crd::MinecraftServerStatus;
use cubeharbor_core::template::{AppliedTemplate, MinecraftServerTemplate, merge_template};
use kube::{Api, Client};

/// `mcs` with its template merged in, and the applied revision recorded in `status`. Without a
/// `templateRef` the spec is returned untouched. A missing or invalid template is an error, which
/// leaves the server's existing objects (and any running pod) as they are.
pub async fn effective(client: &Client, ns: &str, mcs: &MinecraftServer, status: &mut MinecraftServerStatus) -> Result<MinecraftServer> {
    let Some(r) = &mcs.spec.template_ref else {
        status.template = None;
        return Ok(mcs.clone());
    };
    let api: Api<MinecraftServerTemplate> = Api::namespaced(client.clone(), ns);
    let template = api
        .get_opt(&r.name)
        .await
        .with_context(|| format!("reading MinecraftServerTemplate {ns}/{}", r.name))?
        .with_context(|| format!("MinecraftServerTemplate {ns}/{} not found", r.name))?;
    template.spec.check().map_err(|e| anyhow::anyhow!("MinecraftServerTemplate {ns}/{}: {e}", r.name))?;
    status.template = Some(AppliedTemplate { name: r.name.clone(), generation: template.metadata.generation });
    let mut merged = mcs.clone();
    merged.spec = merge_template(&mcs.spec, &template.spec);
    Ok(merged)
}
