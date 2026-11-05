//! cubeharbor-dashboard: web UI and API for creating and operating servers.

mod api;
mod handlers;
mod site;
mod views;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::CookieJar;
use clap::Parser;
use cubeharbor_core::sources::Http;
use kube::Client;
use sha2::{Digest, Sha256};

#[derive(Parser, Debug, Clone)]
#[command(version, about = "cubeharbor dashboard")]
pub struct Config {
    #[arg(long, env = "LISTEN_ADDR", default_value = "0.0.0.0:8080")]
    pub listen: String,
    /// Login password (also accepted as a Bearer token on /api).
    #[arg(long, env = "DASHBOARD_PASSWORD", hide_env_values = true)]
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
}

pub struct AppState {
    pub client: Client,
    pub http: Http,
    pub cfg: Config,
    pub remotes: Vec<(String, String)>,
    pub remote_cache: tokio::sync::Mutex<Option<api::RemoteCache>>,
    pub cors_origins: Vec<String>,
    password_hash: [u8; 32],
    sessions: Mutex<HashMap<String, Instant>>,
    failures: Mutex<(Instant, u32)>,
}

/// Sessions live this long; the cookie's Max-Age matches.
pub const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// Failed password checks (login form and Bearer) allowed per minute, across all clients.
const MAX_FAILURES_PER_MINUTE: u32 = 10;

pub type St = State<Arc<AppState>>;

pub const COOKIE: &str = "cubeharbor_session";

impl AppState {
    pub fn public_host(&self) -> Option<&str> {
        [self.cfg.public_host.as_str(), self.cfg.base_domain.as_str()].into_iter().find(|s| !s.is_empty())
    }

    /// Too many recent failed password checks? Brute force gets 429 until the minute is over.
    pub fn throttled(&self) -> bool {
        let f = self.failures.lock().unwrap();
        f.0.elapsed() < Duration::from_secs(60) && f.1 >= MAX_FAILURES_PER_MINUTE
    }

    /// Constant-time password check that counts failures.
    pub fn password_ok(&self, pw: &str) -> bool {
        let ok = constant_eq(&Sha256::digest(pw.as_bytes()), &self.password_hash);
        if !ok {
            let mut f = self.failures.lock().unwrap();
            if f.0.elapsed() >= Duration::from_secs(60) {
                *f = (Instant::now(), 0);
            }
            f.1 += 1;
            tracing::warn!("failed password attempt ({} in the last minute)", f.1);
        }
        ok
    }

    /// Start a session: a random token known only to this process (logout revokes it).
    pub fn new_session(&self) -> anyhow::Result<String> {
        let token = random_hex(32)?;
        let mut s = self.sessions.lock().unwrap();
        s.retain(|_, exp| *exp > Instant::now());
        s.insert(token.clone(), Instant::now() + SESSION_TTL);
        Ok(token)
    }

    pub fn end_session(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }

    fn session_ok(&self, token: &str) -> bool {
        self.sessions.lock().unwrap().get(token).is_some_and(|exp| *exp > Instant::now())
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

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_hex(n: usize) -> anyhow::Result<String> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// The namespace segment of `/servers/{ns}/...` and `/api/v1/servers/{ns}/...`.
fn path_namespace(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/servers/").or_else(|| path.strip_prefix("/api/v1/servers/"))?;
    rest.split('/').next().filter(|s| !s.is_empty())
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
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("frame-ancestors 'none'; base-uri 'self'; form-action 'self'"));
    resp
}

async fn auth(State(st): St, jar: CookieJar, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let public_list = st.cfg.public_server_list && path == "/api/v1/servers" && req.method() == axum::http::Method::GET;
    if path == "/login" || path == "/healthz" || public_list || req.method() == axum::http::Method::OPTIONS {
        return next.run(req).await;
    }
    let cookie_ok = jar.get(COOKIE).is_some_and(|c| st.session_ok(c.value()));
    let bearer = req.headers().get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()).and_then(|h| h.strip_prefix("Bearer "));
    let bearer_ok = !cookie_ok && bearer.is_some() && {
        if st.throttled() {
            return (StatusCode::TOO_MANY_REQUESTS, "too many failed logins, try again in a minute").into_response();
        }
        bearer.is_some_and(|t| st.password_ok(t))
    };
    if cookie_ok || bearer_ok {
        if cookie_ok && !bearer_ok && !matches!(*req.method(), Method::GET | Method::HEAD) && cross_site(&req) {
            return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
        }
        if let Some(ns) = path_namespace(path)
            && !st.ns_allowed(ns)
        {
            return (StatusCode::FORBIDDEN, format!("namespace {ns} is not managed by this dashboard")).into_response();
        }
        return next.run(req).await;
    }
    if path.starts_with("/api/") {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    Redirect::to("/login").into_response()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,kube=warn".into()))
        .init();
    let cfg = Config::parse();
    site::install(site::Site::load(cfg.site_config.as_deref())?);
    let state = Arc::new(AppState {
        client: Client::try_default().await?,
        http: Http::new(cfg.curseforge_api_key.clone())?,
        password_hash: Sha256::digest(cfg.password.as_bytes()).into(),
        sessions: Mutex::new(HashMap::new()),
        failures: Mutex::new((Instant::now(), 0)),
        remotes: api::parse_remotes(&cfg.remotes),
        remote_cache: tokio::sync::Mutex::new(None),
        cors_origins: cfg.cors_origins.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        cfg: cfg.clone(),
    });

    use handlers::*;
    let app = Router::new()
        .route("/", get(index))
        .route("/fragments/servers", get(servers_fragment))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/new", get(new_page))
        .route("/new/quick", post(quick_create))
        .route("/compose/preview", post(compose_preview))
        .route("/compose/apply", post(compose_apply))
        .route("/servers/{ns}/{name}", get(server_page))
        .route("/servers/{ns}/{name}/action/{action}", post(server_action))
        .route("/servers/{ns}/{name}/spec", post(update_spec))
        .route("/servers/{ns}/{name}/console", post(console))
        .route("/servers/{ns}/{name}/logs", get(logs))
        .route("/servers/{ns}/{name}/compose.yaml", get(api::server_compose))
        .route("/compose/export", get(api::compose_export))
        .route("/api/servers", get(api_servers))
        .route("/api/compose", post(api_compose))
        .route("/api/modpacks/search", get(api_search))
        .route("/api/v1/servers", get(api::servers))
        .route("/api/v1/servers/{ns}/{name}/compose", get(api::server_compose))
        .route("/api/v1/compose", get(api::compose_export).post(api::compose_apply))
        .route("/healthz", get(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(middleware::from_fn_with_state(state.clone(), api::cors))
        .layer(middleware::from_fn(security_headers))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    tracing::info!("dashboard listening on {}", cfg.listen);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            if let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                term.recv().await;
            }
        })
        .await?;
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
    }

    #[test]
    fn tokens_are_random() {
        let a = random_hex(32).unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, random_hex(32).unwrap());
    }
}
