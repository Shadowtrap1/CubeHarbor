//! Parallel, verified, resumable-by-skipping downloads with an optional shared cache.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt};
use md5::Md5;
use sha1::{Digest, Sha1};
use sha2::{Sha256, Sha512};
use tokio::io::AsyncWriteExt;

use cubeharbor_core::reqwest;
use cubeharbor_core::sources::{check_url, transient_status};

use crate::Ctx;

#[derive(Debug, Clone, Default)]
pub struct Item {
    pub urls: Vec<String>,
    pub dest: PathBuf,
    pub sha1: Option<String>,
    pub sha256: Option<String>,
    pub sha512: Option<String>,
    /// Only where nothing stronger is published.
    pub md5: Option<String>,
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

    pub fn md5(mut self, h: Option<String>) -> Self {
        self.md5 = h;
        self
    }

    /// A hash that also protects against a hostile network (MD5 only catches corruption).
    fn has_strong_hash(&self) -> bool {
        self.sha1.is_some() || self.sha256.is_some() || self.sha512.is_some()
    }

    fn has_hash(&self) -> bool {
        self.has_strong_hash() || self.md5.is_some()
    }

    /// Hashes come from modpack manifests: reject anything that isn't hex of the right length
    /// before it is compared or used as a cache path.
    fn check_hashes(&self) -> Result<()> {
        for (name, h, len) in [("sha1", &self.sha1, 40), ("sha256", &self.sha256, 64), ("sha512", &self.sha512, 128), ("md5", &self.md5, 32)] {
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
    md5: Md5,
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
        if item.md5.is_some() {
            self.md5.update(data);
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
        check("sha512", &item.sha512, hex(&self.sha512.finalize()))?;
        check("md5", &item.md5, hex(&self.md5.finalize()))
    }
}

/// A failure that retrying the same URL can't fix (policy refusals, 404s, oversized files).
#[derive(Debug)]
struct Permanent(String);

impl std::fmt::Display for Permanent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Permanent {}

/// Removes a temp file when dropped: covers errors, and downloads cancelled by a failure elsewhere.
pub struct TempGuard(Option<PathBuf>);

impl TempGuard {
    pub fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    /// The file was renamed into place: nothing to remove.
    pub fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Does `path` already hold the expected content? Reads in chunks: jars and packs can be huge.
/// A symlink never counts, even to the right content: it would stay in place.
async fn matches(path: &Path, item: &Item) -> bool {
    if !item.has_hash() || !tokio::fs::symlink_metadata(path).await.is_ok_and(|m| m.is_file()) {
        return false;
    }
    let path = path.to_path_buf();
    let item = item.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut h = Hashers::default();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                return Ok::<bool, std::io::Error>(h.verify(&item).is_ok());
            }
            h.update(&buf[..n], &item);
        }
    })
    .await
    .is_ok_and(|r| r.unwrap_or(false))
}

/// Copy to the temp name, then rename, so a crash can't leave a partial file at `dest`.
async fn copy_atomic(from: &Path, dest: &Path) -> Result<()> {
    let tmp = part_path(dest);
    let _ = tokio::fs::remove_file(&tmp).await;
    let guard = TempGuard::new(tmp.clone());
    tokio::fs::copy(from, &tmp).await?;
    tokio::fs::rename(&tmp, dest).await?;
    guard.disarm();
    Ok(())
}

/// Replace a small file in one step.
pub async fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = part_path(path);
    let _ = tokio::fs::remove_file(&tmp).await;
    let guard = TempGuard::new(tmp.clone());
    let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).await.with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(data).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, path).await.with_context(|| format!("writing {}", path.display()))?;
    guard.disarm();
    Ok(())
}

/// Delay before retry number `attempt` (1, 2, 3...): 1s, 2s, 4s.
fn backoff(attempt: u32) -> Duration {
    let base = if cfg!(test) { Duration::from_millis(1) } else { Duration::from_millis(500) };
    base * 2u32.pow(attempt)
}

