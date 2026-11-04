//! Configuration files applied on every start: server.properties, EULA, ops/whitelist,
//! voice chat and proxy settings, and user-provided files.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use cubeharbor_core::Loader;
use cubeharbor_core::crd::{MINECRAFT_PORT, RCON_PORT, VoiceKind};
use cubeharbor_core::plan::InstallPlan;
use cubeharbor_core::sources::mojang;
use serde_json::{Value, json};

use crate::Ctx;
use crate::download::safe_join;

pub async fn apply(ctx: &Ctx, plan: &InstallPlan, loader: Loader) -> Result<()> {
    if !plan.accept_eula {
        bail!(
            "the Minecraft EULA (https://aka.ms/MinecraftEULA) has not been accepted: set `acceptEula: true` on the server or `minecraft.acceptEula` in the Helm values"
        );
    }
    write(&ctx.root.join("eula.txt"), "# Accepted via cubeharbor\neula=true\n").await?;

    let mut props: BTreeMap<String, String> = BTreeMap::new();
    if let Some(m) = &plan.motd {
        props.insert("motd".into(), m.clone());
    }
    if !plan.whitelist.is_empty() {
        props.insert("white-list".into(), "true".into());
        props.insert("enforce-whitelist".into(), "true".into());
    }
    props.extend(plan.properties.clone());
    // Managed keys always win: the gateway and dashboard depend on them.
    props.insert("server-port".into(), MINECRAFT_PORT.to_string());
    props.insert("server-ip".into(), String::new());
    props.insert("enable-rcon".into(), "true".into());
    props.insert("rcon.port".into(), RCON_PORT.to_string());
    if let Ok(pw) = std::env::var("RCON_PASSWORD") {
        props.insert("rcon.password".into(), pw);
    }
    props.insert("enable-status".into(), "true".into());
    merge_properties(&ctx.root.join("server.properties"), &props).await?;

    let online = plan.properties.get("online-mode").is_none_or(|v| v != "false");
    if !plan.ops.is_empty() {
        players_file(ctx, "ops.json", &plan.ops, online, |e| {
            if let Some(o) = e.as_object_mut() {
                o.entry("level").or_insert(json!(4));
                o.entry("bypassesPlayerLimit").or_insert(json!(false));
            }
        })
        .await;
    }
    if !plan.whitelist.is_empty() {
        players_file(ctx, "whitelist.json", &plan.whitelist, online, |_| {}).await;
    }

    if let Some(v) = &plan.voice {
        match v.kind {
            VoiceKind::SimpleVoiceChat => {
                let base = if loader.is_plugin_server() { "plugins/voicechat" } else { "config/voicechat" };
                let mut p = BTreeMap::from([("port".to_string(), v.container_port.to_string())]);
                p.insert("bind_address".into(), String::new());
                if let Some(addr) = &v.public_address {
                    p.insert("voice_host".into(), addr.clone());
                }
                merge_properties(&ctx.root.join(base).join("voicechat-server.properties"), &p).await?;
            }
            VoiceKind::PlasmoVoice => {
                tracing::info!("Plasmo Voice: set host.port={} in its config if you changed it", v.container_port);
            }
        }
    }

    if plan.proxy_protocol && loader.is_plugin_server() {
        let path = ctx.root.join("config/paper-global.yml");
        let mut doc: Value = match tokio::fs::read_to_string(&path).await {
            Ok(s) => serde_norway::from_str(&s).unwrap_or(json!({})),
            Err(_) => json!({}),
        };
        if !doc.is_object() {
            doc = json!({});
        }
        let proxies = doc.as_object_mut().unwrap().entry("proxies").or_insert(json!({}));
        if proxies.is_object() {
            proxies["proxy-protocol"] = json!(true);
        }
        write(&path, &serde_norway::to_string(&doc)?).await?;
    }

    for f in &plan.files {
        let path = safe_join(&ctx.root, &f.path)?;
        if f.if_missing && path.exists() {
            continue;
        }
        write(&path, &f.content).await?;
    }
    Ok(())
}

async fn write(path: &Path, content: &str) -> Result<()> {
    if let Some(p) = path.parent() {
        tokio::fs::create_dir_all(p).await?;
    }
    tokio::fs::write(path, content).await.with_context(|| format!("writing {}", path.display()))
}

/// Update keys in a Java properties file, keeping comments, ordering and unrelated keys.
pub async fn merge_properties(path: &Path, set: &BTreeMap<String, String>) -> Result<()> {
    let existing = tokio::fs::read_to_string(path).await.unwrap_or_default();
    write(path, &merge_properties_text(&existing, set)).await
}

pub fn merge_properties_text(existing: &str, set: &BTreeMap<String, String>) -> String {
    let mut remaining = set.clone();
    let mut out = String::new();
    for line in existing.lines() {
        let t = line.trim_start();
        if !t.starts_with('#')
            && !t.starts_with('!')
            && let Some((k, _)) = t.split_once('=')
            && let Some(v) = remaining.remove(k.trim())
        {
            out.push_str(&format!("{}={}\n", k.trim(), escape(&v)));
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    for (k, v) in remaining {
        out.push_str(&format!("{k}={}\n", escape(&v)));
    }
    out
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n")
}

/// Merge players into ops.json / whitelist.json, resolving UUIDs through Mojang.
async fn players_file(ctx: &Ctx, file: &str, names: &[String], online: bool, extra: impl Fn(&mut Value)) {
    let path = ctx.root.join(file);
    let mut list: Vec<Value> = tokio::fs::read_to_string(&path)
        .await
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    for name in names {
        if list.iter().any(|e| e["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name))) {
            continue;
        }
        let uuid = if online {
            match mojang::profile(&ctx.http, name).await {
                Ok(p) => mojang::dashed_uuid(&p.id),
                Err(e) => {
                    tracing::warn!("could not look up player {name}: {e:#}");
                    continue;
                }
            }
        } else {
            offline_uuid(name)
        };
        let mut entry = json!({ "uuid": uuid, "name": name });
        extra(&mut entry);
        list.push(entry);
    }
    if let Ok(s) = serde_json::to_string_pretty(&list) {
        let _ = write(&path, &s).await;
    }
}

/// UUID v3 of "OfflinePlayer:<name>", as used by offline-mode servers.
fn offline_uuid(name: &str) -> String {
    use md5::Digest;
    let mut h: [u8; 16] = md5::Md5::digest(format!("OfflinePlayer:{name}").as_bytes()).into();
    h[6] = (h[6] & 0x0f) | 0x30;
    h[8] = (h[8] & 0x3f) | 0x80;
    mojang::dashed_uuid(&crate::download::hex(&h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_merge_keeps_layout() {
        let existing = "#Minecraft server properties\nmotd=Old\nlevel-name=world\n";
        let set = BTreeMap::from([("motd".to_string(), "New".to_string()), ("server-port".to_string(), "25565".to_string())]);
        assert_eq!(merge_properties_text(existing, &set), "#Minecraft server properties\nmotd=New\nlevel-name=world\nserver-port=25565\n");
    }

    #[test]
    fn offline_uuid_matches_vanilla() {
        // Known value for "Notch" in offline mode.
        assert_eq!(offline_uuid("Notch"), "b50ad385-829d-3141-a216-7e7d7539ba7f");
    }
}
