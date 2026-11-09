//! PurpurMC. Its API publishes an MD5 per build.

use anyhow::Result;
use cubeharbor_core::Loader;
use cubeharbor_core::plan::LaunchSpec;
use futures::future::BoxFuture;
use serde::Deserialize;

use super::{LoaderInstaller, is_hex, jar_launch};
use crate::Ctx;
use crate::download::{Item, fetch};

#[derive(Deserialize)]
struct PurpurBuild {
    #[serde(default)]
    build: Option<String>,
    #[serde(default)]
    md5: Option<String>,
}

async fn install(ctx: &Ctx, mc: &str, build: Option<&str>) -> Result<LaunchSpec> {
    let wanted = build.unwrap_or("latest");
    // Pin to the concrete build the checksum belongs to: `latest` can move between the two calls.
    let info = ctx.http.get_json::<PurpurBuild>(&format!("https://api.purpurmc.org/v2/purpur/{mc}/{wanted}")).await;
    let (id, md5) = match info {
        Ok(b) => (b.build.unwrap_or_else(|| wanted.to_string()), b.md5.filter(|h| is_hex(h, 32))),
        Err(e) => {
            tracing::warn!("no checksum for Purpur {wanted}: {e:#}");
            (wanted.to_string(), None)
        }
    };
    let url = format!("https://api.purpurmc.org/v2/purpur/{mc}/{id}/download");
    fetch(ctx, &Item::new(url, ctx.root.join("server.jar")).md5(md5)).await?;
    Ok(jar_launch("server.jar"))
}

pub struct Purpur;

impl LoaderInstaller for Purpur {
    fn loader(&self) -> Loader {
        Loader::Purpur
    }

    fn install<'a>(&'a self, ctx: &'a Ctx, mc: &'a str, version: Option<&'a str>) -> BoxFuture<'a, Result<LaunchSpec>> {
        Box::pin(install(ctx, mc, version))
    }
}
