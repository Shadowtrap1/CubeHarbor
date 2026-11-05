//! Parallel, verified, resumable-by-skipping downloads with an optional shared cache.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt};
use sha1::{Digest, Sha1};
use sha2::{Sha256, Sha512};
use tokio::io::AsyncWriteExt;

use cubeharbor_core::reqwest;

use crate::Ctx;

#[derive(Debug, Clone, Default)]
pub struct Item {
    pub urls: Vec<String>,
    pub dest: PathBuf,
    pub sha1: Option<String>,
    pub sha256: Option<String>,
    pub sha512: Option<String>,
}

impl Item {
    pub fn new(url: impl Into<String>, dest: impl Into<PathBuf>) -> Self {
        Self { urls: vec![url.into()], dest: dest.into(), ..Default::default() }
    }
    pub fn sha1(mut self, h: Option<String>) -> Self {
        self.sha1 = h;
        self
    }
    pub fn sha256(mut self, h: Option<String>) -> Self {
        self.sha256 = h;
        self
    }

    fn has_hash(&self) -> bool {
        self.sha1.is_some() || self.sha256.is_some() || self.sha512.is_some()
    }

    /// Hashes come from modpack manifests: reject anything that isn't hex of the right length
    /// before it is compared or used as a cache path.
    fn check_hashes(&self) -> Result<()> {
        for (name, h, len) in [("sha1", &self.sha1, 40), ("sha256", &self.sha256, 64), ("sha512", &self.sha512, 128)] {
            if let Some(h) = h
                && (h.len() != len || !h.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                bail!("invalid {name} `{h}` for {}", self.dest.display());
            }
        }
        Ok(())
    }

    fn cache_key(&self) -> Option<String> {
        self.sha1.clone().or_else(|| self.sha512.clone()).or_else(|| self.sha256.clone())
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Default)]
struct Hashers {
    sha1: Sha1,
    sha256: Sha256,
    sha512: Sha512,
}

impl Hashers {
    fn update(&mut self, data: &[u8], item: &Item) {
        if item.sha1.is_some() {
            self.sha1.update(data);
        }
        if item.sha256.is_some() {
            self.sha256.update(data);
        }
        if item.sha512.is_some() {
            self.sha512.update(data);
        }
    }

    fn verify(self, item: &Item) -> Result<()> {
        let check = |name: &str, want: &Option<String>, got: String| -> Result<()> {
            match want {
                Some(w) if !w.eq_ignore_ascii_case(&got) => bail!("{name} mismatch: expected {w}, got {got}"),
                _ => Ok(()),
            }
        };
        check("sha1", &item.sha1, hex(&self.sha1.finalize()))?;
        check("sha256", &item.sha256, hex(&self.sha256.finalize()))?;
        check("sha512", &item.sha512, hex(&self.sha512.finalize()))
    }
}

/// Does `path` already hold the expected content?
async fn matches(path: &Path, item: &Item) -> bool {
    if !item.has_hash() {
        return false;
    }
    let Ok(data) = tokio::fs::read(path).await else { return false };
    let item2 = item.clone();
    tokio::task::spawn_blocking(move || {
        let mut h = Hashers::default();
        h.update(&data, &item2);
        h.verify(&item2).is_ok()
    })
    .await
    .unwrap_or(false)
}

pub async fn fetch(ctx: &Ctx, item: &Item) -> Result<()> {
    item.check_hashes()?;
    if matches(&item.dest, item).await {
        return Ok(());
    }
    if let Some(parent) = item.dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let cached = match (&ctx.cache, item.cache_key()) {
        (Some(dir), Some(key)) => Some(dir.join(&key[..2]).join(&key)),
        _ => None,
    };
    if let Some(c) = &cached
        && matches(c, item).await
    {
        tokio::fs::copy(c, &item.dest).await?;
        return Ok(());
    }

    let mut last_err = None;
    'outer: for url in &item.urls {
        for attempt in 0..4 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await;
            }
            match download_once(ctx, url, item).await {
                Ok(()) => {
                    last_err = None;
                    break 'outer;
                }
                Err(e) => {
                    tracing::debug!("download {url} attempt {attempt} failed: {e:#}");
                    last_err = Some(e);
                }
            }
        }
    }
    if let Some(e) = last_err {
        return Err(e.context(format!("downloading {}", item.dest.display())));
    }
    if let Some(c) = cached {
        if let Some(parent) = c.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let tmp = c.with_extension("tmp");
        if tokio::fs::copy(&item.dest, &tmp).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &c).await;
        }
    }
    Ok(())
}

/// Largest single download (`CUBEHARBOR_MAX_DOWNLOAD_MIB`, default 8 GiB): a hostile or broken
/// URL must not fill the data volume.
fn max_download_bytes() -> u64 {
    std::env::var("CUBEHARBOR_MAX_DOWNLOAD_MIB").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(8 * 1024) * 1024 * 1024
}

/// Without a checksum, transport security is the only integrity check: refuse plain HTTP
/// (including after redirects) unless `CUBEHARBOR_ALLOW_INSECURE_HTTP=true`.
fn check_transport(url: &reqwest::Url, item: &Item) -> Result<()> {
    let allow = crate::env_flag("CUBEHARBOR_ALLOW_INSECURE_HTTP");
    if url.scheme() != "https" && !item.has_hash() && !allow {
        bail!("refusing unverified download over {}: {url} (use https, or set CUBEHARBOR_ALLOW_INSECURE_HTTP=true)", url.scheme());
    }
    Ok(())
}