pub async fn fetch(ctx: &Ctx, item: &Item) -> Result<()> {
    item.check_hashes()?;
    anyhow::ensure!(!item.urls.is_empty(), "no download URL for {}", item.dest.display());
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
        return copy_atomic(c, &item.dest).await;
    }

    let mut last_err = None;
    'outer: for url in &item.urls {
        for attempt in 0..4 {
            if attempt > 0 {
                tokio::time::sleep(backoff(attempt)).await;
            }
            match download_once(ctx, url, item).await {
                Ok(()) => {
                    last_err = None;
                    break 'outer;
                }
                Err(e) => {
                    tracing::debug!("download {url} attempt {attempt} failed: {e:#}");
                    let permanent = e.is::<Permanent>();
                    last_err = Some(e);
                    if permanent {
                        break;
                    }
                }
            }
        }
    }
    if let Some(e) = last_err {
        return Err(e.context(format!("downloading {}", item.dest.display())));
    }
    if let Some(c) = cached {
        store_in_cache(&item.dest, &c).await;
    }
    Ok(())
}

/// Best effort. Pods may share the cache, so each writer needs its own temp name.
async fn store_in_cache(from: &Path, cached: &Path) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = cached.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let mut name = cached.as_os_str().to_owned();
    name.push(format!(".{}.{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let tmp = PathBuf::from(name);
    let guard = TempGuard::new(tmp.clone());
    if tokio::fs::copy(from, &tmp).await.is_ok() && tokio::fs::rename(&tmp, cached).await.is_ok() {
        guard.disarm();
    }
}

/// Largest single download (`CUBEHARBOR_MAX_DOWNLOAD_MIB`, default 8 GiB): a hostile or broken
/// URL must not fill the data volume.
pub fn max_download_bytes() -> u64 {
    std::env::var("CUBEHARBOR_MAX_DOWNLOAD_MIB").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(8 * 1024).saturating_mul(1024 * 1024)
}

/// Overall deadline per download attempt (`CUBEHARBOR_DOWNLOAD_TIMEOUT_SECS`, default 1 hour), so
/// a server trickling bytes can't hold the install forever.
pub fn download_timeout() -> Duration {
    Duration::from_secs(std::env::var("CUBEHARBOR_DOWNLOAD_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(3600))
}

/// Is this URL allowed at all? Without a real checksum, transport security is the only integrity
/// check: refuse plain HTTP (including after redirects) unless `CUBEHARBOR_ALLOW_INSECURE_HTTP=true`.
fn check_transport(url: &reqwest::Url, item: &Item) -> Result<()> {
    check_url(url).map_err(|e| Permanent(format!("{e:#}")))?;
    let allow = crate::env_flag("CUBEHARBOR_ALLOW_INSECURE_HTTP");
    if url.scheme() != "https" && !item.has_strong_hash() && !allow {
        bail!(Permanent(format!("refusing unverified download over {}: {url} (use https, or set CUBEHARBOR_ALLOW_INSECURE_HTTP=true)", url.scheme())));
    }
    Ok(())
}

async fn download_once(ctx: &Ctx, url: &str, item: &Item) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|e| Permanent(format!("invalid URL {url}: {e}")))?;
    check_transport(&parsed, item)?;
    let resp = ctx.http.client.get(url).timeout(ctx.download_timeout).send().await.map_err(|e| {
        if e.is_redirect() { anyhow::Error::new(Permanent(format!("{url}: {e:#}"))) } else { anyhow::Error::new(e) }
    })?;
    let status = resp.status();
    if !status.is_success() {
        let msg = format!("HTTP {status} for {url}");
        if transient_status(status) {
            bail!(msg);
        }
        bail!(Permanent(msg));
    }
    check_transport(resp.url(), item)?;
    let limit = ctx.max_download_bytes;
    let oversized = || Permanent(format!("{url} is larger than the {} MiB download limit", limit / 1024 / 1024));
    if resp.content_length().is_some_and(|n| n > limit) {
        bail!(oversized());
    }
    let tmp = part_path(&item.dest);
    // A leftover (or a planted symlink) must not be written through.
    let _ = tokio::fs::remove_file(&tmp).await;
    let guard = TempGuard::new(tmp.clone());
    let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).await.with_context(|| format!("creating {}", tmp.display()))?;
    let mut hashers = Hashers::default();
    let mut stream = resp.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        total += chunk.len() as u64;
        if total > limit {
            bail!(oversized());
        }
        hashers.update(&chunk, item);
        file.write_all(&chunk).await?;
    }
    hashers.verify(item)?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, &item.dest).await?;
    guard.disarm();
    Ok(())
}

