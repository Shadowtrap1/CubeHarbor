use cubeharbor_core::compose::ComposeOutput;
use cubeharbor_core::crd::{MinecraftServer, Phase};
use kube::ResourceExt;
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::AppState;
use crate::api::{SourceStatus, Summary};
use crate::site::{self, Site};

/// `<head>` shared by every page: title, favicon and the themed stylesheet.
fn head(title: &str) -> Markup {
    let site = site::get();
    html! {
        head {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width,initial-scale=1";
            title { (title) " · " (site.name) }
            @if !site.favicon_url.is_empty() { link rel="icon" href=(site.favicon_url); }
            style { (PreEscaped(site::css())) }
        }
    }
}

/// Logo (or icon) and name of the site.
fn brand(site: &Site) -> Markup {
    html! {
        @if !site.logo_url.is_empty() { img src=(site.logo_url) alt=""; }
        @else if !site.icon.is_empty() { span { (site.icon) } }
        span { (site.name) }
    }
}

fn layout(title: &str, body: Markup) -> Markup {
    let site = site::get();
    html! {
        (DOCTYPE)
        html lang="en" {
            (head(title))
            body {
                header {
                    a.brand href="/" { (brand(site)) }
                    nav {
                        a href="/" { "Servers" } a href="/new" { "New server" } a href="/new#compose" { "Compose" } a href="/compose/export" { "Export compose" }
                        @for l in &site.links {
                            @if l.new_tab { a href=(l.url) target="_blank" rel="noopener" { (l.label) } } @else { a href=(l.url) { (l.label) } }
                        }
                    }
                    a href="/account" { "Account" }
                    form method="post" action="/logout" { button { "Log out" } }
                }
                main { (body) }
                @if !site.footer.is_empty() { footer { (site.footer) } }
            }
        }
    }
}

pub fn error_page(msg: &str) -> Markup {
    layout("Error", html! { h1 { "Something went wrong" } .card.error { pre.mono { (msg) } } a href="javascript:history.back()" { "← Back" } })
}

pub fn login(err: Option<&str>) -> Markup {
    let site = site::get();
    html! {
        (DOCTYPE)
        html lang="en" { (head("Log in"))
        body { main style="max-width:360px;margin-top:12vh" {
            h1.brand-title { (brand(site)) }
            .card { form method="post" action="/login" {
                @if let Some(e) = err { .error { (e) } }
                label { "Password" } input type="password" name="password" autofocus;
                br; br; button.primary type="submit" { "Log in" }
            } }
        } } }
    }
}

/// Change the dashboard password. `msg` is the outcome of a change: Ok(info) or Err(problem).
pub fn account(changeable: bool, msg: Option<Result<&str, &str>>) -> Markup {
    layout("Account", html! {
        h1 { "Account" }
        .card style="max-width:420px" {
            h2 { "Change password" }
            @match msg {
                Some(Ok(m)) => { p { b { (m) } } }
                Some(Err(e)) => { .error { (e) } }
                None => {}
            }
            @if changeable {
                form method="post" action="/account/password" {
                    label { "Current password" } input type="password" name="current" autocomplete="current-password" required;
                    label { "New password (at least " (crate::handlers::MIN_PASSWORD_LEN) " characters)" }
                    input type="password" name="new" autocomplete="new-password" minlength=(crate::handlers::MIN_PASSWORD_LEN) required;
                    label { "Repeat new password" }
                    input type="password" name="confirm" autocomplete="new-password" minlength=(crate::handlers::MIN_PASSWORD_LEN) required;
                    br; br; button.primary type="submit" { "Change password" }
                }
                p.muted { small { "Other sessions are logged out. If you set " code { "dashboard.password" } " in your Helm values, "
                    "the next " code { "helm upgrade" } " puts that value back." } }
            } @else {
                p.muted { "Changing the password here is turned off. Set " code { "dashboard.password" }
                    " in the Helm values, or change it in the Secret set as " code { "dashboard.existingSecret" } "." }
            }
        }
    })
}

fn phase_badge(p: Phase) -> Markup {
    html! { span class=(format!("badge {}", p.as_str())) { (p.as_str()) } }
}

pub fn address(st: &AppState, s: &MinecraftServer) -> Option<String> {
    let host = s.status.as_ref()?.hostnames.first()?.clone();
    Some(if st.cfg.public_minecraft_port == 25565 { host } else { format!("{host}:{}", st.cfg.public_minecraft_port) })
}

