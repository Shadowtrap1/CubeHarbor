//! Install the server software for each loader and work out how to launch it.
//!
//! Each loader is one module implementing [`LoaderInstaller`], listed once in [`REGISTRY`].
//! Modules fetch through `crate::download`, so checksums, retries, limits and atomic writes
//! don't need repeating here.

mod fabric;
mod forge;
mod neoforge;
mod paper;
mod purpur;
mod quilt;
mod vanilla;

use std::path::Path;

use anyhow::{Context, Result, bail};
use cubeharbor_core::Loader;
use cubeharbor_core::plan::LaunchSpec;
use futures::future::BoxFuture;

use crate::Ctx;
use crate::download::fetch_temp;

pub trait LoaderInstaller: Sync {
    /// The loader this module installs.
    fn loader(&self) -> Loader;

    /// Put the server software into `ctx.root` and say how to start it. `version` is a concrete
    /// loader version, or `None` for the latest suitable one.
    fn install<'a>(
        &'a self,
        ctx: &'a Ctx,
        mc: &'a str,
        version: Option<&'a str>,
    ) -> BoxFuture<'a, Result<LaunchSpec>>;
}

static REGISTRY: &[&dyn LoaderInstaller] = &[
    &vanilla::Vanilla,
    &paper::Paper,
    &purpur::Purpur,
    &fabric::Fabric,
    &quilt::Quilt,
    &forge::Forge,
    &neoforge::NeoForge,
];

fn find(loader: Loader) -> Option<&'static dyn LoaderInstaller> {
    REGISTRY.iter().copied().find(|i| i.loader() == loader)
}

pub async fn install(
    ctx: &Ctx,
    mc: &str,
    loader: Loader,
    version: Option<&str>,
) -> Result<LaunchSpec> {
    let version = version.filter(|v| !v.is_empty() && *v != "latest");
    tracing::info!(
        "installing {loader} {} for Minecraft {mc}",
        version.unwrap_or("(latest)")
    );
    let Some(installer) = find(loader) else {
        bail!("no installer for loader {loader}")
    };
    installer.install(ctx, mc, version).await
}

pub(crate) fn jar_launch(jar: &str) -> LaunchSpec {
    LaunchSpec {
        java_args: vec!["-jar".into(), jar.into()],
        program_args: vec!["nogui".into()],
        required_java: None,
    }
}

async fn run_installer(ctx: &Ctx, url: &str, name: &str) -> Result<()> {
    let jar = fetch_temp(ctx, url, name, maven_sha1(ctx, url).await).await?;
    java(
        &ctx.root,
        &["-jar", &jar.to_string_lossy(), "--installServer"],
    )
    .await?;
    // The installer leaves a log and its own copy behind.
    let _ = tokio::fs::remove_file(ctx.root.join(format!("{name}.log"))).await;
    let _ = tokio::fs::remove_file(ctx.root.join("installer.log")).await;
    Ok(())
}

fn is_hex(h: &str, len: usize) -> bool {
    h.len() == len && h.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The `<url>.sha1` a Maven repository publishes next to an artifact. A missing or malformed one
/// only costs the check, as old artifacts don't always have it.
async fn maven_sha1(ctx: &Ctx, url: &str) -> Option<String> {
    let sidecar = match ctx.http.get_text(&format!("{url}.sha1")).await {
        Ok(text) => parse_sha1(&text),
        Err(e) => {
            tracing::debug!("{e:#}");
            None
        }
    };
    if sidecar.is_none() {
        tracing::warn!("no usable checksum published for {url}");
    }
    sidecar
}

/// Sidecars hold the bare hash, sometimes followed by a file name.
fn parse_sha1(text: &str) -> Option<String> {
    text.split_whitespace()
        .next()
        .filter(|h| is_hex(h, 40))
        .map(str::to_string)
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
pub async fn launch_from_install(
    root: &Path,
    lib_dirs: &[String],
    jars: &[String],
) -> Result<LaunchSpec> {
    for dir in lib_dirs {
        let args = format!("{dir}/unix_args.txt");
        if root.join(&args).exists() {
            return Ok(LaunchSpec {
                java_args: vec![format!("@{args}")],
                program_args: vec!["nogui".into()],
                required_java: None,
            });
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
    detect_launch(root)
        .await
        .context("could not determine how to start the server after install")
}

/// Extract the java invocation from a `run.sh` / `start.sh` written by an installer or pack author.
async fn parse_run_sh(root: &Path) -> Option<LaunchSpec> {
    for script in ["run.sh", "start.sh", "startserver.sh"] {
        let Ok(text) = tokio::fs::read_to_string(root.join(script)).await else {
            continue;
        };
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
                .filter(|a| {
                    a != "$@"
                        && a != "nogui"
                        && a != "--nogui"
                        && !a.starts_with("@user_jvm_args")
                        && !a.starts_with("-Xm")
                        && !a.starts_with('$')
                })
                .collect();
            if args.iter().any(|a| a.starts_with('@') || a == "-jar") {
                return Some(LaunchSpec {
                    java_args: args,
                    program_args: vec!["nogui".into()],
                    required_java: None,
                });
            }
        }
    }
    None
}

/// Best-effort detection for server packs that ship a ready installation.
pub async fn detect_launch(root: &Path) -> Option<LaunchSpec> {
    for base in [
        "libraries/net/neoforged/neoforge",
        "libraries/net/minecraftforge/forge",
        "libraries/net/neoforged/forge",
    ] {
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
                return Some(LaunchSpec {
                    java_args: vec![format!("@{a}")],
                    program_args: vec!["nogui".into()],
                    required_java: None,
                });
            }
        }
    }
    if let Some(l) = parse_run_sh(root).await {
        return Some(l);
    }
    for jar in [
        "fabric-server-launch.jar",
        "quilt-server-launch.jar",
        "server.jar",
    ] {
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

    /// A loader added to the CRD without a module here would only fail at install time.
    #[test]
    fn every_loader_has_an_installer() {
        for l in [
            Loader::Vanilla,
            Loader::Paper,
            Loader::Purpur,
            Loader::Fabric,
            Loader::Quilt,
            Loader::Forge,
            Loader::NeoForge,
        ] {
            assert_eq!(find(l).unwrap().loader(), l);
        }
    }

    #[test]
    fn registry_has_no_duplicates() {
        let mut names: Vec<&str> = REGISTRY.iter().map(|i| i.loader().as_str()).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before);
    }

    #[test]
    fn maven_sidecars() {
        let sha = "a".repeat(40);
        assert_eq!(parse_sha1(&format!("{sha}\n")), Some(sha.clone()));
        assert_eq!(
            parse_sha1(&format!("{sha}  forge-installer.jar\n")),
            Some(sha)
        );
        assert_eq!(parse_sha1("<html>not found</html>"), None);
        assert_eq!(parse_sha1(""), None);
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
        assert_eq!(
            l.java_args,
            vec!["@libraries/net/minecraftforge/forge/1.20.1-47.2.0/unix_args.txt"]
        );
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
