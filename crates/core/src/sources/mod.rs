//! Clients for upstream distribution APIs (Mojang, Modrinth, CurseForge, loader metadata).

pub mod curseforge;
pub mod modrinth;
pub mod mojang;
pub mod resolve;

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

pub const USER_AGENT: &str = concat!("cubeharbor/", env!("CARGO_PKG_VERSION"), " (+", env!("CARGO_PKG_REPOSITORY"), ")");

/// Metadata (API) calls: per-attempt limit, attempts, and first retry delay.
const API_TIMEOUT: Duration = Duration::from_secs(60);
const API_ATTEMPTS: u32 = 3;
const API_BACKOFF: Duration = if cfg!(test) { Duration::from_millis(1) } else { Duration::from_millis(500) };

fn is_link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_link_local(),
        IpAddr::V6(a) => a.to_ipv4_mapped().is_some_and(|m| m.is_link_local()) || (a.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Only `http(s)` URLs may be fetched, and never link-local addresses (cloud metadata endpoints).
/// Applied to every download and to every redirect hop.
pub fn check_url(url: &reqwest::Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!("refusing URL scheme `{}`: {url}", url.scheme());
    }
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    if host.parse().is_ok_and(is_link_local) {
        bail!("refusing link-local address: {url}");
    }
    Ok(())
}

/// Worth another try? Rate limits, timeouts and server errors are; other client errors are not.
pub fn transient_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || matches!(status.as_u16(), 408 | 425 | 429)
}

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
            // A stall limit, not a total one: big downloads may legitimately take a long time.
            // Downloads set their own overall deadline, API calls use `API_TIMEOUT`.
            .read_timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 {
                    return attempt.error("too many redirects");
                }
                match check_url(attempt.url()) {
                    Ok(()) => attempt.follow(),
                    Err(e) => attempt.error(e.to_string()),
                }
            }))
            .build()?;
        Ok(Self { client, curseforge_key: curseforge_key.filter(|k| !k.trim().is_empty()) })
    }

    pub fn from_env() -> Result<Self> {
        Self::new(std::env::var("CURSEFORGE_API_KEY").ok())
    }

    /// Send an API request, retrying transient failures with backoff. The last response is
    /// returned whatever its status, so callers keep their own error messages.
    pub async fn send(&self, req: reqwest::RequestBuilder) -> reqwest::Result<reqwest::Response> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let Some(this) = req.try_clone() else { return req.timeout(API_TIMEOUT).send().await };
            match this.timeout(API_TIMEOUT).send().await {
                Ok(resp) if attempt >= API_ATTEMPTS || !transient_status(resp.status()) => return Ok(resp),
                Ok(resp) => tracing::debug!("{}: HTTP {}, retrying", resp.url(), resp.status()),
                Err(e) if attempt >= API_ATTEMPTS || e.is_builder() || e.is_redirect() => return Err(e),
                Err(e) => tracing::debug!("request failed, retrying: {e}"),
            }
            tokio::time::sleep(API_BACKOFF * 2u32.pow(attempt - 1)).await;
        }
    }

    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self.send(self.client.get(url)).await.with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("GET {url}: HTTP {status}: {}", body.chars().take(300).collect::<String>());
        }
        resp.json().await.with_context(|| format!("decoding {url}"))
    }

    pub async fn get_text(&self, url: &str) -> Result<String> {
        let resp = self.send(self.client.get(url)).await.with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            bail!("GET {url}: HTTP {}", resp.status());
        }
        Ok(resp.text().await?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// Local stub: answers the n-th connection with the n-th status (the last one repeats).
    async fn stub(statuses: Vec<u16>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let n = count.fetch_add(1, Ordering::SeqCst);
                let status = statuses[n.min(statuses.len() - 1)];
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let body = "[1]";
                let _ = s.write_all(format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
            }
        });
        (url, hits)
    }

    #[tokio::test]
    async fn api_calls_retry_transient_errors_only() {
        let http = Http::new(None).unwrap();
        let (url, hits) = stub(vec![503, 429, 200]).await;
        assert_eq!(http.get_json::<Vec<u8>>(&url).await.unwrap(), vec![1]);
        assert_eq!(hits.load(Ordering::SeqCst), 3);

        let (url, hits) = stub(vec![404]).await;
        assert!(http.get_json::<Vec<u8>>(&url).await.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 1, "a 404 is final");

        let (url, hits) = stub(vec![500]).await;
        assert!(http.get_json::<Vec<u8>>(&url).await.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), API_ATTEMPTS as usize, "gives up eventually");
    }

    #[test]
    fn url_policy() {
        let check = |u: &str| check_url(&reqwest::Url::parse(u).unwrap());
        assert!(check("https://cdn.modrinth.com/a.jar").is_ok());
        assert!(check("http://10.0.0.5/mirror/a.jar").is_ok());
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/a",
            "gopher://example.com/a",
            "http://169.254.169.254/latest/meta-data/",
            "http://2852039166/latest/meta-data/",
            "http://[fe80::1]/x",
            "http://[::ffff:169.254.169.254]/x",
        ] {
            assert!(check(bad).is_err(), "{bad}");
        }
    }
}
