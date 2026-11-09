use anyhow::{Context, Result};
use serde::Deserialize;

use super::Http;

const MANIFEST: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

#[derive(Deserialize)]
pub struct Manifest {
    pub latest: Latest,
    pub versions: Vec<ManifestEntry>,
}

#[derive(Deserialize)]
pub struct Latest {
    pub release: String,
    pub snapshot: String,
}

#[derive(Deserialize)]
pub struct ManifestEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    pub id: String,
    pub java_version: Option<JavaVersion>,
    pub downloads: Downloads,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JavaVersion {
    pub major_version: u8,
}

#[derive(Deserialize)]
pub struct Downloads {
    pub server: Option<Download>,
}

#[derive(Deserialize)]
pub struct Download {
    pub url: String,
    pub sha1: String,
    pub size: u64,
}

pub async fn manifest(http: &Http) -> Result<Manifest> {
    http.get_json(MANIFEST).await
}

/// Resolve `latest` / `snapshot` aliases to a concrete version id.
pub fn resolve_alias(m: &Manifest, v: &str) -> String {
    match v {
        "" | "latest" | "release" => m.latest.release.clone(),
        "snapshot" | "latest-snapshot" => m.latest.snapshot.clone(),
        other => other.to_string(),
    }
}

pub async fn version_info(http: &Http, version: &str) -> Result<VersionInfo> {
    let m = manifest(http).await?;
    let id = resolve_alias(&m, version);
    let entry = m
        .versions
        .iter()
        .find(|e| e.id == id)
        .with_context(|| format!("unknown Minecraft version {id}"))?;
    http.get_json(&entry.url).await
}

/// Mojang profile lookup for ops/whitelist files.
#[derive(Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
}

pub async fn profile(http: &Http, name: &str) -> Result<Profile> {
    anyhow::ensure!(
        valid_player_name(name),
        "`{name}` is not a valid Java Edition player name"
    );
    http.get_json(&format!(
        "https://api.mojang.com/users/profiles/minecraft/{name}"
    ))
    .await
}

/// Java Edition names: 1-16 letters, digits or `_`. (Bedrock players via Floodgate have a prefix
/// and no Mojang profile, so they can't be resolved here either.)
pub fn valid_player_name(name: &str) -> bool {
    (1..=16).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `0123...` -> `01234567-89ab-...`
pub fn dashed_uuid(id: &str) -> String {
    if id.len() != 32 {
        return id.to_string();
    }
    format!(
        "{}-{}-{}-{}-{}",
        &id[0..8],
        &id[8..12],
        &id[12..16],
        &id[16..20],
        &id[20..32]
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn player_names() {
        for ok in ["Notch", "jeb_", "a", "Sixteen_chars_ab"] {
            assert!(super::valid_player_name(ok), "{ok}");
        }
        for bad in [
            "",
            "seventeen_chars_x",
            "a/b",
            "../x",
            "a b",
            ".BedrockGuy",
            "ä",
        ] {
            assert!(!super::valid_player_name(bad), "{bad}");
        }
    }
}
