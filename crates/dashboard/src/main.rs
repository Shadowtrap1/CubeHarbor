//! cubeharbor-dashboard: web UI and API for creating and operating servers.

mod account;
mod api;
#[cfg(test)]
mod e2e_tests;
mod error;
#[cfg(test)]
mod failure_tests;
mod handlers;
mod ops;
mod routes;
mod site;
mod users;
mod views;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use clap::Parser;
use cubeharbor_core::cidr;
use cubeharbor_core::sources::Http;
use kube::Client;
use users::{Backend, Identity, Role, Users};

#[derive(Parser, Debug, Clone)]
#[command(version, about = "cubeharbor dashboard")]
pub struct Config {
    #[arg(long, env = "LISTEN_ADDR", default_value = "0.0.0.0:8080")]
    pub listen: String,
    /// Password of the first admin (`admin`), used when the user store is empty: on first start,
    /// or to recover after removing `users.json` from `USERS_SECRET`.
    #[arg(long, env = "DASHBOARD_PASSWORD", default_value = "", hide_env_values = true)]
    pub password: String,
    /// Namespace for servers created without one.
    #[arg(long, env = "DEFAULT_NAMESPACE", default_value = cubeharbor_core::defaults::NAMESPACE)]
    pub default_namespace: String,
    #[arg(long, env = "BASE_DOMAIN", default_value = "")]
    pub base_domain: String,
    #[arg(long, env = "PUBLIC_HOST", default_value = "")]
    pub public_host: String,
    /// Port players use for Java connections (shown in addresses when not 25565).
    #[arg(long, env = "PUBLIC_MINECRAFT_PORT", default_value_t = 25565)]
    pub public_minecraft_port: u16,
    #[arg(long, env = "CURSEFORGE_API_KEY", hide_env_values = true)]
    pub curseforge_api_key: Option<String>,
    /// Other setups to show next to cubeharbor's servers: `name=url,...`, each URL returning a
    /// server list (games-dashboard `/api/servers` or another cubeharbor's `/api/v1/servers`).
    #[arg(long, env = "REMOTES", default_value = "")]
    pub remotes: String,
    /// Origins allowed to call /api from a browser (comma separated, `*` for any).
    #[arg(long, env = "CORS_ORIGINS", default_value = "")]
    pub cors_origins: String,
    /// Serve `GET /api/v1/servers` without a login (read-only list for public status pages).
    #[arg(long, env = "PUBLIC_SERVER_LIST", default_value_t = false, value_parser = clap::builder::BoolishValueParser::new())]
    pub public_server_list: bool,
    /// Unused since sessions became random server-side tokens; kept so existing deployments start.
    #[arg(long, env = "SESSION_SECRET", default_value = "", hide_env_values = true)]
    pub session_secret: String,
    /// Namespaces the dashboard may read and write (comma separated, `*` for all). The default
    /// namespace is always allowed.
    #[arg(long, env = "ALLOWED_NAMESPACES", default_value = "")]
    pub allowed_namespaces: String,
    /// Mark the session cookie `Secure` (set when the dashboard is served over HTTPS). Also set
    /// automatically for requests arriving with `X-Forwarded-Proto: https`.
    #[arg(long, env = "COOKIE_SECURE", default_value_t = false, action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new())]
    pub cookie_secure: bool,
    /// YAML file with branding, theme and form defaults (see `examples/site.yaml`).
    #[arg(long, env = "SITE_CONFIG")]
    pub site_config: Option<PathBuf>,
    /// Secret the dashboard keeps its users in (key `users.json`; it needs get and patch on it).
    /// Without it there is one user, `admin` with `DASHBOARD_PASSWORD`, and users and passwords
    /// can't be changed from the dashboard.
    #[arg(long, env = "USERS_SECRET", default_value = "")]
    pub users_secret: String,
    /// Namespace of `USERS_SECRET` (the dashboard's own).
    #[arg(long, env = "POD_NAMESPACE", default_value = "")]
    pub pod_namespace: String,
    /// Proxies (comma separated CIDRs, e.g. the ingress controller's pods) whose
    /// `X-Forwarded-For` is believed when rate-limiting failed logins. Without it, every client
    /// behind a proxy shares one limit.
    #[arg(long, env = "TRUSTED_PROXIES", default_value = "")]
    pub trusted_proxies: String,
    /// Failed password checks per minute across all clients before every login gets 429
    /// (0 = no global limit; each client is still limited to 10 a minute). The global limit
    /// slows a distributed guesser but also lets one block new logins for everyone.
    #[arg(long, env = "LOGIN_MAX_FAILURES_TOTAL", default_value_t = 100)]
    pub login_max_failures_total: u32,
    /// Address of `/healthz`, `/readyz` and `/metrics`. Kept off the dashboard's own port, which
    /// may be public, so metrics never need a login exemption there.
    #[arg(long, env = "HEALTH_ADDR", default_value = "0.0.0.0:8081")]
    pub health: String,
    /// On SIGTERM, keep serving for this long while `/readyz` reports not ready, so the pod
    /// leaves the Service endpoints before it stops accepting.
    #[arg(long, env = "SHUTDOWN_DELAY_SECS", default_value_t = 5)]
    pub shutdown_delay_secs: u64,
    /// Then wait up to this long for open requests. Keep SHUTDOWN_DELAY_SECS + DRAIN_TIMEOUT_SECS
    /// below the pod's terminationGracePeriodSeconds (30).
    #[arg(long, env = "DRAIN_TIMEOUT_SECS", default_value_t = 20)]
    pub drain_timeout_secs: u64,
}

