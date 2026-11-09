//! The `MinecraftServerTemplate` custom resource: shared settings a `MinecraftServer` references
//! with `spec.templateRef`, and [`merge_template`], the one place that combines the two.
//!
//! Identity is deliberately not templatable. Routing (hostnames, default server) and `paused` are
//! read straight from the `MinecraftServer` by the gateway and the dashboard, which never see a
//! template, and a hostname can only belong to one server anyway.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Affinity, Toleration};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::crd::{AutoSleepSpec, ExtraPort, FileSpec, GeyserSpec, JvmSpec, Loader, MinecraftServerSpec, ModRef, ModpackSource, ServerResources, VoiceSpec};

/// Reference to a `MinecraftServerTemplate` in the server's own namespace.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TemplateRef {
    pub name: String,
}

/// The template revision a server's objects were last computed from.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppliedTemplate {
    pub name: String,
    /// `metadata.generation` of the template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<i64>,
}

/// Settings shared by the servers that reference this template. The fields mean the same as on
/// `MinecraftServer`; see [`merge_template`] for how the two combine.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[kube(
    group = "cubeharbor.dev",
    version = "v1alpha1",
    kind = "MinecraftServerTemplate",
    namespaced,
    shortname = "mcst",
    printcolumn = r#"{"name":"Version","type":"string","jsonPath":".spec.version"}"#,
    printcolumn = r#"{"name":"Loader","type":"string","jsonPath":".spec.loader"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftServerTemplateSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader: Option<Loader>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modpack: Option<ModpackSource>,
    /// Merged by Modrinth project or URL; a server's entry for the same mod replaces this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mods: Vec<ModRef>,
    #[serde(default)]
    pub resources: TemplateResources,
    #[serde(default)]
    pub jvm: JvmSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept_eula: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motd: Option<String>,
    /// Replaced as a whole by a server's non-empty list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<String>,
    /// Replaced as a whole by a server's non-empty list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub whitelist: Vec<String>,
    /// Merged per key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,
    /// Merged by path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileSpec>,
    /// `publicPort` is not allowed here: one port can't belong to every server using the template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<VoiceSpec>,
    /// `publicPort` is not allowed here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geyser: Option<GeyserSpec>,
    /// Merged by name. `publicPort` and `host` are not allowed here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_ports: Vec<ExtraPort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_sleep: Option<AutoSleepSpec>,
    /// Merged per key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Subject to the operator's image allow-list, like `spec.image` on a server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Merged per key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    /// Replaced as a whole by a server's non-empty list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<Toleration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<Affinity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_class_name: Option<String>,
}

/// Like `ServerResources`, but nothing has a default, so an unset field stays unset.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TemplateResources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_limit: Option<String>,
    /// Only used when a server's volume is first created; volume claims are immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
}

impl MinecraftServerTemplateSpec {
    /// Reject settings that can only be valid for a single server. Without this, the first server
    /// would get the pinned port or host and all the others would fail.
    pub fn check(&self) -> Result<(), String> {
        if self.voice.as_ref().is_some_and(|v| v.public_port.is_some()) {
            return Err("voice.publicPort can't be set in a template; set it on the server".into());
        }
        if self.geyser.as_ref().is_some_and(|g| g.public_port.is_some()) {
            return Err("geyser.publicPort can't be set in a template; set it on the server".into());
        }
        if let Some(p) = self.extra_ports.iter().find(|p| p.public_port.is_some() || p.host.is_some()) {
            return Err(format!("extraPorts `{}` can't set publicPort or host in a template; set them on the server", p.name));
        }
        Ok(())
    }
}

fn pick<T: Clone>(own: &Option<T>, template: &Option<T>) -> Option<T> {
    own.clone().or_else(|| template.clone())
}

fn pick_list<T: Clone>(own: &[T], template: &[T]) -> Vec<T> {
    if own.is_empty() { template.to_vec() } else { own.to_vec() }
}

fn merge_map(own: &BTreeMap<String, String>, template: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut out = template.clone();
    out.extend(own.clone());
    out
}

/// The template's entries in order (a server's entry takes over the slot of the one it replaces),
/// then the server's new ones. The order is stable, so the install plan hash is too.
fn merge_keyed<T: Clone, K: PartialEq>(own: &[T], template: &[T], key: impl Fn(&T) -> K) -> Vec<T> {
    let mut out = template.to_vec();
    for item in own {
        match out.iter_mut().find(|x| key(x) == key(item)) {
            Some(slot) => *slot = item.clone(),
            None => out.push(item.clone()),
        }
    }
    out
}

