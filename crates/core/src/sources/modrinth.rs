use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::Http;

pub const API: &str = "https://api.modrinth.com/v2";

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Project {
    pub id: String,
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub project_type: String,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub server_side: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Version {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub version_number: String,
    pub version_type: String,
    pub game_versions: Vec<String>,
    pub loaders: Vec<String>,
    pub files: Vec<VersionFile>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct VersionFile {
    pub url: String,
    pub filename: String,
    #[serde(default)]
    pub primary: bool,
    pub hashes: Hashes,
    #[serde(default)]
    pub size: u64,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct Hashes {
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub sha512: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Dependency {
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    pub dependency_type: String,
}

impl Version {
    pub fn primary_file(&self) -> Option<&VersionFile> {
        self.files.iter().find(|f| f.primary).or_else(|| self.files.first())
    }
}

pub async fn project(http: &Http, id_or_slug: &str) -> Result<Project> {
    http.get_json(&format!("{API}/project/{}", urlencode(id_or_slug))).await
}

/// Versions of a project, newest first, optionally filtered.
pub async fn versions(http: &Http, project: &str, loaders: &[&str], game_versions: &[&str]) -> Result<Vec<Version>> {
    let mut url = format!("{API}/project/{}/version", urlencode(project));
    let mut q = vec![];
    if !loaders.is_empty() {
        q.push(format!("loaders={}", urlencode(&serde_json::to_string(loaders)?)));
    }
    if !game_versions.is_empty() {
        q.push(format!("game_versions={}", urlencode(&serde_json::to_string(game_versions)?)));
    }
    if !q.is_empty() {
        url.push('?');
        url.push_str(&q.join("&"));
    }
    http.get_json(&url).await
}

pub async fn version(http: &Http, id: &str) -> Result<Version> {
    http.get_json(&format!("{API}/version/{}", urlencode(id))).await
}

/// Pick a version: exact id / version number when pinned, otherwise the newest release
/// (falling back to the newest of any type).
pub fn pick<'a>(versions: &'a [Version], wanted: Option<&str>) -> Result<&'a Version> {
    if let Some(w) = wanted.filter(|w| !w.is_empty() && *w != "latest") {
        return versions
            .iter()
            .find(|v| v.id == w || v.version_number == w || v.name == w)
            .with_context(|| format!("version {w} not found"));
    }
    match versions.iter().find(|v| v.version_type == "release").or_else(|| versions.first()) {
        Some(v) => Ok(v),
        None => bail!("no versions available"),
    }
}

/// Find the best version of a mod for a loader + game version.
pub async fn compatible_version(
    http: &Http,
    project: &str,
    loader: &str,
    game_version: &str,
    pinned: Option<&str>,
) -> Result<Version> {
    // Paper/Purpur servers can run Bukkit/Spigot plugins too.
    let loaders: Vec<&str> = match loader {
        "paper" => vec!["paper", "spigot", "bukkit"],
        "purpur" => vec!["purpur", "paper", "spigot", "bukkit"],
        "quilt" => vec!["quilt", "fabric"],
        other => vec![other],
    };
    let list = versions(http, project, &loaders, &[game_version]).await?;
    if list.is_empty() {
        bail!("{project} has no {loader} build for Minecraft {game_version}");
    }
    pick(&list, pinned).cloned()
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct SearchHit {
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub latest_version: Option<String>,
}

#[derive(Deserialize)]
struct SearchResp {
    hits: Vec<SearchHit>,
}

pub async fn search(http: &Http, query: &str, project_type: &str, limit: u32) -> Result<Vec<SearchHit>> {
    let facets = format!("[[\"project_type:{project_type}\"],[\"server_side!=unsupported\"]]");
    let url = format!(
        "{API}/search?query={}&facets={}&limit={limit}&index=relevance",
        urlencode(query),
        urlencode(&facets)
    );
    Ok(http.get_json::<SearchResp>(&url).await?.hits)
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `modrinth.index.json` inside an `.mrpack`.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PackIndex {
    pub name: String,
    #[serde(default)]
    pub version_id: String,
    pub files: Vec<PackFile>,
    pub dependencies: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PackFile {
    pub path: String,
    pub hashes: Hashes,
    #[serde(default)]
    pub env: Option<PackEnv>,
    pub downloads: Vec<String>,
    #[serde(default)]
    pub file_size: u64,
}

#[derive(Deserialize, Debug)]
pub struct PackEnv {
    #[serde(default)]
    pub server: Option<String>,
}

impl PackIndex {
    pub fn minecraft_version(&self) -> Option<&str> {
        self.dependencies.get("minecraft").map(String::as_str)
    }

    /// (loader, version) declared by the pack.
    pub fn loader(&self) -> Option<(crate::crd::Loader, String)> {
        use crate::crd::Loader;
        for (key, loader) in [
            ("neoforge", Loader::NeoForge),
            ("forge", Loader::Forge),
            ("fabric-loader", Loader::Fabric),
            ("quilt-loader", Loader::Quilt),
        ] {
            if let Some(v) = self.dependencies.get(key) {
                return Some((loader, v.clone()));
            }
        }
        None
    }
}

/// Identify files by SHA-1 (used to detect mods a modpack already ships).
pub async fn versions_by_hash(http: &Http, sha1s: &[String]) -> Result<std::collections::HashMap<String, Version>> {
    if sha1s.is_empty() {
        return Ok(Default::default());
    }
    let resp = http.send(http.client.post(format!("{API}/version_files")).json(&serde_json::json!({ "hashes": sha1s, "algorithm": "sha1" }))).await?;
    if !resp.status().is_success() {
        bail!("Modrinth hash lookup: HTTP {}", resp.status());
    }
    Ok(resp.json().await?)
}

/// Fetch several projects at once.
pub async fn projects(http: &Http, ids: &[String]) -> Result<Vec<Project>> {
    let mut out = vec![];
    for chunk in ids.chunks(400) {
        let url = format!("{API}/projects?ids={}", urlencode(&serde_json::to_string(chunk)?));
        out.extend(http.get_json::<Vec<Project>>(&url).await?);
    }
    Ok(out)
}
