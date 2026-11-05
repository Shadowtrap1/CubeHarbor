//! The install plan: what the operator hands to the in-pod installer (as JSON in a ConfigMap).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::crd::{FileSpec, Loader, ModRef, ResolvedModpack, VoiceKind};

pub const PLAN_PATH: &str = "/etc/cubeharbor/plan.json";
pub const STATE_DIR: &str = ".cubeharbor";
pub const LAUNCH_FILE: &str = ".cubeharbor/launch.json";
pub const STATE_FILE: &str = ".cubeharbor/state.json";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InstallPlan {
    pub server_name: String,
    pub minecraft_version: String,
    pub loader: Loader,
    #[serde(default)]
    pub loader_version: Option<String>,
    #[serde(default)]
    pub modpack: Option<ResolvedModpack>,
    #[serde(default)]
    pub modpack_exclude: Vec<String>,
    #[serde(default)]
    pub modpack_include: Vec<String>,
    #[serde(default)]
    pub mods: Vec<ModRef>,
    pub accept_eula: bool,
    #[serde(default)]
    pub motd: Option<String>,
    #[serde(default)]
    pub ops: Vec<String>,
    #[serde(default)]
    pub whitelist: Vec<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
    #[serde(default)]
    pub files: Vec<FileSpec>,
    #[serde(default)]
    pub voice: Option<VoicePlan>,
    #[serde(default)]
    pub geyser: Option<GeyserPlan>,
    #[serde(default)]
    pub proxy_protocol: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VoicePlan {
    pub kind: VoiceKind,
    pub container_port: u16,
    /// `host:port` clients should use for voice traffic.
    #[serde(default)]
    pub public_address: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GeyserPlan {
    pub container_port: u16,
    #[serde(default)]
    pub public_port: Option<u16>,
}

/// How the runner starts the server, written by the installer.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LaunchSpec {
    /// Arguments placed after JVM flags, e.g. `["-jar", "server.jar"]` or `["@libraries/.../unix_args.txt"]`.
    pub java_args: Vec<String>,
    /// Arguments passed to the server itself.
    #[serde(default)]
    pub program_args: Vec<String>,
    #[serde(default)]
    pub required_java: Option<u8>,
}

/// Persisted installer state, used to make re-installs incremental.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct InstallState {
    /// Hash of everything that affects the base install (version, loader, modpack).
    #[serde(default)]
    pub base_hash: String,
    /// Hash of the extra-mod list.
    #[serde(default)]
    pub mods_hash: String,
    /// Files placed by the modpack (relative paths), removed when the pack changes.
    #[serde(default)]
    pub pack_files: Vec<String>,
    /// Files placed by the extra-mod list.
    #[serde(default)]
    pub mod_files: Vec<String>,
    /// What the base install ended up being (a modpack may override the plan).
    #[serde(default)]
    pub minecraft_version: Option<String>,
    #[serde(default)]
    pub loader: Option<Loader>,
    /// Players (lower-case) the spec put into ops.json / whitelist.json, so they can be removed
    /// again when they leave the spec. Players added in-game are never touched. `None` means the
    /// state predates this tracking: the spec's names already in the file are then adopted.
    #[serde(default)]
    pub managed_ops: Option<Vec<String>>,
    #[serde(default)]
    pub managed_whitelist: Option<Vec<String>>,
}

/// Stable FNV-1a hash used for change detection (not security).
pub fn stable_hash(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

pub fn hash_json<T: Serialize>(v: &T) -> String {
    stable_hash(&serde_json::to_vec(v).unwrap_or_default())
}
