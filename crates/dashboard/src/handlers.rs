use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, Form, Path, Query};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use axum::http::HeaderMap;
use axum_extra::extract::cookie::Cookie;
use cubeharbor_core::compose::{self, ComposeOutput};
use cubeharbor_core::crd::{MinecraftServer, MinecraftServerSpec, RCON_PORT};
use cubeharbor_core::labels;
use cubeharbor_core::rcon::{Rcon, strip_formatting};
use cubeharbor_core::sources::{curseforge, modrinth};
use futures::{AsyncBufReadExt, Stream, StreamExt};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret};
use kube::api::{Api, DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams};
use kube::ResourceExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{COOKIE, St, site, views};

pub struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::warn!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, Html(views::error_page(&format!("{:#}", self.0)).into_string())).into_response()
    }
}

type R<T = Response> = Result<T, AppError>;

fn html(m: maud::Markup) -> Response {
    Html(m.into_string()).into_response()
}

async fn list_servers(st: &St) -> anyhow::Result<Vec<MinecraftServer>> {
    let api: Api<MinecraftServer> = Api::all(st.client.clone());
    let mut items = api.list(&ListParams::default()).await?.items;
    items.retain(|s| st.ns_allowed(&s.namespace().unwrap_or_default()));
    items.sort_by_key(|s| (s.namespace(), s.name_any()));
    Ok(items)
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

pub async fn login_page() -> Response {
    html(views::login(None))
}

#[derive(Deserialize)]
pub struct LoginForm {
    password: String,
}

pub async fn login(
    st: St,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    jar: CookieJar,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Response {
    let client = st.client_ip(Some(peer.ip()), &headers);
    if st.throttled(client) {
        return (StatusCode::TOO_MANY_REQUESTS, html(views::login(Some("Too many failed attempts, try again in a minute")))).into_response();
    }
    if !st.password_ok(&f.password, client) {
        return (StatusCode::UNAUTHORIZED, html(views::login(Some("Wrong password")))).into_response();
    }
    let token = match st.new_session() {
        Ok(t) => t,
        Err(e) => return AppError::from(e).into_response(),
    };
    let https = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("https"));
    let secure = if st.cfg.cookie_secure || https { "; Secure" } else { "" };
    let cookie = Cookie::parse(format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}", crate::SESSION_TTL.as_secs()))
        .expect("static cookie format");
    (jar.add(cookie), Redirect::to("/")).into_response()
}

pub async fn logout(st: St, jar: CookieJar) -> Response {
    if let Some(c) = jar.get(COOKIE) {
        st.end_session(c.value());
    }
    (jar.remove(Cookie::build(COOKIE).path("/")), Redirect::to("/login")).into_response()
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

pub async fn index(st: St) -> R {
    let servers = list_servers(&st).await?;
    let (remote, sources) = crate::api::remote_servers(&st).await;
    Ok(html(views::index(&st, &servers, &remote, &sources)))
}

pub async fn servers_fragment(st: St) -> R {
    let servers = list_servers(&st).await?;
    Ok(html(views::server_rows(&st, &servers)))
}

pub async fn new_page(st: St) -> Response {
    html(views::new_server(&st, st.cfg.curseforge_api_key.is_some()))
}

#[derive(Deserialize)]
pub struct QuickForm {
    name: String,
    #[serde(default)]
    namespace: String,
    source: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    modpack: String,
    memory: String,
    #[serde(default)]
    storage: String,
    #[serde(default)]
    hostname: String,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    geyser: Option<String>,
    #[serde(default)]
    auto_sleep: String,
    #[serde(default)]
    mods: String,
    #[serde(default)]
    ops: String,
    #[serde(default)]
    eula: Option<String>,
}

/// The quick form is just a one-server compose file, so it gets the same shorthands.
pub async fn quick_create(st: St, Form(f): Form<QuickForm>) -> R {
    let mut server = serde_json::Map::new();
    match f.source.as_str() {
        "modpack" => {
            server.insert("modpack".into(), json!(f.modpack.trim()));
        }
        loader => {
            server.insert("loader".into(), json!(loader));
        }
    }
    let put = |m: &mut serde_json::Map<String, Value>, k: &str, v: &str| {
        if !v.trim().is_empty() {
            m.insert(k.into(), json!(v.trim()));
        }
    };
    put(&mut server, "version", &f.version);
    put(&mut server, "memory", &f.memory);
    put(&mut server, "storage", &f.storage);
    put(&mut server, "hostname", &f.hostname);
    if f.voice.is_some() {
        server.insert("voice".into(), json!(true));
    }
    if f.geyser.is_some() {
        server.insert("geyser".into(), json!(true));
    }
    if let Ok(m) = f.auto_sleep.trim().parse::<u32>()
        && m > 0
    {
        server.insert("autoSleep".into(), json!(m));
    }
    let list = |s: &str| s.split([',', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(Value::from).collect::<Vec<_>>();
    if !f.mods.trim().is_empty() {
        server.insert("mods".into(), Value::Array(list(&f.mods)));
    }
    if !f.ops.trim().is_empty() {
        server.insert("ops".into(), Value::Array(list(&f.ops)));
    }
    if f.eula.is_some() {
        server.insert("acceptEula".into(), json!(true));
    }
    let mut doc = json!({ "servers": { f.name.trim(): server } });
    if !f.namespace.trim().is_empty() {
        doc["namespace"] = json!(f.namespace.trim());
    }
    let out = compose::parse(&serde_norway::to_string(&doc)?)?;
    let created = apply(&st, &out).await?;
    Ok(Redirect::to(&format!("/servers/{}", created[0])).into_response())
}

#[derive(Deserialize)]
pub struct ComposeForm {
    compose: String,
    /// Checkbox: replace hostnames outside the base domain.
    #[serde(default)]
    rehost: Option<String>,
}

fn parse_form(st: &St, f: &ComposeForm) -> anyhow::Result<ComposeOutput> {
    let mut out = compose::parse(&f.compose)?;
    if f.rehost.is_some() {
        compose::rehost(&mut out, &st.cfg.base_domain);
    }
    Ok(out)
}

pub async fn compose_preview(st: St, Form(f): Form<ComposeForm>) -> Response {
    let rehost = f.rehost.is_some();
    match parse_form(&st, &f) {
        Ok(out) => {
            let yaml = out.to_yaml(Some(&st.cfg.default_namespace)).unwrap_or_else(|e| e.to_string());
            html(views::compose_preview(&st, &f.compose, rehost, &out, &yaml, None))
        }
        Err(e) => html(views::compose_preview(&st, &f.compose, rehost, &ComposeOutput::default(), "", Some(&format!("{e:#}")))),
    }
}

pub async fn compose_apply(st: St, Form(f): Form<ComposeForm>) -> R {
    let out = parse_form(&st, &f)?;
    apply(&st, &out).await?;
    Ok(Redirect::to("/").into_response())
}

/// Server-side apply every server in a compose result. Returns `ns/name` of each.
pub async fn apply(st: &St, out: &ComposeOutput) -> anyhow::Result<Vec<String>> {
    let mut done = vec![];
    for m in out.manifests(Some(&st.cfg.default_namespace)) {
        let ns = m.namespace().unwrap_or_else(|| st.cfg.default_namespace.clone());
        st.check_ns(&ns)?;
        let api: Api<MinecraftServer> = Api::namespaced(st.client.clone(), &ns);
        let mut v = serde_json::to_value(&m)?;
        v.as_object_mut().map(|o| o.remove("status"));
        api.patch(&m.name_any(), &PatchParams::apply("cubeharbor-dashboard").force(), &Patch::Apply(&v)).await?;
        done.push(format!("{ns}/{}", m.name_any()));
    }
    Ok(done)
}

pub async fn server_page(st: St, Path((ns, name)): Path<(String, String)>) -> R {
    let api: Api<MinecraftServer> = Api::namespaced(st.client.clone(), &ns);
    let mcs = api.get(&name).await?;
    let mut spec = serde_json::to_value(&mcs.spec)?;
    prune_nulls(&mut spec);
    let spec_yaml = serde_norway::to_string(&spec)?;
    Ok(html(views::server_detail(&st, &mcs, &spec_yaml)))
}

fn prune_nulls(v: &mut Value) {
    if let Value::Object(o) = v {
        o.retain(|_, v| !v.is_null() && v != &json!({}) && v != &json!([]) && v != &json!(false));
        o.values_mut().for_each(prune_nulls);
    }
}

pub async fn server_action(st: St, Path((ns, name, action)): Path<(String, String, String)>) -> R {
    let api: Api<MinecraftServer> = Api::namespaced(st.client.clone(), &ns);
    let merge = |v: Value| Patch::Merge(v);
    match action.as_str() {
        "start" => {
            api.patch(&name, &PatchParams::default(), &merge(json!({ "spec": { "paused": false } }))).await?;
            // Also clear auto-sleep.
            let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs().to_string();
            api.patch(&name, &PatchParams::default(), &merge(json!({ "metadata": { "annotations": { labels::WAKE_ANNOTATION: ts } } }))).await?;
        }
        "stop" => {
            api.patch(&name, &PatchParams::default(), &merge(json!({ "spec": { "paused": true } }))).await?;
        }
        "restart" => {
            let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs().to_string();
            api.patch(&name, &PatchParams::default(), &merge(json!({ "metadata": { "annotations": { labels::RESTART_ANNOTATION: ts } } }))).await?;
        }
        "delete" | "delete-data" => {
            api.delete(&name, &DeleteParams::default()).await?;
            if action == "delete-data" {
                let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(st.client.clone(), &ns);
                let _ = pvcs.delete(&format!("data-{name}-0"), &DeleteParams::default()).await;
            }
            return Ok(Redirect::to("/").into_response());
        }
        other => return Ok((StatusCode::BAD_REQUEST, format!("unknown action {other}")).into_response()),
    }
    Ok(Redirect::to(&format!("/servers/{ns}/{name}")).into_response())
}

#[derive(Deserialize)]
pub struct SpecForm {
    spec: String,
}

pub async fn update_spec(st: St, Path((ns, name)): Path<(String, String)>, Form(f): Form<SpecForm>) -> R {
    let spec: MinecraftServerSpec = serde_norway::from_str(&f.spec).map_err(|e| anyhow::anyhow!("invalid spec: {e}"))?;
    let api: Api<MinecraftServer> = Api::namespaced(st.client.clone(), &ns);
    let mut current = api.get(&name).await?;
    current.spec = spec;
    current.status = None;
    api.replace(&name, &PostParams::default(), &current).await?;
    Ok(Redirect::to(&format!("/servers/{ns}/{name}")).into_response())
}

#[derive(Deserialize)]
pub struct ConsoleForm {
    command: String,
}

pub async fn console(st: St, Path((ns, name)): Path<(String, String)>, Form(f): Form<ConsoleForm>) -> Response {
    let run = async {
        let secrets: Api<Secret> = Api::namespaced(st.client.clone(), &ns);
        let s = secrets.get(&format!("{name}-cubeharbor")).await?;
        let pw = s
            .data
            .and_then(|d| d.get("RCON_PASSWORD").map(|b| String::from_utf8_lossy(&b.0).to_string()))
            .ok_or_else(|| anyhow::anyhow!("no RCON password"))?;
        let mut rcon = Rcon::connect(&format!("{name}.{ns}.svc:{RCON_PORT}"), &pw).await?;
        let out = rcon.command(f.command.trim_start_matches('/')).await?;
        anyhow::Ok(strip_formatting(&out))
    };
    match run.await {
        Ok(out) if out.is_empty() => "(no output)".into_response(),
        Ok(out) => out.into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("console error: {e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
pub struct LogQuery {
    #[serde(default)]
    container: Option<String>,
}

pub async fn logs(
    st: St,
    Path((ns, name)): Path<(String, String)>,
    Query(q): Query<LogQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let pods: Api<Pod> = Api::namespaced(st.client.clone(), &ns);
    let pod = format!("{name}-0");
    let container = q.container.filter(|c| c == "install" || c == "server").unwrap_or_else(|| "server".into());
    let lp = LogParams { follow: true, tail_lines: Some(site::get().console.tail_lines), container: Some(container), ..Default::default() };
    let stream = async_stream(pods, pod, lp);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn async_stream(pods: Api<Pod>, pod: String, lp: LogParams) -> impl Stream<Item = Result<Event, Infallible>> {
    futures::stream::once(async move { pods.log_stream(&pod, &lp).await })
        .flat_map(|r| match r {
            Ok(reader) => reader
                .lines()
                .map(|l| Ok(Event::default().data(l.unwrap_or_else(|e| format!("[log error: {e}]")))))
                .boxed(),
            Err(e) => futures::stream::once(async move { Ok(Event::default().event("error").data(format!("logs unavailable: {e}"))) }).boxed(),
        })
}

// ---------------------------------------------------------------------------
// JSON API
// ---------------------------------------------------------------------------

pub async fn api_servers(st: St) -> R {
    let servers = list_servers(&st).await?;
    Ok(Json(servers).into_response())
}

#[derive(Deserialize)]
pub struct ComposeQuery {
    #[serde(default)]
    dry_run: bool,
}

/// `POST /api/compose` with a YAML body. `?dry_run=true` only renders.
pub async fn api_compose(st: St, Query(q): Query<ComposeQuery>, body: String) -> Response {
    let out = match compose::parse(&body) {
        Ok(o) => o,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("{e:#}") }))).into_response(),
    };
    if q.dry_run {
        return Json(json!({ "warnings": out.warnings, "manifests": out.manifests(Some(&st.cfg.default_namespace)) })).into_response();
    }
    match apply(&st, &out).await {
        Ok(applied) => Json(json!({ "applied": applied, "warnings": out.warnings })).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: String,
    #[serde(default)]
    source: Option<String>,
}

pub async fn api_search(st: St, Query(q): Query<SearchQuery>) -> R {
    let hits: Vec<BTreeMap<&str, Value>> = if q.source.as_deref() == Some("curseforge") {
        curseforge::search_modpacks(&st.http, &q.q, site::get().new_server.search_results)
            .await?
            .into_iter()
            .map(|m| {
                BTreeMap::from([
                    ("id", json!(format!("curseforge:{}", m.slug))),
                    ("title", json!(m.name)),
                    ("description", json!(m.summary)),
                    ("icon", json!(m.logo.map(|l| l.url))),
                    ("downloads", json!(m.download_count as u64)),
                ])
            })
            .collect()
    } else {
        modrinth::search(&st.http, &q.q, "modpack", site::get().new_server.search_results)
            .await?
            .into_iter()
            .map(|h| {
                BTreeMap::from([
                    ("id", json!(format!("modrinth:{}", h.slug))),
                    ("title", json!(h.title)),
                    ("description", json!(h.description)),
                    ("icon", json!(h.icon_url)),
                    ("downloads", json!(h.downloads)),
                ])
            })
            .collect()
    };
    Ok(Json(hits).into_response())
}

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

/// Shortest password the dashboard accepts when it's changed from the UI.
pub const MIN_PASSWORD_LEN: usize = 12;

pub async fn account_page(st: St) -> Response {
    html(views::account(st.password_changeable(), None))
}

#[derive(Deserialize)]
pub struct PasswordForm {
    current: String,
    new: String,
    confirm: String,
}

/// Is `new` acceptable as the next password?
fn check_new_password(current: &str, new: &str, confirm: &str) -> Result<(), String> {
    if new.chars().count() < MIN_PASSWORD_LEN || new.trim().is_empty() {
        return Err(format!("The new password must be at least {MIN_PASSWORD_LEN} characters."));
    }
    if new != confirm {
        return Err("The new password and its confirmation don't match.".into());
    }
    if new == current {
        return Err("The new password is the same as the current one.".into());
    }
    Ok(())
}

pub async fn change_password(
    st: St,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    jar: CookieJar,
    headers: HeaderMap,
    Form(f): Form<PasswordForm>,
) -> Response {
    let page = |status: StatusCode, msg: Result<&str, &str>| (status, html(views::account(st.password_changeable(), Some(msg)))).into_response();
    if !st.password_changeable() {
        return page(StatusCode::BAD_REQUEST, Err("Changing the password here is turned off; set it in the Helm values instead."));
    }
    // The current password is checked like a login, including the failed-attempt limits, so a
    // stolen session can't be turned into the password.
    let client = st.client_ip(Some(peer.ip()), &headers);
    if st.throttled(client) {
        return page(StatusCode::TOO_MANY_REQUESTS, Err("Too many failed attempts, try again in a minute."));
    }
    if !st.password_ok(&f.current, client) {
        return page(StatusCode::UNAUTHORIZED, Err("The current password is wrong."));
    }
    if let Err(e) = check_new_password(&f.current, &f.new, &f.confirm) {
        return page(StatusCode::BAD_REQUEST, Err(&e));
    }
    // Write it to the Secret first, so a restart can't bring back the old password.
    let secrets: Api<Secret> = Api::namespaced(st.client.clone(), &st.cfg.pod_namespace);
    let patch = json!({ "data": { "password": k8s_openapi::ByteString(f.new.clone().into_bytes()) } });
    if let Err(e) = secrets.patch(&st.cfg.password_secret, &PatchParams::default(), &Patch::Merge(&patch)).await {
        tracing::warn!("could not store the new dashboard password: {e}");
        return page(StatusCode::BAD_GATEWAY, Err("Could not save the new password (see the dashboard logs). Nothing was changed."));
    }
    // Keeps only the caller's session. With Bearer auth there is no cookie, so every session ends.
    st.set_password(&f.new, jar.get(COOKIE).map(|c| c.value()));
    tracing::info!("dashboard password changed from {client}; other sessions logged out");
    page(StatusCode::OK, Ok("Password changed. Other sessions have been logged out."))
}

#[cfg(test)]
mod account_tests {
    use super::*;

    #[test]
    fn new_password_rules() {
        assert!(check_new_password("old-password-1", "correct horse battery", "correct horse battery").is_ok());
        assert!(check_new_password("old", "short", "short").is_err());
        assert!(check_new_password("old", "            ", "            ").is_err(), "whitespace only");
        assert!(check_new_password("old", "correct horse battery", "correct horse batterY").is_err());
        assert!(check_new_password("same-password-1", "same-password-1", "same-password-1").is_err());
        // Length counts characters, not bytes.
        assert!(check_new_password("old", "ééééééééééé", "ééééééééééé").is_err());
    }
}
