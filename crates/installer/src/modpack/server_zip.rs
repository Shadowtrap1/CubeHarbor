//! A zip holding a ready server directory. The catch-all, so it comes last in the registry.

use anyhow::Result;
use futures::future::BoxFuture;

use super::{PackFormat, PackInput, PackResult};
use crate::Ctx;
use crate::archive;

async fn install(ctx: &Ctx, input: &PackInput<'_>) -> Result<PackResult> {
    let files =
        archive::extract(input.archive, "", &ctx.root, true, input.exclude.to_vec()).await?;
    Ok(PackResult {
        minecraft_version: None,
        loader: None,
        files,
        server_pack: true,
    })
}

pub struct ServerZip;

impl PackFormat for ServerZip {
    fn name(&self) -> &'static str {
        "server zip"
    }

    fn accepts<'a>(&'a self, _input: &'a PackInput<'a>) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(true) })
    }

    fn install<'a>(
        &'a self,
        ctx: &'a Ctx,
        input: &'a PackInput<'a>,
    ) -> BoxFuture<'a, Result<PackResult>> {
        Box::pin(install(ctx, input))
    }
}
