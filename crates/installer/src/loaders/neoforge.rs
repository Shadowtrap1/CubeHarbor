//! NeoForge, through its installer jar.

use anyhow::{Context, Result};
use cubeharbor_core::Loader;
use cubeharbor_core::plan::LaunchSpec;
use futures::future::BoxFuture;
use serde::Deserialize;

use super::{LoaderInstaller, launch_from_install, run_installer};
use crate::Ctx;

#[derive(Deserialize)]
struct MavenVersions {
    versions: Vec<String>,
}

/// NeoForge versions drop the leading `1.` of the Minecraft version: MC 1.21.1 -> 21.1.x, MC 26.3 -> 26.3.x.
pub fn neoforge_prefix(mc: &str) -> String {
    match mc.strip_prefix("1.") {
        Some(rest) if rest.contains('.') => format!("{rest}."),
        Some(rest) => format!("{rest}.0."),
        None => format!("{mc}."),
    }
}

async fn install(ctx: &Ctx, mc: &str, version: Option<&str>) -> Result<LaunchSpec> {
    const MAVEN: &str = "https://maven.neoforged.net/releases/net/neoforged";
    if mc == "1.20.1" {
        // NeoForge for 1.20.1 was published under the legacy Forge coordinates.
        let v = version.context("NeoForge 1.20.1 needs an explicit loaderVersion")?;
        let full = format!("1.20.1-{}", v.trim_start_matches("1.20.1-"));
        run_installer(ctx, &format!("{MAVEN}/forge/{full}/forge-{full}-installer.jar"), "neoforge-installer.jar").await?;
        return launch_from_install(&ctx.root, &[format!("libraries/net/neoforged/forge/{full}")], &[]).await;
    }
    let v = match version {
        Some(v) => v.to_string(),
        None => {
            let list: MavenVersions =
                ctx.http.get_json("https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/neoforge").await?;
            let prefix = neoforge_prefix(mc);
            let matching: Vec<&String> = list.versions.iter().filter(|v| v.starts_with(&prefix)).collect();
            matching
                .iter()
                .rev()
                .find(|v| !v.contains("beta") && !v.contains("alpha"))
                .or(matching.last())
                .map(|v| v.to_string())
                .with_context(|| format!("NeoForge does not support Minecraft {mc}"))?
        }
    };
    run_installer(ctx, &format!("{MAVEN}/neoforge/{v}/neoforge-{v}-installer.jar"), "neoforge-installer.jar").await?;
    launch_from_install(&ctx.root, &[format!("libraries/net/neoforged/neoforge/{v}")], &[]).await
}

pub struct NeoForge;

impl LoaderInstaller for NeoForge {
    fn loader(&self) -> Loader {
        Loader::NeoForge
    }

    fn install<'a>(&'a self, ctx: &'a Ctx, mc: &'a str, version: Option<&'a str>) -> BoxFuture<'a, Result<LaunchSpec>> {
        Box::pin(install(ctx, mc, version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neoforge_prefixes() {
        assert_eq!(neoforge_prefix("1.21.1"), "21.1.");
        assert_eq!(neoforge_prefix("1.21"), "21.0.");
        assert_eq!(neoforge_prefix("26.3"), "26.3.");
        assert_eq!(neoforge_prefix("26.1.2"), "26.1.2.");
    }
}