/// Shared by every request. All of this is per process, which is why the chart runs one replica:
/// `users` holds the sessions (see `users.rs`); with more replicas `failures` and `log_streams`
/// would become per-replica limits and `stats` per-replica counters, which is harmless.
pub struct AppState {
    pub client: Client,
    pub http: Http,
    pub cfg: Config,
    pub remotes: Vec<(String, String)>,
    pub remote_cache: tokio::sync::Mutex<Option<api::RemoteCache>>,
    pub cors_origins: Vec<String>,
    pub users: Users,
    failures: Mutex<Failures>,
    trusted_proxies: Vec<cidr::Cidr>,
    pub stats: ops::Stats,
    /// Set on SIGTERM: `/readyz` fails.
    pub draining: AtomicBool,
    /// Becomes true when the server stops accepting; ends the log streams, which never finish
    /// on their own and would otherwise hold up the shutdown.
    pub stop: tokio::sync::watch::Sender<bool>,
    /// One permit per open log stream.
    pub log_streams: Arc<tokio::sync::Semaphore>,
}

/// Failed password checks in the current window, per client and in total.
struct Failures {
    window: Instant,
    total: u32,
    per_client: HashMap<IpAddr, u32>,
    /// Limit across all clients (`LOGIN_MAX_FAILURES_TOTAL`), 0 = none.
    max_total: u32,
}

impl Failures {
    fn new(max_total: u32) -> Self {
        Self { window: Instant::now(), total: 0, per_client: HashMap::new(), max_total }
    }

    /// Start a new window once the minute is over.
    fn roll(&mut self) {
        if self.window.elapsed() >= FAILURE_WINDOW {
            *self = Self::new(self.max_total);
        }
    }

    fn limited(&self, client: IpAddr) -> bool {
        let key = bucket(client);
        (self.max_total > 0 && self.total >= self.max_total)
            || self.per_client.get(&key).is_some_and(|n| *n >= MAX_FAILURES_PER_CLIENT)
            // Without a global limit, cap the memory a flood of distinct clients can use.
            || (self.per_client.len() >= MAX_TRACKED_CLIENTS && !self.per_client.contains_key(&key))
    }

    fn record(&mut self, client: IpAddr) -> u32 {
        self.total += 1;
        let n = self.per_client.entry(bucket(client)).or_default();
        *n += 1;
        *n
    }

    /// Take back a `record` whose password check succeeded.
    fn refund(&mut self, client: IpAddr) {
        self.total = self.total.saturating_sub(1);
        let key = bucket(client);
        if let Some(n) = self.per_client.get_mut(&key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.per_client.remove(&key);
            }
        }
    }
}