/// `mods/a.jar` -> `mods/a.jar.cubeharbor-part` (appended, so `a.jar` and `a.zip` don't share one).
pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(PART_SUFFIX);
    PathBuf::from(name)
}

const PART_SUFFIX: &str = ".cubeharbor-part";

/// Delete temp files left by a run that was killed. Doesn't follow symlinks.
pub async fn remove_part_files(root: &Path) {
    let root = root.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        let mut dirs = vec![root];
        while let Some(dir) = dirs.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                match e.file_type() {
                    Ok(t) if t.is_dir() => dirs.push(e.path()),
                    Ok(_) if e.file_name().to_string_lossy().ends_with(PART_SUFFIX) => {
                        let _ = std::fs::remove_file(e.path());
                    }
                    _ => {}
                }
            }
        }
    })
    .await;
}

/// Download many files concurrently, logging progress.
pub async fn fetch_all(ctx: &Ctx, items: Vec<Item>, what: &str) -> Result<()> {
    // Two items for one path would race on the same temp file.
    let mut seen = std::collections::HashSet::new();
    let items: Vec<Item> = items
        .into_iter()
        .filter(|i| {
            let new = seen.insert(i.dest.clone());
            if !new {
                tracing::warn!("  ignoring duplicate entry for {}", i.dest.display());
            }
            new
        })
        .collect();
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

/// Join a relative path from untrusted input onto `root`, rejecting traversal and paths through a
/// symlink that leads out of `root` (the server itself can create those on the volume).
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
    check_symlinks(root, &out)?;
    Ok(out)
}

/// Walk the existing parents of `path` below `root`. The last component doesn't matter: files are
/// renamed into place, which replaces a symlink instead of following it.
fn check_symlinks(root: &Path, path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else { return Ok(()) };
    let Ok(below) = parent.strip_prefix(root) else { return Ok(()) };
    let mut cur = root.to_path_buf();
    for part in below.components() {
        cur.push(part);
        let Ok(meta) = std::fs::symlink_metadata(&cur) else { return Ok(()) };
        if meta.file_type().is_symlink() {
            let inside = match (cur.canonicalize(), root.canonicalize()) {
                (Ok(target), Ok(root)) => target.starts_with(root),
                _ => false,
            };
            anyhow::ensure!(inside, "path goes through a symlink out of the server directory: {}", cur.display());
        }
    }
    Ok(())
}

