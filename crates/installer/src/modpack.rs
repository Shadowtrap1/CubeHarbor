//! Modpack installation: Modrinth `.mrpack`, CurseForge (server pack or manifest), raw server zips.

use std::collections::HashMap;

use anyhow::{Context, Result};
use cubeharbor_core::Loader;
use cubeharbor_core::crd::{ModpackKind, ResolvedModpack};
use cubeharbor_core::sources::{curseforge, modrinth};

use crate::Ctx;
use crate::archive;
use crate::download::{Item, fetch_all, fetch_temp, safe_join};

/// Mods that are client-only but often not flagged as such by pack authors, and crash dedicated servers.
const CLIENT_ONLY: &[&str] = &[
    "oculus", "embeddium", "rubidium", "optifine", "iris-", "sodium-", "sodiumextra", "reeses-sodium", "entityculling",
    "legendarytooltips", "fancymenu", "drippyloadingscreen", "betterf3", "skinlayers3d", "3dskinlayers",
    "notenoughanimations", "itemphysiclite", "ambientsounds", "watermedia", "cherishedworlds", "loadingscreens",
    "konkrete", "mainmenucredits", "betterpingdisplay", "torohealth", "physicsmod", "dynamiclights",
];

/// Hosts `.mrpack` files may download from, per the Modrinth pack format. Anything else could
/// point the installer at cluster-internal services or cloud metadata endpoints.
const MRPACK_HOSTS: &[&str] = &["cdn.modrinth.com", "github.com", "raw.githubusercontent.com", "gitlab.com"];

/// Keep only download URLs the pack format allows (HTTPS on `MRPACK_HOSTS`), unless
/// `CUBEHARBOR_MRPACK_ANY_HOST` is set.
fn allowed_downloads(path: &str, urls: &[String], any_host: bool) -> Result<Vec<String>> {
    if any_host {
        return Ok(urls.to_vec());
    }
    let allowed = |u: &String| {
        cubeharbor_core::reqwest::Url::parse(u)
            .is_ok_and(|u| u.scheme() == "https" && u.host_str().is_some_and(|h| MRPACK_HOSTS.contains(&h)) && u.port().is_none())
    };
    let ok: Vec<String> = urls.iter().filter(|u| allowed(u)).cloned().collect();
    for u in urls.iter().filter(|u| !allowed(u)) {
        tracing::warn!("  {path}: ignoring download URL outside the allowed hosts: {u}");
    }
    anyhow::ensure!(
        !ok.is_empty(),
        "{path}: no download URL on an allowed host ({}); set CUBEHARBOR_MRPACK_ANY_HOST=true in the server's env to allow any",
        MRPACK_HOSTS.join(", ")
    );
    Ok(ok)
}

pub struct PackResult {
    pub minecraft_version: Option<String>,
    pub loader: Option<(Loader, String)>,
    /// Files placed by the pack (relative paths).
    pub files: Vec<String>,
    /// The pack shipped a ready-to-run server (server pack / server zip).
    pub server_pack: bool,
}

/// Exclusion match: filename (last path segment) starts with or contains the pattern.
pub fn excluded(path_lower: &str, pattern: &str) -> bool {
    let name = path_lower.rsplit('/').next().unwrap_or(path_lower);
    let p = pattern.to_ascii_lowercase();
    !p.is_empty() && name.contains(&p)
}

fn is_client_only(path_lower: &str) -> bool {
    let name = path_lower.rsplit('/').next().unwrap_or(path_lower);
    CLIENT_ONLY.iter().any(|c| name.starts_with(c))
}

