//! Modrinth `.mrpack` files.

use anyhow::{Context, Result};
use cubeharbor_core::sources::modrinth;
use futures::future::BoxFuture;

use super::{PackFormat, PackInput, PackResult, client_only_files, excluded};
use crate::Ctx;
use crate::archive;
use crate::download::{Item, fetch_all, safe_pack_path};

const INDEX: &str = "modrinth.index.json";

/// Hosts `.mrpack` files may download from, per the Modrinth pack format. Anything else could
/// point the installer at cluster-internal services or cloud metadata endpoints.
const MRPACK_HOSTS: &[&str] = &[
    "cdn.modrinth.com",
    "github.com",
    "raw.githubusercontent.com",
    "gitlab.com",
];

/// Keep only download URLs the pack format allows (HTTPS on `MRPACK_HOSTS`), unless
/// `CUBEHARBOR_MRPACK_ANY_HOST` is set.
fn allowed_downloads(path: &str, urls: &[String], any_host: bool) -> Result<Vec<String>> {
    if any_host {
        return Ok(urls.to_vec());
    }
    let allowed = |u: &String| {
        cubeharbor_core::reqwest::Url::parse(u).is_ok_and(|u| {
            u.scheme() == "https"
                && u.host_str().is_some_and(|h| MRPACK_HOSTS.contains(&h))
                && u.port().is_none()
        })
    };
    let ok: Vec<String> = urls.iter().filter(|u| allowed(u)).cloned().collect();
    for u in urls.iter().filter(|u| !allowed(u)) {
        tracing::warn!("  {path}: ignoring download URL outside the allowed hosts: {u}");
    }
    anyhow::ensure!(
        !ok.is_empty(),
        "{path}: no download URL on an allowed host ({}); set CUBEHARBOR_MRPACK_ANY_HOST=true in the server's env to allow any",
        MRPACK_HOSTS.join(", ")
    );
    Ok(ok)
}

async fn install(ctx: &Ctx, input: &PackInput<'_>) -> Result<PackResult> {
    let PackInput {
        archive,
        exclude,
        include,
        ..
    } = *input;
    let raw = archive::read_entry(archive, INDEX)
        .await?
        .context("missing modrinth.index.json")?;
    let index: modrinth::PackIndex =
        serde_json::from_slice(&raw).context("parsing modrinth.index.json")?;
    tracing::info!("{} — {} files", index.name, index.files.len());

    validate_index(ctx, &index)?;

    let candidates: Vec<(String, Option<String>)> = index
        .files
        .iter()
        .filter(|f| f.env.as_ref().and_then(|e| e.server.as_deref()) != Some("unsupported"))
        .map(|f| (f.path.clone(), f.hashes.sha1.clone()))
        .collect();
    let client_only = client_only_files(ctx, &candidates, include).await;

    let any_host = crate::env_flag("CUBEHARBOR_MRPACK_ANY_HOST");
    let mut items = vec![];
    let mut files = vec![];
    for f in &index.files {
        let lower = f.path.to_ascii_lowercase();
        if f.env.as_ref().and_then(|e| e.server.as_deref()) == Some("unsupported") {
            continue;
        }
        if client_only.contains(&f.path) {
            continue;
        }
        if exclude.iter().any(|e| excluded(&lower, e)) {
            tracing::info!("  skipping {} (excluded)", f.path);
            continue;
        }
        let dest = safe_pack_path(&ctx.root, &f.path)?;
        items.push(Item {
            urls: allowed_downloads(&f.path, &f.downloads, any_host)?,
            dest,
            sha1: f.hashes.sha1.clone(),
            sha512: f.hashes.sha512.clone(),
            ..Default::default()
        });
        files.push(f.path.clone());
    }
    fetch_all(ctx, items, "pack files").await?;

    for prefix in ["overrides/", "server-overrides/"] {
        let mut written =
            archive::extract(archive, prefix, &ctx.root, false, exclude.to_vec()).await?;
        files.append(&mut written);
    }

    Ok(PackResult {
        minecraft_version: index.minecraft_version().map(str::to_string),
        loader: index.loader(),
        files,
        server_pack: false,
    })
}

