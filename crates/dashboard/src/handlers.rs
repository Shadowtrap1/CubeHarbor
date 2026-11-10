use std::collections::BTreeMap;
use std::convert::Infallible;

use axum::Json;
use axum::extract::{Extension, Form, Path, Query};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
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

use crate::users::{Identity, Role};
use crate::error::{AppError, api_failure, public};
use crate::{AppState, St, site, valid_label, valid_name, views};

/// Environment variables can hold credentials (`spec.env`, e.g. an API key for a mod), and
/// viewers and operators get to read a server's config but not its secrets.
pub fn mask_env(me: &Identity, s: &mut MinecraftServer) {
    if !me.can(Role::Admin) {
        s.spec.env.values_mut().for_each(|v| *v = "********".into());
    }
}

type R<T = Response> = Result<T, AppError>;

pub fn html(m: maud::Markup) -> Response {
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
// Pages
// ---------------------------------------------------------------------------

pub async fn index(st: St, Extension(me): Extension<Identity>) -> R {
    let servers = list_servers(&st).await?;
    let (remote, sources) = crate::api::remote_servers(&st).await;
    Ok(html(views::index(&st, &me, &servers, &remote, &sources)))
}

pub async fn servers_fragment(st: St, Extension(me): Extension<Identity>) -> R {
    let servers = list_servers(&st).await?;
    Ok(html(views::server_rows(&st, &me, &servers)))
}

pub async fn new_page(st: St, Extension(me): Extension<Identity>) -> Response {
    html(views::new_server(&st, &me, st.cfg.curseforge_api_key.is_some()))
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
    let out = compose::parse(&serde_norway::to_string(&doc)?).map_err(invalid)?;
    let created = apply(&st, &out).await?;
    let first = created.first().ok_or_else(|| public(StatusCode::BAD_REQUEST, "The form didn't describe a server."))?;
    Ok(Redirect::to(&format!("/servers/{first}")).into_response())
}

#[derive(Deserialize)]
pub struct ComposeForm {
    compose: String,
    /// Checkbox: replace hostnames outside the base domain.
    #[serde(default)]
    rehost: Option<String>,
}

/// A compose file the user got wrong (its message only quotes what they sent).
fn invalid(e: anyhow::Error) -> anyhow::Error {
    public(StatusCode::BAD_REQUEST, format!("{e:#}"))
}

fn parse_form(st: &St, f: &ComposeForm) -> anyhow::Result<ComposeOutput> {
    let mut out = compose::parse(&f.compose).map_err(invalid)?;
    if f.rehost.is_some() {
        compose::rehost(&mut out, &st.cfg.base_domain);
    }
    Ok(out)
}

pub async fn compose_preview(st: St, Extension(me): Extension<Identity>, Form(f): Form<ComposeForm>) -> Response {
    let rehost = f.rehost.is_some();
    match parse_form(&st, &f) {
        Ok(out) => {
            let yaml = out.to_yaml(Some(&st.cfg.default_namespace)).unwrap_or_else(|e| e.to_string());
            html(views::compose_preview(&st, &me, &f.compose, rehost, &out, &yaml, None))
        }
        Err(e) => html(views::compose_preview(&st, &me, &f.compose, rehost, &ComposeOutput::default(), "", Some(&format!("{e:#}")))),
    }
}

pub async fn compose_apply(st: St, Form(f): Form<ComposeForm>) -> R {
    let out = parse_form(&st, &f)?;
    apply(&st, &out).await?;
    Ok(Redirect::to("/").into_response())
}

/// Server-side apply every server in a compose result. Returns `ns/name` of each. Everything is
/// checked before the first write, so a refused server doesn't leave half a file applied.
pub async fn apply(st: &St, out: &ComposeOutput) -> anyhow::Result<Vec<String>> {
    let manifests = out.manifests(Some(&st.cfg.default_namespace));
    for m in &manifests {
        let ns = m.namespace().unwrap_or_else(|| st.cfg.default_namespace.clone());
        st.check_ns(&ns).map_err(|e| public(StatusCode::FORBIDDEN, e.to_string()))?;
        if !valid_label(&ns) || !valid_name(&m.name_any()) {
            return Err(public(StatusCode::BAD_REQUEST, format!("{ns}/{} is not a valid namespace and server name.", m.name_any())));
        }
    }
    let mut done = vec![];
    for m in manifests {
        let ns = m.namespace().unwrap_or_else(|| st.cfg.default_namespace.clone());
        let api: Api<MinecraftServer> = Api::namespaced(st.client.clone(), &ns);
        let mut v = serde_json::to_value(&m)?;
        v.as_object_mut().map(|o| o.remove("status"));
        api.patch(&m.name_any(), &PatchParams::apply("cubeharbor-dashboard").force(), &Patch::Apply(&v)).await?;
        done.push(format!("{ns}/{}", m.name_any()));
    }
    Ok(done)
}

pub async fn server_page(st: St, Extension(me): Extension<Identity>, Path((ns, name)): Path<(String, String)>) -> R {
    let api: Api<MinecraftServer> = Api::namespaced(st.client.clone(), &ns);
    let mut mcs = api.get(&name).await?;
    mask_env(&me, &mut mcs);
    let mut spec = serde_json::to_value(&mcs.spec)?;
    prune_nulls(&mut spec);
    let spec_yaml = serde_norway::to_string(&spec)?;
    Ok(html(views::server_detail(&st, &me, &mcs, &spec_yaml)))
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
                let pvc = format!("data-{name}-0");
                ignore_not_found(pvcs.delete(&pvc, &DeleteParams::default()).await).map_err(|e| {
                    public(StatusCode::BAD_GATEWAY, format!("server {ns}/{name} was deleted, but its world volume {pvc} was not: {e}. Delete it with: kubectl -n {ns} delete pvc {pvc}"))
                })?;
            }
            return Ok(Redirect::to("/").into_response());
        }
        other => return Ok((StatusCode::BAD_REQUEST, format!("unknown action {other}")).into_response()),
    }
    Ok(Redirect::to(&format!("/servers/{ns}/{name}")).into_response())
}

