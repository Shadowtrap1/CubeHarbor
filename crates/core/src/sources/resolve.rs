//! Turn a `MinecraftServerSpec` into concrete versions (Minecraft, loader, Java, modpack file).

use anyhow::{Context, Result, bail};

use super::{Http, curseforge, modrinth, mojang};
use crate::crd::{Loader, MinecraftServerSpec, ModpackKind, Resolved, ResolvedModpack};
use crate::java;

pub async fn resolve(http: &Http, spec: &MinecraftServerSpec) -> Result<Resolved> {
    let mut mc = spec.version.clone();
    let mut loader = spec.loader;
    let mut modpack = None;

    if let Some(src) = &spec.modpack {
        let (pack, pack_mc, pack_loader) = resolve_modpack(http, src).await?;
        mc = mc.or(pack_mc);
        loader = loader.or(pack_loader);
        modpack = Some(pack);
    }

    let loader = loader.unwrap_or(Loader::Vanilla);
    let wanted = mc.unwrap_or_else(|| "latest".into());

    // Mojang's metadata is the authority for the Java requirement.
    let (minecraft_version, mojang_java) = match mojang::version_info(http, &wanted).await {
        Ok(info) => (info.id, info.java_version.map(|j| j.major_version)),
        Err(e) if wanted != "latest" => {
            tracing::warn!("Mojang lookup for {wanted} failed, guessing Java: {e:#}");
            (wanted.clone(), None)
        }
        Err(e) => return Err(e.context("resolving latest Minecraft version")),
    };

    let required = spec
        .jvm
        .java_version
        .or(mojang_java)
        .unwrap_or_else(|| java::guess_for_minecraft(&minecraft_version));

    Ok(Resolved {
        minecraft_version,
        loader,
        loader_version: spec.loader_version.clone(),
        java_version: java::image_java(required),
        modpack,
    })
}

async fn resolve_modpack(
    http: &Http,
    src: &crate::crd::ModpackSource,
) -> Result<(ResolvedModpack, Option<String>, Option<Loader>)> {
    if let Some(m) = &src.modrinth {
        let project = modrinth::project(http, &m.project).await.with_context(|| format!("Modrinth project {}", m.project))?;
        if project.project_type != "modpack" {
            bail!("Modrinth project {} is a {}, not a modpack", m.project, project.project_type);
        }
        let versions = modrinth::versions(http, &project.id, &[], &[]).await?;
        let v = modrinth::pick(&versions, m.version.as_deref())?;
        let file = v.primary_file().context("modpack version has no files")?;
        let loader = v.loaders.iter().find_map(|l| Loader::parse(l));
        return Ok((
            ResolvedModpack {
                name: project.title,
                version: v.version_number.clone(),
                url: file.url.clone(),
                sha1: file.hashes.sha1.clone(),
                kind: ModpackKind::Mrpack,
                server_pack_url: None,
                icon_url: project.icon_url,
            },
            v.game_versions.first().cloned(),
            loader,
        ));
    }

    if let Some(c) = &src.curseforge {
        let m = curseforge::find_mod(http, &c.project, curseforge::CLASS_MODPACK).await?;
        let f = match c.file_id {
            Some(id) => curseforge::file(http, m.id, id).await?,
            None => curseforge::latest_file(http, m.id).await?,
        };
        let server_pack_url = match (c.use_server_pack.unwrap_or(true), f.server_pack_file_id) {
            (true, Some(sp)) => match curseforge::file(http, m.id, sp).await {
                Ok(spf) => Some(spf.url()),
                Err(e) => {
                    tracing::warn!("server pack {sp} for {} unavailable: {e:#}", m.slug);
                    None
                }
            },
            _ => None,
        };
        return Ok((
            ResolvedModpack {
                name: m.name.clone(),
                version: f.display_name.clone(),
                url: f.url(),
                sha1: f.sha1().map(str::to_string),
                kind: ModpackKind::CurseForge,
                server_pack_url,
                icon_url: m.logo.map(|l| l.url),
            },
            f.minecraft_version().map(str::to_string),
            f.loader(),
        ));
    }

    if let Some(url) = &src.url {
        let path = url.split('?').next().unwrap_or(url);
        let kind = if path.ends_with(".mrpack") { ModpackKind::Mrpack } else { ModpackKind::ServerZip };
        let name = path.rsplit('/').next().unwrap_or("modpack").to_string();
        // Version/loader for raw URLs come from the spec or from the pack index at install time.
        return Ok((
            ResolvedModpack {
                name: name.clone(),
                version: name,
                url: url.clone(),
                sha1: None,
                kind,
                server_pack_url: None,
                icon_url: None,
            },
            None,
            None,
        ));
    }

    bail!("modpack needs one of: modrinth, curseforge, url")
}
