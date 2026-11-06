//! Keep Cloudflare A records pointed at the cluster's public IPv4 address.
//!
//! Home installs sit behind a router whose public IP changes now and then. external-dns would
//! publish the gateway Service's address, which behind NAT is a LAN address, so this task asks a
//! lookup service for the public address instead and updates Cloudflare when it changes.
//!
//! Only the configured record names (by default `*.<baseDomain>`), plus optionally the servers'
//! own hostnames inside the zone, are touched. Records are created when missing and updated when
//! they point elsewhere or are proxied (Cloudflare's proxy can't carry Minecraft TCP). Nothing is
//! ever deleted.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use cubeharbor_core::MinecraftServer;
use cubeharbor_core::sources::USER_AGENT;
use cubeharbor_core::sources::modrinth::urlencode;
use kube::{Api, Client};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::config::Config;

pub const DEFAULT_API_URL: &str = "https://api.cloudflare.com/client/v4";
pub const DEFAULT_IP_URLS: &str = "https://api.ipify.org,https://ipv4.icanhazip.com";
/// Set on records this task creates, so people can tell them apart in the Cloudflare UI.
const COMMENT: &str = "managed by cubeharbor";
const MIN_INTERVAL: Duration = Duration::from_secs(30);
const FIRST_RETRY: Duration = Duration::from_secs(15);
/// Even with an unchanged IP, records are re-checked this often, so one deleted or switched to
/// proxied in Cloudflare is repaired.
const RESYNC: Duration = Duration::from_secs(3600);

/// A Cloudflare API token. `Debug` never shows it, so it can't leak through a logged config.
#[derive(Clone)]
pub struct ApiToken(String);

impl FromStr for ApiToken {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.trim().to_string()))
    }
}

impl fmt::Debug for ApiToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiToken(<redacted>)")
    }
}

/// Validated settings, built from the operator config.
#[derive(Debug, Clone)]
pub struct Settings {
    pub api_url: String,
    pub token: ApiToken,
    /// Zone name (`example.com`) or zone ID.
    pub zone: String,
    pub records: Vec<String>,
    pub server_hostnames: bool,
    pub interval: Duration,
    pub ttl: u32,
    pub ip_urls: Vec<String>,
    /// Never managed: the dashboard's host (RESERVED_HOSTS).
    pub reserved: Vec<String>,
}

impl Settings {
    /// `Ok(None)` when the feature is off.
    pub fn from_config(cfg: &Config) -> Result<Option<Self>, String> {
        if !cfg.cloudflare_dns {
            return Ok(None);
        }
        let token = cfg.cloudflare_api_token.clone().filter(|t| !t.0.is_empty()).ok_or("CLOUDFLARE_API_TOKEN is empty")?;
        let zone = cfg.cloudflare_zone.trim().trim_end_matches('.').to_ascii_lowercase();
        if zone.is_empty() {
            return Err("CLOUDFLARE_ZONE is empty".into());
        }
        let mut records: Vec<String> = csv(&cfg.dns_records).map(cubeharbor_core::crd::normalize_host).collect();
        if records.is_empty() {
            let base = cfg.base_domain().ok_or("DNS_RECORDS is empty and there is no BASE_DOMAIN to default to")?;
            records.push(format!("*.{}", cubeharbor_core::crd::normalize_host(base)));
        }
        let mut seen = std::collections::HashSet::new();
        records.retain(|r| seen.insert(r.clone()));
        let ip_urls: Vec<String> = csv(&cfg.public_ip_urls).map(str::to_string).collect();
        if ip_urls.is_empty() {
            return Err("PUBLIC_IP_URLS is empty".into());
        }
        if cfg.dns_ttl != 1 && !(60..=86400).contains(&cfg.dns_ttl) {
            return Err(format!("DNS_TTL {} must be 1 (automatic) or 60-86400 seconds", cfg.dns_ttl));
        }
        Ok(Some(Self {
            api_url: cfg.cloudflare_api_url.trim_end_matches('/').to_string(),
            token,
            zone,
            records,
            server_hostnames: cfg.dns_server_hostnames,
            interval: parse_interval(&cfg.dns_interval)?,
            ttl: cfg.dns_ttl,
            ip_urls,
            reserved: csv(&cfg.reserved_hosts).map(cubeharbor_core::crd::normalize_host).collect(),
        }))
    }
}

fn csv(s: &str) -> impl Iterator<Item = &str> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// `300`, `30s`, `5m` or `1h`; at least 30 seconds.
pub fn parse_interval(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().map_err(|_| format!("DNS_INTERVAL `{s}` is not a duration like 5m"))?;
    let scale = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(format!("DNS_INTERVAL `{s}`: use s, m or h")),
    };
    let d = Duration::from_secs(n.checked_mul(scale).ok_or_else(|| format!("DNS_INTERVAL `{s}` is too large"))?);
    if d < MIN_INTERVAL {
        return Err(format!("DNS_INTERVAL `{s}` is shorter than 30s"));
    }
    Ok(d)
}

