//! "Compose charts": declare a whole fleet of servers in one YAML file.
//!
//! Two input formats are accepted:
//!
//! * **Cubeharbor compose** — a `servers:` map where each entry is a `MinecraftServerSpec`
//!   plus shorthands (`modpack: modrinth:slug@1.2`, `voice: true`, `memory: 8Gi`, ...).
//! * **docker-compose** — a `services:` map using the popular `itzg/minecraft-server`
//!   environment conventions, so existing setups can be imported as-is.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::crd::{GEYSER_PORT, MinecraftServer, MinecraftServerSpec, PLASMO_VOICE_PORT, SIMPLE_VOICE_CHAT_PORT, normalize_host};

#[derive(Debug, Clone)]
pub struct ComposeServer {
    pub name: String,
    pub namespace: Option<String>,
    pub spec: MinecraftServerSpec,
}

#[derive(Debug, Clone, Default)]
pub struct ComposeOutput {
    pub project: Option<String>,
    pub servers: Vec<ComposeServer>,
    pub warnings: Vec<String>,
}

pub const PROJECT_LABEL: &str = "cubeharbor.dev/compose-project";

impl ComposeOutput {
    pub fn manifests(&self, default_namespace: Option<&str>) -> Vec<MinecraftServer> {
        self.servers
            .iter()
            .map(|s| {
                let mut m = MinecraftServer::new(&s.name, s.spec.clone());
                m.metadata.namespace = s.namespace.clone().or(default_namespace.map(str::to_string));
                if let Some(p) = &self.project {
                    m.metadata.labels = Some([(PROJECT_LABEL.to_string(), p.clone())].into());
                }
                m
            })
            .collect()
    }

    /// Multi-document YAML of the generated resources.
    pub fn to_yaml(&self, default_namespace: Option<&str>) -> Result<String> {
        let mut out = String::new();
        for m in self.manifests(default_namespace) {
            let mut v = serde_json::to_value(&m)?;
            if let Some(o) = v.as_object_mut() {
                o.remove("status");
            }
            out.push_str("---\n");
            out.push_str(&serde_norway::to_string(&v)?);
        }
        Ok(out)
    }
}

/// Parse one compose file, or several joined as a multi-document YAML stream (`---`).
///
/// With several documents (e.g. every `servers/*/compose.yaml` of a docker-compose setup), a
/// document that fails becomes a warning instead of failing the whole import.
pub fn parse(input: &str) -> Result<ComposeOutput> {
    use serde::Deserialize;
    let docs: Vec<Value> = serde_norway::Deserializer::from_str(input)
        .map(Value::deserialize)
        .collect::<Result<Vec<_>, _>>()
        .context("invalid YAML")?
        .into_iter()
        .filter(|d| !d.is_null())
        .collect();
    match docs.len() {
        0 => bail!("the compose file is empty"),
        1 => parse_doc(&docs[0]),
        _ => {
            let mut out = ComposeOutput::default();
            let mut projects = std::collections::BTreeSet::new();
            let mut errors = vec![];
            for (i, doc) in docs.iter().enumerate() {
                match parse_doc(doc) {
                    Ok(mut o) => {
                        projects.insert(o.project.clone());
                        out.warnings.append(&mut o.warnings);
                        for mut s in o.servers {
                            if out.servers.iter().any(|x| x.name == s.name && x.namespace == s.namespace) {
                                let base = s.name.clone();
                                let n = (2..).find(|n| !out.servers.iter().any(|x| x.name == format!("{base}-{n}"))).unwrap();
                                s.name = format!("{base}-{n}");
                                out.warnings.push(format!("document {}: server `{base}` already defined, renamed to `{}`", i + 1, s.name));
                            }
                            out.servers.push(s);
                        }
                    }
                    Err(e) => errors.push(format!("document {}: {e:#}", i + 1)),
                }
            }
            if out.servers.is_empty() {
                bail!("no servers found:\n{}", errors.join("\n"));
            }
            out.warnings.extend(errors.into_iter().map(|e| format!("{e} (skipped)")));
            out.project = if projects.len() == 1 { projects.into_iter().next().flatten() } else { None };
            Ok(out)
        }
    }
}

fn parse_doc(doc: &Value) -> Result<ComposeOutput> {
    let root = doc.as_object().context("compose file must be a YAML mapping")?;
    if root.contains_key("servers") {
        parse_native(root)
    } else if root.contains_key("services") {
        parse_docker(root)
    } else {
        bail!("expected a `servers:` (cubeharbor) or `services:` (docker-compose) section")
    }
}

