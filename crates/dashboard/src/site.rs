//! Look-and-feel and form defaults of the dashboard ("site config").
//!
//! Loaded from a YAML file (`SITE_CONFIG`, filled from `dashboard.site` in the Helm chart). Every
//! field is optional, and anything left out keeps the built-in default defined here. See
//! `examples/site.yaml` for an annotated copy of all options.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Built-in assets. Edit these files to change the defaults for every deployment.
pub const STYLE_CSS: &str = include_str!("../assets/style.css");
pub const INDEX_JS: &str = include_str!("../assets/index.js");
pub const NEW_JS: &str = include_str!("../assets/new.js");
pub const SERVER_JS: &str = include_str!("../assets/server.js");
pub const EXAMPLE_COMPOSE: &str = include_str!("../assets/example-compose.yaml");

static SITE: OnceLock<Site> = OnceLock::new();
static CSS: OnceLock<String> = OnceLock::new();

/// Make `site` the config every page uses. Call once at startup.
pub fn install(site: Site) {
    let _ = CSS.set(site.stylesheet());
    let _ = SITE.set(site);
}

/// The installed site config (defaults when `install` was never called).
pub fn get() -> &'static Site {
    SITE.get_or_init(Site::default)
}

/// Stylesheet of the installed site, built once.
pub fn css() -> &'static str {
    CSS.get_or_init(|| get().stylesheet())
}

#[derive(Deserialize, Debug, Clone)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Site {
    /// Name in the header, page titles and the login page.
    pub name: String,
    /// Text/emoji shown before the name. Empty to hide.
    pub icon: String,
    /// Image shown instead of `icon` (URL or data: URI).
    pub logo_url: String,
    /// Browser tab icon (URL or data: URI). None when empty.
    pub favicon_url: String,
    /// Small text under every page. Empty to hide.
    pub footer: String,
    /// CSS variable overrides, without the leading `--` (see `assets/style.css`), e.g. `accent: "#7c5cff"`.
    pub theme: BTreeMap<String, String>,
    /// Extra CSS appended after the built-in stylesheet.
    pub custom_css: String,
    /// Extra links in the header after the built-in ones.
    pub links: Vec<Link>,
    /// Seconds between server list refreshes. 0 disables auto refresh.
    pub refresh_seconds: u32,
    pub console: Console,
    pub new_server: NewServer,
}