/// IPv6 clients usually get a whole /64, so they're limited per /64 rather than per address.
fn bucket(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6((u128::from(v6) & !((1u128 << 64) - 1)).into()),
        },
        v4 => v4,
    }
}

/// Sessions live this long; the cookie's Max-Age matches.
pub const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// Failed password checks (login form and Bearer) allowed per minute from one client.
const MAX_FAILURES_PER_CLIENT: u32 = 10;
/// Distinct clients tracked per window; new ones beyond this are throttled.
const MAX_TRACKED_CLIENTS: usize = 100_000;
/// Log streams open at once, across all users. Each holds a request to the Kubernetes API.
pub const MAX_LOG_STREAMS: usize = 64;

pub type St = State<Arc<AppState>>;

pub const COOKIE: &str = "cubeharbor_session";

impl AppState {
    pub fn public_host(&self) -> Option<&str> {
        [self.cfg.public_host.as_str(), self.cfg.base_domain.as_str()].into_iter().find(|s| !s.is_empty())
    }

    /// Too many recent failed password checks from `client` (or overall)? Brute force gets 429
    /// until the minute is over. Counting per client means one attacker can't lock everyone out.
    pub fn throttled(&self, client: IpAddr) -> bool {
        let mut f = self.failures.lock().unwrap();
        f.roll();
        let limited = f.limited(client);
        drop(f);
        if limited {
            ops::bump(&self.stats.throttled);
        }
        limited
    }

    /// Check a user's password (argon2id), counting failures like a wrong login.
    pub async fn check_login(&self, user: &str, pw: &str, client: IpAddr) -> Option<Role> {
        // Counted before the hash and taken back on success: otherwise a burst of concurrent
        // guesses all pass `throttled` while the first ones are still hashing.
        let n = {
            let mut f = self.failures.lock().unwrap();
            f.roll();
            f.record(client)
        };
        let role = self.users.verify(user, pw).await;
        if role.is_some() {
            let mut f = self.failures.lock().unwrap();
            f.roll();
            f.refund(client);
        } else {
            ops::bump(&self.stats.password_failures);
            tracing::warn!("failed password attempt from {client} ({n} in the last minute)");
        }
        role
    }

    fn record_token_failure(&self, client: IpAddr) {
        let mut f = self.failures.lock().unwrap();
        f.roll();
        let n = f.record(client);
        drop(f);
        ops::bump(&self.stats.token_failures);
        tracing::warn!("failed API token attempt from {client} ({n} in the last minute)");
    }

    /// The client's address: the TCP peer, or for a trusted proxy the last `X-Forwarded-For`
    /// hop that isn't one of our proxies.
    pub fn client_ip(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> IpAddr {
        let peer = peer.unwrap_or(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED));
        client_ip(peer, headers, &self.trusted_proxies)
    }

    /// May the dashboard touch servers in `ns`?
    pub fn ns_allowed(&self, ns: &str) -> bool {
        ns == self.cfg.default_namespace
            || self.cfg.allowed_namespaces.split(',').map(str::trim).any(|a| a == "*" || a == ns)
    }

    pub fn check_ns(&self, ns: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.ns_allowed(ns), "namespace {ns} is not managed by this dashboard (ALLOWED_NAMESPACES)");
        Ok(())
    }
}

fn client_ip(peer: IpAddr, headers: &HeaderMap, trusted: &[cidr::Cidr]) -> IpAddr {
    let is_trusted = |ip: IpAddr| trusted.iter().any(|c| c.contains(ip));
    if !is_trusted(peer) {
        return peer;
    }
    // Walk right to left: entries to the left of the first untrusted hop could be forged by the client.
    let hops = headers.get_all("x-forwarded-for").iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).collect::<Vec<_>>();
    let mut client = peer;
    for hop in hops.iter().rev() {
        // Some proxies (e.g. Azure Application Gateway) append the client's port.
        let hop = hop.trim();
        let Some(ip) = hop.parse::<IpAddr>().ok().or_else(|| hop.parse::<SocketAddr>().ok().map(|a| a.ip())) else { break };
        client = ip;
        if !is_trusted(ip) {
            break;
        }
    }
    client
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_bytes(n: usize) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