/// [`safe_join`] for paths inside modpacks, which are stricter: absolute paths are refused rather
/// than re-rooted, and a pack can't touch the installer's own state.
pub fn safe_pack_path(root: &Path, rel: &str) -> Result<PathBuf> {
    let norm = rel.replace('\\', "/");
    anyhow::ensure!(!norm.starts_with('/'), "absolute path in modpack: {rel}");
    let first = norm.split('/').find(|p| !p.is_empty() && *p != ".");
    anyhow::ensure!(first != Some(cubeharbor_core::plan::STATE_DIR), "modpack path inside the installer's state directory: {rel}");
    safe_join(root, rel)
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

    use crate::testutil::{Reply, TempDir, ctx, stub};

    fn sha1_of(data: &[u8]) -> Option<String> {
        Some(hex(&Sha1::digest(data)))
    }

    fn item_for(url: &str, dest: &Path, data: &[u8]) -> Item {
        Item::new(url, dest).sha1(sha1_of(data))
    }

    fn no_leftovers(dir: &Path) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            assert!(!e.file_name().to_string_lossy().ends_with(PART_SUFFIX), "left behind: {:?}", e.path());
        }
    }

    #[tokio::test]
    async fn downloads_and_verifies() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("a.jar");
        let s = stub(vec![Reply::Body(b"data".to_vec())]).await;
        fetch(&ctx(tmp.path()), &item_for(&s.url, &dest, b"data")).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"data");
        no_leftovers(tmp.path());
    }

    #[tokio::test]
    async fn rerun_on_a_good_file_does_not_download() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("a.jar");
        std::fs::write(&dest, "data").unwrap();
        let s = stub(vec![Reply::Status(500)]).await;
        fetch(&ctx(tmp.path()), &item_for(&s.url, &dest, b"data")).await.unwrap();
        assert_eq!(s.hits(), 0);
    }

    #[tokio::test]
    async fn rerun_replaces_a_corrupt_file() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("a.jar");
        std::fs::write(&dest, "corrupt").unwrap();
        let s = stub(vec![Reply::Body(b"data".to_vec())]).await;
        fetch(&ctx(tmp.path()), &item_for(&s.url, &dest, b"data")).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"data");
    }

    #[tokio::test]
    async fn transient_failures_are_retried() {
        for first in [Reply::Status(503), Reply::Status(429), Reply::Hangup, Reply::Truncated(b"da".to_vec())] {
            let tmp = TempDir::new();
            let dest = tmp.path().join("a.jar");
            let s = stub(vec![first, Reply::Body(b"data".to_vec())]).await;
            fetch(&ctx(tmp.path()), &item_for(&s.url, &dest, b"data")).await.unwrap();
            assert_eq!(s.hits(), 2);
            assert_eq!(std::fs::read(&dest).unwrap(), b"data");
            no_leftovers(tmp.path());
        }
    }

    #[tokio::test]
    async fn repeated_failure_leaves_nothing_in_place() {
        for reply in [Reply::Truncated(b"da".to_vec()), Reply::Body(b"tampered".to_vec()), Reply::Status(500)] {
            let tmp = TempDir::new();
            let dest = tmp.path().join("a.jar");
            let s = stub(vec![reply]).await;
            assert!(fetch(&ctx(tmp.path()), &item_for(&s.url, &dest, b"data")).await.is_err());
            assert_eq!(s.hits(), 4);
            assert!(!dest.exists());
            no_leftovers(tmp.path());
        }
    }

    #[tokio::test]
    async fn failed_update_keeps_the_previous_file() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("a.jar");
        std::fs::write(&dest, "previous").unwrap();
        let s = stub(vec![Reply::Body(b"tampered".to_vec())]).await;
        assert!(fetch(&ctx(tmp.path()), &item_for(&s.url, &dest, b"data")).await.is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"previous");
        no_leftovers(tmp.path());
    }

    #[tokio::test]
    async fn not_found_is_final_but_tries_the_next_mirror() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("a.jar");
        let gone = stub(vec![Reply::Status(404)]).await;
        let good = stub(vec![Reply::Body(b"data".to_vec())]).await;
        let mut item = item_for(&gone.url, &dest, b"data");
        item.urls.push(good.url.clone());
        fetch(&ctx(tmp.path()), &item).await.unwrap();
        assert_eq!(gone.hits(), 1, "no retries on 404");
        assert_eq!(good.hits(), 1);
    }

    #[tokio::test]
    async fn stalled_servers_time_out() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("a.jar");
        let s = stub(vec![Reply::Stall]).await;
        let mut c = ctx(tmp.path());
        c.download_timeout = Duration::from_millis(50);
        let started = std::time::Instant::now();
        assert!(fetch(&c, &item_for(&s.url, &dest, b"data")).await.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!dest.exists());
        no_leftovers(tmp.path());
    }

    #[tokio::test]
    async fn oversized_downloads_stop() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("a.jar");
        let s = stub(vec![Reply::Body(vec![0; 2048])]).await;
        let mut c = ctx(tmp.path());
        c.max_download_bytes = 1024;
        let err = fetch(&c, &item_for(&s.url, &dest, b"data")).await.unwrap_err();
        assert!(format!("{err:#}").contains("limit"), "{err:#}");
        assert_eq!(s.hits(), 1, "retrying can't help");
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn unverified_http_is_refused_without_a_request() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("a.jar");
        let s = stub(vec![Reply::Body(b"data".to_vec())]).await;
        assert!(fetch(&ctx(tmp.path()), &Item::new(&s.url, &dest)).await.is_err());
        assert_eq!(s.hits(), 0);
        // MD5 alone doesn't justify plain HTTP.
        let md5 = hex(&Md5::digest(b"data"));
        assert!(fetch(&ctx(tmp.path()), &Item::new(&s.url, &dest).md5(Some(md5))).await.is_err());
        assert_eq!(s.hits(), 0);
    }

    #[tokio::test]
    async fn md5_is_verified() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("a.jar");
        let s = stub(vec![Reply::Body(b"data".to_vec())]).await;
        // The stub is plain HTTP, so a strong hash has to come along for the transport check.
        let mut item = item_for(&s.url, &dest, b"data").md5(Some("0".repeat(32)));
        assert!(fetch(&ctx(tmp.path()), &item).await.is_err());
        item.md5 = Some(hex(&Md5::digest(b"data")));
        fetch(&ctx(tmp.path()), &item).await.unwrap();
    }

    #[tokio::test]
    async fn bad_urls_are_refused() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("a.jar");
        let c = ctx(tmp.path());
        for url in ["file:///etc/passwd", "ftp://example.invalid/a", "http://169.254.169.254/latest/meta-data/", "not a url"] {
            assert!(fetch(&c, &item_for(url, &dest, b"data")).await.is_err(), "{url}");
        }
        assert!(fetch(&c, &Item { dest: dest.clone(), ..Default::default() }).await.is_err(), "no URLs at all");
    }

    #[tokio::test]
    async fn redirects_to_metadata_endpoints_are_not_followed() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("a.jar");
        let s = stub(vec![Reply::Redirect("http://169.254.169.254/latest/meta-data/".into())]).await;
        assert!(fetch(&ctx(tmp.path()), &item_for(&s.url, &dest, b"data")).await.is_err());
        assert_eq!(s.hits(), 1);
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn redirects_to_other_hosts_work() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("a.jar");
        let target = stub(vec![Reply::Body(b"data".to_vec())]).await;
        let s = stub(vec![Reply::Redirect(target.url.clone())]).await;
        fetch(&ctx(tmp.path()), &item_for(&s.url, &dest, b"data")).await.unwrap();
        assert_eq!(target.hits(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_at_the_destination_are_replaced_not_followed() {
        let tmp = TempDir::new();
        let root = tmp.path().join("server");
        std::fs::create_dir_all(&root).unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, "data").unwrap();
        let dest = root.join("a.jar");
        std::os::unix::fs::symlink(&victim, &dest).unwrap();
        std::os::unix::fs::symlink(&victim, part_path(&dest)).unwrap();

        let s = stub(vec![Reply::Body(b"data".to_vec())]).await;
        // The link's target has the right hash, but it must not count as installed.
        fetch(&ctx(&root), &item_for(&s.url, &dest, b"data")).await.unwrap();
        assert_eq!(s.hits(), 1);
        assert!(!std::fs::symlink_metadata(&dest).unwrap().file_type().is_symlink());

        std::fs::write(&victim, "original").unwrap();
        std::fs::remove_file(&dest).unwrap();
        std::os::unix::fs::symlink(&victim, part_path(&dest)).unwrap();
        fetch(&ctx(&root), &item_for(&s.url, &dest, b"data")).await.unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
    }

    #[cfg(unix)]
    #[test]
    fn safe_join_refuses_symlinks_leading_out() {
        let tmp = TempDir::new();
        let root = tmp.path().join("server");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("in")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("missing"), root.join("dangling")).unwrap();
        assert!(safe_join(&root, "out/a.jar").is_err());
        assert!(safe_join(&root, "dangling/a.jar").is_err());
        assert!(safe_join(&root, "in/a.jar").is_ok(), "stays inside");
        assert!(safe_join(&root, "new/dir/a.jar").is_ok());
    }

    #[test]
    fn pack_paths_are_stricter() {
        let root = Path::new("/data");
        assert_eq!(safe_pack_path(root, "mods/a.jar").unwrap(), PathBuf::from("/data/mods/a.jar"));
        for bad in ["/etc/passwd", "\\etc\\passwd", "../x", ".cubeharbor/launch.json", "./.cubeharbor/x", ".cubeharbor\\x"] {
            assert!(safe_pack_path(root, bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn cache_is_used_and_verified() {
        let tmp = TempDir::new();
        let cache = tmp.path().join("cache");
        let root = tmp.path().join("server");
        std::fs::create_dir_all(&root).unwrap();
        let mut c = ctx(&root);
        c.cache = Some(cache.clone());
        let s = stub(vec![Reply::Body(b"data".to_vec())]).await;

        let first = root.join("a.jar");
        fetch(&c, &item_for(&s.url, &first, b"data")).await.unwrap();
        let second = root.join("b.jar");
        fetch(&c, &item_for(&s.url, &second, b"data")).await.unwrap();
        assert_eq!(s.hits(), 1, "second copy comes from the cache");
        assert_eq!(std::fs::read(&second).unwrap(), b"data");

        // A corrupted cache entry is ignored and re-downloaded.
        let key = sha1_of(b"data").unwrap();
        std::fs::write(cache.join(&key[..2]).join(&key), "corrupt").unwrap();
        let third = root.join("c.jar");
        fetch(&c, &item_for(&s.url, &third, b"data")).await.unwrap();
        assert_eq!(s.hits(), 2);
        assert_eq!(std::fs::read(&third).unwrap(), b"data");
    }

    #[tokio::test]
    async fn duplicate_entries_download_once() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("a.jar");
        let s = stub(vec![Reply::Body(b"data".to_vec())]).await;
        let item = item_for(&s.url, &dest, b"data");
        fetch_all(&ctx(tmp.path()), vec![item.clone(), item], "files").await.unwrap();
        assert_eq!(s.hits(), 1);
    }

    #[tokio::test]
    async fn one_failure_cancels_the_rest_without_leftovers() {
        let tmp = TempDir::new();
        let good = stub(vec![Reply::Stall]).await;
        let bad = stub(vec![Reply::Status(404)]).await;
        let items = vec![item_for(&good.url, &tmp.path().join("a.jar"), b"data"), item_for(&bad.url, &tmp.path().join("b.jar"), b"data")];
        assert!(fetch_all(&ctx(tmp.path()), items, "files").await.is_err());
        no_leftovers(tmp.path());
    }

    #[tokio::test]
    async fn part_files_from_killed_runs_are_swept() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.path().join("mods")).unwrap();
        std::fs::write(tmp.path().join("mods/a.jar.cubeharbor-part"), "x").unwrap();
        std::fs::write(tmp.path().join("mods/a.jar"), "x").unwrap();
        remove_part_files(tmp.path()).await;
        assert!(!tmp.path().join("mods/a.jar.cubeharbor-part").exists());
        assert!(tmp.path().join("mods/a.jar").exists());
    }

    #[tokio::test]
    async fn atomic_writes_replace_the_file() {
        let tmp = TempDir::new();
        let path = tmp.path().join("state.json");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, b"new").await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        no_leftovers(tmp.path());
    }
}