async fn download_once(ctx: &Ctx, url: &str, item: &Item) -> Result<()> {
    check_transport(&reqwest::Url::parse(url).with_context(|| format!("invalid URL {url}"))?, item)?;
    let resp = ctx.http.client.get(url).send().await?;
    if !resp.status().is_success() {
        bail!("HTTP {} for {url}", resp.status());
    }
    check_transport(resp.url(), item)?;
    let limit = max_download_bytes();
    if resp.content_length().is_some_and(|n| n > limit) {
        bail!("{url} is larger than the {} MiB download limit", limit / 1024 / 1024);
    }
    let tmp = part_path(&item.dest);
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut hashers = Hashers::default();
    let mut stream = resp.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        total += chunk.len() as u64;
        if total > limit {
            drop(file);
            let _ = tokio::fs::remove_file(&tmp).await;
            bail!("{url} exceeded the {} MiB download limit", limit / 1024 / 1024);
        }
        hashers.update(&chunk, item);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    if let Err(e) = hashers.verify(item) {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    tokio::fs::rename(&tmp, &item.dest).await?;
    Ok(())
}

/// `mods/a.jar` -> `mods/a.jar.cubeharbor-part` (appended, so `a.jar` and `a.zip` don't share one).
fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(".cubeharbor-part");
    PathBuf::from(name)
}

/// Download many files concurrently, logging progress.
pub async fn fetch_all(ctx: &Ctx, items: Vec<Item>, what: &str) -> Result<()> {
    let total = items.len();
    if total == 0 {
        return Ok(());
    }
    tracing::info!("downloading {total} {what} ({} parallel)", ctx.concurrency);
    let done = Arc::new(AtomicUsize::new(0));
    futures::stream::iter(items)
        .map(|item| {
            let done = done.clone();
            async move {
                fetch(ctx, &item).await?;
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_multiple_of(25) || n == total {
                    tracing::info!("  {n}/{total} {what}");
                }
                Ok::<_, anyhow::Error>(())
            }
        })
        .buffer_unordered(ctx.concurrency)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(())
}

/// Join a relative path from untrusted input onto `root`, rejecting traversal.
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let rel = rel.replace('\\', "/");
    let mut out = root.to_path_buf();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => bail!("path escapes server directory: {rel}"),
            p if p.contains(':') => bail!("invalid path component: {rel}"),
            p => out.push(p),
        }
    }
    if out == root {
        bail!("empty path");
    }
    Ok(out)
}

/// Download a single file to a temporary location under the state dir.
pub async fn fetch_temp(ctx: &Ctx, url: &str, name: &str, sha1: Option<String>) -> Result<PathBuf> {
    let dest = ctx.root.join(cubeharbor_core::plan::STATE_DIR).join("downloads").join(name);
    fetch(ctx, &Item::new(url, &dest).sha1(sha1)).await.with_context(|| format!("fetching {url}"))?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insecure_unverified_downloads_are_refused() {
        let http = reqwest::Url::parse("http://example.com/mod.jar").unwrap();
        let https = reqwest::Url::parse("https://example.com/mod.jar").unwrap();
        let bare = Item::new("x", "/tmp/x");
        assert!(check_transport(&http, &bare).is_err());
        assert!(check_transport(&https, &bare).is_ok());
        assert!(check_transport(&http, &bare.clone().sha1(Some("ab".into()))).is_ok());
    }

    #[test]
    fn malformed_hashes_are_rejected() {
        let sha1 = "a".repeat(40);
        assert!(Item::new("x", "/tmp/x").sha1(Some(sha1.clone())).check_hashes().is_ok());
        assert!(Item::new("x", "/tmp/x").sha1(Some(sha1.to_uppercase())).check_hashes().is_ok());
        assert!(Item::new("x", "/tmp/x").sha1(Some(String::new())).check_hashes().is_err());
        assert!(Item::new("x", "/tmp/x").sha1(Some("é".into())).check_hashes().is_err());
        assert!(Item::new("x", "/tmp/x").sha1(Some(format!("../{}", &sha1[3..]))).check_hashes().is_err());
        assert!(Item::new("x", "/tmp/x").sha256(Some(sha1)).check_hashes().is_err());
    }

    #[test]
    fn part_files_are_distinct() {
        assert_eq!(part_path(Path::new("/data/mods/a.jar")), PathBuf::from("/data/mods/a.jar.cubeharbor-part"));
        assert_ne!(part_path(Path::new("/data/a.jar")), part_path(Path::new("/data/a.zip")));
    }

    #[test]
    fn traversal_is_rejected() {
        let root = Path::new("/data");
        assert_eq!(safe_join(root, "mods/a.jar").unwrap(), PathBuf::from("/data/mods/a.jar"));
        assert_eq!(safe_join(root, "/config/x").unwrap(), PathBuf::from("/data/config/x"));
        assert!(safe_join(root, "../etc/passwd").is_err());
        assert!(safe_join(root, "mods/../../x").is_err());
        assert!(safe_join(root, "C:/x").is_err());
    }
}