impl Default for Site {
    fn default() -> Self {
        Self {
            name: "cubeharbor".into(),
            icon: "⛏".into(),
            logo_url: String::new(),
            favicon_url: String::new(),
            footer: String::new(),
            theme: BTreeMap::new(),
            custom_css: String::new(),
            links: vec![],
            refresh_seconds: 5,
            console: Console::default(),
            new_server: NewServer::default(),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Link {
    pub label: String,
    pub url: String,
    /// Open in a new tab.
    #[serde(default)]
    pub new_tab: bool,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Console {
    /// Log lines loaded when the console opens.
    pub tail_lines: i64,
    /// Lines kept in the browser before the oldest are dropped.
    pub max_lines: u32,
}

impl Default for Console {
    fn default() -> Self {
        Self { tail_lines: 500, max_lines: 4000 }
    }
}

/// Defaults of the "New server" page.
#[derive(Deserialize, Debug, Clone)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct NewServer {
    /// Memory sizes offered in the quick-create form.
    pub memory_options: Vec<String>,
    /// Preselected memory size.
    pub memory: String,
    /// Placeholder of the disk field (the server default applies when it's left empty).
    pub storage: String,
    /// Preset of the auto-sleep field, in minutes. 0 = never.
    pub auto_sleep_minutes: u32,
    /// Preselected checkboxes.
    pub voice: bool,
    pub geyser: bool,
    /// Entries of the "Type" select, in order. `value` is `modpack` or a loader name.
    pub types: Vec<TypeOption>,
    /// Text in the compose editor when the page opens.
    pub example_compose: String,
    /// Modpack search results per query.
    pub search_results: u32,
}

impl Default for NewServer {
    fn default() -> Self {
        let types = [
            ("modpack", "Modpack"),
            ("paper", "Paper (plugins)"),
            ("purpur", "Purpur (plugins)"),
            ("fabric", "Fabric"),
            ("neoforge", "NeoForge"),
            ("forge", "Forge"),
            ("quilt", "Quilt"),
            ("vanilla", "Vanilla"),
        ];
        Self {
            memory_options: ["2Gi", "4Gi", "6Gi", "8Gi", "10Gi", "12Gi", "16Gi", "20Gi", "24Gi", "32Gi"].map(String::from).to_vec(),
            memory: "8Gi".into(),
            storage: cubeharbor_core::defaults::STORAGE.into(),
            auto_sleep_minutes: 0,
            voice: false,
            geyser: false,
            types: types.map(|(value, label)| TypeOption { value: value.into(), label: label.into() }).to_vec(),
            example_compose: EXAMPLE_COMPOSE.into(),
            search_results: 12,
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct TypeOption {
    pub value: String,
    pub label: String,
}

impl Site {
    /// Read the site config; no path means all defaults.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else { return Ok(Self::default()) };
        let text = std::fs::read_to_string(path).with_context(|| format!("reading site config {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing site config {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        // An empty file (or `{}` / `null` from an empty Helm block) means defaults.
        if text.trim().is_empty() || text.trim() == "null" {
            return Ok(Self::default());
        }
        let site: Self = serde_norway::from_str(text)?;
        site.validate()?;
        Ok(site)
    }

    fn validate(&self) -> Result<()> {
        for (k, v) in &self.theme {
            anyhow::ensure!(
                !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "theme: `{k}` is not a CSS variable name (letters, digits and `-`)"
            );
            anyhow::ensure!(!v.contains(['<', '>', ';', '{', '}']), "theme.{k}: `{v}` must be a single CSS value");
        }
        anyhow::ensure!(!self.new_server.types.is_empty(), "newServer.types must not be empty");
        anyhow::ensure!(!self.new_server.memory_options.is_empty(), "newServer.memoryOptions must not be empty");
        Ok(())
    }

    /// Built-in stylesheet, theme overrides and custom CSS, ready for a `<style>` element.
    pub fn stylesheet(&self) -> String {
        let mut css = String::from(STYLE_CSS);
        if !self.theme.is_empty() {
            css.push_str(":root{");
            for (k, v) in &self.theme {
                css.push_str(&format!("--{k}:{v};"));
            }
            css.push_str("}\n");
        }
        css.push_str(&self.custom_css);
        // Never let config text close the <style> element.
        css.replace("</", "<\\/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default() {
        for t in ["", "  \n", "null", "{}"] {
            let s = Site::parse(t).unwrap();
            assert_eq!(s.name, "cubeharbor");
            assert_eq!(s.new_server.memory, "8Gi");
        }
    }

    #[test]
    fn partial_override() {
        let s = Site::parse("name: Blockhaus\ntheme:\n  accent: \"#7c5cff\"\nnewServer:\n  memory: 6Gi\n").unwrap();
        assert_eq!(s.name, "Blockhaus");
        assert_eq!(s.icon, "⛏");
        assert_eq!(s.new_server.memory, "6Gi");
        assert_eq!(s.new_server.types.len(), 8);
        assert!(s.stylesheet().contains("--accent:#7c5cff;"));
    }

    /// `examples/site.yaml` documents the defaults; keep it parseable and in sync.
    #[test]
    fn example_matches_defaults() {
        let s = Site::parse(include_str!("../../../examples/site.yaml")).unwrap();
        let d = Site::default();
        assert_eq!((s.name, s.icon, s.refresh_seconds), (d.name, d.icon, d.refresh_seconds));
        assert_eq!((s.console.tail_lines, s.console.max_lines), (d.console.tail_lines, d.console.max_lines));
        let (n, m) = (&s.new_server, &d.new_server);
        assert_eq!((&n.memory_options, &n.memory, &n.storage, n.search_results), (&m.memory_options, &m.memory, &m.storage, m.search_results));
        let labels = |t: &[TypeOption]| t.iter().map(|t| (t.value.clone(), t.label.clone())).collect::<Vec<_>>();
        assert_eq!(labels(&n.types), labels(&m.types));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Site::parse("nmae: typo\n").is_err());
        assert!(Site::parse("theme:\n  accent: \"red;}body{display:none\"\n").is_err());
        assert!(Site::parse("theme:\n  \"a b\": red\n").is_err());
        let s = Site::parse("customCss: \"</style><script>\"\n").unwrap();
        assert!(!s.stylesheet().contains("</style>"));
    }
}