/// Read a lookup service's answer: one public IPv4 address, optionally surrounded by whitespace.
pub fn parse_ip(body: &str) -> Result<Ipv4Addr, String> {
    let text = body.trim();
    let shown: String = text.chars().take(60).collect();
    match text.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) if is_public(ip) => Ok(ip),
        Ok(IpAddr::V4(ip)) => Err(format!("{ip} is not a public address")),
        Ok(IpAddr::V6(ip)) => Err(format!("got IPv6 address {ip}; use an IPv4-only lookup URL")),
        Err(_) => Err(format!("not an IP address: {shown:?}")),
    }
}

fn is_public(ip: Ipv4Addr) -> bool {
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast())
}

/// Ask the lookup URLs in order; the first good answer wins.
pub async fn public_ip(http: &reqwest::Client, urls: &[String]) -> Result<Ipv4Addr> {
    let mut errors = Vec::new();
    for url in urls {
        let answer = async {
            let resp = http.get(url).send().await?.error_for_status()?;
            parse_ip(&resp.text().await?).map_err(|e| anyhow!(e))
        };
        match answer.await {
            Ok(ip) => return Ok(ip),
            Err(e) => {
                tracing::debug!("dns: public IP lookup via {url} failed: {e:#}");
                errors.push(format!("{url}: {e:#}"));
            }
        }
    }
    bail!("could not find the public IP ({})", errors.join("; "))
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    pub id: String,
    pub name: String,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub content: String,
    #[serde(default)]
    pub proxied: bool,
    #[serde(default)]
    pub comment: Option<String>,
}

/// What to do about one record name.
#[derive(Debug, PartialEq, Eq)]
pub enum Action<'a> {
    Keep,
    /// The name is a CNAME (e.g. a Cloudflare Tunnel hostname): never ours to change.
    Cname,
    Create,
    Update(&'a Record),
}

/// Decide from the records Cloudflare has for a name. A CNAME there means someone else owns the
/// name, and Cloudflare wouldn't accept an A record next to it anyway. With several A records (round robin),
/// one that is already right is enough; otherwise ours (by comment) or the first is updated, and
/// the others are left alone.
pub fn plan(existing: &[Record], ip: Ipv4Addr) -> Action<'_> {
    if existing.iter().any(|r| r.kind == "CNAME") {
        return Action::Cname;
    }
    let ip = ip.to_string();
    let a: Vec<&Record> = existing.iter().filter(|r| r.kind == "A").collect();
    if a.iter().any(|r| r.content == ip && !r.proxied) {
        return Action::Keep;
    }
    match a.iter().find(|r| r.comment.as_deref() == Some(COMMENT)).or(a.first()) {
        Some(r) => Action::Update(r),
        None => Action::Create,
    }
}

fn in_zone(name: &str, zone: &str) -> bool {
    name == zone || name.strip_suffix(zone).is_some_and(|p| p.ends_with('.'))
}

/// Is `host` one label below a configured wildcard (`*.mc.example.com` covers `lobby.mc.example.com`)?
fn covered_by_wildcard(host: &str, records: &[String]) -> bool {
    records.iter().filter_map(|r| r.strip_prefix("*.")).any(|base| {
        host.strip_suffix(base).and_then(|p| p.strip_suffix('.')).is_some_and(|label| !label.is_empty() && !label.contains('.'))
    })
}

/// Record names to manage: the configured ones inside the zone, then server hostnames inside the
/// zone that no configured wildcard already covers. `reserved` names (the dashboard's host, which
/// may be a tunnel CNAME) are never included. Also returns configured names outside the zone.
pub fn desired_names(records: &[String], server_hosts: &[String], zone: &str, reserved: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut names, outside): (Vec<String>, Vec<String>) =
        records.iter().filter(|r| !reserved.contains(r)).cloned().partition(|r| in_zone(r, zone));
    let mut extra: Vec<String> = server_hosts
        .iter()
        .map(|h| cubeharbor_core::crd::normalize_host(h))
        .filter(|h| !h.is_empty() && !h.contains('*') && in_zone(h, zone) && !covered_by_wildcard(h, records) && !reserved.contains(h))
        .filter(|h| !names.contains(h))
        .collect();
    extra.sort();
    extra.dedup();
    names.extend(extra);
    (names, outside)
}

#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
    result: Option<T>,
}

#[derive(Deserialize)]
struct ApiError {
    code: i64,
    message: String,
}

