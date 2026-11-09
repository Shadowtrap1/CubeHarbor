use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::Http;
use crate::crd::Loader;

pub const API: &str = "https://api.curseforge.com/v1";
pub const GAME_MINECRAFT: u32 = 432;
pub const CLASS_MODPACK: u32 = 4471;
pub const CLASS_MOD: u32 = 6;
pub const CLASS_BUKKIT_PLUGIN: u32 = 5;

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Mod {
    pub id: u64,
    pub name: String,
    pub slug: String,
    #[serde(default)]
    pub class_id: Option<u32>,
    #[serde(default)]
    pub logo: Option<Logo>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub download_count: f64,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Logo {
    pub url: String,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct File {
    pub id: u64,
    pub mod_id: u64,
    pub display_name: String,
    pub file_name: String,
    #[serde(default)]
    pub release_type: u8,
    #[serde(default)]
    pub file_date: String,
    #[serde(default)]
    pub download_url: Option<String>,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub hashes: Vec<FileHash>,
    #[serde(default)]
    pub server_pack_file_id: Option<u64>,
    #[serde(default)]
    pub is_server_pack: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct FileHash {
    pub value: String,
    /// 1 = sha1, 2 = md5
    pub algo: u8,
}

#[derive(Deserialize)]
struct Data<T> {
    data: T,
}

impl File {
    pub fn sha1(&self) -> Option<&str> {
        self.hashes.iter().find(|h| h.algo == 1).map(|h| h.value.as_str())
    }

    pub fn md5(&self) -> Option<&str> {
        self.hashes.iter().find(|h| h.algo == 2).map(|h| h.value.as_str())
    }

    /// Download URL, reconstructing the CDN path when the author disabled third-party downloads.
    pub fn url(&self) -> String {
        match &self.download_url {
            Some(u) if !u.is_empty() => u.clone(),
            _ => format!(
                "https://mediafilez.forgecdn.net/files/{}/{}/{}",
                self.id / 1000,
                self.id % 1000,
                super::modrinth::urlencode(&self.file_name)
            ),
        }
    }

    pub fn minecraft_version(&self) -> Option<&str> {
        self.game_versions
            .iter()
            .map(String::as_str)
            .find(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()) && v.contains('.'))
    }

    pub fn loader(&self) -> Option<Loader> {
        self.game_versions.iter().find_map(|v| match v.as_str() {
            "NeoForge" => Some(Loader::NeoForge),
            "Forge" => Some(Loader::Forge),
            "Fabric" => Some(Loader::Fabric),
            "Quilt" => Some(Loader::Quilt),
            _ => None,
        })
    }

    /// Newer CurseForge files carry environment tags; skip ones explicitly marked client-only.
    pub fn client_only(&self) -> bool {
        let has = |t: &str| self.game_versions.iter().any(|v| v == t);
        has("Client") && !has("Server")
    }
}

fn key(http: &Http) -> Result<&str> {
    http.curseforge_key
        .as_deref()
        .context("CurseForge modpacks need an API key (set curseforge.apiKey in the Helm values)")
}

async fn get<T: serde::de::DeserializeOwned>(http: &Http, url: &str) -> Result<T> {
    let resp = http.send(http.client.get(url).header("x-api-key", key(http)?)).await.with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url}: HTTP {}", resp.status());
    }
    Ok(resp.json::<Data<T>>().await?.data)
}

async fn post<T: serde::de::DeserializeOwned>(http: &Http, url: &str, body: serde_json::Value) -> Result<T> {
    let resp = http.send(http.client.post(url).header("x-api-key", key(http)?).json(&body)).await.with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        bail!("POST {url}: HTTP {}", resp.status());
    }
    Ok(resp.json::<Data<T>>().await?.data)
}

/// Look up a project by numeric id or slug.
pub async fn find_mod(http: &Http, project: &str, class_id: u32) -> Result<Mod> {
    if let Ok(id) = project.parse::<u64>() {
        return get(http, &format!("{API}/mods/{id}")).await;
    }
    let url = format!("{API}/mods/search?gameId={GAME_MINECRAFT}&classId={class_id}&slug={}", super::modrinth::urlencode(project));
    let mods: Vec<Mod> = get(http, &url).await?;
    mods.into_iter().next().with_context(|| format!("CurseForge project {project} not found"))
}

pub async fn file(http: &Http, mod_id: u64, file_id: u64) -> Result<File> {
    get(http, &format!("{API}/mods/{mod_id}/files/{file_id}")).await
}

/// Newest non-server-pack file, preferring releases.
pub async fn latest_file(http: &Http, mod_id: u64) -> Result<File> {
    let mut files: Vec<File> = get(http, &format!("{API}/mods/{mod_id}/files?pageSize=50")).await?;
    files.retain(|f| !f.is_server_pack.unwrap_or(false));
    files.sort_by(|a, b| b.file_date.cmp(&a.file_date));
    files
        .iter()
        .find(|f| f.release_type == 1)
        .or_else(|| files.first())
        .cloned()
        .context("project has no files")
}

pub async fn files(http: &Http, ids: &[u64]) -> Result<Vec<File>> {
    let mut out = vec![];
    for chunk in ids.chunks(500) {
        let mut part: Vec<File> = post(http, &format!("{API}/mods/files"), serde_json::json!({ "fileIds": chunk })).await?;
        out.append(&mut part);
    }
    Ok(out)
}

pub async fn mods(http: &Http, ids: &[u64]) -> Result<Vec<Mod>> {
    let mut out = vec![];
    for chunk in ids.chunks(500) {
        let mut part: Vec<Mod> = post(http, &format!("{API}/mods"), serde_json::json!({ "modIds": chunk })).await?;
        out.append(&mut part);
    }
    Ok(out)
}

/// `manifest.json` inside a CurseForge client pack.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub minecraft: ManifestMinecraft,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    pub files: Vec<ManifestFile>,
    #[serde(default = "default_overrides")]
    pub overrides: String,
}

fn default_overrides() -> String {
    "overrides".into()
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ManifestMinecraft {
    pub version: String,
    #[serde(default)]
    pub mod_loaders: Vec<ManifestLoader>,
}

#[derive(Deserialize, Debug)]
pub struct ManifestLoader {
    pub id: String,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFile {
    #[serde(rename = "projectID")]
    pub project_id: u64,
    #[serde(rename = "fileID")]
    pub file_id: u64,
    #[serde(default = "yes")]
    pub required: bool,
}

fn yes() -> bool {
    true
}

impl Manifest {
    /// Parse `forge-47.2.0` / `neoforge-21.1.77` / `fabric-0.16.9`.
    pub fn loader(&self) -> Option<(Loader, String)> {
        let l = self.minecraft.mod_loaders.iter().find(|l| l.primary).or(self.minecraft.mod_loaders.first())?;
        let (name, version) = l.id.split_once('-')?;
        Some((Loader::parse(name)?, version.to_string()))
    }
}

/// Search modpacks by name, most popular first.
pub async fn search_modpacks(http: &Http, query: &str, limit: u32) -> Result<Vec<Mod>> {
    let url = format!(
        "{API}/mods/search?gameId={GAME_MINECRAFT}&classId={CLASS_MODPACK}&searchFilter={}&sortField=2&sortOrder=desc&pageSize={limit}",
        super::modrinth::urlencode(query)
    );
    get(http, &url).await
}
