//! Zip helpers (blocking work runs on the blocking pool). Archives are untrusted input.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::download::{TempGuard, part_path, safe_pack_path};

/// Largest single entry read into memory (pack manifests are a few MiB at most).
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 200_000;

/// Zip bomb limits.
#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    bytes: u64,
}

impl Limits {
    /// `CUBEHARBOR_MAX_EXTRACT_MIB`, default 32 GiB.
    fn from_env() -> Self {
        let mib = std::env::var("CUBEHARBOR_MAX_EXTRACT_MIB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(32 * 1024);
        Self {
            entries: MAX_ENTRIES,
            bytes: mib.saturating_mul(1024 * 1024),
        }
    }
}

/// Read a single file from an archive.
pub async fn read_entry(zip: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    let zip = zip.to_path_buf();
    let name = name.to_string();
    tokio::task::spawn_blocking(move || read_entry_blocking(&zip, &name, MAX_ENTRY_BYTES)).await?
}

fn read_entry_blocking(zip: &Path, name: &str, limit: u64) -> Result<Option<Vec<u8>>> {
    let mut a = zip::ZipArchive::new(std::fs::File::open(zip)?)?;
    let result = match a.by_name(name) {
        Ok(f) => {
            let mut buf = vec![];
            // The header's size can lie, so count what actually comes out.
            f.take(limit + 1).read_to_end(&mut buf)?;
            anyhow::ensure!(
                buf.len() as u64 <= limit,
                "{name} in {} is larger than {} MiB",
                zip.display(),
                limit / 1024 / 1024
            );
            Some(buf)
        }
        Err(zip::result::ZipError::FileNotFound) => None,
        Err(e) => return Err(e.into()),
    };
    Ok(result)
}

/// Extract entries under `prefix` (e.g. `overrides/`) into `dest`, returning written paths relative to `dest`.
///
/// With `strip_single_root`, an archive whose entries all live in one top-level folder
/// (common for server packs) is extracted as if that folder were the root.
pub async fn extract(
    zip: &Path,
    prefix: &str,
    dest: &Path,
    strip_single_root: bool,
    skip: Vec<String>,
) -> Result<Vec<String>> {
    let zip = zip.to_path_buf();
    let prefix = prefix.to_string();
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || {
        extract_blocking(
            &zip,
            &prefix,
            &dest,
            strip_single_root,
            &skip,
            Limits::from_env(),
        )
    })
    .await?
}

fn extract_blocking(
    zip: &Path,
    prefix: &str,
    dest: &Path,
    strip_single_root: bool,
    skip: &[String],
    limits: Limits,
) -> Result<Vec<String>> {
    let mut a = zip::ZipArchive::new(
        std::fs::File::open(zip).with_context(|| format!("opening {}", zip.display()))?,
    )?;
    let mut prefix = prefix.to_string();
    if strip_single_root
        && prefix.is_empty()
        && let Some(root) = single_root(&mut a)
    {
        prefix = root;
    }
    anyhow::ensure!(
        a.len() <= limits.entries,
        "{} has {} entries (limit {})",
        zip.display(),
        a.len(),
        limits.entries
    );
    let too_big = || {
        format!(
            "{} unpacks to more than {} MiB",
            zip.display(),
            limits.bytes / 1024 / 1024
        )
    };
    let excluded = |rel: &str| {
        let lower = rel.to_ascii_lowercase();
        skip.iter().any(|s| crate::modpack::excluded(&lower, s))
    };

    // Check every entry first, so a hostile archive is refused before anything is written.
    let mut declared = 0u64;
    for i in 0..a.len() {
        let entry = a.by_index_raw(i)?;
        let name = entry.name()?.replace('\\', "/");
        let Some(rel) = name.strip_prefix(&prefix) else {
            continue;
        };
        if rel.is_empty() || entry.is_dir() {
            continue;
        }
        safe_pack_path(dest, rel)?;
        if !excluded(rel) && !entry.is_symlink() {
            declared = declared.saturating_add(entry.size());
        }
    }
    anyhow::ensure!(declared <= limits.bytes, "{}", too_big());

    let mut unpacked = 0u64;
    let mut written = vec![];
    for i in 0..a.len() {
        let mut entry = a.by_index(i)?;
        let name = entry.name()?.replace('\\', "/");
        let Some(rel) = name.strip_prefix(&prefix) else {
            continue;
        };
        if rel.is_empty() || entry.is_dir() {
            continue;
        }
        if excluded(rel) {
            tracing::info!("  skipping {rel} (excluded)");
            continue;
        }
        if entry.is_symlink() {
            tracing::warn!("  skipping {rel}: symbolic links in archives are not supported");
            continue;
        }
        let out: PathBuf = safe_pack_path(dest, rel)?;
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p)?;
        }
        // Temp file + rename: a partial file never takes the real name, and an existing symlink
        // at the destination is replaced instead of being written through.
        let tmp = part_path(&out);
        let _ = std::fs::remove_file(&tmp);
        let guard = TempGuard::new(tmp.clone());
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        // Declared sizes can lie, so the limit is enforced on the real output too.
        let n = std::io::copy(&mut (&mut entry).take(limits.bytes - unpacked + 1), &mut f)?;
        unpacked += n;
        anyhow::ensure!(unpacked <= limits.bytes, "{}", too_big());
        drop(f);
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &tmp,
                std::fs::Permissions::from_mode(mode & 0o755 | 0o600),
            );
        }
        std::fs::rename(&tmp, &out).with_context(|| format!("writing {}", out.display()))?;
        guard.disarm();
        written.push(rel.to_string());
    }
    Ok(written)
}