/// Unwrap Cloudflare's `{success, errors, result}` envelope.
fn decode<T: DeserializeOwned>(status: StatusCode, body: &str) -> Result<T> {
    let env: Envelope<T> = serde_json::from_str(body).map_err(|e| {
        let shown: String = body.chars().take(200).collect();
        anyhow!("HTTP {status}: unexpected response ({e}): {shown}")
    })?;
    if !status.is_success() || !env.success {
        let errors: Vec<String> = env.errors.iter().map(|e| format!("{} (code {})", e.message, e.code)).collect();
        bail!("HTTP {status}: {}", if errors.is_empty() { "request failed".to_string() } else { errors.join("; ") });
    }
    env.result.ok_or_else(|| anyhow!("HTTP {status}: response has no result"))
}

pub struct Cloudflare {
    http: reqwest::Client,
    api_url: String,
    token: ApiToken,
}

impl Cloudflare {
    pub fn new(http: reqwest::Client, api_url: &str, token: ApiToken) -> Self {
        Self { http, api_url: api_url.trim_end_matches('/').to_string(), token }
    }

    async fn call<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T> {
        let resp = req.bearer_auth(&self.token.0).send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        decode(status, &body)
    }

    /// Look the zone up by ID or by name.
    pub async fn zone(&self, zone: &str) -> Result<Zone> {
        if zone.len() == 32 && zone.bytes().all(|b| b.is_ascii_hexdigit()) {
            return self.call(self.http.get(format!("{}/zones/{zone}", self.api_url))).await.with_context(|| format!("reading zone {zone}"));
        }
        let url = format!("{}/zones?name={}", self.api_url, urlencode(zone));
        let zones: Vec<Zone> = self.call(self.http.get(url)).await.with_context(|| format!("looking up zone {zone}"))?;
        zones.into_iter().next().ok_or_else(|| anyhow!("zone {zone} not found (or the API token has no access to it)"))
    }

    /// All records with exactly this name (every type, so a CNAME is noticed).
    pub async fn records(&self, zone_id: &str, name: &str) -> Result<Vec<Record>> {
        let url = format!("{}/zones/{zone_id}/dns_records?name={}&per_page=100", self.api_url, urlencode(name));
        self.call(self.http.get(url)).await.with_context(|| format!("listing records for {name}"))
    }

    pub async fn create(&self, zone_id: &str, name: &str, ip: Ipv4Addr, ttl: u32) -> Result<Record> {
        let body = json!({ "type": "A", "name": name, "content": ip.to_string(), "ttl": ttl, "proxied": false, "comment": COMMENT });
        let url = format!("{}/zones/{zone_id}/dns_records", self.api_url);
        self.call(self.http.post(url).json(&body)).await.with_context(|| format!("creating A record {name}"))
    }

    /// Point an existing record at `ip`. Its name and comment are left as they are.
    pub async fn update(&self, zone_id: &str, record: &Record, ip: Ipv4Addr, ttl: u32) -> Result<Record> {
        let body = json!({ "content": ip.to_string(), "ttl": ttl, "proxied": false });
        let url = format!("{}/zones/{zone_id}/dns_records/{}", self.api_url, record.id);
        self.call(self.http.patch(url).json(&body)).await.with_context(|| format!("updating A record {}", record.name))
    }
}

/// What the loop remembers between runs.
pub struct Syncer {
    zone: Option<Zone>,
    last_ip: Option<Ipv4Addr>,
    /// IP and names of the last fully successful sync, and when it was; Cloudflare isn't called
    /// while both match, until `resync` has passed.
    synced: Option<(Ipv4Addr, Vec<String>, tokio::time::Instant)>,
    resync: Duration,
}

impl Default for Syncer {
    fn default() -> Self {
        Self { zone: None, last_ip: None, synced: None, resync: RESYNC }
    }
}