pub fn server_rows(st: &AppState, servers: &[MinecraftServer]) -> Markup {
    html! {
        @if servers.is_empty() {
            tr { td colspan="6" .muted { "No servers yet. " a href="/new" { "Create one" } " — or paste a compose file." } }
        }
        @for s in servers {
            @let ns = s.namespace().unwrap_or_default();
            @let status = s.status.clone().unwrap_or_default();
            @let link = format!("/servers/{ns}/{}", s.name_any());
            tr {
                td { a href=(link) { b { (s.name_any()) } } br; small.muted { (ns) } }
                td { (phase_badge(status.phase)) }
                td {
                    @if let Some(r) = &status.resolved {
                        (r.minecraft_version) " · " (r.loader)
                        @if let Some(m) = &r.modpack { br; small.muted { (m.name) " " (m.version) } }
                    } @else { span.muted { "resolving…" } }
                }
                td { @if let Some(p) = &status.players { (p.online) "/" (p.max) } @else { span.muted { "–" } } }
                td.mono { @if let Some(a) = address(st, s) { (a) } @else { span.muted { "set a hostname" } } }
                td { .row {
                    @if s.spec.paused || status.sleeping {
                        form method="post" action=(format!("{link}/action/start")) { button { "Start" } }
                    } @else {
                        form method="post" action=(format!("{link}/action/stop")) { button { "Stop" } }
                    }
                } }
            }
        }
    }
}

pub fn index(st: &AppState, servers: &[MinecraftServer], remote: &[Summary], sources: &[SourceStatus]) -> Markup {
    layout("Servers", html! {
        .row style="justify-content:space-between" {
            h1 { "Servers" }
            .row { a.btn href="/compose/export" { "Export compose" } a.btn href="/new#compose" { "Import compose" } a.btn href="/new" { "+ New server" } }
        }
        .card {
            table {
                thead { tr { th { "Name" } th { "Status" } th { "Version" } th { "Players" } th { "Address" } th {} } }
                tbody #rows data-refresh=(site::get().refresh_seconds) { (server_rows(st, servers)) }
            }
        }
        @for src in sources.iter() {
            h2 { "Other setup: " (src.name) }
            .card {
                @if let Some(e) = &src.error { .error { "Could not reach " (src.name) ": " (e) } }
                @let rows: Vec<&Summary> = remote.iter().filter(|r| r.source == src.name).collect();
                @if src.ok && rows.is_empty() { p.muted { "No servers." } }
                @if !rows.is_empty() {
                    table {
                        thead { tr { th { "Name" } th { "Status" } th { "Version" } th { "Players" } th { "Address" } } }
                        tbody { @for r in rows { tr {
                            td { b { (r.name) } }
                            td { span class=(format!("badge {}", if r.online { "Running" } else { "Stopped" })) { (r.phase) } }
                            td { (r.version.clone().unwrap_or_else(|| "–".into())) @if let Some(l) = &r.loader { " · " (l) } }
                            td { @if let Some(p) = &r.players { (p.online) @if p.max > 0 { "/" (p.max) } } @else { span.muted { "–" } } }
                            td.mono { (r.address.clone().unwrap_or_default()) }
                        } } }
                    }
                    p.muted { small { "Read-only: managed by its own setup. Recreate one here by importing its compose file." } }
                }
            }
        }
        script { (PreEscaped(site::INDEX_JS)) }
    })
}

