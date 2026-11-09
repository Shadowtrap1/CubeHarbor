//! CurseForge packs: the author's server pack when there is one, otherwise built from the manifest.

use std::collections::HashMap;

use anyhow::{Context, Result};
use cubeharbor_core::crd::ModpackKind;
use cubeharbor_core::sources::curseforge;
use futures::future::BoxFuture;

use super::{PackFormat, PackInput, PackResult, client_only_files, excluded, is_client_only};
use crate::Ctx;
use crate::archive;
use crate::download::{Item, fetch_all, fetch_temp, safe_join};

/// `manifest.json`'s `overrides` folder, as an archive prefix.
fn overrides_prefix(overrides: &str) -> Result<String> {
    let dir = overrides.replace('\\', "/");
    anyhow::ensure!(
        !dir.starts_with('/') && !dir.split('/').any(|p| p == ".."),
        "invalid overrides folder `{overrides}`"
    );
    Ok(format!("{}/", dir.trim_end_matches('/')))
}

async fn install(ctx: &Ctx, input: &PackInput<'_>) -> Result<PackResult> {
    let PackInput {
        pack,
        archive,
        exclude,
        include,
    } = *input;
    let raw = archive::read_entry(archive, "manifest.json")
        .await?
        .context("CurseForge pack has no manifest.json")?;
    let manifest: curseforge::Manifest =
        serde_json::from_slice(&raw).context("parsing manifest.json")?;
    let loader = manifest.loader();
    let mc = Some(manifest.minecraft.version.clone());

    // Prefer the author's server pack: it is curated for dedicated servers.
    if let Some(url) = &pack.server_pack_url {
        tracing::info!("using the published server pack");
        let sp = fetch_temp(ctx, url, "serverpack.zip", None).await?;
        let files = archive::extract(&sp, "", &ctx.root, true, exclude.to_vec()).await?;
        return Ok(PackResult {
            minecraft_version: mc,
            loader,
            files,
            server_pack: true,
        });
    }

    tracing::info!(
        "building server from manifest ({} mods)",
        manifest.files.len()
    );
    let ids: Vec<u64> = manifest
        .files
        .iter()
        .filter(|f| f.required)
        .map(|f| f.file_id)
        .collect();
    let infos = curseforge::files(&ctx.http, &ids).await?;
    let mod_ids: Vec<u64> = infos.iter().map(|f| f.mod_id).collect();
    let classes: HashMap<u64, Option<u32>> = curseforge::mods(&ctx.http, &mod_ids)
        .await?
        .into_iter()
        .map(|m| (m.id, m.class_id))
        .collect();

    let candidates: Vec<(String, Option<String>)> = infos
        .iter()
        .map(|f| (f.file_name.clone(), f.sha1().map(str::to_string)))
        .collect();
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
        if (f.client_only() || is_client_only(&lower))
            && !include.iter().any(|i| excluded(&lower, i))
        {
            tracing::info!("  skipping {} (client-only)", f.file_name);
            continue;
        }
        if exclude.iter().any(|e| excluded(&lower, e)) {
            tracing::info!("  skipping {} (excluded)", f.file_name);
            continue;
        }
        let rel = format!("{dir}/{}", f.file_name);
        items.push(Item {
            urls: vec![f.url()],
            dest: safe_join(&ctx.root, &rel)?,
            sha1: f.sha1().map(str::to_string),
            md5: f.md5().map(str::to_string),
            ..Default::default()
        });
        files.push(rel);
    }
    fetch_all(ctx, items, "mods").await?;

    let prefix = overrides_prefix(&manifest.overrides)?;
    let mut written =
        archive::extract(archive, &prefix, &ctx.root, false, exclude.to_vec()).await?;
    files.append(&mut written);
    Ok(PackResult {
        minecraft_version: mc,
        loader,
        files,
        server_pack: false,
    })
}

pub struct CurseForge;

impl PackFormat for CurseForge {
    fn name(&self) -> &'static str {
        "curseforge"
    }

    fn accepts<'a>(&'a self, input: &'a PackInput<'a>) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(input.pack.kind == ModpackKind::CurseForge) })
    }

    fn install<'a>(
        &'a self,
        ctx: &'a Ctx,
        input: &'a PackInput<'a>,
    ) -> BoxFuture<'a, Result<PackResult>> {
        Box::pin(install(ctx, input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_folder() {
        assert_eq!(overrides_prefix("overrides").unwrap(), "overrides/");
        assert_eq!(
            overrides_prefix("server/overrides/").unwrap(),
            "server/overrides/"
        );
        for bad in ["../..", "a/../../b", "/etc", "..\\x"] {
            assert!(overrides_prefix(bad).is_err(), "{bad}");
        }
    }
}
