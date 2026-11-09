//! Fabric. The meta server builds the launcher jar on demand and publishes no checksum for it.

use anyhow::{Context, Result};
use cubeharbor_core::Loader;
use cubeharbor_core::plan::LaunchSpec;
use futures::future::BoxFuture;
use serde::Deserialize;

use super::{LoaderInstaller, jar_launch};
use crate::Ctx;
use crate::download::{Item, fetch};

#[derive(Deserialize)]
struct FabricLoaderEntry {
    loader: FabricVersion,
}
#[derive(Deserialize)]
struct FabricVersion {
    version: String,
    #[serde(default)]
    stable: bool,
}

async fn install(ctx: &Ctx, mc: &str, loader: Option<&str>) -> Result<LaunchSpec> {
    const META: &str = "https://meta.fabricmc.net/v2";
    let loader = match loader {
        Some(l) => l.to_string(),
        None => {
            let list: Vec<FabricLoaderEntry> = ctx.http.get_json(&format!("{META}/versions/loader/{mc}")).await?;
            list.iter()
                .find(|e| e.loader.stable)
                .or(list.first())
                .map(|e| e.loader.version.clone())
                .with_context(|| format!("Fabric does not support Minecraft {mc}"))?
        }
    };
    let installers: Vec<FabricVersion> = ctx.http.get_json(&format!("{META}/versions/installer")).await?;
    let installer = installers.iter().find(|i| i.stable).or(installers.first()).context("no Fabric installer")?;
    let url = format!("{META}/versions/loader/{mc}/{loader}/{}/server/jar", installer.version);
    fetch(ctx, &Item::new(url, ctx.root.join("fabric-server-launch.jar"))).await?;
    // The launcher downloads the vanilla jar itself, but fetching it here keeps first boot predictable.
    super::vanilla::server_jar(ctx, mc).await?;
    Ok(jar_launch("fabric-server-launch.jar"))
}

pub struct Fabric;

impl LoaderInstaller for Fabric {
    fn loader(&self) -> Loader {
        Loader::Fabric
    }

    fn install<'a>(&'a self, ctx: &'a Ctx, mc: &'a str, version: Option<&'a str>) -> BoxFuture<'a, Result<LaunchSpec>> {
        Box::pin(install(ctx, mc, version))
    }
}
