//! cubeharbor-installer: runs as an init container and prepares `/data` for the server.
//!
//! Heavy steps (loader, modpack, mods) only run when their inputs change; configuration
//! is re-applied on every start.

mod archive;
mod config;
mod download;
mod loaders;
mod modpack;
mod mods;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cubeharbor_core::defaults;
use cubeharbor_core::plan::{self, InstallPlan, InstallState, LaunchSpec};
use cubeharbor_core::sources::Http;

pub struct Ctx {
    pub http: Http,
    pub root: PathBuf,
    pub cache: Option<PathBuf>,
    pub concurrency: usize,
}

/// Is the boolean env var `name` set to a true-ish value (`true`, `1`, `yes`, `y`, `on`)?
pub fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "y" | "on"))
}

#[tokio::main]
async fn main() {
    cubeharbor_core::logging::init("info", false);
    if let Err(e) = run().await {
        tracing::error!("install failed: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let plan_path = std::env::var("CUBEHARBOR_PLAN").unwrap_or_else(|_| plan::PLAN_PATH.into());
    let plan: InstallPlan = serde_json::from_slice(&tokio::fs::read(&plan_path).await.with_context(|| format!("reading {plan_path}"))?)
        .context("parsing install plan")?;
    let ctx = Ctx {
        http: Http::from_env()?,
        root: PathBuf::from(std::env::var("CUBEHARBOR_DATA").unwrap_or_else(|_| defaults::DATA_DIR.into())),
        cache: std::env::var("CUBEHARBOR_CACHE").ok().filter(|s| !s.is_empty()).map(PathBuf::from),
        concurrency: std::env::var("CUBEHARBOR_DOWNLOAD_CONCURRENCY").ok().and_then(|s| s.parse().ok()).unwrap_or(defaults::DOWNLOAD_CONCURRENCY as usize),
    };
    tokio::fs::create_dir_all(ctx.root.join(plan::STATE_DIR)).await?;

    let state_path = ctx.root.join(plan::STATE_FILE);
    let mut state: InstallState = match tokio::fs::read(&state_path).await {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => InstallState::default(),
    };

    let base_hash = plan::hash_json(&(
        &plan.minecraft_version,
        plan.loader,
        &plan.loader_version,
        &plan.modpack,
        &plan.modpack_exclude,
        &plan.modpack_include,
    ));
    let launch_path = ctx.root.join(plan::LAUNCH_FILE);
    let base_changed = base_hash != state.base_hash || !launch_path.exists();

    let (mc, loader) = if base_changed {
        tracing::info!("server {}: installing", plan.server_name);
        let (mc, loader, launch, pack_files) = install_base(&ctx, &plan, &state).await?;
        tokio::fs::write(&launch_path, serde_json::to_vec_pretty(&launch)?).await?;
        state.base_hash = base_hash;
        state.pack_files = pack_files;
        state.minecraft_version = Some(mc.clone());
        state.loader = Some(loader);
        // Force the mod list to be re-resolved against the new base.
        state.mods_hash.clear();
        save(&state_path, &state).await?;
        (mc, loader)
    } else {
        tracing::info!("server {}: base install up to date", plan.server_name);
        (
            state.minecraft_version.clone().unwrap_or_else(|| plan.minecraft_version.clone()),
            state.loader.unwrap_or(plan.loader),
        )
    };

    let mods_hash = plan::hash_json(&(&plan.mods, &mc, loader));
    if mods_hash != state.mods_hash {
        remove_stale(&ctx.root, &state.mod_files, &[]).await;
        let pack: HashSet<String> = state.pack_files.iter().cloned().collect();
        state.mod_files = mods::install(&ctx, &plan.mods, &mc, loader, &pack).await?;
        state.mods_hash = mods_hash;
        save(&state_path, &state).await?;
    }

    config::apply(&ctx, &plan, loader, &mut state).await?;
    save(&state_path, &state).await?;
    let _ = tokio::fs::remove_dir_all(ctx.root.join(plan::STATE_DIR).join("downloads")).await;
    tracing::info!("server {} ready: Minecraft {mc} on {loader}", plan.server_name);
    Ok(())
}

async fn install_base(ctx: &Ctx, plan: &InstallPlan, state: &InstallState) -> Result<(String, cubeharbor_core::Loader, LaunchSpec, Vec<String>)> {
    let mut mc = plan.minecraft_version.clone();
    let mut loader = plan.loader;
    let mut loader_version = plan.loader_version.clone();
    let mut pack_files = vec![];
    let mut server_pack = false;

    if let Some(pack) = &plan.modpack {
        // Overrides (configs) are only copied when the pack changes, so local config edits survive restarts.
        let result = modpack::install(ctx, pack, &plan.modpack_exclude, &plan.modpack_include).await?;
        if let Some(v) = result.minecraft_version {
            mc = v;
        }
        if let Some((l, v)) = result.loader {
            loader = l;
            loader_version = loader_version.or(Some(v));
        }
        server_pack = result.server_pack;
        pack_files = result.files;
    }
    // Also when the modpack was removed from the spec: its jars must not stay in mods/ untracked.
    let keep: HashSet<&str> = pack_files.iter().map(String::as_str).collect();
    remove_stale(&ctx.root, &state.pack_files, &keep.into_iter().collect::<Vec<_>>()).await;

    let launch = match loaders::install(ctx, &mc, loader, loader_version.as_deref()).await {
        Ok(l) => l,
        // A server pack may already contain a complete installation.
        Err(e) if server_pack => {
            tracing::warn!("loader install failed ({e:#}); trying the server pack's own launcher");
            loaders::detect_launch(&ctx.root).await.context("server pack has no recognizable launcher")?
        }
        Err(e) => return Err(e),
    };
    Ok((mc, loader, launch, pack_files))
}

/// Delete previously managed jars that are no longer wanted. Configs are left alone.
async fn remove_stale(root: &Path, previous: &[String], keep: &[&str]) {
    for rel in previous {
        if keep.contains(&rel.as_str()) {
            continue;
        }
        let lower = rel.to_ascii_lowercase();
        let removable = lower.ends_with(".jar") || lower.ends_with(".zip") || lower.starts_with("mods/") || lower.starts_with("plugins/");
        if !removable {
            continue;
        }
        if let Ok(p) = download::safe_join(root, rel)
            && tokio::fs::remove_file(&p).await.is_ok()
        {
            tracing::info!("  removed stale {rel}");
        }
    }
}

async fn save(path: &Path, state: &InstallState) -> Result<()> {
    tokio::fs::write(path, serde_json::to_vec_pretty(state)?).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dropping a modpack (nothing kept) removes its jars but leaves configs and worlds alone.
    #[tokio::test]
    async fn stale_pack_files_are_removed() {
        let root = std::env::temp_dir().join(format!("cubeharbor-stale-{}", std::process::id()));
        for f in ["mods/create.jar", "mods/jei.jar", "config/create.toml", "world/level.dat"] {
            tokio::fs::create_dir_all(root.join(f).parent().unwrap()).await.unwrap();
            tokio::fs::write(root.join(f), "x").await.unwrap();
        }
        let previous: Vec<String> = ["mods/create.jar", "mods/jei.jar", "config/create.toml"].map(String::from).to_vec();
        remove_stale(&root, &previous, &["mods/jei.jar"]).await;
        assert!(!root.join("mods/create.jar").exists());
        assert!(root.join("mods/jei.jar").exists(), "kept by the new pack");
        assert!(root.join("config/create.toml").exists(), "configs are left alone");
        remove_stale(&root, &previous, &[]).await;
        assert!(!root.join("mods/jei.jar").exists(), "pack removed entirely");
        assert!(root.join("world/level.dat").exists());
        tokio::fs::remove_dir_all(&root).await.unwrap();
    }
}