fn random_hex(n: usize) -> anyhow::Result<String> {
    Ok(random_bytes(n)?.iter().map(|b| format!("{b:02x}")).collect())
}

/// The namespace segment of `/servers/{ns}/...` and `/api/v1/servers/{ns}/...`.
fn path_namespace(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/servers/").or_else(|| path.strip_prefix("/api/v1/servers/"))?;
    rest.split('/').next().filter(|s| !s.is_empty())
}

/// The server name segment of `/servers/{ns}/{name}...` and `/api/v1/servers/{ns}/{name}...`.
fn path_server(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/servers/").or_else(|| path.strip_prefix("/api/v1/servers/"))?;
    rest.split('/').nth(1).filter(|s| !s.is_empty())
}

/// An RFC 1123 label: what a namespace can be called.
pub fn valid_label(s: &str) -> bool {
    let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    (1..=63).contains(&s.len()) && s.starts_with(alnum) && s.ends_with(alnum) && s.chars().all(|c| alnum(c) || c == '-')
}

/// An RFC 1123 subdomain: what a Kubernetes object can be called. Names are pasted into API
/// URLs (kube-rs doesn't escape them) and RCON addresses, so `/`, `?` and `..` must never get
/// through, in any encoding: this checks the raw path, where those would still be `%2F` etc.
pub fn valid_name(s: &str) -> bool {
    s.len() <= 253 && s.split('.').all(valid_label)
}

/// A state-changing browser request from another site? (SameSite=Strict already keeps the
/// cookie off those; this is defence in depth for older browsers.)
fn cross_site(req: &Request) -> bool {
    let h = req.headers();
    if h.get("sec-fetch-site").and_then(|v| v.to_str().ok()).is_some_and(|v| v == "cross-site") {
        return true;
    }
    let Some(origin) = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else { return false };
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
    origin.split_once("://").map(|(_, o)| o) != Some(host)
}

/// Headers that keep the dashboard out of frames and stop content sniffing.
async fn security_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    // Pages can show a new API token or a server's config: keep them out of disk caches.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("frame-ancestors 'none'; base-uri 'self'; form-action 'self'"));
    resp
}

/// The one authorization check: every route's access comes from `routes::table`. Who is asking
/// is a session cookie, or an API token as `Authorization: Bearer chb_...`; the identity is passed
/// on to handlers as a request extension.
async fn auth(State(st): St, jar: CookieJar, mut req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if path == "/login" && *req.method() == Method::POST && cross_site(&req) {
        // Login CSRF: another site could otherwise log the victim in to the attacker's account.
        ops::bump(&st.stats.csrf_refused);
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }
    // No entry means no handler for this method and path: the router answers 404 or 405.
    let access = req
        .extensions()
        .get::<MatchedPath>()
        .zip(req.extensions().get::<routes::SharedPolicy>())
        .and_then(|(pattern, policy)| policy.lookup(req.method(), pattern.as_str()));
    let Some(access) = access else { return next.run(req).await };
    let Some(needed) = access.required_role(path, st.cfg.public_server_list) else { return next.run(req).await };
    let from_cookie = jar.get(COOKIE).and_then(|c| st.users.session(c.value()));
    let bearer = req.headers().get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()).and_then(|h| h.strip_prefix("Bearer "));
    let me = match (from_cookie, bearer) {
        (Some(me), _) => {
            if !matches!(*req.method(), Method::GET | Method::HEAD) && cross_site(&req) {
                ops::bump(&st.stats.csrf_refused);
                return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
            }
            me
        }
        (None, Some(token)) => {
            let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
            let client = st.client_ip(peer, req.headers());
            if st.throttled(client) {
                return (StatusCode::TOO_MANY_REQUESTS, "too many failed logins, try again in a minute").into_response();
            }
            match st.users.check_token(token.trim()) {
                Some((name, role)) => Identity { name, role, session: None },
                None => {
                    st.record_token_failure(client);
                    return (StatusCode::UNAUTHORIZED, bearer_hint(token)).into_response();
                }
            }
        }
        (None, None) if path.starts_with("/api/") => return (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
        (None, None) => return Redirect::to("/login").into_response(),
    };
    if !me.can(needed) {
        ops::bump(&st.stats.forbidden);
        return (StatusCode::FORBIDDEN, format!("{} needs the {} role (you are {})", path, needed.as_str(), me.role.as_str())).into_response();
    }
    if let Some(ns) = path_namespace(path) {
        if !st.ns_allowed(ns) {
            ops::bump(&st.stats.forbidden);
            return (StatusCode::FORBIDDEN, format!("namespace {ns} is not managed by this dashboard")).into_response();
        }
        if !valid_label(ns) || path_server(path).is_some_and(|n| !valid_name(n)) {
            return (StatusCode::BAD_REQUEST, "invalid namespace or server name").into_response();
        }
    }
    req.extensions_mut().insert(me);
    next.run(req).await
}

