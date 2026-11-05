//! Install the server software for each loader and work out how to launch it.

use std::path::Path;

use anyhow::{Context, Result, bail};
use cubeharbor_core::Loader;
use cubeharbor_core::plan::LaunchSpec;
use cubeharbor_core::sources::mojang;
use serde::Deserialize;

use crate::Ctx;
use crate::download::{Item, fetch, fetch_temp};

pub async fn install(ctx: &Ctx, mc: &str, loader: Loader, version: Option<&str>) -> Result<LaunchSpec> {
    let version = version.filter(|v| !v.is_empty() && *v != "latest");
    tracing::info!("installing {loader} {} for Minecraft {mc}", version.unwrap_or("(latest)"));
    match loader {
        Loader::Vanilla => vanilla(ctx, mc).await,
        Loader::Paper => paper(ctx, mc, version).await,
        Loader::Purpur => purpur(ctx, mc, version).await,
        Loader::Fabric => fabric(ctx, mc, version).await,
        Loader::Quilt => quilt(ctx, mc, version).await,
        Loader::Forge => forge(ctx, mc, version).await,
        Loader::NeoForge => neoforge(ctx, mc, version).await,
    }
}

fn jar_launch(jar: &str) -> LaunchSpec {
    LaunchSpec { java_args: vec!["-jar".into(), jar.into()], program_args: vec!["nogui".into()], required_java: None }
}

async fn vanilla(ctx: &Ctx, mc: &str) -> Result<LaunchSpec> {
    let info = mojang::version_info(&ctx.http, mc).await?;
    let server = info.downloads.server.with_context(|| format!("Minecraft {mc} has no server download"))?;
    fetch(ctx, &Item::new(server.url, ctx.root.join("server.jar")).sha1(Some(server.sha1))).await?;
    let mut l = jar_launch("server.jar");
    l.required_java = info.java_version.map(|j| j.major_version);
    Ok(l)
}

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

async fn paper(ctx: &Ctx, mc: &str, build: Option<&str>) -> Result<LaunchSpec> {
    let url = format!(
        "https://fill.papermc.io/v3/projects/paper/versions/{mc}/builds/{}",
        build.unwrap_or("latest")
    );
    let b: PaperBuild = ctx.http.get_json(&url).await.with_context(|| format!("no Paper build for Minecraft {mc}"))?;
    let d = b.downloads.get("server:default").context("Paper build has no server download")?;
    tracing::info!("Paper build {}", b.id);
    fetch(ctx, &Item::new(&d.url, ctx.root.join("server.jar")).sha256(d.checksums.get("sha256").cloned())).await?;
    Ok(jar_launch("server.jar"))
}

async fn purpur(ctx: &Ctx, mc: &str, build: Option<&str>) -> Result<LaunchSpec> {
    let url = format!("https://api.purpurmc.org/v2/purpur/{mc}/{}/download", build.unwrap_or("latest"));
    fetch(ctx, &Item::new(url, ctx.root.join("server.jar"))).await?;
    Ok(jar_launch("server.jar"))
}

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

async fn fabric(ctx: &Ctx, mc: &str, loader: Option<&str>) -> Result<LaunchSpec> {
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
    vanilla(ctx, mc).await?;
    Ok(jar_launch("fabric-server-launch.jar"))
}

#[derive(Deserialize)]
struct QuiltVersion {
    version: String,
    #[serde(default)]
    url: Option<String>,
}