/// The API server fills in `resources.memory` and `resources.storage` on every stored
/// `MinecraftServer`, so "not set" can't be told apart from the default: a value equal to the
/// built-in default counts as not set. A server that really wants that amount under a template
/// with a different one spells it differently (`4096Mi` instead of `4Gi`).
fn pick_defaulted(own: &str, default: &str, template: &Option<String>) -> String {
    match template {
        Some(t) if own == default => t.clone(),
        _ => own.to_string(),
    }
}

/// The spec a server runs with: `server`, with anything it leaves unset taken from `template`.
///
/// * Single values and whole objects (`modpack`, `voice`, `geyser`, `autoSleep`, `affinity`): the
///   server's wins. `resources` and `jvm` are merged field by field.
/// * Maps (`properties`, `env`, `nodeSelector`): merged per key, the server's value wins.
/// * `mods`, `files`, `extraPorts`: merged by identity (project or URL, path, name).
/// * Other lists (`ops`, `whitelist`, `tolerations`, `jvm.extraArgs`): a non-empty list replaces
///   the template's.
/// * A server with its own `modpack` doesn't inherit `version`, `loader` or `loaderVersion`: an
///   explicit version takes precedence over the pack's, so a template's would silently break it.
///
/// Pure and deterministic, so a server's objects change only when this result changes. Neither
/// struct is built or taken apart with `..`: a new field on either one fails to compile here until
/// someone decides how it merges.
pub fn merge_template(server: &MinecraftServerSpec, template: &MinecraftServerTemplateSpec) -> MinecraftServerSpec {
    let MinecraftServerTemplateSpec {
        version,
        loader,
        loader_version,
        modpack,
        mods,
        resources,
        jvm,
        accept_eula,
        motd,
        ops,
        whitelist,
        properties,
        files,
        voice,
        geyser,
        extra_ports,
        auto_sleep,
        env,
        image,
        node_selector,
        tolerations,
        affinity,
        priority_class_name,
    } = template;
    let own_pack = server.modpack.is_some();
    let unless_own_pack = |own: &Option<String>, t: &Option<String>| if own_pack { own.clone() } else { pick(own, t) };

    MinecraftServerSpec {
        template_ref: server.template_ref.clone(),
        version: unless_own_pack(&server.version, version),
        loader: if own_pack { server.loader } else { server.loader.or(*loader) },
        loader_version: unless_own_pack(&server.loader_version, loader_version),
        modpack: pick(&server.modpack, modpack),
        mods: merge_keyed(&server.mods, mods, |m| (m.modrinth.clone(), m.url.clone())),
        resources: ServerResources {
            memory: pick_defaulted(&server.resources.memory, crate::defaults::MEMORY, &resources.memory),
            cpu: pick(&server.resources.cpu, &resources.cpu),
            cpu_limit: pick(&server.resources.cpu_limit, &resources.cpu_limit),
            storage: pick_defaulted(&server.resources.storage, crate::defaults::STORAGE, &resources.storage),
            storage_class: pick(&server.resources.storage_class, &resources.storage_class),
        },
        jvm: JvmSpec {
            heap_percent: server.jvm.heap_percent.or(jvm.heap_percent),
            java_version: server.jvm.java_version.or(jvm.java_version),
            aikar_flags: server.jvm.aikar_flags.or(jvm.aikar_flags),
            extra_args: pick_list(&server.jvm.extra_args, &jvm.extra_args),
        },
        accept_eula: server.accept_eula.or(*accept_eula),
        motd: pick(&server.motd, motd),
        ops: pick_list(&server.ops, ops),
        whitelist: pick_list(&server.whitelist, whitelist),
        properties: merge_map(&server.properties, properties),
        files: merge_keyed(&server.files, files, |f| f.path.clone()),
        routing: server.routing.clone(),
        voice: pick(&server.voice, voice),
        geyser: pick(&server.geyser, geyser),
        extra_ports: merge_keyed(&server.extra_ports, extra_ports, |p| p.name.clone()),
        auto_sleep: pick(&server.auto_sleep, auto_sleep),
        paused: server.paused,
        env: merge_map(&server.env, env),
        image: pick(&server.image, image),
        node_selector: merge_map(&server.node_selector, node_selector),
        tolerations: pick_list(&server.tolerations, tolerations),
        affinity: pick(&server.affinity, affinity),
        priority_class_name: pick(&server.priority_class_name, priority_class_name),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn server(v: serde_json::Value) -> MinecraftServerSpec {
        serde_json::from_value(v).unwrap()
    }
    fn template(v: serde_json::Value) -> MinecraftServerTemplateSpec {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn empty_template_changes_nothing() {
        let s = server(json!({
            "version": "1.21.4", "loader": "paper", "resources": { "memory": "6Gi", "storage": "50Gi" },
            "mods": [{ "modrinth": "chunky" }], "properties": { "pvp": "false" }, "ops": ["Steve"],
            "routing": { "hostnames": ["a.example.com"], "default": true }, "paused": true,
        }));
        assert_eq!(merge_template(&s, &Default::default()), s);
    }

    #[test]
    fn a_server_without_a_reference_serializes_as_before() {
        let v = serde_json::to_value(MinecraftServerSpec::default()).unwrap();
        assert!(v.get("templateRef").is_none());
        let status = serde_json::to_value(crate::crd::MinecraftServerStatus::default()).unwrap();
        assert!(status.get("template").is_none());
    }

    #[test]
    fn unset_fields_come_from_the_template_and_set_ones_win() {
        let t = template(json!({
            "version": "1.21.1", "loader": "fabric", "motd": "shared", "acceptEula": true, "image": "registry.example/mc:1",
            "autoSleep": { "idleMinutes": 30 }, "jvm": { "heapPercent": 70, "aikarFlags": false },
            "resources": { "cpu": "2", "cpuLimit": "4" },
        }));
        let m = merge_template(&server(json!({ "motd": "mine", "jvm": { "heapPercent": 50 }, "resources": { "cpu": "1" } })), &t);
        assert_eq!(m.version.as_deref(), Some("1.21.1"));
        assert_eq!(m.loader, Some(Loader::Fabric));
        assert_eq!(m.accept_eula, Some(true));
        assert_eq!(m.image.as_deref(), Some("registry.example/mc:1"));
        assert_eq!(m.auto_sleep.unwrap().idle_minutes, 30);
        assert_eq!(m.motd.as_deref(), Some("mine"));
        assert_eq!((m.jvm.heap_percent, m.jvm.aikar_flags), (Some(50), Some(false)));
        assert_eq!((m.resources.cpu.as_deref(), m.resources.cpu_limit.as_deref()), (Some("1"), Some("4")));
    }

    #[test]
    fn memory_and_storage_at_the_built_in_default_count_as_unset() {
        let t = template(json!({ "resources": { "memory": "8Gi", "storage": "40Gi" } }));
        // What the API server stores for a server that sets neither.
        let defaulted = server(json!({ "resources": { "memory": crate::defaults::MEMORY, "storage": crate::defaults::STORAGE } }));
        let m = merge_template(&defaulted, &t);
        assert_eq!((m.resources.memory.as_str(), m.resources.storage.as_str()), ("8Gi", "40Gi"));
        let m = merge_template(&server(json!({ "resources": { "memory": "2Gi", "storage": "5Gi" } })), &t);
        assert_eq!((m.resources.memory.as_str(), m.resources.storage.as_str()), ("2Gi", "5Gi"));
        // With nothing in the template the default stays.
        let m = merge_template(&defaulted, &Default::default());
        assert_eq!(m.resources.memory, crate::defaults::MEMORY);
    }

    #[test]
    fn maps_merge_per_key() {
        let t = template(json!({ "properties": { "pvp": "false", "difficulty": "hard" }, "env": { "A": "1" }, "nodeSelector": { "pool": "games" } }));
        let m = merge_template(&server(json!({ "properties": { "pvp": "true" }, "env": { "B": "2" } })), &t);
        assert_eq!(m.properties["pvp"], "true");
        assert_eq!(m.properties["difficulty"], "hard");
        assert_eq!(m.env.len(), 2);
        assert_eq!(m.node_selector["pool"], "games");
    }

    #[test]
    fn identified_lists_merge_and_keep_a_stable_order() {
        let t = template(json!({
            "mods": [{ "modrinth": "chunky" }, { "modrinth": "lithium", "version": "1" }],
            "files": [{ "path": "a.txt", "content": "template" }, { "path": "b.txt", "content": "template" }],
            "extraPorts": [{ "name": "map", "containerPort": 8100 }],
        }));
        let m = merge_template(
            &server(json!({
                "mods": [{ "url": "https://example.com/x.jar" }, { "modrinth": "chunky", "version": "2" }],
                "files": [{ "path": "a.txt", "content": "server" }],
                "extraPorts": [{ "name": "query", "containerPort": 25566 }, { "name": "map", "containerPort": 8200 }],
            })),
            &t,
        );
        let mods: Vec<_> = m.mods.iter().map(|x| (x.modrinth.as_deref(), x.version.as_deref())).collect();
        assert_eq!(mods, [(Some("chunky"), Some("2")), (Some("lithium"), Some("1")), (None, None)]);
        assert_eq!(m.files.iter().map(|f| f.content.as_str()).collect::<Vec<_>>(), ["server", "template"]);
        assert_eq!(m.extra_ports.iter().map(|p| (p.name.as_str(), p.container_port)).collect::<Vec<_>>(), [("map", 8200), ("query", 25566)]);
    }

    #[test]
    fn plain_lists_are_replaced_when_the_server_has_one() {
        let t = template(json!({ "ops": ["Steve"], "whitelist": ["Alex"], "jvm": { "extraArgs": ["-Da"] } }));
        let m = merge_template(&server(json!({ "ops": ["Herobrine"] })), &t);
        assert_eq!(m.ops, ["Herobrine"]);
        assert_eq!(m.whitelist, ["Alex"]);
        assert_eq!(m.jvm.extra_args, ["-Da"]);
    }

    #[test]
    fn a_servers_own_modpack_decides_its_version_and_loader() {
        let t = template(json!({ "version": "1.20.1", "loader": "forge", "loaderVersion": "47" }));
        let pack = json!({ "modrinth": { "project": "cobblemon-fabric" } });
        let m = merge_template(&server(json!({ "modpack": pack })), &t);
        assert_eq!((m.version, m.loader, m.loader_version), (None, None, None));
        // ...but it can still pin them itself, and a pack from the template doesn't block them.
        let m = merge_template(&server(json!({ "modpack": pack, "version": "1.21.1" })), &t);
        assert_eq!(m.version.as_deref(), Some("1.21.1"));
        let m = merge_template(&server(json!({})), &template(json!({ "modpack": pack, "version": "1.20.1" })));
        assert_eq!(m.version.as_deref(), Some("1.20.1"));
    }

    #[test]
    fn identity_is_never_inherited() {
        let s = server(json!({ "paused": true, "routing": { "hostnames": ["a.example.com"] }, "templateRef": { "name": "t" } }));
        let m = merge_template(&s, &template(json!({ "version": "1.21.1" })));
        assert_eq!((m.paused, m.routing, m.template_ref), (true, s.routing.clone(), s.template_ref.clone()));
    }

    /// Rollouts hinge on this: a template edit restarts only the servers whose result changes.
    #[test]
    fn a_template_edit_only_affects_servers_that_inherit_the_field() {
        let before = template(json!({ "motd": "old", "version": "1.21.1" }));
        let after = template(json!({ "motd": "new", "version": "1.21.1" }));
        let overriding = server(json!({ "motd": "mine" }));
        assert_eq!(merge_template(&overriding, &before), merge_template(&overriding, &after));
        let inheriting = server(json!({}));
        assert_ne!(merge_template(&inheriting, &before), merge_template(&inheriting, &after));
    }

    #[test]
    fn per_server_settings_are_rejected_in_a_template() {
        assert!(template(json!({ "voice": {}, "geyser": {}, "extraPorts": [{ "name": "map", "containerPort": 8100 }] })).check().is_ok());
        assert!(template(json!({ "voice": { "publicPort": 25600 } })).check().is_err());
        assert!(template(json!({ "geyser": { "publicPort": 25600 } })).check().is_err());
        assert!(template(json!({ "extraPorts": [{ "name": "map", "containerPort": 8100, "publicPort": 25600 }] })).check().is_err());
        assert!(template(json!({ "extraPorts": [{ "name": "map", "containerPort": 8100, "host": "map.example.com" }] })).check().is_err());
    }
}