/// The 401 body for a rejected Bearer token. Anything that isn't shaped like a `chb_` token is
/// most likely a script still sending the old dashboard password.
fn bearer_hint(presented: &str) -> &'static str {
    if presented.trim().starts_with("chb_") {
        "unauthorized: this API token is unknown or was revoked; create a new one on the dashboard's Account page (/account)"
    } else {
        "unauthorized: the dashboard password is no longer accepted as a Bearer token; log in, create an API token \
         on the Account page (/account) and send it as `Authorization: Bearer chb_...`"
    }
}

fn new_state(cfg: &Config, client: Client, users: Users) -> Result<AppState> {
    Ok(AppState {
        stats: ops::Stats::default(),
        draining: AtomicBool::new(false),
        stop: tokio::sync::watch::channel(false).0,
        log_streams: Arc::new(tokio::sync::Semaphore::new(MAX_LOG_STREAMS)),
        client,
        http: Http::new(cfg.curseforge_api_key.clone())?,
        users,
        failures: Mutex::new(Failures::new(cfg.login_max_failures_total)),
        trusted_proxies: cidr::parse_list(&cfg.trusted_proxies)?,
        remotes: api::parse_remotes(&cfg.remotes),
        remote_cache: tokio::sync::Mutex::new(None),
        cors_origins: cfg.cors_origins.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        cfg: cfg.clone(),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    cubeharbor_core::logging::init("info,kube=warn", true);
    let cfg = Config::parse();
    site::install(site::Site::load(cfg.site_config.as_deref())?);
    let client = Client::try_default().await?;
    let backend = (!cfg.users_secret.is_empty()).then(|| {
        let ns = if cfg.pod_namespace.is_empty() { client.default_namespace().to_string() } else { cfg.pod_namespace.clone() };
        Backend { secrets: kube::Api::namespaced(client.clone(), &ns), name: cfg.users_secret.clone() }
    });
    let users = Users::load(backend, Some(&cfg.password)).await?;
    let state = Arc::new(new_state(&cfg, client, users)?);
    let app = routes::router(state.clone());

    // Up only after the users loaded: until then there is nothing that could answer a login.
    let probe = state.clone();
    cubeharbor_core::health::spawn(cfg.health.clone(), move |path| ops::endpoint(&probe, path));

    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    tracing::info!("dashboard listening on {}", cfg.listen);

    // Graceful shutdown: report not ready and keep serving while the pod is removed from the
    // Service endpoints, then stop accepting, end the log streams and let requests finish.
    let delay = Duration::from_secs(cfg.shutdown_delay_secs);
    let drain = Duration::from_secs(cfg.drain_timeout_secs);
    let on_signal = state.clone();
    tokio::spawn(async move {
        cubeharbor_core::health::shutdown_signal().await;
        tracing::info!("shutting down: not ready, still serving for {}s", delay.as_secs());
        on_signal.draining.store(true, Ordering::Relaxed);
        tokio::time::sleep(delay).await;
        let _ = on_signal.stop.send(true);
    });
    let mut stopped = state.stop.subscribe();
    let mut deadline = state.stop.subscribe();
    let server = std::future::IntoFuture::into_future(axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(async move {
        let _ = stopped.wait_for(|s| *s).await;
    }));
    tokio::select! {
        r = server => r?,
        _ = async move {
            let _ = deadline.wait_for(|s| *s).await;
            tokio::time::sleep(drain).await;
        } => tracing::info!("drain timeout reached, closing the remaining requests"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_from_path() {
        assert_eq!(path_namespace("/servers/kube-system/x/console"), Some("kube-system"));
        assert_eq!(path_namespace("/api/v1/servers/ns/x/compose"), Some("ns"));
        assert_eq!(path_namespace("/api/v1/servers"), None);
        assert_eq!(path_namespace("/"), None);
        assert_eq!(path_server("/servers/ns/x/console"), Some("x"));
        assert_eq!(path_server("/api/v1/servers/ns/x/compose"), Some("x"));
        assert_eq!(path_server("/api/v1/servers"), None);
    }

    #[test]
    fn kubernetes_names() {
        for ok in ["a", "survival-2", "my.server", "0a"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "..", "a/b", "a%2Fb", "a..b", ".a", "a.", "-a", "a-", "A", "a?b=c", "a b", &"a".repeat(254)] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(valid_label("kube-system"));
        assert!(!valid_label("a.b"), "namespaces have no dots");
        assert!(!valid_label(&"a".repeat(64)));
    }

    #[test]
    fn refund_undoes_a_record() {
        let ip = "192.0.2.9".parse::<IpAddr>().unwrap();
        let mut f = Failures::new(100);
        f.record(ip);
        f.refund(ip);
        assert_eq!(f.total, 0);
        assert!(f.per_client.is_empty(), "no entry is left behind");
        // A refund landing in a fresh window has nothing to undo.
        f.refund(ip);
        assert_eq!(f.total, 0);
    }

    #[test]
    fn cross_site_detection() {
        let req = |origin: Option<&str>, fetch: Option<&str>| {
            let mut b = axum::http::Request::builder().method("POST").uri("/x").header("host", "dash.example.com");
            if let Some(o) = origin {
                b = b.header("origin", o);
            }
            if let Some(f) = fetch {
                b = b.header("sec-fetch-site", f);
            }
            b.body(axum::body::Body::empty()).unwrap()
        };
        assert!(!cross_site(&req(None, None)));
        assert!(!cross_site(&req(Some("https://dash.example.com"), Some("same-origin"))));
        assert!(cross_site(&req(Some("https://evil.example"), None)));
        assert!(cross_site(&req(None, Some("cross-site"))));
        // Through a Cloudflare Tunnel: cloudflared keeps the public Host, so the browser's Origin
        // matches (also for browsers that don't send Sec-Fetch-Site); a look-alike host doesn't.
        assert!(!cross_site(&req(Some("https://dash.example.com"), None)));
        assert!(cross_site(&req(Some("https://dash.example.com.evil.example"), None)));
    }

    #[test]
    fn client_ip_from_trusted_proxies_only() {
        let trusted = cidr::parse_list("10.0.0.0/8").unwrap();
        let headers = |xff: &str| {
            let mut h = HeaderMap::new();
            h.insert("x-forwarded-for", xff.parse().unwrap());
            h
        };
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        // Direct clients can't pick their own address.
        assert_eq!(client_ip(ip("203.0.113.9"), &headers("1.2.3.4"), &trusted), ip("203.0.113.9"));
        // Behind a trusted proxy, the last untrusted hop is the client; earlier entries may be forged.
        assert_eq!(client_ip(ip("10.1.1.1"), &headers("6.6.6.6, 198.51.100.7"), &trusted), ip("198.51.100.7"));
        assert_eq!(client_ip(ip("10.1.1.1"), &headers("198.51.100.7, 10.2.2.2"), &trusted), ip("198.51.100.7"));
        assert_eq!(client_ip(ip("10.1.1.1"), &headers("198.51.100.7:51234"), &trusted), ip("198.51.100.7"));
        assert_eq!(client_ip(ip("10.1.1.1"), &headers("[2001:db8::7]:443"), &trusted), ip("2001:db8::7"));
        // Garbage stops the walk at the last good address.
        assert_eq!(client_ip(ip("10.1.1.1"), &headers("nonsense"), &trusted), ip("10.1.1.1"));
        assert_eq!(client_ip(ip("10.1.1.1"), &HeaderMap::new(), &trusted), ip("10.1.1.1"));
    }

    #[test]
    fn client_ip_behind_cloudflare_tunnel() {
        // cloudflared runs in a pod (dashboard.trustedProxies = pod CIDR) and passes on the edge's
        // X-Forwarded-For, which ends with the visitor's address.
        let trusted = cidr::parse_list("10.42.0.0/16").unwrap();
        let headers = |xff: &str| {
            let mut h = HeaderMap::new();
            h.insert("x-forwarded-for", xff.parse().unwrap());
            h.insert("cf-connecting-ip", "198.51.100.7".parse().unwrap());
            h
        };
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let cloudflared = ip("10.42.0.17");
        assert_eq!(client_ip(cloudflared, &headers("198.51.100.7"), &trusted), ip("198.51.100.7"));
        assert_eq!(client_ip(cloudflared, &headers("2001:db8::7"), &trusted), ip("2001:db8::7"));
        // A visitor sending their own X-Forwarded-For only adds entries left of the edge's.
        assert_eq!(client_ip(cloudflared, &headers("6.6.6.6, 198.51.100.7"), &trusted), ip("198.51.100.7"));
        // Clients outside the pod CIDR (port-forward, a hostNetwork pod) can't pick an address.
        assert_eq!(client_ip(ip("127.0.0.1"), &headers("6.6.6.6"), &trusted), ip("127.0.0.1"));
        assert_eq!(client_ip(ip("192.168.1.20"), &headers("6.6.6.6"), &trusted), ip("192.168.1.20"));

        // Two visitors arriving through the same cloudflared pod get separate login limits.
        let mut f = Failures::new(100);
        let guesser = client_ip(cloudflared, &headers("198.51.100.7"), &trusted);
        for _ in 0..MAX_FAILURES_PER_CLIENT {
            f.record(guesser);
        }
        assert!(f.limited(guesser));
        assert!(!f.limited(client_ip(cloudflared, &headers("203.0.113.50"), &trusted)));
        // Without trustedProxies they would all be the cloudflared pod and share one limit.
        assert_eq!(client_ip(cloudflared, &headers("203.0.113.50"), &[]), cloudflared);
    }

    #[test]
    fn failures_are_counted_per_client() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let mut f = Failures::new(100);
        for _ in 0..MAX_FAILURES_PER_CLIENT {
            f.record(ip("192.0.2.1"));
        }
        // One noisy client is locked out; everyone else can still log in.
        assert!(f.limited(ip("192.0.2.1")));
        assert!(f.limited(ip("::ffff:192.0.2.1")));
        assert!(!f.limited(ip("192.0.2.2")));
        // Rotating addresses inside one IPv6 /64 doesn't help.
        for i in 0..MAX_FAILURES_PER_CLIENT {
            f.record(ip(&format!("2001:db8::{i:x}")));
        }
        assert!(f.limited(ip("2001:db8::ffff")));
        assert!(!f.limited(ip("2001:db8:0:1::1")));
        // Past the global cap, everyone waits...
        f.total = 100;
        assert!(f.limited(ip("192.0.2.2")));
        // ...unless the global cap is turned off.
        f.max_total = 0;
        assert!(!f.limited(ip("192.0.2.2")));
        assert!(f.limited(ip("192.0.2.1")), "the per-client limit still applies");
    }

    #[test]
    fn bearer_hints() {
        assert!(bearer_hint("my-old-dashboard-password").contains("no longer accepted"));
        assert!(bearer_hint("chb_0123_abcd").contains("unknown or was revoked"));
    }

    #[test]
    fn tokens_are_random() {
        let a = random_hex(32).unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, random_hex(32).unwrap());
    }
}
