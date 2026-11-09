//! Modpack installation.
//!
//! Each pack format is one module implementing [`PackFormat`], listed once in [`REGISTRY`].
//! Modules fetch and unpack through `crate::download` and `crate::archive`, and use
//! `safe_pack_path` for paths taken from a pack.

mod curseforge;
mod mrpack;
mod server_zip;

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use cubeharbor_core::Loader;
use cubeharbor_core::crd::{ModpackKind, ResolvedModpack};
use cubeharbor_core::sources::modrinth;
use futures::future::BoxFuture;

use crate::Ctx;
use crate::download::fetch_temp;

/// Mods that are client-only but often not flagged as such by pack authors, and crash dedicated servers.
const CLIENT_ONLY: &[&str] = &[
    "oculus",
    "embeddium",
    "rubidium",
    "optifine",
    "iris-",
    "sodium-",
    "sodiumextra",
    "reeses-sodium",
    "entityculling",
    "legendarytooltips",
    "fancymenu",
    "drippyloadingscreen",
    "betterf3",
    "skinlayers3d",
    "3dskinlayers",
    "notenoughanimations",
    "itemphysiclite",
    "ambientsounds",
    "watermedia",
    "cherishedworlds",
    "loadingscreens",
    "konkrete",
    "mainmenucredits",
    "betterpingdisplay",
    "torohealth",
    "physicsmod",
    "dynamiclights",
];

/// A downloaded pack and what to do with it.
#[derive(Clone, Copy)]
pub struct PackInput<'a> {
    pub pack: &'a ResolvedModpack,
    pub archive: &'a Path,
    pub exclude: &'a [String],
    pub include: &'a [String],
}

pub trait PackFormat: Sync {
    fn name(&self) -> &'static str;

    /// Is this pack in this format?
    fn accepts<'a>(&'a self, input: &'a PackInput<'a>) -> BoxFuture<'a, Result<bool>>;

    fn install<'a>(
        &'a self,
        ctx: &'a Ctx,
        input: &'a PackInput<'a>,
    ) -> BoxFuture<'a, Result<PackResult>>;
}

/// The first format that accepts a pack installs it, so order matters.
static REGISTRY: &[&dyn PackFormat] = &[
    &mrpack::Mrpack,
    &curseforge::CurseForge,
    &server_zip::ServerZip,
];

async fn find_format(input: &PackInput<'_>) -> Result<&'static dyn PackFormat> {
    for format in REGISTRY {
        if format.accepts(input).await? {
            return Ok(*format);
        }
    }
    anyhow::bail!("{} is not in a known modpack format", input.pack.name)
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
async fn client_only_files(
    ctx: &Ctx,
    files: &[(String, Option<String>)],
    include: &[String],
) -> std::collections::HashSet<String> {
    let kept = |path: &str| {
        include
            .iter()
            .any(|i| excluded(&path.to_ascii_lowercase(), i))
    };
    let mut out: std::collections::HashSet<String> = files
        .iter()
        .filter(|(p, _)| {
            let l = p.to_ascii_lowercase();
            l.starts_with("resourcepacks/") || l.starts_with("shaderpacks/") || is_client_only(&l)
        })
        .map(|(p, _)| p.clone())
        .collect();

    let by_hash: HashMap<String, String> = files
        .iter()
        .filter_map(|(p, h)| Some((h.clone()?, p.clone())))
        .collect();
    let hashes: Vec<String> = by_hash.keys().cloned().collect();
    let lookup = async {
        let versions = modrinth::versions_by_hash(&ctx.http, &hashes).await?;
        let ids: Vec<String> = versions
            .values()
            .map(|v| v.project_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
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
            let rescued: Vec<String> = client
                .iter()
                .filter(|c| required.contains(c))
                .cloned()
                .collect();
            if rescued.is_empty() {
                break;
            }
            for r in rescued {
                client.remove(&r);
            }
        }
        anyhow::Ok(
            versions
                .into_iter()
                .filter(|(_, v)| client.contains(&v.project_id))
                .filter_map(|(h, _)| by_hash.get(&h).cloned())
                .collect::<Vec<_>>(),
        )
    };
    match lookup.await {
        Ok(paths) => out.extend(paths),
        Err(e) => tracing::warn!("could not check mods against Modrinth metadata: {e:#}"),
    }
    out.retain(|p| !kept(p));
    if !out.is_empty() {
        let mut names: Vec<&str> = out
            .iter()
            .map(|p| p.rsplit('/').next().unwrap_or(p))
            .collect();
        names.sort_unstable();
        tracing::info!(
            "skipping {} client-only files: {}",
            out.len(),
            names.join(", ")
        );
    }
    out
}
pub async fn install(
    ctx: &Ctx,
    pack: &ResolvedModpack,
    exclude: &[String],
    include: &[String],
) -> Result<PackResult> {
    tracing::info!("installing modpack {} {}", pack.name, pack.version);
    let ext = if pack.kind == ModpackKind::Mrpack {
        "mrpack"
    } else {
        "zip"
    };
    let archive = fetch_temp(ctx, &pack.url, &format!("modpack.{ext}"), pack.sha1.clone())
        .await
        .context("fetching the modpack")?;
    let input = PackInput {
        pack,
        archive: &archive,
        exclude,
        include,
    };
    let format = find_format(&input).await?;
    tracing::info!("pack format: {}", format.name());
    format.install(ctx, &input).await
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

    fn zip_with(dir: &Path, entries: &[&str]) -> std::path::PathBuf {
        use std::io::Write;
        let path = dir.join("pack.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        for name in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(b"{}").unwrap();
        }
        w.finish().unwrap();
        path
    }

    #[tokio::test]
    async fn formats_are_picked_in_order() {
        let tmp = crate::testutil::TempDir::new();
        let pack = |kind| ResolvedModpack {
            name: "p".into(),
            version: "1".into(),
            url: String::new(),
            sha1: None,
            kind,
            server_pack_url: None,
            icon_url: None,
        };
        for (entries, kind, want) in [
            (
                &["modrinth.index.json"][..],
                ModpackKind::ServerZip,
                "mrpack",
            ),
            (
                &["modrinth.index.json"][..],
                ModpackKind::CurseForge,
                "mrpack",
            ),
            (
                &["manifest.json"][..],
                ModpackKind::CurseForge,
                "curseforge",
            ),
            (&["server.jar"][..], ModpackKind::ServerZip, "server zip"),
        ] {
            let archive = zip_with(tmp.path(), entries);
            let pack = pack(kind);
            let input = PackInput {
                pack: &pack,
                archive: &archive,
                exclude: &[],
                include: &[],
            };
            assert_eq!(find_format(&input).await.unwrap().name(), want);
        }
    }
}
