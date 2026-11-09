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
use std::time::Duration;

use anyhow::{Context, Result};
use cubeharbor_core::defaults;
use cubeharbor_core::plan::{self, InstallPlan, InstallState, LaunchSpec};
use cubeharbor_core::sources::Http;

pub struct Ctx {
    pub http: Http,
    pub root: PathBuf,
    pub cache: Option<PathBuf>,
    pub concurrency: usize,
    /// Largest single download.
    pub max_download_bytes: u64,
    /// Overall deadline for one download attempt (stalls are caught sooner by the read timeout).
    pub download_timeout: Duration,
}

/// Is the boolean env var `name` set to a true-ish value (`true`, `1`, `yes`, `y`, `on`)?
pub fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "y" | "on"
        )
    })
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
    let plan: InstallPlan = serde_json::from_slice(
        &tokio::fs::read(&plan_path)
            .await
            .with_context(|| format!("reading {plan_path}"))?,
    )
    .context("parsing install plan")?;
    let ctx = Ctx {
        http: Http::from_env()?,
        root: PathBuf::from(
            std::env::var("CUBEHARBOR_DATA").unwrap_or_else(|_| defaults::DATA_DIR.into()),
        ),
        cache: std::env::var("CUBEHARBOR_CACHE")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
        concurrency: std::env::var("CUBEHARBOR_DOWNLOAD_CONCURRENCY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(defaults::DOWNLOAD_CONCURRENCY as usize)
            .max(1),
        max_download_bytes: download::max_download_bytes(),
        download_timeout: download::download_timeout(),
    };
    tokio::fs::create_dir_all(ctx.root.join(plan::STATE_DIR)).await?;
    // A killed run can't clean up after itself.
    download::remove_part_files(&ctx.root).await;

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
        download::write_atomic(&launch_path, &serde_json::to_vec_pretty(&launch)?).await?;
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
            state
                .minecraft_version
                .clone()
                .unwrap_or_else(|| plan.minecraft_version.clone()),
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
    tracing::info!(
        "server {} ready: Minecraft {mc} on {loader}",
        plan.server_name
    );
    Ok(())
}

async fn install_base(
    ctx: &Ctx,
    plan: &InstallPlan,
    state: &InstallState,
) -> Result<(String, cubeharbor_core::Loader, LaunchSpec, Vec<String>)> {
    let mut mc = plan.minecraft_version.clone();
    let mut loader = plan.loader;
    let mut loader_version = plan.loader_version.clone();
    let mut pack_files = vec![];
    let mut server_pack = false;

    if let Some(pack) = &plan.modpack {
        // Overrides (configs) are only copied when the pack changes, so local config edits survive restarts.
        let result =
            modpack::install(ctx, pack, &plan.modpack_exclude, &plan.modpack_include).await?;
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
    remove_stale(
        &ctx.root,
        &state.pack_files,
        &keep.into_iter().collect::<Vec<_>>(),
    )
    .await;

    let launch = match loaders::install(ctx, &mc, loader, loader_version.as_deref()).await {
        Ok(l) => l,
        // A server pack may already contain a complete installation.
        Err(e) if server_pack => {
            tracing::warn!("loader install failed ({e:#}); trying the server pack's own launcher");
            loaders::detect_launch(&ctx.root)
                .await
                .context("server pack has no recognizable launcher")?
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
        let removable = lower.ends_with(".jar")
            || lower.ends_with(".zip")
            || lower.starts_with("mods/")
            || lower.starts_with("plugins/");
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
    download::write_atomic(path, &serde_json::to_vec_pretty(state)?).await
}

#[cfg(test)]
pub mod testutil {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use cubeharbor_core::sources::Http;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::Ctx;

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    pub struct TempDir(PathBuf);

    impl TempDir {
        #[allow(clippy::new_without_default)] // Creates a directory, so it isn't a cheap default value.
        pub fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "cubeharbor-t-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub fn ctx(root: &Path) -> Ctx {
        Ctx {
            http: Http::new(None).unwrap(),
            root: root.to_path_buf(),
            cache: None,
            concurrency: 4,
            max_download_bytes: 1024 * 1024,
            download_timeout: Duration::from_secs(10),
        }
    }

    /// What the stub does with one connection.
    #[derive(Clone)]
    pub enum Reply {
        Body(Vec<u8>),
        Status(u16),
        /// Promises more bytes than it sends, then hangs up.
        Truncated(Vec<u8>),
        /// Hangs up without a response.
        Hangup,
        /// Sends headers, then never sends the body.
        Stall,
        Redirect(String),
    }

    pub struct Stub {
        pub url: String,
        hits: Arc<AtomicUsize>,
    }

    impl Stub {
        pub fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    /// Local HTTP stub: the n-th connection gets the n-th reply, and the last reply repeats.
    pub async fn stub(replies: Vec<Reply>) -> Stub {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/file.jar", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let n = count.fetch_add(1, Ordering::SeqCst);
                let reply = replies[n.min(replies.len() - 1)].clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let head = |status: &str, extra: String| {
                        format!("HTTP/1.1 {status}\r\n{extra}connection: close\r\n\r\n")
                            .into_bytes()
                    };
                    let out = match reply {
                        Reply::Body(b) => [
                            head("200 OK", format!("content-length: {}\r\n", b.len())),
                            b,
                        ]
                        .concat(),
                        Reply::Status(c) => head(&format!("{c} X"), "content-length: 0\r\n".into()),
                        Reply::Truncated(b) => [
                            head("200 OK", format!("content-length: {}\r\n", b.len() + 100)),
                            b,
                        ]
                        .concat(),
                        Reply::Hangup => return,
                        Reply::Stall => {
                            let _ = s
                                .write_all(&head("200 OK", "content-length: 100\r\n".into()))
                                .await;
                            tokio::time::sleep(Duration::from_secs(60)).await;
                            return;
                        }
                        Reply::Redirect(to) => head(
                            "302 Found",
                            format!("location: {to}\r\ncontent-length: 0\r\n"),
                        ),
                    };
                    let _ = s.write_all(&out).await;
                });
            }
        });
        Stub { url, hits }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dropping a modpack (nothing kept) removes its jars but leaves configs and worlds alone.
    #[tokio::test]
    async fn stale_pack_files_are_removed() {
        let root = std::env::temp_dir().join(format!("cubeharbor-stale-{}", std::process::id()));
        for f in [
            "mods/create.jar",
            "mods/jei.jar",
            "config/create.toml",
            "world/level.dat",
        ] {
            tokio::fs::create_dir_all(root.join(f).parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(root.join(f), "x").await.unwrap();
        }
        let previous: Vec<String> = ["mods/create.jar", "mods/jei.jar", "config/create.toml"]
            .map(String::from)
            .to_vec();
        remove_stale(&root, &previous, &["mods/jei.jar"]).await;
        assert!(!root.join("mods/create.jar").exists());
        assert!(root.join("mods/jei.jar").exists(), "kept by the new pack");
        assert!(
            root.join("config/create.toml").exists(),
            "configs are left alone"
        );
        remove_stale(&root, &previous, &[]).await;
        assert!(!root.join("mods/jei.jar").exists(), "pack removed entirely");
        assert!(root.join("world/level.dat").exists());
        tokio::fs::remove_dir_all(&root).await.unwrap();
    }
}
