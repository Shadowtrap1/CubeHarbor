//! MinecraftForge, through its installer jar.

use anyhow::{Context, Result};
use cubeharbor_core::Loader;
use cubeharbor_core::plan::LaunchSpec;
use futures::future::BoxFuture;
use serde::Deserialize;

use super::{LoaderInstaller, launch_from_install, run_installer};
use crate::Ctx;

#[derive(Deserialize)]
struct ForgePromos {
    promos: std::collections::HashMap<String, String>,
}

async fn install(ctx: &Ctx, mc: &str, version: Option<&str>) -> Result<LaunchSpec> {
    let v = match version {
        Some(v) => v.trim_start_matches(&format!("{mc}-")).to_string(),
        None => {
            let p: ForgePromos = ctx
                .http
                .get_json("https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json")
                .await?;
            p.promos
                .get(&format!("{mc}-recommended"))
                .or_else(|| p.promos.get(&format!("{mc}-latest")))
                .cloned()
                .with_context(|| format!("Forge does not support Minecraft {mc}"))?
        }
    };
    let full = format!("{mc}-{v}");
    let url = format!("https://maven.minecraftforge.net/net/minecraftforge/forge/{full}/forge-{full}-installer.jar");
    run_installer(ctx, &url, "forge-installer.jar").await?;
    launch_from_install(&ctx.root, &[format!("libraries/net/minecraftforge/forge/{full}")], &[
        format!("forge-{full}-shim.jar"),
        format!("forge-{full}.jar"),
        format!("forge-{full}-universal.jar"),
    ])
    .await
}

pub struct Forge;

impl LoaderInstaller for Forge {
    fn loader(&self) -> Loader {
        Loader::Forge
    }

    fn install<'a>(&'a self, ctx: &'a Ctx, mc: &'a str, version: Option<&'a str>) -> BoxFuture<'a, Result<LaunchSpec>> {
        Box::pin(install(ctx, mc, version))
    }
}