pub fn new_server(st: &AppState, curseforge: bool) -> Markup {
    let suffix = st.cfg.base_domain.clone();
    let form = &site::get().new_server;
    layout("New server", html! {
        h1 { "New server" }
        .tabs { button.on data-tab="compose" { "Compose file" } button data-tab="quick" { "Quick create" } }
        .card #tab-compose {
            p.muted {
                "Write a cubeharbor compose file, or bring an existing setup: pick " code { "docker-compose.yml" } " / "
                code { "compose.yaml" } " files (or a whole " code { "servers/" } " folder) using " code { "itzg/minecraft-server" }
                ". Several files become one import; each server keeps its name from the mc-router hostname or data folder."
            }
            form method="post" action="/compose/preview" #compose-form {
                .row style="margin-bottom:8px" {
                    label.btn style="margin:0" { "Choose files…" input type="file" #files accept=".yml,.yaml" multiple style="display:none"; }
                    label.btn style="margin:0" { "Choose a folder…" input type="file" #folder webkitdirectory style="display:none"; }
                    button type="button" #fromcurrent { "Start from current servers" }
                    span.muted #filestate {}
                }
                textarea name="compose" spellcheck="false" { (form.example_compose) }
                .row style="margin-top:8px;gap:20px" {
                    @if !suffix.is_empty() {
                        label.check { input type="checkbox" name="rehost" checked; "Give imported servers " code { "<name>." (suffix) } " (drop hostnames from other domains)" }
                    }
                    button.primary type="submit" { "Preview" }
                }
            }
        }
        .card #tab-quick style="display:none" {
            form method="post" action="/new/quick" {
                .grid {
                    div { label { "Name" } input name="name" required pattern="[a-zA-Z0-9][a-zA-Z0-9 _-]*" placeholder="survival"; }
                    div { label { "Namespace" } input name="namespace" placeholder=(st.cfg.default_namespace); }
                    div { label { "Type" }
                        select name="source" #source {
                            @for t in &form.types { option value=(t.value) { (t.label) } }
                        }
                    }
                    div { label { "Minecraft version" } input name="version" placeholder="latest (or from modpack)"; }
                    div { label { "Memory" }
                        select name="memory" {
                            @for m in &form.memory_options {
                                option value=(m) selected[*m == form.memory] { (m) }
                            }
                        }
                    }
                    div { label { "Disk" } input name="storage" placeholder=(form.storage); }
                }
                #pack-box {
                    label { "Modpack (" code { "modrinth:slug" } ", " code { "curseforge:slug" } ", a modpack page URL or a .mrpack/zip URL)" }
                    input name="modpack" #modpack placeholder="modrinth:cobblemon-fabric";
                    .row style="margin-top:8px" {
                        input #q placeholder="Search modpacks…" style="flex:1";
                        select #src style="width:auto" {
                            option value="modrinth" { "Modrinth" }
                            @if curseforge { option value="curseforge" { "CurseForge" } }
                        }
                        button type="button" #search { "Search" }
                    }
                    .results #results {}
                }
                .grid {
                    div { label { "Hostname" } input name="hostname" placeholder=(if suffix.is_empty() { "play.example.com".to_string() } else { format!("<name>.{suffix}") }); }
                    div { label { "Auto-sleep after (minutes, 0 = never)" } input name="auto_sleep" type="number" min="0" value=(form.auto_sleep_minutes); }
                    div { label { "Operators" } input name="ops" placeholder="Steve, Alex"; }
                }
                label { "Extra mods / plugins (Modrinth slugs or URLs, comma separated)" }
                input name="mods" placeholder="lithium, ferrite-core, https://…/mod.jar";
                .row style="margin-top:12px;gap:20px" {
                    label.check { input type="checkbox" name="voice" checked[form.voice]; "Proximity voice chat (Simple Voice Chat)" }
                    label.check { input type="checkbox" name="geyser" checked[form.geyser]; "Bedrock cross-play (Geyser)" }
                    label.check { input type="checkbox" name="eula" required; "I accept the " a href="https://aka.ms/MinecraftEULA" target="_blank" { "Minecraft EULA" } }
                }
                br; button.primary type="submit" { "Create server" }
            }
        }
        script { (PreEscaped(site::NEW_JS)) }
    })
}

pub fn compose_preview(st: &AppState, input: &str, rehost: bool, out: &ComposeOutput, yaml: &str, err: Option<&str>) -> Markup {
    let suffix = st.cfg.base_domain.clone();
    let rehost_box = html! {
        @if !suffix.is_empty() {
            label.check { input type="checkbox" name="rehost" checked[rehost]; "Give imported servers " code { "<name>." (suffix) } }
        }
    };
    layout("Compose preview", html! {
        h1 { "Compose preview" }
        @if let Some(e) = err { .card.error { pre.mono { (e) } } }
        @if !out.servers.is_empty() {
            .card {
                p { b { (out.servers.len()) } " server(s) will be created or updated"
                    @if let Some(p) = &out.project { " in project " code { (p) } } "." }
                @for w in &out.warnings { .warn { (w) } }
                table {
                    thead { tr { th { "Name" } th { "Source" } th { "Memory" } th { "Address" } th { "Extras" } } }
                    tbody { @for s in &out.servers { tr {
                        td { b { (s.name) } }
                        td {
                            @if let Some(m) = &s.spec.modpack {
                                @if let Some(x) = &m.modrinth { "Modrinth " code { (x.project) } }
                                @if let Some(x) = &m.curseforge { "CurseForge " code { (x.project) } }
                                @if let Some(x) = &m.url { code { (x) } }
                            } @else {
                                (s.spec.loader.map(|l| l.as_str()).unwrap_or("vanilla")) " " (s.spec.version.clone().unwrap_or_else(|| "latest".into()))
                            }
                        }
                        td { (s.spec.resources.memory) }
                        td.mono { (cubeharbor_core::crd::effective_hostnames(&s.name, &s.spec, Some(&suffix)).join(", ")) }
                        td.muted {
                            @if s.spec.voice.is_some() { "voice " }
                            @if s.spec.geyser.is_some() { "bedrock " }
                            @if s.spec.auto_sleep.is_some() { "auto-sleep " }
                            @if !s.spec.mods.is_empty() { (s.spec.mods.len()) " mods" }
                        }
                    } } }
                }
                form method="post" action="/compose/apply" {
                    textarea name="compose" style="display:none" { (input) }
                    @if rehost { input type="hidden" name="rehost" value="on"; }
                    br; button.primary type="submit" { "Apply" }
                }
            }
            h2 { "Generated resources" }
            .card { pre.mono style="overflow:auto;max-height:480px" { (yaml) } }
        }
        h2 { "Edit" }
        .card { form method="post" action="/compose/preview" {
            textarea name="compose" spellcheck="false" { (input) }
            .row style="margin-top:8px;gap:20px" { (rehost_box) button type="submit" { "Preview again" } }
        } }
    })
}