/// DNS-1035 label usable as a StatefulSet and Service name (Services must start with a letter).
pub fn sanitize_name(raw: &str) -> String {
    let mut s: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-');
    let s: String = s.chars().take(40).collect();
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() {
        "server".into()
    } else if s.starts_with(|c: char| c.is_ascii_digit()) {
        format!("mc-{s}")
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// Native format
// ---------------------------------------------------------------------------

fn parse_native(root: &Map<String, Value>) -> Result<ComposeOutput> {
    let mut out = ComposeOutput {
        project: root.get("name").and_then(Value::as_str).map(sanitize_name),
        ..Default::default()
    };
    let namespace = root.get("namespace").and_then(Value::as_str).map(str::to_string);
    let defaults = root.get("defaults").cloned().unwrap_or(json!({}));
    let servers = root.get("servers").and_then(Value::as_object).context("`servers` must be a mapping")?;
    let known = spec_fields();

    for (raw_name, entry) in servers {
        let name = sanitize_name(raw_name);
        if &name != raw_name {
            out.warnings.push(format!("server `{raw_name}` renamed to `{name}` (Kubernetes naming rules)"));
        }
        let mut merged = defaults.clone();
        deep_merge(&mut merged, entry.clone());
        let mut obj = match merged {
            Value::Object(o) => o,
            Value::Null => Map::new(),
            _ => bail!("server `{raw_name}` must be a mapping"),
        };
        let ns = obj.remove("namespace").and_then(|v| v.as_str().map(str::to_string)).or(namespace.clone());
        expand_shorthands(&mut obj).with_context(|| format!("server `{raw_name}`"))?;
        for k in obj.keys() {
            if !known.contains(k) {
                out.warnings.push(format!("server `{raw_name}`: unknown field `{k}` ignored"));
            }
        }
        let spec: MinecraftServerSpec =
            serde_json::from_value(Value::Object(obj)).with_context(|| format!("server `{raw_name}`"))?;
        validate(&name, &spec, &mut out.warnings);
        out.servers.push(ComposeServer { name, namespace: ns, spec });
    }
    Ok(out)
}

fn spec_fields() -> Vec<String> {
    let schema = schemars::schema_for!(MinecraftServerSpec);
    schema
        .as_value()
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default()
}

fn deep_merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                deep_merge(b.entry(k).or_insert(Value::Null), v);
            }
        }
        (b, o) => *b = o,
    }
}

