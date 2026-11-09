//! PaperMC, through the Fill API.

use anyhow::{Context, Result};
use cubeharbor_core::Loader;
use cubeharbor_core::plan::LaunchSpec;
use futures::future::BoxFuture;
use serde::Deserialize;

use super::{LoaderInstaller, jar_launch};
use crate::Ctx;
use crate::download::{Item, fetch};

#[derive(Deserialize)]
struct PaperBuild {
    id: u64,
    downloads: std::collections::HashMap<String, PaperDownload>,
}

#[derive(Deserialize)]
struct PaperDownload {
    url: String,
    checksums: std::collections::HashMap<String, String>,
}

async fn install(ctx: &Ctx, mc: &str, build: Option<&str>) -> Result<LaunchSpec> {
    let url = format!(
        "https://fill.papermc.io/v3/projects/paper/versions/{mc}/builds/{}",
        build.unwrap_or("latest")
    );
    let b: PaperBuild = ctx
        .http
        .get_json(&url)
        .await
        .with_context(|| format!("no Paper build for Minecraft {mc}"))?;
    let d = b
        .downloads
        .get("server:default")
        .context("Paper build has no server download")?;
    tracing::info!("Paper build {}", b.id);
    fetch(
        ctx,
        &Item::new(&d.url, ctx.root.join("server.jar")).sha256(d.checksums.get("sha256").cloned()),
    )
    .await?;
    Ok(jar_launch("server.jar"))
}

pub struct Paper;

impl LoaderInstaller for Paper {
    fn loader(&self) -> Loader {
        Loader::Paper
    }

    fn install<'a>(
        &'a self,
        ctx: &'a Ctx,
        mc: &'a str,
        version: Option<&'a str>,
    ) -> BoxFuture<'a, Result<LaunchSpec>> {
        Box::pin(install(ctx, mc, version))
    }
}
