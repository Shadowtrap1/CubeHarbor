//! Quilt, through its installer jar.

use anyhow::{Context, Result};
use cubeharbor_core::Loader;
use cubeharbor_core::plan::LaunchSpec;
use futures::future::BoxFuture;
use serde::Deserialize;

use super::{LoaderInstaller, jar_launch, java, maven_sha1};
use crate::Ctx;
use crate::download::fetch_temp;

#[derive(Deserialize)]
struct QuiltVersion {
    version: String,
    #[serde(default)]
    url: Option<String>,
}

async fn install(ctx: &Ctx, mc: &str, loader: Option<&str>) -> Result<LaunchSpec> {
    const META: &str = "https://meta.quiltmc.org/v3";
    let loader = match loader {
        Some(l) => l.to_string(),
        None => {
            let list: Vec<QuiltVersion> = ctx.http.get_json(&format!("{META}/versions/loader")).await?;
            list.iter()
                .find(|v| !v.version.contains('-'))
                .map(|v| v.version.clone())
                .context("no Quilt loader versions")?
        }
    };
    let installers: Vec<QuiltVersion> = ctx.http.get_json(&format!("{META}/versions/installer")).await?;
    // Like the loader: skip pre-releases (`-beta.1`), which Quilt's meta lists first.
    let inst = installers
        .iter()
        .find(|i| !i.version.contains('-'))
        .or(installers.first())
        .and_then(|i| i.url.clone())
        .context("no Quilt installer")?;
    let jar = fetch_temp(ctx, &inst, "quilt-installer.jar", maven_sha1(ctx, &inst).await).await?;
    let dir = ctx.root.to_string_lossy().to_string();
    java(
        &ctx.root,
        &[
            "-jar",
            &jar.to_string_lossy(),
            "install",
            "server",
            mc,
            &loader,
            "--download-server",
            &format!("--install-dir={dir}"),
        ],
    )
    .await?;
    Ok(jar_launch("quilt-server-launch.jar"))
}

pub struct Quilt;

impl LoaderInstaller for Quilt {
    fn loader(&self) -> Loader {
        Loader::Quilt
    }

    fn install<'a>(&'a self, ctx: &'a Ctx, mc: &'a str, version: Option<&'a str>) -> BoxFuture<'a, Result<LaunchSpec>> {
        Box::pin(install(ctx, mc, version))
    }
}