async fn quilt(ctx: &Ctx, mc: &str, loader: Option<&str>) -> Result<LaunchSpec> {
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
    let jar = fetch_temp(ctx, &inst, "quilt-installer.jar", None).await?;
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

#[derive(Deserialize)]
struct ForgePromos {
    promos: std::collections::HashMap<String, String>,
}

async fn forge(ctx: &Ctx, mc: &str, version: Option<&str>) -> Result<LaunchSpec> {
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

async fn neoforge(ctx: &Ctx, mc: &str, version: Option<&str>) -> Result<LaunchSpec> {
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

async fn run_installer(ctx: &Ctx, url: &str, name: &str) -> Result<()> {
    let jar = fetch_temp(ctx, url, name, None).await?;
    java(&ctx.root, &["-jar", &jar.to_string_lossy(), "--installServer"]).await?;
    // The installer leaves a log and its own copy behind.
    let _ = tokio::fs::remove_file(ctx.root.join(format!("{name}.log"))).await;
    let _ = tokio::fs::remove_file(ctx.root.join("installer.log")).await;
    Ok(())
}

async fn java(cwd: &Path, args: &[&str]) -> Result<()> {
    tracing::info!("running java {}", args.join(" "));
    let status = tokio::process::Command::new("java")
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .context("failed to start java (is it on PATH?)")?;
    if !status.success() {
        bail!("java {} exited with {status}", args.join(" "));
    }
    Ok(())
}

/// Work out the launch command after a Forge-style installer ran (or a server pack was unpacked).
pub async fn launch_from_install(root: &Path, lib_dirs: &[String], jars: &[String]) -> Result<LaunchSpec> {
    for dir in lib_dirs {
        let args = format!("{dir}/unix_args.txt");
        if root.join(&args).exists() {
            return Ok(LaunchSpec { java_args: vec![format!("@{args}")], program_args: vec!["nogui".into()], required_java: None });
        }
    }
    if let Some(l) = parse_run_sh(root).await {
        return Ok(l);
    }
    for jar in jars {
        if root.join(jar).exists() {
            return Ok(jar_launch(jar));
        }
    }
    detect_launch(root).await.context("could not determine how to start the server after install")
}

/// Extract the java invocation from a `run.sh` / `start.sh` written by an installer or pack author.
async fn parse_run_sh(root: &Path) -> Option<LaunchSpec> {
    for script in ["run.sh", "start.sh", "startserver.sh"] {
        let Ok(text) = tokio::fs::read_to_string(root.join(script)).await else { continue };
        for line in text.lines() {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.first().is_some_and(|t| t.starts_with('#')) {
                continue;
            }
            let Some(pos) = tokens.iter().position(|t| {
                let t = t.trim_matches('"');
                t == "java" || t.ends_with("/java") || t == "$JAVA" || t == "${JAVA}"
            }) else {
                continue;
            };
            let args: Vec<String> = tokens[pos + 1..]
                .iter()
                .map(|a| a.trim_matches('"').to_string())
                .filter(|a| a != "$@" && a != "nogui" && a != "--nogui" && !a.starts_with("@user_jvm_args") && !a.starts_with("-Xm") && !a.starts_with('$'))
                .collect();
            if args.iter().any(|a| a.starts_with('@') || a == "-jar") {
                return Some(LaunchSpec { java_args: args, program_args: vec!["nogui".into()], required_java: None });
            }
        }
    }
    None
}

/// Best-effort detection for server packs that ship a ready installation.
pub async fn detect_launch(root: &Path) -> Option<LaunchSpec> {
    for base in ["libraries/net/neoforged/neoforge", "libraries/net/minecraftforge/forge", "libraries/net/neoforged/forge"] {
        if let Ok(mut rd) = tokio::fs::read_dir(root.join(base)).await {
            let mut found = vec![];
            while let Ok(Some(e)) = rd.next_entry().await {
                let args = format!("{base}/{}/unix_args.txt", e.file_name().to_string_lossy());
                if root.join(&args).exists() {
                    found.push(args);
                }
            }
            found.sort();
            if let Some(a) = found.pop() {
                return Some(LaunchSpec { java_args: vec![format!("@{a}")], program_args: vec!["nogui".into()], required_java: None });
            }
        }
    }
    if let Some(l) = parse_run_sh(root).await {
        return Some(l);
    }
    for jar in ["fabric-server-launch.jar", "quilt-server-launch.jar", "server.jar"] {
        if root.join(jar).exists() {
            return Some(jar_launch(jar));
        }
    }
    // Legacy Forge: forge-<mc>-<ver>.jar in the root.
    let mut rd = tokio::fs::read_dir(root).await.ok()?;
    while let Ok(Some(e)) = rd.next_entry().await {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with("forge-") && n.ends_with(".jar") && !n.contains("installer") {
            return Some(jar_launch(&n));
        }
    }
    None
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

    #[tokio::test]
    async fn run_sh_parsing() {
        let dir = std::env::temp_dir().join(format!("cubeharbor-test-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(
            dir.join("run.sh"),
            "#!/usr/bin/env sh\n# comment\njava @user_jvm_args.txt @libraries/net/minecraftforge/forge/1.20.1-47.2.0/unix_args.txt \"$@\"\n",
        )
        .await
        .unwrap();
        let l = parse_run_sh(&dir).await.unwrap();
        assert_eq!(l.java_args, vec!["@libraries/net/minecraftforge/forge/1.20.1-47.2.0/unix_args.txt"]);
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