impl Syncer {
    /// One pass. Errors are worth retrying; settings problems are only logged.
    pub async fn sync(&mut self, http: &reqwest::Client, cf: &Cloudflare, s: &Settings, server_hosts: &[String]) -> Result<()> {
        let ip = public_ip(http, &s.ip_urls).await?;
        if self.last_ip != Some(ip) {
            match self.last_ip {
                Some(old) => tracing::info!("dns: public IP changed from {old} to {ip}"),
                None => tracing::info!("dns: public IP is {ip}"),
            }
            self.last_ip = Some(ip);
        }
        let zone = match &self.zone {
            Some(z) => z.clone(),
            None => {
                let z = cf.zone(&s.zone).await?;
                tracing::info!("dns: using Cloudflare zone {} ({})", z.name, z.id);
                self.zone.insert(z).clone()
            }
        };
        let (names, outside) = desired_names(&s.records, server_hosts, &zone.name, &s.reserved);
        if self.synced.as_ref().is_some_and(|(i, n, at)| *i == ip && *n == names && at.elapsed() < self.resync) {
            tracing::debug!("dns: {ip} unchanged, nothing to do");
            return Ok(());
        }
        for name in &outside {
            tracing::warn!("dns: {name} is not in zone {}; skipping it", zone.name);
        }
        let mut failed = 0;
        for name in &names {
            if let Err(e) = self.apply(cf, &zone, name, ip, s.ttl).await {
                tracing::warn!("dns: {name}: {e:#}");
                failed += 1;
            }
        }
        if failed > 0 {
            bail!("{failed} of {} records could not be updated", names.len());
        }
        self.synced = Some((ip, names, tokio::time::Instant::now()));
        Ok(())
    }

    async fn apply(&self, cf: &Cloudflare, zone: &Zone, name: &str, ip: Ipv4Addr, ttl: u32) -> Result<()> {
        let existing = cf.records(&zone.id, name).await?;
        let others = existing.iter().filter(|r| r.kind == "A").count().saturating_sub(1);
        match plan(&existing, ip) {
            Action::Keep => tracing::debug!("dns: {name} already points at {ip}"),
            Action::Cname => {
                tracing::warn!("dns: {name} is a CNAME (a Cloudflare Tunnel?); leaving it alone");
                return Ok(());
            }
            Action::Create => {
                cf.create(&zone.id, name, ip, ttl).await?;
                tracing::info!("dns: created A {name} -> {ip}");
            }
            Action::Update(r) => {
                cf.update(&zone.id, r, ip, ttl).await?;
                let proxied = if r.proxied { ", proxy turned off" } else { "" };
                tracing::info!("dns: updated A {name} -> {ip} (was {}{proxied})", r.content);
            }
        }
        if others > 0 {
            tracing::warn!("dns: {name} has {others} more A record(s) that cubeharbor leaves alone; players may be sent to them");
        }
        Ok(())
    }
}

/// Next wait after `failures` failed runs in a row: 15s, 30s, 60s, ... up to the interval.
fn retry_delay(failures: u32, interval: Duration) -> Duration {
    FIRST_RETRY.saturating_mul(1 << failures.saturating_sub(1).min(16)).min(interval)
}

async fn server_hosts(client: &Client, watch_namespace: &str) -> Result<Vec<String>> {
    let api: Api<MinecraftServer> =
        if watch_namespace.is_empty() { Api::all(client.clone()) } else { Api::namespaced(client.clone(), watch_namespace) };
    let servers = api.list(&Default::default()).await.context("listing MinecraftServers")?;
    Ok(servers.items.into_iter().filter_map(|s| s.status).flat_map(|st| st.hostnames).collect())
}