/// Files that are client-only according to their own Modrinth project (`server_side: unsupported`),
/// the built-in list, or that live in client-only folders. Pack authors frequently forget to flag
/// these, and they crash dedicated servers.
async fn client_only_files(ctx: &Ctx, files: &[(String, Option<String>)], include: &[String]) -> std::collections::HashSet<String> {
    let kept = |path: &str| include.iter().any(|i| excluded(&path.to_ascii_lowercase(), i));
    let mut out: std::collections::HashSet<String> = files
        .iter()
        .filter(|(p, _)| {
            let l = p.to_ascii_lowercase();
            l.starts_with("resourcepacks/") || l.starts_with("shaderpacks/") || is_client_only(&l)
        })
        .map(|(p, _)| p.clone())
        .collect();

    let by_hash: HashMap<String, String> = files.iter().filter_map(|(p, h)| Some((h.clone()?, p.clone()))).collect();
    let hashes: Vec<String> = by_hash.keys().cloned().collect();
    let lookup = async {
        let versions = modrinth::versions_by_hash(&ctx.http, &hashes).await?;
        let ids: Vec<String> = versions.values().map(|v| v.project_id.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        let mut client: std::collections::HashSet<String> = modrinth::projects(&ctx.http, &ids)
            .await?
            .into_iter()
            .filter(|p| p.server_side.as_deref() == Some("unsupported"))
            .map(|p| p.id)
            .collect();
        // Never drop something a kept mod requires: project metadata is sometimes wrong too.
        loop {
            let required: std::collections::HashSet<&String> = versions
                .values()
                .filter(|v| !client.contains(&v.project_id))
                .flat_map(|v| &v.dependencies)
                .filter(|d| d.dependency_type == "required")
                .filter_map(|d| d.project_id.as_ref())
                .collect();
            let rescued: Vec<String> = client.iter().filter(|c| required.contains(c)).cloned().collect();
            if rescued.is_empty() {
                break;
            }
            for r in rescued {
                client.remove(&r);
            }
        }
        anyhow::Ok(versions.into_iter().filter(|(_, v)| client.contains(&v.project_id)).filter_map(|(h, _)| by_hash.get(&h).cloned()).collect::<Vec<_>>())
    };
    match lookup.await {
        Ok(paths) => out.extend(paths),
        Err(e) => tracing::warn!("could not check mods against Modrinth metadata: {e:#}"),
    }
    out.retain(|p| !kept(p));
    if !out.is_empty() {
        let mut names: Vec<&str> = out.iter().map(|p| p.rsplit('/').next().unwrap_or(p)).collect();
        names.sort_unstable();
        tracing::info!("skipping {} client-only files: {}", out.len(), names.join(", "));
    }
    out
}

pub async fn install(ctx: &Ctx, pack: &ResolvedModpack, exclude: &[String], include: &[String]) -> Result<PackResult> {
    tracing::info!("installing modpack {} {}", pack.name, pack.version);
    let ext = if pack.kind == ModpackKind::Mrpack { "mrpack" } else { "zip" };
    let archive = fetch_temp(ctx, &pack.url, &format!("modpack.{ext}"), pack.sha1.clone()).await?;

    if archive::read_entry(&archive, "modrinth.index.json").await?.is_some() {
        return mrpack(ctx, &archive, exclude, include).await;
    }
    match pack.kind {
        ModpackKind::CurseForge => curseforge_pack(ctx, pack, &archive, exclude, include).await,
        _ => {
            let files = archive::extract(&archive, "", &ctx.root, true, exclude.to_vec()).await?;
            Ok(PackResult { minecraft_version: None, loader: None, files, server_pack: true })
        }
    }
}

async fn mrpack(ctx: &Ctx, archive: &std::path::Path, exclude: &[String], include: &[String]) -> Result<PackResult> {
    let raw = archive::read_entry(archive, "modrinth.index.json").await?.context("missing modrinth.index.json")?;
    let index: modrinth::PackIndex = serde_json::from_slice(&raw).context("parsing modrinth.index.json")?;
    tracing::info!("{} — {} files", index.name, index.files.len());

    let candidates: Vec<(String, Option<String>)> = index
        .files
        .iter()
        .filter(|f| f.env.as_ref().and_then(|e| e.server.as_deref()) != Some("unsupported"))
        .map(|f| (f.path.clone(), f.hashes.sha1.clone()))
        .collect();
    let client_only = client_only_files(ctx, &candidates, include).await;

    let any_host = crate::env_flag("CUBEHARBOR_MRPACK_ANY_HOST");
    let mut items = vec![];
    let mut files = vec![];
    for f in &index.files {
        let lower = f.path.to_ascii_lowercase();
        if f.env.as_ref().and_then(|e| e.server.as_deref()) == Some("unsupported") {
            continue;
        }
        if client_only.contains(&f.path) {
            continue;
        }
        if exclude.iter().any(|e| excluded(&lower, e)) {
            tracing::info!("  skipping {} (excluded)", f.path);
            continue;
        }
        let dest = safe_join(&ctx.root, &f.path)?;
        items.push(Item {
            urls: allowed_downloads(&f.path, &f.downloads, any_host)?,
            dest,
            sha1: f.hashes.sha1.clone(),
            sha256: None,
            sha512: f.hashes.sha512.clone(),
        });
        files.push(f.path.trim_start_matches('/').to_string());
    }
    fetch_all(ctx, items, "pack files").await?;

    for prefix in ["overrides/", "server-overrides/"] {
        let mut written = archive::extract(archive, prefix, &ctx.root, false, exclude.to_vec()).await?;
        files.append(&mut written);
    }

    Ok(PackResult {
        minecraft_version: index.minecraft_version().map(str::to_string),
        loader: index.loader(),
        files,
        server_pack: false,
    })
}

async fn curseforge_pack(
    ctx: &Ctx,
    pack: &ResolvedModpack,
    archive: &std::path::Path,
    exclude: &[String],
    include: &[String],
) -> Result<PackResult> {
    let raw = archive::read_entry(archive, "manifest.json").await?.context("CurseForge pack has no manifest.json")?;
    let manifest: curseforge::Manifest = serde_json::from_slice(&raw).context("parsing manifest.json")?;
    let loader = manifest.loader();
    let mc = Some(manifest.minecraft.version.clone());

    // Prefer the author's server pack: it is curated for dedicated servers.
    if let Some(url) = &pack.server_pack_url {
        tracing::info!("using the published server pack");
        let sp = fetch_temp(ctx, url, "serverpack.zip", None).await?;
        let files = archive::extract(&sp, "", &ctx.root, true, exclude.to_vec()).await?;
        return Ok(PackResult { minecraft_version: mc, loader, files, server_pack: true });
    }

    tracing::info!("building server from manifest ({} mods)", manifest.files.len());
    let ids: Vec<u64> = manifest.files.iter().filter(|f| f.required).map(|f| f.file_id).collect();
    let infos = curseforge::files(&ctx.http, &ids).await?;
    let mod_ids: Vec<u64> = infos.iter().map(|f| f.mod_id).collect();
    let classes: HashMap<u64, Option<u32>> =
        curseforge::mods(&ctx.http, &mod_ids).await?.into_iter().map(|m| (m.id, m.class_id)).collect();

    let candidates: Vec<(String, Option<String>)> = infos.iter().map(|f| (f.file_name.clone(), f.sha1().map(str::to_string))).collect();
    let client_only = client_only_files(ctx, &candidates, include).await;

    let mut items = vec![];
    let mut files = vec![];
    for f in &infos {
        let lower = f.file_name.to_ascii_lowercase();
        if client_only.contains(&f.file_name) {
            continue;
        }
        let class = classes.get(&f.mod_id).copied().flatten();
        let dir = match class {
            Some(curseforge::CLASS_MOD) | None => "mods",
            Some(curseforge::CLASS_BUKKIT_PLUGIN) => "plugins",
            // Resource packs, shaders and worlds are client-side.
            Some(_) => continue,
        };
        if (f.client_only() || is_client_only(&lower)) && !include.iter().any(|i| excluded(&lower, i)) {
            tracing::info!("  skipping {} (client-only)", f.file_name);
            continue;
        }
        if exclude.iter().any(|e| excluded(&lower, e)) {
            tracing::info!("  skipping {} (excluded)", f.file_name);
            continue;
        }
        let rel = format!("{dir}/{}", f.file_name);
        items.push(Item { urls: vec![f.url()], dest: safe_join(&ctx.root, &rel)?, sha1: f.sha1().map(str::to_string), ..Default::default() });
        files.push(rel);
    }
    fetch_all(ctx, items, "mods").await?;

    let prefix = format!("{}/", manifest.overrides.trim_end_matches('/'));
    let mut written = archive::extract(archive, &prefix, &ctx.root, false, exclude.to_vec()).await?;
    files.append(&mut written);
    Ok(PackResult { minecraft_version: mc, loader, files, server_pack: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusion() {
        assert!(excluded("mods/oculus-1.6.jar", "oculus"));
        assert!(!excluded("config/oculus.toml/mods", "jei"));
        assert!(is_client_only("mods/embeddium-0.3.jar"));
        assert!(!is_client_only("mods/create-0.5.jar"));
    }

    #[test]
    fn mrpack_download_hosts() {
        let urls = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let ok = urls(&["https://cdn.modrinth.com/data/AANobbMI/versions/x/sodium.jar"]);
        assert_eq!(allowed_downloads("mods/a.jar", &ok, false).unwrap(), ok);
        // Disallowed mirrors are dropped as long as one allowed URL remains.
        let mixed = urls(&["http://169.254.169.254/latest/meta-data/", "https://github.com/o/r/releases/download/v1/a.jar"]);
        assert_eq!(allowed_downloads("mods/a.jar", &mixed, false).unwrap(), urls(&["https://github.com/o/r/releases/download/v1/a.jar"]));
        for bad in [
            "http://cdn.modrinth.com/a.jar",
            "https://cdn.modrinth.com.evil.example/a.jar",
            "https://evil.example/cdn.modrinth.com/a.jar",
            "https://cdn.modrinth.com:8443/a.jar",
            "https://some-svc.other-ns.svc:8080/admin",
        ] {
            assert!(allowed_downloads("mods/a.jar", &urls(&[bad]), false).is_err(), "{bad}");
        }
        assert!(allowed_downloads("mods/a.jar", &urls(&["https://my.mirror.example/a.jar"]), true).is_ok());
    }
}
