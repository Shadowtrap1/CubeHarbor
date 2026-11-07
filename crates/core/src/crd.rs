//! The `MinecraftServer` custom resource.
//!
//! The spec describes *intent* (what to run); the operator resolves it into concrete
//! versions, ports and Kubernetes objects and records the outcome in the status.

use std::collections::BTreeMap;

use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const GROUP: &str = "cubeharbor.dev";
pub const MINECRAFT_PORT: u16 = 25565;
pub const RCON_PORT: u16 = 25575;
pub const SIMPLE_VOICE_CHAT_PORT: u16 = 24454;
pub const PLASMO_VOICE_PORT: u16 = 60606;
pub const GEYSER_PORT: u16 = 19132;

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, Default, KubeSchema, PartialEq)]
#[kube(
    // Checked by the API server, so `kubectl apply` rejects a bad name instead of the operator
    // reporting an error later. Keep in sync with `check_server_name`. (Names are immutable, so
    // this can't break updates of existing objects that were valid before.)
    validation = Rule::new("self.metadata.name.size() <= 52 && self.metadata.name.matches('^[a-z]([-a-z0-9]*[a-z0-9])?$')")
        .message("metadata.name must be a DNS-1035 label of at most 52 characters: lower-case letters, digits and '-', starting with a letter"),
    group = "cubeharbor.dev",
    version = "v1alpha1",
    kind = "MinecraftServer",
    namespaced,
    status = "MinecraftServerStatus",
    shortname = "mcs",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Version","type":"string","jsonPath":".status.resolved.minecraftVersion"}"#,
    printcolumn = r#"{"name":"Loader","type":"string","jsonPath":".status.resolved.loader"}"#,
    printcolumn = r#"{"name":"Players","type":"integer","jsonPath":".status.players.online"}"#,
    printcolumn = r#"{"name":"Address","type":"string","jsonPath":".status.hostnames[0]"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftServerSpec {
    /// Minecraft version (e.g. `1.21.1`, `26.3`, `latest`). Optional when a modpack defines it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// Server software. Inferred from the modpack when one is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader: Option<Loader>,

    /// Loader version (Fabric loader, Forge/NeoForge build, Paper build...). Defaults to the latest stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,

    /// Install a modpack. Mod loader and Minecraft version are taken from the pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modpack: Option<ModpackSource>,

    /// Extra mods / plugins installed on top of the base server or modpack.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mods: Vec<ModRef>,

    #[serde(default)]
    pub resources: ServerResources,

    #[serde(default)]
    pub jvm: JvmSpec,

    /// Accept the Minecraft EULA (https://aka.ms/MinecraftEULA). Falls back to the operator-wide default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept_eula: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motd: Option<String>,

    /// Player names granted operator status.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<String>,

    /// Player names allowed to join. Enables the whitelist when non-empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub whitelist: Vec<String>,

    /// Raw `server.properties` overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,

    /// Files written into the server directory on every start (path relative to the server root).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileSpec>,

    #[serde(default)]
    pub routing: RoutingSpec,

    /// Proximity voice chat. Allocates a public UDP port and configures the mod automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<VoiceSpec>,

    /// Bedrock cross-play through Geyser. Allocates a public UDP port and installs Geyser (+ Floodgate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geyser: Option<GeyserSpec>,

    /// Additional ports (web maps, query, custom plugins).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_ports: Vec<ExtraPort>,

    /// Stop the server after a period without players. The gateway wakes it when someone joins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_sleep: Option<AutoSleepSpec>,

    /// Scale the server to zero. Data is kept.
    #[serde(default)]
    pub paused: bool,

    /// Extra environment variables for the server container.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,

    /// Override the server image (normally picked from the required Java version).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,

    /// Added to the operator-wide `serverPods.tolerations`, e.g. to run on tainted game nodes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<k8s_openapi::api::core::v1::Toleration>,

    /// Replaces the operator-wide `serverPods.affinity` for this server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<k8s_openapi::api::core::v1::Affinity>,

    /// Replaces the operator-wide `serverPods.priorityClassName` for this server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_class_name: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Loader {
    #[default]
    Vanilla,
    Paper,
    Purpur,
    Fabric,
    Quilt,
    Forge,
    NeoForge,
}