pub async fn run(client: Client, s: Settings, watch_namespace: String) {
    let http = match reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(h) => h,
        Err(e) => return tracing::error!("dns: building HTTP client: {e}; Cloudflare DNS updates are off"),
    };
    let cf = Cloudflare::new(http.clone(), &s.api_url, s.token.clone());
    tracing::info!(
        "dns: keeping {}{} pointed at the public IPv4 (zone {}, checked every {}s)",
        s.records.join(", "),
        if s.server_hostnames { " and server hostnames" } else { "" },
        s.zone,
        s.interval.as_secs()
    );
    let mut syncer = Syncer::default();
    let mut failures = 0u32;
    loop {
        let result = async {
            let hosts = if s.server_hostnames { server_hosts(&client, &watch_namespace).await? } else { Vec::new() };
            syncer.sync(&http, &cf, &s, &hosts).await
        };
        let wait = match result.await {
            Ok(()) => {
                failures = 0;
                s.interval
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                let wait = retry_delay(failures, s.interval);
                tracing::warn!("dns: {e:#}; retrying in {}s", wait.as_secs());
                wait
            }
        };
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use clap::Parser;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const TOKEN: &str = "test-token-abc123";

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    fn rec(id: &str, content: &str, proxied: bool) -> Record {
        Record { id: id.into(), name: "*.mc.example.com".into(), kind: "A".into(), content: content.into(), proxied, comment: None }
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_lookup_answers() {
        assert_eq!(parse_ip("203.0.113.7\n").unwrap_err(), "203.0.113.7 is not a public address", "documentation range");
        assert_eq!(parse_ip("  81.2.69.160\r\n").unwrap(), ip("81.2.69.160"));
        assert!(parse_ip("192.168.1.10").unwrap_err().contains("not a public address"));
        assert!(parse_ip("10.0.0.1").is_err());
        assert!(parse_ip("127.0.0.1").is_err());
        assert!(parse_ip("2001:db8::1").unwrap_err().contains("IPv6"));
        assert!(parse_ip("<html>rate limited</html>").unwrap_err().contains("not an IP address"));
        assert!(parse_ip("").is_err());
    }

    #[test]
    fn parses_intervals() {
        assert_eq!(parse_interval("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_interval("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_interval("120"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_interval("1h"), Ok(Duration::from_secs(3600)));
        assert!(parse_interval("10s").is_err(), "too short");
        assert!(parse_interval("5d").is_err());
        assert!(parse_interval("m").is_err());
        assert!(parse_interval("99999999999999999999h").is_err());
    }

    #[test]
    fn plans_create_update_keep() {
        let now = ip("81.2.69.160");
        assert_eq!(plan(&[], now), Action::Create);
        assert_eq!(plan(&[rec("a", "81.2.69.160", false)], now), Action::Keep);
        let stale = [rec("a", "81.2.69.1", false)];
        assert_eq!(plan(&stale, now), Action::Update(&stale[0]));
        let proxied = [rec("a", "81.2.69.160", true)];
        assert_eq!(plan(&proxied, now), Action::Update(&proxied[0]), "proxied records are switched to DNS only");
        // Round robin: one right record is enough.
        assert_eq!(plan(&[rec("a", "81.2.69.1", false), rec("b", "81.2.69.160", false)], now), Action::Keep);
        // Otherwise the one we created is preferred.
        let mut ours = rec("b", "81.2.69.2", false);
        ours.comment = Some(COMMENT.into());
        let both = [rec("a", "81.2.69.1", false), ours];
        assert_eq!(plan(&both, now), Action::Update(&both[1]));
        // Other record types don't count.
        let txt = [Record { kind: "TXT".into(), ..rec("t", "hello", false) }];
        assert_eq!(plan(&txt, now), Action::Create);
        // A CNAME (Cloudflare Tunnel) is someone else's.
        let cname = [Record { kind: "CNAME".into(), ..rec("c", "abc.cfargotunnel.com", true) }];
        assert_eq!(plan(&cname, now), Action::Cname);
    }

    #[test]
    fn picks_record_names() {
        let records = strings(&["*.mc.example.com", "mc.example.com", "play.other.org"]);
        let hosts = strings(&["lobby.mc.example.com", "survival.example.com", "a.b.mc.example.com", "x.other.org", "Survival.example.com."]);
        let (names, outside) = desired_names(&records, &hosts, "example.com", &[]);
        assert_eq!(names, strings(&["*.mc.example.com", "mc.example.com", "a.b.mc.example.com", "survival.example.com"]));
        assert_eq!(outside, strings(&["play.other.org"]));
        // The dashboard's host is never managed, even when configured.
        let (names, _) = desired_names(&records, &hosts, "example.com", &strings(&["mc.example.com", "survival.example.com"]));
        assert_eq!(names, strings(&["*.mc.example.com", "a.b.mc.example.com"]));
        assert!(in_zone("example.com", "example.com"));
        assert!(!in_zone("notexample.com", "example.com"));
    }

    #[test]
    fn decodes_cloudflare_envelopes() {
        let ok: Vec<Zone> = decode(StatusCode::OK, r#"{"success":true,"errors":[],"result":[{"id":"z1","name":"example.com"}]}"#).unwrap();
        assert_eq!(ok, vec![Zone { id: "z1".into(), name: "example.com".into() }]);
        let err = decode::<Vec<Zone>>(
            StatusCode::FORBIDDEN,
            r#"{"success":false,"errors":[{"code":10000,"message":"Authentication error"}],"result":null}"#,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "HTTP 403 Forbidden: Authentication error (code 10000)");
        let err = decode::<Vec<Zone>>(StatusCode::BAD_GATEWAY, "<html>bad gateway</html>").unwrap_err();
        assert!(err.to_string().starts_with("HTTP 502 Bad Gateway: unexpected response"), "{err}");
        assert!(decode::<Vec<Zone>>(StatusCode::OK, r#"{"success":false,"errors":[]}"#).is_err());
    }

    #[test]
    fn token_stays_out_of_debug_output() {
        let cfg = Config::parse_from(["op", "--cloudflare-dns", "true", "--cloudflare-api-token", TOKEN, "--cloudflare-zone", "Example.com", "--base-domain", "mc.example.com"]);
        assert!(!format!("{cfg:?}").contains(TOKEN));
        let s = Settings::from_config(&cfg).unwrap().unwrap();
        assert!(!format!("{s:?}").contains(TOKEN));
        assert_eq!(s.records, strings(&["*.mc.example.com"]));
        assert_eq!(s.zone, "example.com");
        assert_eq!(s.interval, Duration::from_secs(300));
        let dup = Config::parse_from(["op", "--cloudflare-dns", "true", "--cloudflare-api-token", "t", "--cloudflare-zone", "example.com", "--dns-records", "a.example.com,b.example.com,A.example.com."]);
        assert_eq!(Settings::from_config(&dup).unwrap().unwrap().records, strings(&["a.example.com", "b.example.com"]));
    }

    #[test]
    fn settings_validation() {
        let off = Config::parse_from(["op"]);
        assert!(Settings::from_config(&off).unwrap().is_none());
        let no_token = Config::parse_from(["op", "--cloudflare-dns", "true", "--cloudflare-zone", "example.com", "--base-domain", "mc.example.com"]);
        assert!(Settings::from_config(&no_token).is_err());
        let no_names = Config::parse_from(["op", "--cloudflare-dns", "true", "--cloudflare-api-token", "t", "--cloudflare-zone", "example.com"]);
        assert!(Settings::from_config(&no_names).unwrap_err().contains("BASE_DOMAIN"));
        let bad_ttl = Config::parse_from(["op", "--cloudflare-dns", "true", "--cloudflare-api-token", "t", "--cloudflare-zone", "z", "--dns-records", "a.z", "--dns-ttl", "5"]);
        assert!(Settings::from_config(&bad_ttl).is_err());
    }

    #[test]
    fn retry_backs_off_up_to_the_interval() {
        let interval = Duration::from_secs(300);
        let waits: Vec<u64> = (1..=7).map(|n| retry_delay(n, interval).as_secs()).collect();
        assert_eq!(waits, vec![15, 30, 60, 120, 240, 300, 300]);
        assert_eq!(retry_delay(u32::MAX, interval), interval);
    }

    // --- Against a local mock of the lookup service and the Cloudflare API ---

    #[derive(Debug, Clone)]
    struct Req {
        method: String,
        target: String,
        auth: Option<String>,
        body: String,
    }

    type Handler = Box<dyn Fn(&Req) -> (u16, String) + Send + Sync>;

    /// A one-request-per-connection HTTP/1.1 server that records what it was asked.
    async fn mock(handler: Handler) -> (String, Arc<Mutex<Vec<Req>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let (handler, seen) = (Arc::new(handler), log.clone());
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let (handler, seen) = (handler.clone(), seen.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        let n = sock.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let header = |name: &str| {
                        head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string()))
                    };
                    let len: usize = header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut chunk).await.unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let mut first = head.lines().next().unwrap_or_default().split(' ');
                    let req = Req {
                        method: first.next().unwrap_or_default().to_string(),
                        target: first.next().unwrap_or_default().to_string(),
                        auth: header("authorization"),
                        body: String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string(),
                    };
                    let (status, body) = handler(&req);
                    seen.lock().unwrap().push(req);
                    let resp = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        (base, log)
    }

    fn ok(result: serde_json::Value) -> (u16, String) {
        (200, json!({ "success": true, "errors": [], "result": result }).to_string())
    }

    fn settings(base: &str, records: &[&str]) -> Settings {
        Settings {
            api_url: base.into(),
            token: ApiToken(TOKEN.into()),
            zone: "example.com".into(),
            records: strings(records),
            server_hostnames: false,
            interval: Duration::from_secs(300),
            ttl: 1,
            ip_urls: vec![format!("{base}/broken-ip"), format!("{base}/ip")],
            reserved: vec![],
        }
    }

    /// A fake Cloudflare holding A records in memory, plus an IP lookup at `/ip`.
    struct Fake {
        public_ip: Mutex<String>,
        records: Mutex<Vec<Record>>,
        fail_writes: Mutex<bool>,
    }

    fn handle(fake: &Fake, req: &Req) -> (u16, String) {
        let t = req.target.as_str();
        if t == "/ip" {
            return (200, format!("{}\n", fake.public_ip.lock().unwrap()));
        }
        if t == "/broken-ip" {
            return (503, "busy".into());
        }
        if req.auth.as_deref() != Some(&format!("Bearer {TOKEN}")) {
            return (403, json!({ "success": false, "errors": [{ "code": 10000, "message": "Authentication error" }] }).to_string());
        }
        if req.method != "GET" && *fake.fail_writes.lock().unwrap() {
            return (500, json!({ "success": false, "errors": [{ "code": 1000, "message": "internal" }] }).to_string());
        }
        let mut records = fake.records.lock().unwrap();
        match (req.method.as_str(), t) {
            ("GET", "/zones?name=example.com") => ok(json!([{ "id": "zone1", "name": "example.com" }])),
            ("GET", t) if t.starts_with("/zones/zone1/dns_records?name=") => {
                let name = t.split("name=").nth(1).unwrap().split('&').next().unwrap().replace("%2A", "*");
                ok(serde_json::to_value(records.iter().filter(|r| r.name == name).collect::<Vec<_>>()).unwrap())
            }
            ("POST", "/zones/zone1/dns_records") => {
                let b: serde_json::Value = serde_json::from_str(&req.body).unwrap();
                let r = Record {
                    id: format!("r{}", records.len() + 1),
                    name: b["name"].as_str().unwrap().into(),
                    kind: b["type"].as_str().unwrap().into(),
                    content: b["content"].as_str().unwrap().into(),
                    proxied: b["proxied"].as_bool().unwrap(),
                    comment: b["comment"].as_str().map(Into::into),
                };
                records.push(r.clone());
                ok(serde_json::to_value(r).unwrap())
            }
            ("PATCH", t) if t.starts_with("/zones/zone1/dns_records/") => {
                let id = t.rsplit('/').next().unwrap();
                let b: serde_json::Value = serde_json::from_str(&req.body).unwrap();
                let r = records.iter_mut().find(|r| r.id == id).unwrap();
                r.content = b["content"].as_str().unwrap().into();
                r.proxied = b["proxied"].as_bool().unwrap();
                ok(serde_json::to_value(r.clone()).unwrap())
            }
            _ => (404, json!({ "success": false, "errors": [{ "code": 7003, "message": "no route" }] }).to_string()),
        }
    }

    async fn fake(public_ip: &str, records: Vec<Record>) -> (Arc<Fake>, String, Arc<Mutex<Vec<Req>>>) {
        let fake = Arc::new(Fake { public_ip: Mutex::new(public_ip.into()), records: Mutex::new(records), fail_writes: Mutex::new(false) });
        let f = fake.clone();
        let (base, log) = mock(Box::new(move |req| handle(&f, req))).await;
        (fake, base, log)
    }

    fn calls(log: &Mutex<Vec<Req>>) -> Vec<String> {
        log.lock().unwrap().drain(..).map(|r| format!("{} {}", r.method, r.target)).collect()
    }

    #[tokio::test]
    async fn creates_then_skips_then_updates() {
        let (fake, base, log) = fake("81.2.69.160", vec![]).await;
        let http = reqwest::Client::new();
        let s = settings(&base, &["*.mc.example.com"]);
        let cf = Cloudflare::new(http.clone(), &base, s.token.clone());
        let mut syncer = Syncer::default();

        syncer.sync(&http, &cf, &s, &[]).await.unwrap();
        assert_eq!(
            calls(&log),
            vec![
                "GET /broken-ip",
                "GET /ip",
                "GET /zones?name=example.com",
                "GET /zones/zone1/dns_records?name=%2A.mc.example.com&per_page=100",
                "POST /zones/zone1/dns_records",
            ]
        );
        let created = fake.records.lock().unwrap()[0].clone();
        assert_eq!((created.name.as_str(), created.content.as_str(), created.proxied), ("*.mc.example.com", "81.2.69.160", false));
        assert_eq!(created.comment.as_deref(), Some(COMMENT));

        // Same IP: only the lookup, no Cloudflare calls.
        syncer.sync(&http, &cf, &s, &[]).await.unwrap();
        assert_eq!(calls(&log), vec!["GET /broken-ip", "GET /ip"]);

        // New IP: the record is patched in place.
        *fake.public_ip.lock().unwrap() = "81.2.69.161".into();
        syncer.sync(&http, &cf, &s, &[]).await.unwrap();
        let c = calls(&log);
        assert_eq!(c.last().unwrap(), "PATCH /zones/zone1/dns_records/r1");
        assert!(!c.iter().any(|c| c.starts_with("GET /zones?")), "the zone is looked up once: {c:?}");
        assert_eq!(fake.records.lock().unwrap()[0].content, "81.2.69.161");
    }

    #[tokio::test]
    async fn repairs_drift_on_the_periodic_recheck() {
        let (fake, base, log) = fake("81.2.69.160", vec![]).await;
        let http = reqwest::Client::new();
        let s = settings(&base, &["*.mc.example.com"]);
        let cf = Cloudflare::new(http.clone(), &base, s.token.clone());
        let mut syncer = Syncer::default();
        syncer.sync(&http, &cf, &s, &[]).await.unwrap();

        // Someone deletes the record in Cloudflare; the IP stays the same.
        fake.records.lock().unwrap().clear();
        log.lock().unwrap().clear();
        syncer.sync(&http, &cf, &s, &[]).await.unwrap();
        assert_eq!(calls(&log), vec!["GET /broken-ip", "GET /ip"], "not yet due");
        assert!(fake.records.lock().unwrap().is_empty());

        // Once the re-check is due (made immediate here), the record comes back.
        syncer.resync = Duration::ZERO;
        syncer.sync(&http, &cf, &s, &[]).await.unwrap();
        assert!(calls(&log).contains(&"POST /zones/zone1/dns_records".to_string()));
        let records = fake.records.lock().unwrap();
        assert_eq!((records.len(), records[0].content.as_str(), records[0].proxied), (1, "81.2.69.160", false));
    }

    #[tokio::test]
    async fn fixes_proxied_records_and_leaves_others_alone() {
        let other = Record { name: "www.example.com".into(), ..rec("w", "1.1.1.1", true) };
        let (fake, base, log) = fake("81.2.69.160", vec![rec("r1", "81.2.69.160", true), other.clone()]).await;
        let http = reqwest::Client::new();
        let s = settings(&base, &["*.mc.example.com"]);
        let cf = Cloudflare::new(http.clone(), &base, s.token.clone());
        Syncer::default().sync(&http, &cf, &s, &[]).await.unwrap();
        let req = log.lock().unwrap().iter().find(|r| r.method == "PATCH").cloned().unwrap();
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body, json!({ "content": "81.2.69.160", "ttl": 1, "proxied": false }));
        assert_eq!(fake.records.lock().unwrap()[1], other, "unmanaged record untouched");
    }

    #[tokio::test]
    async fn publishes_server_hostnames_in_the_zone() {
        let (fake, base, _log) = fake("81.2.69.160", vec![]).await;
        let http = reqwest::Client::new();
        let s = settings(&base, &["*.mc.example.com"]);
        let cf = Cloudflare::new(http.clone(), &base, s.token.clone());
        let hosts = strings(&["lobby.mc.example.com", "play.example.com", "play.elsewhere.net"]);
        Syncer::default().sync(&http, &cf, &s, &hosts).await.unwrap();
        let names: Vec<String> = fake.records.lock().unwrap().iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, strings(&["*.mc.example.com", "play.example.com"]));
    }

    #[tokio::test]
    async fn leaves_tunnel_cnames_alone() {
        let tunnel = Record { name: "panel.example.com".into(), kind: "CNAME".into(), ..rec("t", "abc.cfargotunnel.com", true) };
        let (fake, base, log) = fake("81.2.69.160", vec![tunnel.clone()]).await;
        let http = reqwest::Client::new();
        let s = settings(&base, &["*.mc.example.com", "panel.example.com"]);
        let cf = Cloudflare::new(http.clone(), &base, s.token.clone());
        Syncer::default().sync(&http, &cf, &s, &[]).await.unwrap();
        assert_eq!(calls(&log).iter().filter(|c| c.starts_with("POST") || c.starts_with("PATCH")).count(), 1, "only the wildcard is written");
        assert_eq!(fake.records.lock().unwrap()[0], tunnel);
    }

    #[tokio::test]
    async fn errors_are_retried_and_never_show_the_token() {
        let (fake, base, log) = fake("81.2.69.160", vec![]).await;
        *fake.fail_writes.lock().unwrap() = true;
        let http = reqwest::Client::new();
        let s = settings(&base, &["*.mc.example.com"]);
        let cf = Cloudflare::new(http.clone(), &base, s.token.clone());
        let mut syncer = Syncer::default();
        let err = syncer.sync(&http, &cf, &s, &[]).await.unwrap_err();
        assert!(!format!("{err:#} {err:?}").contains(TOKEN));

        // The failed run isn't remembered, so the next one tries again.
        *fake.fail_writes.lock().unwrap() = false;
        log.lock().unwrap().clear();
        syncer.sync(&http, &cf, &s, &[]).await.unwrap();
        assert!(calls(&log).contains(&"POST /zones/zone1/dns_records".to_string()));
        assert_eq!(fake.records.lock().unwrap().len(), 1);

        // A wrong token is reported without echoing it.
        let bad = Cloudflare::new(http.clone(), &base, ApiToken("wrong-secret".into()));
        let err = Syncer::default().sync(&http, &bad, &s, &[]).await.unwrap_err();
        let shown = format!("{err:#}");
        assert!(shown.contains("Authentication error") && !shown.contains("wrong-secret"), "{shown}");
    }

    #[tokio::test]
    async fn lookup_failures_are_errors() {
        let (_fake, base, _log) = fake("192.168.1.2", vec![]).await;
        let http = reqwest::Client::new();
        let err = public_ip(&http, &[format!("{base}/broken-ip"), format!("{base}/ip")]).await.unwrap_err().to_string();
        assert!(err.contains("503") && err.contains("not a public address"), "{err}");
    }
}
