//! Extra mods/plugins from Modrinth or URLs, with dependency resolution and de-duplication
//! against whatever the modpack already ships.

use std::collections::{HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use cubeharbor_core::Loader;
use cubeharbor_core::crd::ModRef;
use cubeharbor_core::sources::modrinth;
use sha1::{Digest, Sha1};

use crate::Ctx;
use crate::download::{Item, fetch_all, hex, safe_join};

/// Install `mods`, returning the relative paths written.
pub async fn install(
    ctx: &Ctx,
    mods: &[ModRef],
    mc: &str,
    loader: Loader,
    skip_files: &HashSet<String>,
) -> Result<Vec<String>> {
    if mods.is_empty() {
        return Ok(vec![]);
    }
    let default_dir = loader.mods_dir();
    let present = present_projects(ctx, &ctx.root.join(default_dir), skip_files).await;

    let mut items = vec![];
    let mut files = vec![];
    let mut seen: HashSet<String> = present;
    // (project, pinned version, dir, explicitly requested)
    let mut queue: VecDeque<(String, Option<String>, String, bool)> = VecDeque::new();

    for m in mods {
        let dir = m.dir.clone().unwrap_or_else(|| default_dir.to_string());
        if let Some(url) = &m.url {
            // A `#file.jar` fragment names the file when the URL path doesn't.
            let (url, fragment) = url
                .split_once('#')
                .map_or((url.as_str(), None), |(u, f)| (u, Some(f)));
            let name = fragment.unwrap_or_else(|| {
                url.split('?')
                    .next()
                    .unwrap_or(url)
                    .rsplit('/')
                    .next()
                    .unwrap_or("mod.jar")
            });
            let name = if name.ends_with(".jar") || name.ends_with(".zip") {
                name.to_string()
            } else {
                format!("{name}.jar")
            };
            let rel = format!("{dir}/{name}");
            items.push(Item::new(url, safe_join(&ctx.root, &rel)?));
            files.push(rel);
        } else if let Some(p) = &m.modrinth {
            queue.push_back((p.clone(), m.version.clone(), dir, true));
        }
    }

    while let Some((project, pinned, dir, explicit)) = queue.pop_front() {
        let v = match modrinth::compatible_version(
            &ctx.http,
            &project,
            loader.modrinth_name(),
            mc,
            pinned.as_deref(),
        )
        .await
        {
            Ok(v) => v,
            Err(e) if !explicit => {
                tracing::warn!("optional dependency {project}: {e:#}");
                continue;
            }
            Err(e) => return Err(e.context(format!("mod {project}"))),
        };
        if !seen.insert(v.project_id.clone()) {
            tracing::info!("  {project} already provided, skipping");
            continue;
        }
        let file = v
            .primary_file()
            .with_context(|| format!("{project} has no files"))?;
        let rel = format!("{dir}/{}", file.filename);
        tracing::info!("  {project} {} -> {rel}", v.version_number);
        items.push(Item {
            urls: vec![file.url.clone()],
            dest: safe_join(&ctx.root, &rel)?,
            sha1: file.hashes.sha1.clone(),
            sha512: file.hashes.sha512.clone(),
            ..Default::default()
        });
        files.push(rel);
        for d in v
            .dependencies
            .iter()
            .filter(|d| d.dependency_type == "required")
        {
            if let Some(pid) = &d.project_id
                && !seen.contains(pid)
            {
                queue.push_back((pid.clone(), d.version_id.clone(), dir.clone(), false));
            }
        }
    }

    fetch_all(ctx, items, "extra mods").await?;
    Ok(files)
}

/// Modrinth project IDs of jars already in `dir` (excluding ones we manage ourselves).
async fn present_projects(ctx: &Ctx, dir: &Path, skip_files: &HashSet<String>) -> HashSet<String> {
    let mut hashes = vec![];
    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let name = e.file_name().to_string_lossy().to_string();
            let rel = format!(
                "{}/{name}",
                dir.file_name().unwrap_or_default().to_string_lossy()
            );
            if !name.ends_with(".jar") || skip_files.contains(&rel) {
                continue;
            }
            if let Ok(data) = tokio::fs::read(e.path()).await {
                hashes.push(hex(&Sha1::digest(&data)));
            }
        }
    }
    match modrinth::versions_by_hash(&ctx.http, &hashes).await {
        Ok(map) => map.into_values().map(|v| v.project_id).collect(),
        Err(e) => {
            tracing::warn!("could not identify installed mods: {e:#}");
            HashSet::new()
        }
    }
}