impl Loader {
    pub fn as_str(self) -> &'static str {
        match self {
            Loader::Vanilla => "vanilla",
            Loader::Paper => "paper",
            Loader::Purpur => "purpur",
            Loader::Fabric => "fabric",
            Loader::Quilt => "quilt",
            Loader::Forge => "forge",
            Loader::NeoForge => "neoforge",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "vanilla" | "minecraft" => Loader::Vanilla,
            "paper" | "papermc" => Loader::Paper,
            "purpur" => Loader::Purpur,
            "fabric" => Loader::Fabric,
            "quilt" => Loader::Quilt,
            "forge" => Loader::Forge,
            "neoforge" | "neo-forge" | "neoforged" => Loader::NeoForge,
            _ => return None,
        })
    }

    /// Plugin servers load jars from `plugins/`, modded servers from `mods/`.
    pub fn is_plugin_server(self) -> bool {
        matches!(self, Loader::Paper | Loader::Purpur)
    }

    pub fn mods_dir(self) -> &'static str {
        if self.is_plugin_server() { "plugins" } else { "mods" }
    }

    /// Loader name as used by Modrinth's `loaders` filter.
    pub fn modrinth_name(self) -> &'static str {
        self.as_str()
    }
}

impl std::fmt::Display for Loader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Exactly one of `modrinth`, `curseforge` or `url` should be set.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModpackSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modrinth: Option<ModrinthPack>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curseforge: Option<CurseForgePack>,
    /// Direct link to a `.mrpack` or a server-pack zip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Case-insensitive substrings of file names to skip (e.g. client-only mods a pack forgot to flag).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// File-name substrings to keep even when detected as client-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModrinthPack {
    /// Project slug or ID, e.g. `cobblemon-fabric`.
    pub project: String,
    /// Version number or ID. Latest release when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CurseForgePack {
    /// Project slug (e.g. `all-the-mods-10`) or numeric ID. Requires a CurseForge API key.
    pub project: String,
    /// File ID. Latest file when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<u64>,
    /// Use the author's server pack when one is published (recommended). Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_server_pack: Option<bool>,
}

/// A single mod or plugin. Exactly one of `modrinth` or `url` should be set.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct ModRef {
    /// Modrinth slug or ID. The newest version compatible with the server's loader and version is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modrinth: Option<String>,
    /// Pin a Modrinth version number or ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Direct download URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Destination directory relative to the server root. Defaults to `mods` or `plugins`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerResources {
    /// Container memory limit. The JVM heap is derived from it.
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_limit: Option<String>,
    /// Size of the world/data volume.
    #[serde(default = "default_storage")]
    pub storage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
}

fn default_memory() -> String {
    crate::defaults::MEMORY.into()
}
fn default_storage() -> String {
    crate::defaults::STORAGE.into()
}