fn expand_shorthands(o: &mut Map<String, Value>) -> Result<()> {
    // Resources
    let mut res = o.remove("resources").unwrap_or(json!({}));
    for (short, long) in [("memory", "memory"), ("cpu", "cpu"), ("storage", "storage"), ("storageClass", "storageClass")] {
        if let Some(v) = o.remove(short) {
            res[long] = Value::String(scalar(&v));
        }
    }
    if res.as_object().is_some_and(|r| !r.is_empty()) {
        o.insert("resources".into(), res);
    }

    if let Some(v) = o.remove("type") {
        o.entry("loader").or_insert(v);
    }
    if let Some(v) = o.get("version") {
        o.insert("version".into(), Value::String(scalar(v)));
    }
    if let Some(Value::String(s)) = o.get("modpack") {
        let pack = modpack_shorthand(s)?;
        o.insert("modpack".into(), pack);
    }

    let mut mods = vec![];
    for key in ["mods", "plugins"] {
        if let Some(v) = o.remove(key) {
            for item in v.as_array().context("`mods`/`plugins` must be a list")? {
                mods.push(match item {
                    Value::String(s) => mod_shorthand(s),
                    other => other.clone(),
                });
            }
        }
    }
    if !mods.is_empty() {
        o.insert("mods".into(), Value::Array(mods));
    }

    match o.remove("voice") {
        Some(Value::Bool(true)) => {
            o.insert("voice".into(), json!({ "kind": "simple-voice-chat" }));
        }
        Some(Value::String(kind)) => {
            o.insert("voice".into(), json!({ "kind": kind }));
        }
        Some(Value::Bool(false)) | Some(Value::Null) | None => {}
        Some(v) => {
            o.insert("voice".into(), v);
        }
    }
    match o.remove("geyser") {
        Some(Value::Bool(true)) => {
            o.insert("geyser".into(), json!({}));
        }
        Some(Value::Bool(false)) | Some(Value::Null) | None => {}
        Some(v) => {
            o.insert("geyser".into(), v);
        }
    }
    match o.remove("autoSleep") {
        Some(Value::Number(n)) => {
            o.insert("autoSleep".into(), json!({ "idleMinutes": n }));
        }
        Some(Value::Bool(true)) => {
            o.insert("autoSleep".into(), json!({}));
        }
        Some(Value::Bool(false)) | Some(Value::Null) | None => {}
        Some(v) => {
            o.insert("autoSleep".into(), v);
        }
    }

    let mut routing = o.remove("routing").unwrap_or(json!({}));
    if let Some(h) = o.remove("hostname") {
        routing["hostnames"] = json!([scalar(&h)]);
    }
    if let Some(h) = o.remove("hostnames") {
        routing["hostnames"] = h;
    }
    if routing.as_object().is_some_and(|r| !r.is_empty()) {
        o.insert("routing".into(), routing);
    }

    if let Some(ports) = o.remove("ports") {
        let mut extra = o.remove("extraPorts").and_then(|v| v.as_array().cloned()).unwrap_or_default();
        for p in ports.as_array().context("`ports` must be a list")? {
            extra.push(match p {
                Value::String(s) => port_shorthand(s)?,
                other => other.clone(),
            });
        }
        o.insert("extraPorts".into(), Value::Array(extra));
    }

    if let Some(Value::Object(props)) = o.get_mut("properties") {
        for v in props.values_mut() {
            *v = Value::String(scalar(v));
        }
    }
    if let Some(Value::Object(env)) = o.get_mut("env") {
        for v in env.values_mut() {
            *v = Value::String(scalar(v));
        }
    }
    Ok(())
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `modrinth:slug@version`, `curseforge:slug@fileId`, `cf:slug`, `mr:slug` or a URL.
fn modpack_shorthand(s: &str) -> Result<Value> {
    if s.starts_with("http://") || s.starts_with("https://") {
        if let Some(slug) = curseforge_page_slug(s) {
            return Ok(json!({ "curseforge": { "project": slug } }));
        }
        if let Some(slug) = modrinth_page_slug(s) {
            return Ok(json!({ "modrinth": { "project": slug } }));
        }
        return Ok(json!({ "url": s }));
    }
    let (source, rest) = s.split_once(':').unwrap_or(("modrinth", s));
    let (project, version) = match rest.split_once('@') {
        Some((p, v)) => (p, Some(v)),
        None => (rest, None),
    };
    Ok(match source {
        "modrinth" | "mr" => json!({ "modrinth": { "project": project, "version": version } }),
        "curseforge" | "cf" => {
            let file_id = version.map(|v| v.parse::<u64>()).transpose().context("CurseForge @version must be a numeric file ID")?;
            json!({ "curseforge": { "project": project, "fileId": file_id } })
        }
        other => bail!("unknown modpack source `{other}` (use modrinth:, curseforge: or a URL)"),
    })
}

fn mod_shorthand(s: &str) -> Value {
    if s.starts_with("http://") || s.starts_with("https://") {
        return json!({ "url": s });
    }
    let rest = s.strip_prefix("modrinth:").or_else(|| s.strip_prefix("mr:")).unwrap_or(s);
    match rest.split_once('@').or_else(|| rest.split_once(':')) {
        Some((p, v)) => json!({ "modrinth": p, "version": v }),
        None => json!({ "modrinth": rest }),
    }
}

/// `bluemap:8100`, `query:25565/udp`, `bluemap:8100@map.example.com` (ingress), `rcon:25575!internal`.
fn port_shorthand(s: &str) -> Result<Value> {
    let (name, rest) = s.split_once(':').context("port shorthand is `name:port[/udp][@host]`")?;
    let (rest, host) = match rest.split_once('@') {
        Some((r, h)) => (r, Some(h)),
        None => (rest, None),
    };
    let (rest, internal) = match rest.strip_suffix("!internal") {
        Some(r) => (r, true),
        None => (rest, false),
    };
    let (port, proto) = match rest.split_once('/') {
        Some((p, pr)) => (p, pr.to_ascii_uppercase()),
        None => (rest, "TCP".into()),
    };
    let port: u16 = port.parse().with_context(|| format!("bad port in `{s}`"))?;
    let expose = if host.is_some() { "ingress" } else if internal { "internal" } else { "gateway" };
    Ok(json!({ "name": sanitize_name(name), "containerPort": port, "protocol": proto, "expose": expose, "host": host }))
}

fn curseforge_page_slug(url: &str) -> Option<String> {
    let rest = url.split("curseforge.com/minecraft/modpacks/").nth(1)?;
    rest.split('/').next().filter(|s| !s.is_empty()).map(str::to_string)
}

fn modrinth_page_slug(url: &str) -> Option<String> {
    let rest = url.split("modrinth.com/modpack/").nth(1)?;
    rest.split(['/', '?']).next().filter(|s| !s.is_empty()).map(str::to_string)
}

fn validate(name: &str, spec: &MinecraftServerSpec, warnings: &mut Vec<String>) {
    if let Some(p) = &spec.modpack {
        let n = [p.modrinth.is_some(), p.curseforge.is_some(), p.url.is_some()].iter().filter(|b| **b).count();
        if n != 1 {
            warnings.push(format!("server `{name}`: modpack should set exactly one of modrinth/curseforge/url"));
        }
    }
    if spec.modpack.is_none() && spec.version.is_none() {
        warnings.push(format!("server `{name}`: no version given, the latest release will be used"));
    }
    let mut seen = std::collections::HashSet::new();
    for p in &spec.extra_ports {
        if !seen.insert(&p.name) {
            warnings.push(format!("server `{name}`: duplicate port name `{}`", p.name));
        }
    }
}

// ---------------------------------------------------------------------------
// docker-compose (itzg/minecraft-server) import
// ---------------------------------------------------------------------------

fn parse_docker(root: &Map<String, Value>) -> Result<ComposeOutput> {
    let mut out = ComposeOutput {
        project: root.get("name").and_then(Value::as_str).map(sanitize_name),
        ..Default::default()
    };
    let services = root.get("services").and_then(Value::as_object).context("`services` must be a mapping")?;
    for (svc_name, svc) in services {
        let image = svc.get("image").and_then(Value::as_str).unwrap_or("");
        let env = docker_env(svc.get("environment"));
        let is_mc = image.contains("minecraft-server") || env.contains_key("EULA") || env.contains_key("TYPE");
        if !is_mc {
            let note = if image.contains("mc-router") || image.contains("velocity") || image.contains("bungeecord") {
                "routing is handled by the cubeharbor gateway"
            } else if image.contains("mc-backup") {
                "use volume snapshots or a backup CronJob instead"
            } else {
                "not a Minecraft server"
            };
            out.warnings.push(format!("service `{svc_name}` ({image}) skipped: {note}"));
            continue;
        }
        let name = docker_server_name(svc_name, svc, out.project.as_deref(), services.len() == 1);
        if name != sanitize_name(svc_name) {
            out.warnings.push(format!("service `{svc_name}` imported as server `{name}`"));
        }
        let (spec, mut w) = docker_service_to_spec(&name, svc, &env)?;
        out.warnings.append(&mut w);
        validate(&name, &spec, &mut out.warnings);
        out.servers.push(ComposeServer { name, namespace: None, spec });
    }
    if out.servers.is_empty() {
        bail!("no Minecraft server services found (expected itzg/minecraft-server images): {}", out.warnings.join("; "));
    }
    Ok(out)
}

/// itzg setups usually call the service `mc` in every file, so a generic service name is
/// replaced by something more telling: `container_name`, the first mc-router hostname label,
/// the `/data` volume's directory or the compose project name.
fn docker_server_name(svc_name: &str, svc: &Value, project: Option<&str>, only_service: bool) -> String {
    if let Some(c) = svc.get("container_name").and_then(Value::as_str) {
        return sanitize_name(c);
    }
    const GENERIC: &[&str] = &["mc", "minecraft", "server", "minecraft-server", "mc-server", "java", "app"];
    let own = sanitize_name(svc_name);
    if !GENERIC.contains(&own.as_str()) {
        return own;
    }
    let from_host = svc
        .pointer("/labels/mc-router.host")
        .map(scalar)
        .and_then(|h| split_list(&h).into_iter().next())
        .and_then(|h| normalize_host(&h).split('.').next().map(str::to_string))
        .filter(|l| !l.is_empty());
    let from_volume = data_volume(svc).and_then(|src| {
        src.trim_end_matches('/').rsplit('/').next().filter(|b| !b.is_empty() && *b != "data" && *b != ".").map(str::to_string)
    });
    let from_project = project.filter(|_| only_service).map(str::to_string);
    from_host.or(from_volume).or(from_project).map(|n| sanitize_name(&n)).unwrap_or(own)
}

/// Host path or volume name mounted at `/data`.
fn data_volume(svc: &Value) -> Option<String> {
    svc.get("volumes")?.as_array()?.iter().find_map(|v| match v {
        Value::String(s) => {
            let mut parts = s.split(':');
            let (src, dst) = (parts.next()?, parts.next()?);
            (dst.trim_end_matches('/') == "/data").then(|| src.to_string())
        }
        Value::Object(o) => {
            (o.get("target")?.as_str()?.trim_end_matches('/') == "/data").then(|| o.get("source").map(scalar)).flatten()
        }
        _ => None,
    })
}

/// Drop imported hostnames outside `base_domain` (they belong to the old setup) so the servers
/// fall back to `<name>.<base_domain>`.
pub fn rehost(out: &mut ComposeOutput, base_domain: &str) {
    let d = base_domain.trim().trim_matches('.').to_ascii_lowercase();
    if d.is_empty() {
        return;
    }
    for s in &mut out.servers {
        let before = std::mem::take(&mut s.spec.routing.hostnames);
        let (keep, dropped): (Vec<String>, Vec<String>) = before.into_iter().partition(|h| {
            let h = normalize_host(h);
            h == d || h.ends_with(&format!(".{d}"))
        });
        if !dropped.is_empty() {
            let now = if keep.is_empty() { format!("{}.{d}", s.name) } else { keep.join(", ") };
            out.warnings.push(format!("server `{}`: hostname {} replaced by {now}", s.name, dropped.join(", ")));
        }
        s.spec.routing.hostnames = keep;
    }
}

/// Render servers back into a cubeharbor compose file (the inverse of [`parse`]).
pub fn export(servers: &[MinecraftServer], project: Option<&str>) -> Result<String> {
    use kube::ResourceExt;
    let namespaces: std::collections::BTreeSet<_> = servers.iter().map(|s| s.namespace()).collect();
    let shared_ns = if namespaces.len() == 1 { namespaces.into_iter().next().flatten() } else { None };
    let mut map = Map::new();
    for s in servers {
        let mut spec = serde_json::to_value(&s.spec)?;
        prune_empty(&mut spec);
        if let (None, Some(ns), Some(o)) = (&shared_ns, s.namespace(), spec.as_object_mut()) {
            o.insert("namespace".into(), json!(ns));
        }
        map.insert(s.name_any(), spec);
    }
    let mut root = Map::new();
    if let Some(p) = project {
        root.insert("name".into(), json!(p));
    }
    if let Some(ns) = shared_ns {
        root.insert("namespace".into(), json!(ns));
    }
    root.insert("servers".into(), Value::Object(map));
    Ok(format!(
        "# cubeharbor compose, exported from the cluster. Edit and re-apply with\n#   cubeharbor apply <file>   or the dashboard's Compose tab.\n{}",
        serde_norway::to_string(&Value::Object(root))?
    ))
}

/// Remove nulls and empty lists/maps so exported specs stay short.
pub fn prune_empty(v: &mut Value) {
    match v {
        Value::Object(o) => {
            o.values_mut().for_each(prune_empty);
            o.retain(|_, v| !(v.is_null() || v.as_array().is_some_and(Vec::is_empty) || v.as_object().is_some_and(Map::is_empty)));
        }
        Value::Array(a) => a.iter_mut().for_each(prune_empty),
        _ => {}
    }
}

fn docker_env(v: Option<&Value>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    match v {
        Some(Value::Object(m)) => {
            for (k, v) in m {
                if !v.is_null() {
                    env.insert(k.clone(), scalar(v));
                }
            }
        }
        Some(Value::Array(a)) => {
            for item in a.iter().filter_map(Value::as_str) {
                if let Some((k, v)) = item.split_once('=') {
                    env.insert(k.to_string(), v.trim_matches('"').to_string());
                }
            }
        }
        _ => {}
    }
    env
}

fn split_list(s: &str) -> Vec<String> {
    s.split([',', '\n', ' ']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// `8G` / `4096M` -> MiB
fn parse_docker_mem(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_uppercase();
    let (num, mult) = if let Some(n) = s.strip_suffix("GB").or(s.strip_suffix("G")).or(s.strip_suffix("GI")) {
        (n, 1024.0)
    } else if let Some(n) = s.strip_suffix("MB").or(s.strip_suffix("M")).or(s.strip_suffix("MI")) {
        (n, 1.0)
    } else {
        (s.as_str(), 1.0 / (1024.0 * 1024.0))
    };
    num.trim().parse::<f64>().ok().map(|n| (n * mult).ceil() as u64)
}

fn docker_service_to_spec(
    name: &str,
    svc: &Value,
    env: &BTreeMap<String, String>,
) -> Result<(MinecraftServerSpec, Vec<String>)> {
    let mut w = vec![];
    let mut o = Map::new();
    let get = |k: &str| env.get(k).filter(|v| !v.is_empty()).cloned();
    let truthy = |k: &str| get(k).is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"));

    if let Some(v) = get("VERSION").filter(|v| v != "LATEST") {
        o.insert("version".into(), json!(v));
    }
    let ty = get("TYPE").unwrap_or_else(|| "VANILLA".into()).to_ascii_uppercase();
    match ty.as_str() {
        "VANILLA" | "PAPER" | "PURPUR" | "FABRIC" | "QUILT" | "FORGE" | "NEOFORGE" => {
            o.insert("loader".into(), json!(ty.to_ascii_lowercase()));
        }
        "SPIGOT" | "BUKKIT" | "FOLIA" => {
            w.push(format!("`{name}`: TYPE={ty} mapped to paper"));
            o.insert("loader".into(), json!("paper"));
        }
        "MODRINTH" => {
            let pack = get("MODRINTH_MODPACK").context("TYPE=MODRINTH needs MODRINTH_MODPACK")?;
            let mut v = modpack_shorthand(&pack)?;
            if v.get("url").is_none()
                && let Some(ver) = get("MODRINTH_VERSION")
            {
                v["modrinth"]["version"] = json!(ver);
            }
            o.insert("modpack".into(), v);
        }
        "AUTO_CURSEFORGE" | "CURSEFORGE" => {
            let slug = get("CF_SLUG")
                .or_else(|| get("CF_PAGE_URL").and_then(|u| curseforge_page_slug(&u)))
                .or_else(|| get("CF_MODPACK_ID"))
                .context("TYPE=AUTO_CURSEFORGE needs CF_SLUG or CF_PAGE_URL")?;
            let file_id = get("CF_FILE_ID").and_then(|f| f.parse::<u64>().ok());
            o.insert("modpack".into(), json!({ "curseforge": { "project": slug, "fileId": file_id } }));
            if env.contains_key("CF_API_KEY") {
                w.push(format!("`{name}`: CF_API_KEY ignored; set curseforge.apiKey in the Helm values instead"));
            }
        }
        other => w.push(format!("`{name}`: unsupported TYPE={other}, using vanilla")),
    }
    for (k, field) in [
        ("FORGE_VERSION", "loaderVersion"),
        ("NEOFORGE_VERSION", "loaderVersion"),
        ("FABRIC_LOADER_VERSION", "loaderVersion"),
        ("QUILT_LOADER_VERSION", "loaderVersion"),
        ("PAPER_BUILD", "loaderVersion"),
    ] {
        if let Some(v) = get(k) {
            o.insert(field.into(), json!(v));
        }
    }

    // Memory: itzg's MEMORY is the heap; give the container headroom on top.
    let limit = svc
        .pointer("/deploy/resources/limits/memory")
        .or(svc.get("mem_limit"))
        .map(scalar)
        .and_then(|m| parse_docker_mem(&m));
    let heap = get("MAX_MEMORY").or(get("MEMORY")).and_then(|m| parse_docker_mem(&m));
    let mem_mib = limit.or(heap.map(|h| h + (h / 4).max(1024)));
    if let Some(mib) = mem_mib {
        o.insert("resources".into(), json!({ "memory": format!("{mib}Mi") }));
    }

    if truthy("EULA") {
        o.insert("acceptEula".into(), json!(true));
    }
    if let Some(m) = get("MOTD") {
        o.insert("motd".into(), json!(m));
    }
    if let Some(v) = get("OPS") {
        o.insert("ops".into(), json!(split_list(&v)));
    }
    if let Some(v) = get("WHITELIST") {
        o.insert("whitelist".into(), json!(split_list(&v)));
    }

    let mut props = Map::new();
    for (k, p) in [
        ("DIFFICULTY", "difficulty"),
        ("MODE", "gamemode"),
        ("MAX_PLAYERS", "max-players"),
        ("PVP", "pvp"),
        ("ONLINE_MODE", "online-mode"),
        ("VIEW_DISTANCE", "view-distance"),
        ("SIMULATION_DISTANCE", "simulation-distance"),
        ("LEVEL_TYPE", "level-type"),
        ("SEED", "level-seed"),
        ("LEVEL", "level-name"),
        ("SPAWN_PROTECTION", "spawn-protection"),
        ("ALLOW_FLIGHT", "allow-flight"),
        ("ENABLE_COMMAND_BLOCK", "enable-command-block"),
        ("HARDCORE", "hardcore"),
        ("ALLOW_NETHER", "allow-nether"),
        ("MAX_TICK_TIME", "max-tick-time"),
        ("ENABLE_WHITELIST", "white-list"),
        ("ENFORCE_WHITELIST", "enforce-whitelist"),
    ] {
        if let Some(v) = get(k) {
            props.insert(p.into(), json!(v.to_ascii_lowercase()));
        }
    }
    if !props.is_empty() {
        o.insert("properties".into(), Value::Object(props));
    }

    let mut mods = vec![];
    if let Some(v) = get("MODRINTH_PROJECTS") {
        mods.extend(split_list(&v).iter().map(|m| mod_shorthand(m)));
    }
    for k in ["MODS", "PLUGINS"] {
        if let Some(v) = get(k) {
            mods.extend(split_list(&v).into_iter().map(|u| json!({ "url": u })));
        }
    }

    let mut jvm = Map::new();
    let extra: Vec<String> =
        ["JVM_OPTS", "JVM_XX_OPTS"].iter().filter_map(|k| get(k)).flat_map(|v| split_list(&v)).collect();
    if !extra.is_empty() {
        jvm.insert("extraArgs".into(), json!(extra));
    }
    if env.contains_key("USE_AIKAR_FLAGS") {
        jvm.insert("aikarFlags".into(), json!(truthy("USE_AIKAR_FLAGS")));
    }
    if !jvm.is_empty() {
        o.insert("jvm".into(), Value::Object(jvm));
    }

    // Ports: voice / bedrock / anything else becomes a gateway allocation pinned to the host port.
    let mut extra_ports = vec![];
    for p in svc.get("ports").and_then(Value::as_array).into_iter().flatten() {
        let Some((host_port, container_port, udp)) = docker_port(p) else {
            w.push(format!("`{name}`: could not parse port {p}"));
            continue;
        };
        match (container_port, udp) {
            (crate::crd::MINECRAFT_PORT, false) => {}
            (SIMPLE_VOICE_CHAT_PORT, true) => {
                o.insert("voice".into(), json!({ "kind": "simple-voice-chat", "publicPort": host_port, "install": o.get("modpack").is_none() }));
            }
            (PLASMO_VOICE_PORT, true) => {
                o.insert("voice".into(), json!({ "kind": "plasmo-voice", "publicPort": host_port, "install": false }));
            }
            (GEYSER_PORT, true) => {
                o.insert("geyser".into(), json!({ "publicPort": host_port }));
            }
            (crate::crd::RCON_PORT, false) => w.push(format!("`{name}`: RCON stays cluster-internal (use the dashboard console)")),
            (cp, udp) => extra_ports.push(json!({
                "name": format!("port-{cp}"),
                "containerPort": cp,
                "protocol": if udp { "UDP" } else { "TCP" },
                "publicPort": host_port,
            })),
        }
    }
    if !extra_ports.is_empty() {
        o.insert("extraPorts".into(), Value::Array(extra_ports));
    }
    if !mods.is_empty() {
        o.insert("mods".into(), Value::Array(mods));
    }

    if let Some(h) = svc.pointer("/labels/mc-router.host").map(scalar) {
        o.insert("routing".into(), json!({ "hostnames": split_list(&h) }));
    }
    if let Some(src) = data_volume(svc) {
        w.push(format!(
            "`{name}`: world data was in `{src}`; cubeharbor uses the volume data-{name}-0 instead (copy the world in after the first start)"
        ));
    } else if svc.get("volumes").is_some() {
        w.push(format!("`{name}`: volumes are replaced by a persistent volume; copy existing worlds into it after the first start"));
    }
    if let Some(tz) = get("TZ") {
        o.insert("env".into(), json!({ "TZ": tz }));
    }

    let spec: MinecraftServerSpec = serde_json::from_value(Value::Object(o))?;
    Ok((spec, w))
}

/// "25565:25565", "24454:24454/udp", "127.0.0.1:8100:8100", {target, published, protocol}
fn docker_port(v: &Value) -> Option<(u16, u16, bool)> {
    match v {
        Value::String(s) => {
            let (s, udp) = match s.rsplit_once('/') {
                Some((p, proto)) => (p, proto.eq_ignore_ascii_case("udp")),
                None => (s.as_str(), false),
            };
            let parts: Vec<&str> = s.split(':').collect();
            let container: u16 = parts.last()?.parse().ok()?;
            let host: u16 = if parts.len() >= 2 { parts[parts.len() - 2].parse().ok()? } else { container };
            Some((host, container, udp))
        }
        Value::Number(n) => {
            let p = n.as_u64()? as u16;
            Some((p, p, false))
        }
        Value::Object(o) => {
            let target = o.get("target")?.as_u64()? as u16;
            let published = o.get("published").map(scalar).and_then(|p| p.parse().ok()).unwrap_or(target);
            let udp = o.get("protocol").and_then(Value::as_str).is_some_and(|p| p.eq_ignore_ascii_case("udp"));
            Some((published, target, udp))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{Loader, VoiceKind};

    #[test]
    fn native_shorthands() {
        let out = parse(
            r#"
name: My Network
defaults:
  memory: 6Gi
  acceptEula: true
servers:
  lobby:
    loader: paper
    version: 1.21.4
    plugins: [luckperms, "https://example.com/x.jar"]
  ATM 10:
    modpack: curseforge:all-the-mods-10@123456
    memory: 16Gi
    voice: true
    autoSleep: 30
    hostname: atm.example.com
    ports: ["bluemap:8100@map.example.com"]
  cobble:
    modpack: https://modrinth.com/modpack/cobblemon-fabric
    geyser: true
    bogus: 1
"#,
        )
        .unwrap();
        assert_eq!(out.project.as_deref(), Some("my-network"));
        assert_eq!(out.servers.len(), 3);
        let atm = out.servers.iter().find(|s| s.name == "atm-10").unwrap();
        assert_eq!(atm.spec.resources.memory, "16Gi");
        assert_eq!(atm.spec.accept_eula, Some(true));
        assert_eq!(atm.spec.voice.as_ref().unwrap().kind, VoiceKind::SimpleVoiceChat);
        assert_eq!(atm.spec.modpack.as_ref().unwrap().curseforge.as_ref().unwrap().file_id, Some(123456));
        assert_eq!(atm.spec.auto_sleep.as_ref().unwrap().idle_minutes, 30);
        assert_eq!(atm.spec.routing.hostnames, vec!["atm.example.com"]);
        assert_eq!(atm.spec.extra_ports[0].host.as_deref(), Some("map.example.com"));
        let lobby = out.servers.iter().find(|s| s.name == "lobby").unwrap();
        assert_eq!(lobby.spec.loader, Some(Loader::Paper));
        assert_eq!(lobby.spec.resources.memory, "6Gi");
        assert_eq!(lobby.spec.mods.len(), 2);
        let cobble = out.servers.iter().find(|s| s.name == "cobble").unwrap();
        assert_eq!(cobble.spec.modpack.as_ref().unwrap().modrinth.as_ref().unwrap().project, "cobblemon-fabric");
        assert!(cobble.spec.geyser.is_some());
        assert!(out.warnings.iter().any(|w| w.contains("bogus")));
        assert!(out.to_yaml(Some("mc")).unwrap().contains("kind: MinecraftServer"));
    }

    #[test]
    fn docker_compose_import() {
        let out = parse(
            r#"
services:
  mc:
    image: itzg/minecraft-server
    ports:
      - "25565:25565"
      - "24454:24454/udp"
      - "8100:8100"
    environment:
      EULA: "TRUE"
      TYPE: AUTO_CURSEFORGE
      CF_PAGE_URL: https://www.curseforge.com/minecraft/modpacks/all-the-mods-9/files/5125809
      MEMORY: 8G
      DIFFICULTY: hard
      OPS: alice,bob
    volumes: ["./data:/data"]
  router:
    image: itzg/mc-router
"#,
        )
        .unwrap();
        assert_eq!(out.servers.len(), 1);
        let s = &out.servers[0].spec;
        assert_eq!(s.modpack.as_ref().unwrap().curseforge.as_ref().unwrap().project, "all-the-mods-9");
        assert_eq!(s.resources.memory, "10240Mi");
        assert_eq!(s.voice.as_ref().unwrap().public_port, Some(24454));
        assert_eq!(s.extra_ports[0].container_port, 8100);
        assert_eq!(s.properties["difficulty"], "hard");
        assert_eq!(s.ops, vec!["alice", "bob"]);
        assert!(out.warnings.iter().any(|w| w.contains("router")));
    }

    /// Two files from a games-compose setup (every file names its service `mc`), joined with `---`.
    #[test]
    fn itzg_multi_file_import() {
        let file = |name: &str, ty: &str, port: u16| {
            format!(
                "services:\n  mc:\n    image: itzg/minecraft-server:java21\n    environment:\n      EULA: \"TRUE\"\n      TYPE: {ty}\n      VERSION: \"1.21.10\"\n      MEMORY: 4G\n      TZ: America/Chicago\n    ports:\n      - \"{port}:25565/tcp\"\n    labels:\n      mc-router.host: {name}.mc.example.com\n    volumes:\n      - /srv/games/servers/{name}:/data\n    mem_limit: 5632m\n    stdin_open: true\n    tty: true\n"
            )
        };
        let terraria = "services:\n  terraria:\n    image: ryshe/terraria\n";
        let input = format!("{}---\n{}---\n{terraria}", file("example-paper", "PAPER", 25619), file("example-fabric", "FABRIC", 25618));
        let mut out = parse(&input).unwrap();
        let names: Vec<_> = out.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["example-paper", "example-fabric"]);
        let paper = &out.servers[0].spec;
        assert_eq!(paper.loader, Some(crate::crd::Loader::Paper));
        assert_eq!(paper.resources.memory, "5632Mi");
        assert_eq!(paper.env["TZ"], "America/Chicago");
        assert!(paper.extra_ports.is_empty(), "the Java port is routed, not forwarded");
        assert!(out.warnings.iter().any(|w| w.contains("terraria") && w.contains("skipped")));
        assert!(out.warnings.iter().any(|w| w.contains("/srv/games/servers/example-paper")));

        rehost(&mut out, "cy.example.com");
        assert!(out.servers.iter().all(|s| s.spec.routing.hostnames.is_empty()));
        assert!(out.warnings.iter().any(|w| w.contains("replaced by example-paper.cy.example.com")));
    }

    #[test]
    fn export_round_trips() {
        let out = parse("name: net\nservers:\n  lobby:\n    loader: paper\n    version: 1.21.4\n    memory: 4Gi\n    voice: true\n    hostname: lobby.cy.example.com\n").unwrap();
        let yaml = export(&out.manifests(Some("cubeharbor")), out.project.as_deref()).unwrap();
        assert!(yaml.contains("namespace: cubeharbor"));
        let again = parse(&yaml).unwrap();
        assert!(again.warnings.iter().all(|w| !w.contains("unknown field")), "{:?}", again.warnings);
        assert_eq!(again.project.as_deref(), Some("net"));
        assert_eq!(again.servers[0].spec, out.servers[0].spec);
        assert_eq!(again.servers[0].namespace.as_deref(), Some("cubeharbor"));
    }

    #[test]
    fn names() {
        assert_eq!(sanitize_name("My__Cool Server!"), "my-cool-server");
        assert_eq!(sanitize_name("---"), "server");
        assert_eq!(sanitize_name("2b2t"), "mc-2b2t");
        assert!(crate::crd::check_server_name(&sanitize_name(&"9".repeat(80))).is_ok());
    }
}
