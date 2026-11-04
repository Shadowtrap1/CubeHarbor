//! Versioned JSON API (`/api/v1`) for central dashboards and scripts.
//!
//! * `GET  /api/v1/servers` lists cubeharbor servers plus servers from other setups (`REMOTES`),
//!   in one shape: `{ updated, servers: [...], totals, sources }`. The top level matches the
//!   games-dashboard `/api/servers` format, so a central page can read either.
//! * `POST /api/v1/compose` applies a compose file (`?dry_run=true`, `?rehost=true`).
//! * `GET  /api/v1/compose` exports every server (or `?project=`) as one compose file.
//! * `GET  /api/v1/servers/{ns}/{name}/compose` exports one server.

use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use cubeharbor_core::compose::{self, PROJECT_LABEL};
use cubeharbor_core::crd::{MinecraftServer, Phase};
use kube::ResourceExt;
use kube::api::{Api, ListParams};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{AppState, St};

/// One server, wherever it runs.
#[derive(Serialize, Clone, Debug, Default)]
pub struct Summary {
    /// `cubeharbor` or the name of the remote setup.
    pub source: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// cubeharbor phase (`Running`, `Sleeping`, ...); remotes report `Online`/`Offline`.
    pub phase: String,
    pub online: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hostnames: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modpack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub players: Option<PlayerCount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Dashboard page and compose export (cubeharbor servers only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Value>,
}

#[derive(Serialize, Clone, Debug)]
pub struct PlayerCount {
    pub online: u32,
    pub max: u32,
}

#[derive(Serialize, Clone, Debug)]
pub struct PortSummary {
    pub name: String,
    pub protocol: String,
    pub address: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct SourceStatus {
    pub name: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn summarize(st: &AppState, s: &MinecraftServer) -> Summary {
    let ns = s.namespace().unwrap_or_default();
    let name = s.name_any();
    let status = s.status.clone().unwrap_or_default();
    let host = st.public_host().unwrap_or_default().to_string();
    Summary {
        source: "cubeharbor".into(),
        name: name.clone(),
        namespace: Some(ns.clone()),
        phase: status.phase.as_str().into(),
        online: status.phase == Phase::Running,
        address: crate::views::address(st, s),
        hostnames: status.hostnames.clone(),
        version: status.resolved.as_ref().map(|r| r.minecraft_version.clone()).or(s.spec.version.clone()),
        loader: status.resolved.as_ref().map(|r| r.loader.as_str().into()).or(s.spec.loader.map(|l| l.as_str().into())),
        modpack: status.resolved.as_ref().and_then(|r| r.modpack.as_ref()).map(|m| format!("{} {}", m.name, m.version)),
        players: status.players.as_ref().map(|p| PlayerCount { online: p.online, max: p.max }),
        memory: Some(s.spec.resources.memory.clone()),
        project: s.labels().get(PROJECT_LABEL).cloned(),
        ports: status
            .allocations
            .iter()
            .map(|a| PortSummary { name: a.name.clone(), protocol: a.protocol.as_str().into(), address: format!("{host}:{}", a.public_port) })
            .collect(),
        message: status.message.clone(),
        links: Some(json!({
            "dashboard": format!("/servers/{ns}/{name}"),
            "compose": format!("/api/v1/servers/{ns}/{name}/compose"),
        })),
    }
}

// ---------------------------------------------------------------------------
// Other setups
// ---------------------------------------------------------------------------

/// `name=url` pairs from `REMOTES` (comma separated).
pub fn parse_remotes(s: &str) -> Vec<(String, String)> {
    s.split(',')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .collect()
}

pub struct RemoteCache {
    pub at: Instant,
    pub servers: Vec<Summary>,
    pub sources: Vec<SourceStatus>,
}

const REMOTE_TTL: Duration = Duration::from_secs(15);
const REMOTE_TIMEOUT: Duration = Duration::from_secs(4);

/// Servers of every configured remote, cached for 15 s so page loads don't hammer them.
pub async fn remote_servers(st: &AppState) -> (Vec<Summary>, Vec<SourceStatus>) {
    if st.remotes.is_empty() {
        return (vec![], vec![]);
    }
    let mut cache = st.remote_cache.lock().await;
    if let Some(c) = cache.as_ref().filter(|c| c.at.elapsed() < REMOTE_TTL) {
        return (c.servers.clone(), c.sources.clone());
    }
    let fetches = st.remotes.iter().map(|(name, url)| async move {
        let r = tokio::time::timeout(REMOTE_TIMEOUT, st.http.get_json::<Value>(url)).await;
        let r = match r {
            Ok(Ok(v)) => Ok(parse_remote(name, &v)),
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(_) => Err(format!("{url}: timed out")),
        };
        (name.clone(), r)
    });
    let mut servers = vec![];
    let mut sources = vec![];
    for (name, r) in futures::future::join_all(fetches).await {
        match r {
            Ok(mut s) => {
                servers.append(&mut s);
                sources.push(SourceStatus { name, ok: true, error: None });
            }
            Err(e) => {
                tracing::warn!("remote {name}: {e}");
                sources.push(SourceStatus { name, ok: false, error: Some(e) });
            }
        }
    }
    *cache = Some(RemoteCache { at: Instant::now(), servers: servers.clone(), sources: sources.clone() });
    (servers, sources)
}

/// Accepts the games-dashboard format (`{servers: [{name, address, online, players, version}]}`),
/// another cubeharbor's `/api/v1/servers`, or a bare array.
pub fn parse_remote(source: &str, v: &Value) -> Vec<Summary> {
    let list = v.get("servers").unwrap_or(v).as_array().cloned().unwrap_or_default();
    list.iter()
        .filter_map(|s| {
            let name = s.get("name")?.as_str()?.to_string();
            let str_of = |k: &str| s.get(k).and_then(Value::as_str).map(str::to_string);
            let online = s.get("online").and_then(Value::as_bool).unwrap_or_else(|| str_of("phase").as_deref() == Some("Running"));
            let players = match s.get("players") {
                Some(Value::Number(n)) => Some(PlayerCount { online: n.as_u64().unwrap_or(0) as u32, max: 0 }),
                Some(Value::Object(p)) => Some(PlayerCount {
                    online: p.get("online").and_then(Value::as_u64).unwrap_or(0) as u32,
                    max: p.get("max").and_then(Value::as_u64).unwrap_or(0) as u32,
                }),
                _ => None,
            };
            let version = match s.get("version") {
                Some(Value::String(v)) => Some(v.clone()),
                Some(Value::Object(o)) => o.get("name").and_then(Value::as_str).map(str::to_string),
                _ => None,
            };
            let kind = str_of("type").filter(|t| t != "minecraft");
            Some(Summary {
                source: source.to_string(),
                namespace: str_of("namespace"),
                phase: str_of("phase").unwrap_or_else(|| if online { "Online".into() } else { "Offline".into() }),
                online,
                address: str_of("address"),
                version,
                loader: str_of("loader").or(kind),
                modpack: str_of("modpack"),
                players,
                memory: str_of("memory"),
                project: str_of("project"),
                message: str_of("message").or(str_of("motd")),
                name,
                ..Default::default()
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn api_error(code: StatusCode, e: impl std::fmt::Display) -> Response {
    (code, Json(json!({ "error": e.to_string() }))).into_response()
}

#[derive(Deserialize)]
pub struct ServersQuery {
    /// Include other setups (default true).
    #[serde(default = "yes")]
    remotes: bool,
}

fn yes() -> bool {
    true
}

pub async fn servers(State(st): St, Query(q): Query<ServersQuery>) -> Response {
    let api: Api<MinecraftServer> = Api::all(st.client.clone());
    let mut list = match api.list(&ListParams::default()).await {
        Ok(l) => l.items,
        Err(e) => return api_error(StatusCode::BAD_GATEWAY, format!("listing servers: {e}")),
    };
    list.retain(|s| st.ns_allowed(&s.namespace().unwrap_or_default()));
    list.sort_by_key(|s| (s.namespace(), s.name_any()));
    let mut servers: Vec<Summary> = list.iter().map(|s| summarize(&st, s)).collect();
    let mut sources = vec![SourceStatus { name: "cubeharbor".into(), ok: true, error: None }];
    if q.remotes {
        let (mut r, mut s) = remote_servers(&st).await;
        servers.append(&mut r);
        sources.append(&mut s);
    }
    let online = servers.iter().filter(|s| s.online).count();
    let players: u32 = servers.iter().filter_map(|s| s.players.as_ref()).map(|p| p.online).sum();
    Json(json!({
        "updated": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        "servers": servers,
        "totals": { "servers": servers.len(), "online": online, "players": players },
        "sources": sources,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct ComposeQuery {
    #[serde(default)]
    dry_run: bool,
    /// Replace hostnames outside the base domain (e.g. from an imported setup).
    #[serde(default)]
    rehost: bool,
}

/// `POST /api/v1/compose` with a YAML body (one file, or several joined by `---`).
pub async fn compose_apply(State(st): St, Query(q): Query<ComposeQuery>, body: String) -> Response {
    let mut out = match compose::parse(&body) {
        Ok(o) => o,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    if q.rehost {
        compose::rehost(&mut out, &st.cfg.base_domain);
    }
    let manifests = out.manifests(Some(&st.cfg.default_namespace));
    if q.dry_run {
        return Json(json!({ "dryRun": true, "warnings": out.warnings, "manifests": manifests })).into_response();
    }
    match crate::handlers::apply(&State(st.clone()), &out).await {
        Ok(applied) => Json(json!({ "applied": applied, "warnings": out.warnings })).into_response(),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

fn yaml(body: String, filename: &str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/yaml; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, format!("inline; filename=\"{filename}\"")),
        ],
        body,
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct ExportQuery {
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
}

/// `GET /api/v1/compose` (and `/compose/export`): every server as one compose file.
pub async fn compose_export(State(st): St, Query(q): Query<ExportQuery>) -> Response {
    if let Some(ns) = &q.namespace
        && let Err(e) = st.check_ns(ns)
    {
        return api_error(StatusCode::FORBIDDEN, e);
    }
    let api: Api<MinecraftServer> = match &q.namespace {
        Some(ns) => Api::namespaced(st.client.clone(), ns),
        None => Api::all(st.client.clone()),
    };
    let mut lp = ListParams::default();
    if let Some(p) = &q.project {
        lp = lp.labels(&format!("{PROJECT_LABEL}={p}"));
    }
    let mut list = match api.list(&lp).await {
        Ok(l) => l.items,
        Err(e) => return api_error(StatusCode::BAD_GATEWAY, format!("listing servers: {e}")),
    };
    list.retain(|s| st.ns_allowed(&s.namespace().unwrap_or_default()));
    list.sort_by_key(|s| (s.namespace(), s.name_any()));
    let file: String = q.project.as_deref().unwrap_or("cubeharbor").chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).collect();
    match compose::export(&list, q.project.as_deref()) {
        Ok(y) => yaml(y, &format!("{file}.yaml")),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// `GET /api/v1/servers/{ns}/{name}/compose`: one server as a compose file.
pub async fn server_compose(State(st): St, Path((ns, name)): Path<(String, String)>) -> Response {
    let api: Api<MinecraftServer> = Api::namespaced(st.client.clone(), &ns);
    match api.get_opt(&name).await {
        Ok(Some(s)) => {
            let project = s.labels().get(PROJECT_LABEL).cloned();
            match compose::export(std::slice::from_ref(&s), project.as_deref()) {
                Ok(y) => yaml(y, &format!("{name}.yaml")),
                Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
            }
        }
        Ok(None) => api_error(StatusCode::NOT_FOUND, format!("server {ns}/{name} not found")),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, e),
    }
}

// ---------------------------------------------------------------------------
// CORS for central dashboards on other origins
// ---------------------------------------------------------------------------

pub async fn cors(State(st): St, req: Request, next: Next) -> Response {
    let origin = req.headers().get(header::ORIGIN).and_then(|o| o.to_str().ok()).map(str::to_string);
    let allowed = origin.filter(|o| {
        req.uri().path().starts_with("/api/") && st.cors_origins.iter().any(|a| a == "*" || a.eq_ignore_ascii_case(o))
    });
    let Some(origin) = allowed else { return next.run(req).await };
    let preflight = req.method() == Method::OPTIONS;
    let mut resp = if preflight { StatusCode::NO_CONTENT.into_response() } else { next.run(req).await };
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&origin) {
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
    }
    h.insert(header::VARY, HeaderValue::from_static("Origin"));
    if preflight {
        h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, OPTIONS"));
        h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("authorization, content-type"));
        h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn games_dashboard_format() {
        let v: Value = serde_json::from_str(
            r#"{"updated":1,"servers":[{"name":"compv1","type":"minecraft","address":"compv1.mc.example.com","online":false},
               {"name":"voxy","type":"minecraft","address":"voxy.mc.example.com","online":true,"players":3,"version":"1.21.1","motd":"hi"}],
               "totals":{"servers":2,"online":1,"players":3}}"#,
        )
        .unwrap();
        let s = parse_remote("games", &v);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].phase, "Offline");
        assert!(s[1].online);
        assert_eq!(s[1].players.as_ref().unwrap().online, 3);
        assert_eq!(s[1].version.as_deref(), Some("1.21.1"));
        assert_eq!(s[1].source, "games");
        assert_eq!(s[0].loader, None);
    }

    #[test]
    fn remotes_env() {
        assert_eq!(
            parse_remotes("games=https://games.example.com/api/servers, x=, =y"),
            vec![("games".to_string(), "https://games.example.com/api/servers".to_string())]
        );
    }
}