fn single_root<R: std::io::Read + std::io::Seek>(a: &mut zip::ZipArchive<R>) -> Option<String> {
    let mut root: Option<String> = None;
    for name in a.file_names() {
        let name = name.ok()?.replace('\\', "/");
        let (first, rest) = name.split_once('/')?;
        if matches!(first, "" | "." | "..") || (rest.is_empty() && !name.ends_with('/')) {
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

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::{SimpleFileOptions, ZipWriter};

    use super::*;
    use crate::testutil::TempDir;

    const LIMITS: Limits = Limits {
        entries: 100,
        bytes: 1024 * 1024,
    };

    fn opts() -> SimpleFileOptions {
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)
    }

    fn build(dir: &TempDir, f: impl FnOnce(&mut ZipWriter<std::fs::File>)) -> PathBuf {
        let path = dir.path().join("fixture.zip");
        let mut w = ZipWriter::new(std::fs::File::create(&path).unwrap());
        f(&mut w);
        w.finish().unwrap();
        path
    }

    fn zip_of(dir: &TempDir, entries: &[(&str, &[u8])]) -> PathBuf {
        build(dir, |w| {
            for (name, data) in entries {
                w.start_file(*name, opts()).unwrap();
                w.write_all(data).unwrap();
            }
        })
    }

    fn extract(zip: &Path, dest: &Path, strip: bool) -> Result<Vec<String>> {
        extract_blocking(zip, "", dest, strip, &[], LIMITS)
    }

    /// Everything under `dir`, relative, sorted.
    fn tree(dir: &Path) -> Vec<String> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                if e.path().is_dir() && !e.path().is_symlink() {
                    walk(&e.path(), base, out);
                } else {
                    out.push(
                        e.path()
                            .strip_prefix(base)
                            .unwrap()
                            .to_string_lossy()
                            .to_string(),
                    );
                }
            }
        }
        let mut out = vec![];
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    #[test]
    fn extracts_normal_archives() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("server");
        let z = zip_of(&tmp, &[("config/a.toml", b"a"), ("mods/b.jar", b"b")]);
        let mut files = extract(&z, &dest, false).unwrap();
        files.sort();
        assert_eq!(files, ["config/a.toml", "mods/b.jar"]);
        assert_eq!(tree(&dest), ["config/a.toml", "mods/b.jar"]);
        // Running again over the existing files is fine and gives the same result.
        extract(&z, &dest, false).unwrap();
        assert_eq!(std::fs::read(dest.join("mods/b.jar")).unwrap(), b"b");
    }

    #[test]
    fn hostile_names_refuse_the_whole_archive() {
        for bad in [
            "../evil",
            "a/../../evil",
            "..\\..\\evil",
            "/etc/evil",
            "\\etc\\evil",
            "C:/evil",
            ".cubeharbor/launch.json",
            "./.cubeharbor/launch.json",
        ] {
            let tmp = TempDir::new();
            let dest = tmp.path().join("server");
            std::fs::create_dir_all(&dest).unwrap();
            // The good entry comes first: it must not be written either.
            let z = zip_of(&tmp, &[("good.txt", b"x"), (bad, b"pwned")]);
            assert!(extract(&z, &dest, false).is_err(), "{bad}");
            assert!(tree(&dest).is_empty(), "{bad} wrote {:?}", tree(&dest));
            assert_eq!(tree(tmp.path()), ["fixture.zip"], "{bad} escaped");
        }
    }

    #[test]
    fn traversal_under_an_overrides_prefix_is_refused() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("server");
        let z = zip_of(&tmp, &[("overrides/../../evil", b"pwned")]);
        assert!(extract_blocking(&z, "overrides/", &dest, false, &[], LIMITS).is_err());
        assert_eq!(tree(tmp.path()), ["fixture.zip"]);
    }

    #[test]
    fn dotdot_is_not_stripped_as_a_single_root() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("server");
        let z = zip_of(&tmp, &[("../a", b"x"), ("../b", b"y")]);
        assert!(extract(&z, &dest, true).is_err());
    }

    #[test]
    fn single_root_is_stripped() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("server");
        let z = zip_of(
            &tmp,
            &[
                ("Pack-1.0/mods/a.jar", b"x"),
                ("Pack-1.0/server.properties", b"y"),
            ],
        );
        extract(&z, &dest, true).unwrap();
        assert_eq!(tree(&dest), ["mods/a.jar", "server.properties"]);
    }

    #[test]
    fn symlink_entries_are_not_created() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("server");
        let z = build(&tmp, |w| {
            w.add_symlink("link", "/etc/passwd", opts()).unwrap();
            w.start_file("ok.txt", opts()).unwrap();
            w.write_all(b"x").unwrap();
        });
        assert_eq!(extract(&z, &dest, false).unwrap(), ["ok.txt"]);
        assert!(std::fs::symlink_metadata(dest.join("link")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_already_on_the_volume_are_not_followed() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("server");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        // A directory symlink leading out of the server directory.
        std::os::unix::fs::symlink(&outside, dest.join("mods")).unwrap();
        let z = zip_of(&tmp, &[("mods/a.jar", b"pwned")]);
        assert!(extract(&z, &dest, false).is_err());
        assert!(tree(&outside).is_empty());

        // A file symlink at the destination is replaced, its target stays untouched.
        let target = outside.join("target");
        std::fs::write(&target, "original").unwrap();
        std::os::unix::fs::symlink(&target, dest.join("config.txt")).unwrap();
        // ... and so is a symlink planted at the temp file's name.
        std::os::unix::fs::symlink(&target, dest.join("config.txt.cubeharbor-part")).unwrap();
        let tmp2 = TempDir::new();
        let z = zip_of(&tmp2, &[("config.txt", b"new")]);
        extract(&z, &dest, false).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        assert!(
            !std::fs::symlink_metadata(dest.join("config.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("config.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn zip_bombs_are_refused() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("server");
        let zeros = vec![0u8; 2 * 1024 * 1024];
        let z = zip_of(&tmp, &[("bomb.bin", &zeros)]);
        assert!(
            std::fs::metadata(&z).unwrap().len() < 64 * 1024,
            "the fixture really is compressed"
        );
        assert!(
            extract(&z, &dest, false)
                .unwrap_err()
                .to_string()
                .contains("MiB")
        );
        assert!(!dest.exists() || tree(&dest).is_empty());
    }

    /// The declared total is checked up front: an earlier, small entry must not be written
    /// before a later one pushes the archive over the limit.
    #[test]
    fn oversized_archives_write_nothing() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("server");
        let zeros = vec![0u8; 2 * 1024 * 1024];
        let z = zip_of(&tmp, &[("small.txt", b"x"), ("bomb.bin", &zeros)]);
        assert!(extract(&z, &dest, false).is_err());
        assert!(
            !dest.exists() || tree(&dest).is_empty(),
            "partial output: {:?}",
            tree(&dest)
        );
    }

    /// The central directory claims a tiny size, but the data inflates to much more.
    #[test]
    fn lying_size_headers_do_not_bypass_the_limit() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("server");
        let zeros = vec![0u8; 2 * 1024 * 1024];
        let z = zip_of(&tmp, &[("bomb.bin", &zeros)]);
        let mut raw = std::fs::read(&z).unwrap();
        let cd = raw.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        raw[cd + 24..cd + 28].copy_from_slice(&10u32.to_le_bytes());
        std::fs::write(&z, raw).unwrap();
        assert!(extract(&z, &dest, false).is_err());
        assert!(
            !dest.exists() || tree(&dest).is_empty(),
            "no partial output: {:?}",
            tree(&dest)
        );
    }

    #[test]
    fn too_many_entries_are_refused() {
        let tmp = TempDir::new();
        let z = build(&tmp, |w| {
            for i in 0..=LIMITS.entries {
                w.start_file(format!("f{i}"), opts()).unwrap();
            }
        });
        assert!(
            extract(&z, &tmp.path().join("server"), false)
                .unwrap_err()
                .to_string()
                .contains("entries")
        );
    }

    #[test]
    fn oversized_manifests_are_not_read_into_memory() {
        let tmp = TempDir::new();
        let z = zip_of(&tmp, &[("modrinth.index.json", &vec![b' '; 4096])]);
        assert!(read_entry_blocking(&z, "modrinth.index.json", 1024).is_err());
        assert_eq!(
            read_entry_blocking(&z, "modrinth.index.json", 4096)
                .unwrap()
                .unwrap()
                .len(),
            4096
        );
        assert!(read_entry_blocking(&z, "missing", 1024).unwrap().is_none());
    }

    #[test]
    fn excluded_entries_are_skipped() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("server");
        let z = zip_of(&tmp, &[("mods/oculus.jar", b"x"), ("mods/keep.jar", b"y")]);
        extract_blocking(&z, "", &dest, false, &["oculus".to_string()], LIMITS).unwrap();
        assert_eq!(tree(&dest), ["mods/keep.jar"]);
    }
}