/// A missing object counts as deleted: there is nothing left to remove.
fn ignore_not_found<T>(r: kube::Result<T>) -> kube::Result<()> {
    match r {
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        r => r.map(|_| ()),
    }
}

#[derive(Deserialize)]
pub struct SpecForm {
    spec: String,
}

pub async fn update_spec(st: St, Path((ns, name)): Path<(String, String)>, Form(f): Form<SpecForm>) -> R {
    let spec: MinecraftServerSpec = serde_norway::from_str(&f.spec).map_err(|e| public(StatusCode::BAD_REQUEST, format!("invalid spec: {e}")))?;
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

/// Longest command an RCON packet can carry.
const MAX_CONSOLE_COMMAND: usize = 1446;

pub async fn console(st: St, Path((ns, name)): Path<(String, String)>, Form(f): Form<ConsoleForm>) -> Response {
    if f.command.trim().is_empty() || f.command.len() > MAX_CONSOLE_COMMAND {
        return (StatusCode::BAD_REQUEST, format!("console error: a command is 1 to {MAX_CONSOLE_COMMAND} bytes")).into_response();
    }
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
        Err(e) => {
            tracing::warn!("console of {ns}/{name}: {e:#}");
            (StatusCode::BAD_GATEWAY, "console error: can't reach the server's console (is it running?)").into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct LogQuery {
    #[serde(default)]
    container: Option<String>,
}

pub async fn logs(st: St, Extension(me): Extension<Identity>, Path((ns, name)): Path<(String, String)>, Query(q): Query<LogQuery>) -> Response {
    let Ok(permit) = st.log_streams.clone().try_acquire_owned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many open log streams, try again later").into_response();
    };
    let pods: Api<Pod> = Api::namespaced(st.client.clone(), &ns);
    let pod = format!("{name}-0");
    let container = q.container.filter(|c| c == "install" || c == "server").unwrap_or_else(|| "server".into());
    let lp = LogParams { follow: true, tail_lines: Some(site::get().console.tail_lines), container: Some(container), ..Default::default() };
    let stream = async_stream(pods, pod, lp).take_until(stream_end(st.0.clone(), me)).map(move |event| {
        let _open = &permit;
        event
    });
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

/// How often an open log stream re-checks that its viewer may still see it.
const STREAM_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

/// Is the login behind `me` still good? (An API token's user has to exist; revoking the token
/// itself only takes effect on the next request.)
pub fn still_logged_in(st: &AppState, me: &Identity) -> bool {
    match &me.session {
        Some(token) => st.users.session(token).is_some(),
        None => st.users.role_of(&me.name).is_some(),
    }
}

/// Resolves when a log stream has to end: at shutdown, or once its login is gone.
async fn stream_end(st: std::sync::Arc<AppState>, me: Identity) {
    let mut stop = st.stop.subscribe();
    let logged_out = async {
        while still_logged_in(&st, &me) {
            tokio::time::sleep(STREAM_RECHECK).await;
        }
    };
    tokio::select! {
        _ = stop.wait_for(|s| *s) => {}
        _ = logged_out => {}
    }
}

fn async_stream(pods: Api<Pod>, pod: String, lp: LogParams) -> impl Stream<Item = Result<Event, Infallible>> {
    futures::stream::once(async move { pods.log_stream(&pod, &lp).await })
        .flat_map(|r| match r {
            Ok(reader) => reader
                .lines()
                .map(|l| Ok(Event::default().data(l.unwrap_or_else(|e| format!("[log error: {e}]")))))
                .boxed(),
            Err(e) => {
                tracing::warn!("logs unavailable: {e}");
                futures::stream::once(async move { Ok(Event::default().event("error").data("logs unavailable (is the server running?)")) }).boxed()
            }
        })
}

// ---------------------------------------------------------------------------
// JSON API
// ---------------------------------------------------------------------------

pub async fn api_servers(st: St, Extension(me): Extension<Identity>) -> R {
    let mut servers = list_servers(&st).await?;
    servers.iter_mut().for_each(|s| mask_env(&me, s));
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
        Err(e) => api_failure(e),
    }
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: String,
    #[serde(default)]
    source: Option<String>,
}

/// Longest modpack search sent on to Modrinth or CurseForge.
const MAX_SEARCH_QUERY: usize = 200;

pub async fn api_search(st: St, Query(q): Query<SearchQuery>) -> R {
    if q.q.len() > MAX_SEARCH_QUERY {
        return Err(public(StatusCode::BAD_REQUEST, "That search is too long.").into());
    }
    let upstream = |e: anyhow::Error| {
        tracing::warn!("modpack search: {e:#}");
        public(StatusCode::BAD_GATEWAY, "The modpack search failed; try again later.")
    };
    let hits: Vec<BTreeMap<&str, Value>> = if q.source.as_deref() == Some("curseforge") {
        curseforge::search_modpacks(&st.http, &q.q, site::get().new_server.search_results)
            .await
            .map_err(upstream)?
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
            .await
            .map_err(upstream)?
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

#[cfg(test)]
mod delete_tests {
    use super::*;

    #[test]
    fn pvc_delete_ignores_only_not_found() {
        let api_err = |code: u16| -> kube::Result<()> {
            Err(kube::Error::Api(Box::new(kube::core::Status { code, ..Default::default() })))
        };
        assert!(ignore_not_found(Ok(())).is_ok());
        assert!(ignore_not_found(api_err(404)).is_ok());
        assert!(ignore_not_found(api_err(403)).is_err());
        assert!(ignore_not_found(api_err(500)).is_err());
    }
}
