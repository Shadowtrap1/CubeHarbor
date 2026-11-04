//! Zip helpers (blocking work runs on the blocking pool).

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::download::safe_join;

/// Read a single file from an archive.
pub async fn read_entry(zip: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    let zip = zip.to_path_buf();
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let mut a = zip::ZipArchive::new(std::fs::File::open(&zip)?)?;
        let result = match a.by_name(&name) {
            Ok(mut f) => {
                let mut buf = vec![];
                f.read_to_end(&mut buf)?;
                Some(buf)
            }
            Err(zip::result::ZipError::FileNotFound) => None,
            Err(e) => return Err(e.into()),
        };
        Ok(result)
    })
    .await?
}

/// Extract entries under `prefix` (e.g. `overrides/`) into `dest`, returning written paths relative to `dest`.
///
/// With `strip_single_root`, an archive whose entries all live in one top-level folder
/// (common for server packs) is extracted as if that folder were the root.
pub async fn extract(zip: &Path, prefix: &str, dest: &Path, strip_single_root: bool, skip: Vec<String>) -> Result<Vec<String>> {
    let zip = zip.to_path_buf();
    let prefix = prefix.to_string();
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || extract_blocking(&zip, &prefix, &dest, strip_single_root, &skip)).await?
}

fn extract_blocking(zip: &Path, prefix: &str, dest: &Path, strip_single_root: bool, skip: &[String]) -> Result<Vec<String>> {
    let mut a = zip::ZipArchive::new(std::fs::File::open(zip).with_context(|| format!("opening {}", zip.display()))?)?;
    let mut prefix = prefix.to_string();
    if strip_single_root
        && prefix.is_empty()
        && let Some(root) = single_root(&mut a)
    {
        prefix = root;
    }
    // Zip bombs: cap the number of entries and the total unpacked size.
    const MAX_ENTRIES: usize = 200_000;
    let max_bytes: u64 = std::env::var("CUBEHARBOR_MAX_EXTRACT_MIB").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(32 * 1024) * 1024 * 1024;
    anyhow::ensure!(a.len() <= MAX_ENTRIES, "{} has {} entries (limit {MAX_ENTRIES})", zip.display(), a.len());
    let mut unpacked = 0u64;
    let mut written = vec![];
    for i in 0..a.len() {
        let mut entry = a.by_index(i)?;
        let name = entry.name()?.replace('\\', "/");
        let Some(rel) = name.strip_prefix(&prefix) else { continue };
        if rel.is_empty() || entry.is_dir() {
            continue;
        }
        let lower = rel.to_ascii_lowercase();
        if skip.iter().any(|s| crate::modpack::excluded(&lower, s)) {
            tracing::info!("  skipping {rel} (excluded)");
            continue;
        }
        let out: PathBuf = safe_join(dest, rel)?;
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p)?;
        }
        let mut f = std::fs::File::create(&out)?;
        let n = std::io::copy(&mut (&mut entry).take(max_bytes - unpacked + 1), &mut f)?;
        unpacked += n;
        anyhow::ensure!(unpacked <= max_bytes, "{} unpacks to more than {} MiB", zip.display(), max_bytes / 1024 / 1024);
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode & 0o755 | 0o600));
        }
        written.push(rel.trim_start_matches('/').to_string());
    }
    Ok(written)
}

fn single_root<R: std::io::Read + std::io::Seek>(a: &mut zip::ZipArchive<R>) -> Option<String> {
    let mut root: Option<String> = None;
    for name in a.file_names() {
        let name = name.ok()?.replace('\\', "/");
        let (first, rest) = name.split_once('/')?;
        if rest.is_empty() && !name.ends_with('/') {
            return None;
        }
        match &root {
            None => root = Some(first.to_string()),
            Some(r) if r == first => {}
            Some(_) => return None,
        }
    }
    root.map(|r| format!("{r}/"))
}
