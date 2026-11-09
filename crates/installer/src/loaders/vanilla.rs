//! Plain Mojang server.

use anyhow::{Context, Result};
use cubeharbor_core::Loader;
use cubeharbor_core::plan::LaunchSpec;
use cubeharbor_core::sources::mojang;
use futures::future::BoxFuture;

use super::{LoaderInstaller, jar_launch};
use crate::Ctx;
use crate::download::{Item, fetch};

pub(super) async fn server_jar(ctx: &Ctx, mc: &str) -> Result<LaunchSpec> {
    let info = mojang::version_info(&ctx.http, mc).await?;
    let server = info
        .downloads
        .server
        .with_context(|| format!("Minecraft {mc} has no server download"))?;
    fetch(
        ctx,
        &Item::new(server.url, ctx.root.join("server.jar")).sha1(Some(server.sha1)),
    )
    .await?;
    let mut l = jar_launch("server.jar");
    l.required_java = info.java_version.map(|j| j.major_version);
    Ok(l)
}

pub struct Vanilla;

impl LoaderInstaller for Vanilla {
    fn loader(&self) -> Loader {
        Loader::Vanilla
    }

    fn install<'a>(
        &'a self,
        ctx: &'a Ctx,
        mc: &'a str,
        _version: Option<&'a str>,
    ) -> BoxFuture<'a, Result<LaunchSpec>> {
        Box::pin(server_jar(ctx, mc))
    }
}
