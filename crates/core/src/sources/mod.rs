//! Clients for upstream distribution APIs (Mojang, Modrinth, CurseForge, loader metadata).

pub mod curseforge;
pub mod modrinth;
pub mod mojang;
pub mod resolve;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

pub const USER_AGENT: &str = concat!("cubeharbor/", env!("CARGO_PKG_VERSION"), " (+", env!("CARGO_PKG_REPOSITORY"), ")");

/// Shared HTTP client with sane timeouts and a descriptive user agent (Modrinth requires one).
#[derive(Clone)]
pub struct Http {
    pub client: reqwest::Client,
    pub curseforge_key: Option<String>,
}

impl Http {
    pub fn new(curseforge_key: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self { client, curseforge_key: curseforge_key.filter(|k| !k.trim().is_empty()) })
    }

    pub fn from_env() -> Result<Self> {
        Self::new(std::env::var("CURSEFORGE_API_KEY").ok())
    }

    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self.client.get(url).send().await.with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("GET {url}: HTTP {status}: {}", body.chars().take(300).collect::<String>());
        }
        resp.json().await.with_context(|| format!("decoding {url}"))
    }

    pub async fn get_text(&self, url: &str) -> Result<String> {
        let resp = self.client.get(url).send().await.with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            bail!("GET {url}: HTTP {}", resp.status());
        }
        Ok(resp.text().await?)
    }
}