/// Reject a hostile index before anything is contacted or written: every file the server would
/// install needs a path inside the server directory and a hash (the format requires both).
fn validate_index(ctx: &Ctx, index: &modrinth::PackIndex) -> Result<()> {
    for f in index
        .files
        .iter()
        .filter(|f| f.env.as_ref().and_then(|e| e.server.as_deref()) != Some("unsupported"))
    {
        safe_pack_path(&ctx.root, &f.path)?;
        anyhow::ensure!(
            f.hashes.sha1.is_some() || f.hashes.sha512.is_some(),
            "{}: pack file has no sha1 or sha512",
            f.path
        );
    }
    Ok(())
}

pub struct Mrpack;

impl PackFormat for Mrpack {
    fn name(&self) -> &'static str {
        "mrpack"
    }

    // By content, whatever the pack was declared as: a raw URL can point at an .mrpack.
    fn accepts<'a>(&'a self, input: &'a PackInput<'a>) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(archive::read_entry(input.archive, INDEX).await?.is_some()) })
    }

    fn install<'a>(
        &'a self,
        ctx: &'a Ctx,
        input: &'a PackInput<'a>,
    ) -> BoxFuture<'a, Result<PackResult>> {
        Box::pin(install(ctx, input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(path: &str, hashes: &str) -> modrinth::PackIndex {
        let json = serde_json::json!({
            "name": "evil",
            "dependencies": {},
            "files": [{ "path": path, "hashes": serde_json::from_str::<serde_json::Value>(hashes).unwrap(), "downloads": ["https://cdn.modrinth.com/a.jar"] }],
        });
        serde_json::from_value(json).unwrap()
    }

    #[tokio::test]
    async fn hostile_mrpack_indexes_are_rejected_before_any_request() {
        let dir = crate::testutil::TempDir::new();
        let ctx = crate::testutil::ctx(dir.path());
        let sha = r#"{"sha1":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
        assert!(validate_index(&ctx, &index("mods/a.jar", sha)).is_ok());
        for path in [
            "../../etc/cron.d/x",
            "mods/../../x",
            "/etc/passwd",
            "\\etc\\passwd",
            "..\\..\\x",
            "C:/x",
            ".cubeharbor/launch.json",
            "./.cubeharbor/state.json",
        ] {
            assert!(validate_index(&ctx, &index(path, sha)).is_err(), "{path}");
        }
        assert!(
            validate_index(&ctx, &index("mods/a.jar", "{}")).is_err(),
            "missing hashes"
        );
    }

    #[test]
    fn mrpack_download_hosts() {
        let urls = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let ok = urls(&["https://cdn.modrinth.com/data/AANobbMI/versions/x/sodium.jar"]);
        assert_eq!(allowed_downloads("mods/a.jar", &ok, false).unwrap(), ok);
        // Disallowed mirrors are dropped as long as one allowed URL remains.
        let mixed = urls(&[
            "http://169.254.169.254/latest/meta-data/",
            "https://github.com/o/r/releases/download/v1/a.jar",
        ]);
        assert_eq!(
            allowed_downloads("mods/a.jar", &mixed, false).unwrap(),
            urls(&["https://github.com/o/r/releases/download/v1/a.jar"])
        );
        for bad in [
            "http://cdn.modrinth.com/a.jar",
            "https://cdn.modrinth.com.evil.example/a.jar",
            "https://evil.example/cdn.modrinth.com/a.jar",
            "https://cdn.modrinth.com:8443/a.jar",
            "https://some-svc.other-ns.svc:8080/admin",
        ] {
            assert!(
                allowed_downloads("mods/a.jar", &urls(&[bad]), false).is_err(),
                "{bad}"
            );
        }
        assert!(
            allowed_downloads(
                "mods/a.jar",
                &urls(&["https://my.mirror.example/a.jar"]),
                true
            )
            .is_ok()
        );
    }
}