impl Default for ServerResources {
    fn default() -> Self {
        Self { memory: default_memory(), cpu: None, cpu_limit: None, storage: default_storage(), storage_class: None }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JvmSpec {
    /// Percentage of the memory limit given to the heap. Default: limit minus max(1Gi, 15%).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heap_percent: Option<u8>,
    /// Force a Java major version (8, 17, 21, 25).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub java_version: Option<u8>,
    /// Use Aikar's G1GC flags. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aikar_flags: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileSpec {
    pub path: String,
    pub content: String,
    /// Only write the file if it does not exist yet.
    #[serde(default)]
    pub if_missing: bool,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RoutingSpec {
    /// Hostnames players connect with. Defaults to `<name>.<baseDomain>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hostnames: Vec<String>,
    /// Send a PROXY protocol v2 header so the server sees real client IPs (Paper/Velocity/HAProxy-aware mods).
    #[serde(default)]
    pub proxy_protocol: bool,
    /// Start a sleeping server when a player tries to join. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_wake: Option<bool>,
    /// Route connections with unknown hostnames here.
    #[serde(default)]
    pub default: bool,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VoiceKind {
    SimpleVoiceChat,
    PlasmoVoice,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VoiceSpec {
    #[serde(default = "default_voice")]
    pub kind: VoiceKind,
    /// Install the voice mod/plugin from Modrinth. Disable if your modpack already ships it.
    #[serde(default = "yes")]
    pub install: bool,
    /// Pin the public UDP port instead of allocating one from the pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_port: Option<u16>,
}

fn default_voice() -> VoiceKind {
    VoiceKind::SimpleVoiceChat
}
fn yes() -> bool {
    true
}

impl VoiceKind {
    pub fn container_port(self) -> u16 {
        match self {
            VoiceKind::SimpleVoiceChat => SIMPLE_VOICE_CHAT_PORT,
            VoiceKind::PlasmoVoice => PLASMO_VOICE_PORT,
        }
    }
    pub fn modrinth_slug(self) -> &'static str {
        match self {
            VoiceKind::SimpleVoiceChat => "simple-voice-chat",
            VoiceKind::PlasmoVoice => "plasmo-voice",
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GeyserSpec {
    #[serde(default = "yes")]
    pub install: bool,
    /// Also install Floodgate so Bedrock players don't need a Java account.
    #[serde(default = "yes")]
    pub floodgate: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_port: Option<u16>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, JsonSchema, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "UPPERCASE")]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Tcp => "TCP",
            Protocol::Udp => "UDP",
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Exposure {
    /// Allocate a public port on the gateway.
    #[default]
    Gateway,
    /// Expose over HTTP through an Ingress (needs `host`).
    Ingress,
    /// Cluster-internal only.
    Internal,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtraPort {
    /// DNS-1123 label, unique per server (e.g. `bluemap`).
    pub name: String,
    pub container_port: u16,
    #[serde(default)]
    pub protocol: Protocol,
    #[serde(default)]
    pub expose: Exposure,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_port: Option<u16>,
    /// Ingress host when `expose: ingress`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AutoSleepSpec {
    #[serde(default = "default_idle")]
    pub idle_minutes: u32,
}

fn default_idle() -> u32 {
    crate::defaults::AUTO_SLEEP_IDLE_MINUTES
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftServerStatus {
    #[serde(default)]
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<Resolved>,
    /// Hash of the spec fields `resolved` was computed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_hash: Option<String>,
    /// Hostnames routed to this server by the gateway.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hostnames: Vec<String>,
    /// In-cluster service the gateway forwards to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allocations: Vec<PortAllocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub players: Option<Players>,
    /// Last time a player was seen online (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active: Option<String>,
    /// Stopped by auto-sleep; the gateway clears this when a player joins.
    #[serde(default)]
    pub sleeping: bool,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, JsonSchema, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Pending,
    Installing,
    Starting,
    Running,
    Sleeping,
    Stopped,
    Error,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Pending => "Pending",
            Phase::Installing => "Installing",
            Phase::Starting => "Starting",
            Phase::Running => "Running",
            Phase::Sleeping => "Sleeping",
            Phase::Stopped => "Stopped",
            Phase::Error => "Error",
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Resolved {
    pub minecraft_version: String,
    pub loader: Loader,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,
    pub java_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modpack: Option<ResolvedModpack>,
}


#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedModpack {
    pub name: String,
    pub version: String,
    /// Download URL of the pack (mrpack or zip).
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
    pub kind: ModpackKind,
    /// CurseForge server pack, when one is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_pack_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModpackKind {
    #[default]
    Mrpack,
    CurseForge,
    /// A zip containing a ready server directory.
    ServerZip,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct PortAllocation {
    pub name: String,
    pub protocol: Protocol,
    pub container_port: u16,
    pub public_port: u16,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Players {
    pub online: u32,
    pub max: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample: Vec<String>,
}

impl MinecraftServer {
    pub fn phase(&self) -> Phase {
        self.status.as_ref().map(|s| s.phase).unwrap_or_default()
    }

    /// Whether the pod should be running right now.
    pub fn wants_running(&self) -> bool {
        !self.spec.paused && !self.status.as_ref().is_some_and(|s| s.sleeping)
    }

    pub fn auto_wake(&self) -> bool {
        self.spec.routing.auto_wake.unwrap_or(true) && !self.spec.paused
    }
}

/// Hostnames a server answers to.
pub fn effective_hostnames(name: &str, spec: &MinecraftServerSpec, base_domain: Option<&str>) -> Vec<String> {
    if !spec.routing.hostnames.is_empty() {
        return spec.routing.hostnames.iter().map(|h| normalize_host(h)).collect();
    }
    match base_domain {
        Some(d) if !d.is_empty() => vec![format!("{}.{}", name, d.trim_matches('.')).to_ascii_lowercase()],
        _ => vec![],
    }
}

/// Precedence when servers claim the same hostname, or more than one sets `routing.default`: the
/// oldest server wins, then namespace and name. The operator and the gateway both use this, so
/// routing stays predictable while a conflict is reported.
pub fn claim_order(s: &MinecraftServer) -> impl Ord {
    use kube::ResourceExt;
    (s.metadata.creation_timestamp.clone(), s.namespace().unwrap_or_default(), s.name_any())
}

/// Routing claims of `me` that an earlier server (by [`claim_order`]) already holds.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RoutingConflicts {
    /// (hostname, `namespace/name` of the server that keeps it)
    pub hostnames: Vec<(String, String)>,
    /// `namespace/name` of an earlier default server, when `me` also sets `routing.default`.
    pub default: Option<String>,
}

impl RoutingConflicts {
    pub fn is_empty(&self) -> bool {
        self.hostnames.is_empty() && self.default.is_none()
    }

    pub fn message(&self) -> String {
        let mut parts: Vec<String> = self.hostnames.iter().map(|(h, o)| format!("hostname {h} is already used by {o}")).collect();
        if let Some(o) = &self.default {
            parts.push(format!("{o} is already the default server"));
        }
        parts.join("; ")
    }
}

/// Which of `hostnames` (and `routing.default`) `me` loses to servers in `others`.
pub fn routing_conflicts<'a>(me: &MinecraftServer, hostnames: &[String], others: impl IntoIterator<Item = &'a MinecraftServer>) -> RoutingConflicts {
    use kube::ResourceExt;
    let mut out = RoutingConflicts::default();
    let mine = claim_order(me);
    for other in others {
        if other.namespace() == me.namespace() && other.name_any() == me.name_any() {
            continue;
        }
        if claim_order(other) > mine {
            continue;
        }
        let owner = format!("{}/{}", other.namespace().unwrap_or_default(), other.name_any());
        let theirs = other.status.as_ref().map(|s| s.hostnames.as_slice()).unwrap_or_default();
        for h in hostnames.iter().filter(|h| theirs.contains(h)) {
            if !out.hostnames.iter().any(|(x, _)| x == h) {
                out.hostnames.push((h.clone(), owner.clone()));
            }
        }
        if me.spec.routing.default && other.spec.routing.default && out.default.is_none() {
            out.default = Some(owner);
        }
    }
    out
}

/// The name an extra port gets in the pod and Service. Kubernetes allows 15 characters, and
/// longer names have always been cut rather than rejected, so existing servers keep working.
pub fn k8s_port_name(name: &str) -> &str {
    name.get(..15).unwrap_or(name)
}

/// A Kubernetes port name (IANA service name): at most 15 lower-case alphanumerics or `-`,
/// with at least one letter, no leading/trailing `-` and no `--`.
fn check_port_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 15
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name.chars().any(|c| c.is_ascii_lowercase())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if ok {
        Ok(())
    } else {
        Err(format!("port name `{name}` must be 1-15 lower-case letters, digits or `-`, with at least one letter"))
    }
}

/// Longest server name: the StatefulSet's `controller-revision-hash` label is `<name>-<10-char hash>`
/// and labels are limited to 63 characters.
pub const MAX_SERVER_NAME: usize = 52;

/// Can `name` be used for the server's StatefulSet and Service? Services need a DNS-1035 label
/// (lower-case alphanumerics and `-`, starting with a letter).
pub fn check_server_name(name: &str) -> Result<(), String> {
    let ok = name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        return Err(format!(
            "server name `{name}` is not a valid DNS-1035 label: use lower-case letters, digits and `-`, starting with a letter"
        ));
    }
    if name.len() > MAX_SERVER_NAME {
        return Err(format!("server name `{name}` is longer than {MAX_SERVER_NAME} characters"));
    }
    Ok(())
}

/// Lower-case, strip a trailing dot and any port.
pub fn normalize_host(h: &str) -> String {
    let h = h.trim().trim_end_matches('.');
    let h = h.rsplit_once(':').map_or(h, |(host, port)| if port.chars().all(|c| c.is_ascii_digit()) { host } else { h });
    h.to_ascii_lowercase()
}

/// Port requests derived from the spec, before allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortRequest {
    pub name: String,
    pub protocol: Protocol,
    pub container_port: u16,
    pub public_port: Option<u16>,
}

impl MinecraftServerSpec {
    pub fn port_requests(&self) -> Vec<PortRequest> {
        let mut out = vec![];
        if let Some(v) = &self.voice {
            out.push(PortRequest {
                name: "voice".into(),
                protocol: Protocol::Udp,
                container_port: v.kind.container_port(),
                public_port: v.public_port,
            });
        }
        if let Some(g) = &self.geyser {
            out.push(PortRequest {
                name: "bedrock".into(),
                protocol: Protocol::Udp,
                container_port: GEYSER_PORT,
                public_port: g.public_port,
            });
        }
        for p in &self.extra_ports {
            if p.expose == Exposure::Gateway {
                out.push(PortRequest {
                    name: p.name.clone(),
                    protocol: p.protocol,
                    container_port: p.container_port,
                    public_port: p.public_port,
                });
            }
        }
        out
    }

    /// Check the ports every server pod and Service gets: port names (after the cut to 15
    /// characters, see [`k8s_port_name`]) must be valid Kubernetes port names and unique, and no
    /// two may share a container port and protocol. The API server would otherwise reject the
    /// Service and StatefulSet.
    pub fn check_ports(&self) -> Result<(), String> {
        let mut ports: Vec<(&str, u16, Protocol)> = vec![("minecraft", MINECRAFT_PORT, Protocol::Tcp), ("rcon", RCON_PORT, Protocol::Tcp)];
        if let Some(v) = &self.voice {
            ports.push(("voice", v.kind.container_port(), Protocol::Udp));
        }
        if self.geyser.is_some() {
            ports.push(("bedrock", GEYSER_PORT, Protocol::Udp));
        }
        for p in &self.extra_ports {
            let name = k8s_port_name(&p.name);
            check_port_name(name).map_err(|e| if name == p.name { e } else { format!("{e} (`{}` is cut to 15 characters)", p.name) })?;
            ports.push((name, p.container_port, p.protocol));
        }
        let (mut names, mut numbers) = (std::collections::HashSet::new(), std::collections::HashSet::new());
        for (name, port, proto) in ports {
            if !names.insert(name) {
                return Err(format!(
                    "port name `{name}` is used twice (names are cut to 15 characters; minecraft, rcon, voice and bedrock are reserved)"
                ));
            }
            if !numbers.insert((port, proto)) {
                return Err(format!("container port {port}/{} is used twice (port `{name}`)", proto.as_str()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_checks() {
        let port = |name: &str, container_port: u16| ExtraPort {
            name: name.into(),
            container_port,
            protocol: Protocol::Tcp,
            expose: Exposure::Gateway,
            public_port: None,
            host: None,
        };
        let with = |ports: Vec<ExtraPort>| MinecraftServerSpec { extra_ports: ports, ..Default::default() };
        assert!(with(vec![port("bluemap", 8100), port("dynmap", 8123)]).check_ports().is_ok());
        // Long names are cut to 15 characters, as before: one alone is fine...
        assert!(with(vec![port("bluemap-web-http", 8100)]).check_ports().is_ok());
        // ...but two that are cut to the same `bluemap-web-htt` collide.
        assert!(with(vec![port("bluemap-web-http", 8100), port("bluemap-web-https", 8101)]).check_ports().is_err());
        // The cut can leave an invalid name.
        assert!(with(vec![port("bluemap-webmap-", 8100)]).check_ports().is_err());
        assert!(with(vec![port("bluemap-webmap-x", 8100)]).check_ports().is_err());
        assert!(with(vec![port("BlueMap", 8100)]).check_ports().is_err());
        assert!(with(vec![port("8100", 8100)]).check_ports().is_err());
        assert!(with(vec![port("map", 8100), port("map", 8101)]).check_ports().is_err());
        assert!(with(vec![port("rcon", 8100)]).check_ports().is_err(), "built-in names are reserved");
        assert!(with(vec![port("a", 8100), port("b", 8100)]).check_ports().is_err());
        assert!(with(vec![port("game", MINECRAFT_PORT)]).check_ports().is_err());
        let mut voice = with(vec![ExtraPort { protocol: Protocol::Udp, ..port("vc", SIMPLE_VOICE_CHAT_PORT) }]);
        assert!(voice.check_ports().is_ok());
        voice.voice = Some(VoiceSpec { kind: VoiceKind::SimpleVoiceChat, install: true, public_port: None });
        assert!(voice.check_ports().is_err());
    }

    #[test]
    fn server_names() {
        assert!(check_server_name("lobby").is_ok());
        assert!(check_server_name("atm-10").is_ok());
        assert!(check_server_name("2b2t").is_err(), "Services must start with a letter");
        assert!(check_server_name("Lobby").is_err());
        assert!(check_server_name("lobby-").is_err());
        assert!(check_server_name(&"a".repeat(MAX_SERVER_NAME)).is_ok());
        assert!(check_server_name(&"a".repeat(MAX_SERVER_NAME + 1)).is_err());
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

    fn server(name: &str, created: i64, hosts: &[&str], default: bool) -> MinecraftServer {
        let mut spec = MinecraftServerSpec::default();
        spec.routing.default = default;
        let mut s = MinecraftServer::new(name, spec);
        s.metadata.namespace = Some("mc".into());
        s.metadata.creation_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::from_second(created).unwrap()));
        s.status = Some(MinecraftServerStatus { hostnames: hosts.iter().map(|h| h.to_string()).collect(), ..Default::default() });
        s
    }

    #[test]
    fn newer_server_loses_hostname_and_default() {
        let lobby = server("lobby", 100, &["play.example.com"], true);
        let copy = server("copy", 200, &["play.example.com", "copy.example.com"], true);
        let c = routing_conflicts(&copy, &copy.status.as_ref().unwrap().hostnames, [&lobby, &copy]);
        assert_eq!(c.hostnames, vec![("play.example.com".to_string(), "mc/lobby".to_string())]);
        assert_eq!(c.default.as_deref(), Some("mc/lobby"));
        assert!(c.message().contains("already used by mc/lobby"));
        // The older server keeps everything.
        assert!(routing_conflicts(&lobby, &lobby.status.as_ref().unwrap().hostnames, [&lobby, &copy]).is_empty());
    }

    #[test]
    fn same_age_is_decided_by_name() {
        let a = server("a", 100, &["x.example.com"], false);
        let b = server("b", 100, &["x.example.com"], false);
        assert!(routing_conflicts(&a, &["x.example.com".into()], [&b]).is_empty());
        assert_eq!(routing_conflicts(&b, &["x.example.com".into()], [&a]).hostnames.len(), 1);
    }
}