pub fn server_detail(st: &AppState, s: &MinecraftServer, spec_yaml: &str) -> Markup {
    let ns = s.namespace().unwrap_or_default();
    let name = s.name_any();
    let base = format!("/servers/{ns}/{name}");
    let status = s.status.clone().unwrap_or_default();
    let host = st.public_host().unwrap_or("<gateway address>").to_string();
    layout(&name, html! {
        .row style="justify-content:space-between" {
            h1 { (name) " " (phase_badge(status.phase)) }
            .row {
                @if s.spec.paused || status.sleeping {
                    form method="post" action=(format!("{base}/action/start")) { button.primary { "Start" } }
                } @else {
                    form method="post" action=(format!("{base}/action/stop")) { button { "Stop" } }
                    form method="post" action=(format!("{base}/action/restart")) { button { "Restart" } }
                }
                a.btn href=(format!("{base}/compose.yaml")) { "Compose file" }
                form method="post" action=(format!("{base}/action/delete")) onsubmit="return confirm('Delete this server? The world data volume is kept.')" { button.danger { "Delete" } }
                form method="post" action=(format!("{base}/action/delete-data")) onsubmit="return confirm('Delete this server AND its world data? This cannot be undone.')" { button.danger { "Delete + data" } }
            }
        }
        @if let Some(m) = &status.message { p.muted { (m) } }
        .grid {
            .card { h2 style="margin-top:0" { "Connect" }
                .kv {
                    div { "Java" } div.mono { (address(st, s).unwrap_or_else(|| "no hostname — set routing.hostnames".into())) }
                    @for a in &status.allocations {
                        div { (a.name) } div.mono { (host) ":" (a.public_port) " " span.muted { "(" (a.protocol.as_str()) ")" } }
                    }
                }
            }
            .card { h2 style="margin-top:0" { "Server" }
                .kv {
                    @if let Some(r) = &status.resolved {
                        div { "Minecraft" } div { (r.minecraft_version) }
                        div { "Loader" } div { (r.loader) @if let Some(v) = &r.loader_version { " " (v) } }
                        div { "Java" } div { (r.java_version) }
                        @if let Some(m) = &r.modpack { div { "Modpack" } div { (m.name) " " span.muted { (m.version) } } }
                    }
                    div { "Memory" } div { (s.spec.resources.memory) }
                    div { "Players" } div {
                        @if let Some(p) = &status.players { (p.online) "/" (p.max) @if !p.sample.is_empty() { " — " (p.sample.join(", ")) } } @else { "–" }
                    }
                    @if let Some(a) = &s.spec.auto_sleep { div { "Auto-sleep" } div { (a.idle_minutes) " min idle" } }
                }
            }
        }
        h2 { "Console" }
        .card {
            .row style="margin-bottom:8px" {
                button type="button" data-c="server" .on { "Server log" }
                button type="button" data-c="install" { "Install log" }
                span.muted #logstate {}
            }
            pre #log data-base=(base) data-initial=(if status.phase == Phase::Installing { "install" } else { "server" }) data-max-lines=(site::get().console.max_lines) {}
            form #console .row style="margin-top:8px" {
                input name="command" #cmd placeholder="say hello  (runs over RCON)" autocomplete="off" style="flex:1";
                button.primary type="submit" { "Send" }
            }
        }
        h2 { "Spec" }
        .card { form method="post" action=(format!("{base}/spec")) {
            textarea name="spec" spellcheck="false" { (spec_yaml) }
            br; button.primary type="submit" { "Save & apply" }
        } }
        script { (PreEscaped(site::SERVER_JS)) }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn login_uses_site_branding_and_stylesheet() {
        let page = super::login(Some("Wrong password")).into_string();
        assert!(page.contains("<title>Log in · cubeharbor</title>"));
        assert!(page.contains("--accent:#5bbd72"));
        assert!(page.contains("<span>⛏</span><span>cubeharbor</span>"));
    }
}
