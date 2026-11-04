//! cubeharbor: command-line companion for cubeharbor.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cubeharbor_core::compose;
use cubeharbor_core::MinecraftServer;
use kube::api::{Api, Patch, PatchParams};
use kube::{Client, CustomResourceExt, ResourceExt};

#[derive(Parser)]
#[command(version, about = "cubeharbor command-line tool")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the MinecraftServer CustomResourceDefinition.
    Crd,
    /// Convert a compose file (cubeharbor or docker-compose) into MinecraftServer manifests.
    Render {
        /// One or more compose files; several are imported together (e.g. servers/*/compose.yaml).
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[arg(short, long, default_value = cubeharbor_core::defaults::NAMESPACE)]
        namespace: String,
        /// Drop imported hostnames outside this domain so servers get `<name>.<domain>`.
        #[arg(long, value_name = "DOMAIN")]
        rehost: Option<String>,
    },
    /// Resolve versions, loader, Java and modpack files for each server in a compose file.
    Resolve { file: PathBuf },
    /// Server List Ping a Minecraft server (`host[:port]`).
    Ping {
        address: String,
        /// Hostname to send in the handshake (defaults to the address), e.g. to test gateway routing.
        #[arg(long)]
        host: Option<String>,
    },
    /// Run a console command over RCON.
    Rcon {
        address: String,
        #[arg(short, long, env = "RCON_PASSWORD")]
        password: String,
        command: Vec<String>,
    },
    /// Apply a compose file to the current cluster (server-side apply).
    Apply {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[arg(short, long, default_value = cubeharbor_core::defaults::NAMESPACE)]
        namespace: String,
        #[arg(long, value_name = "DOMAIN")]
        rehost: Option<String>,
    },
}

/// Read compose files. Several are joined into one multi-document import; a docker-compose
/// file without a `name:` gets its directory name as the project name.
fn load_many(files: &[PathBuf], rehost: Option<&str>) -> Result<compose::ComposeOutput> {
    let mut docs = vec![];
    for file in files {
        let mut text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let dir = file.canonicalize().ok().and_then(|p| p.parent()?.file_name()?.to_str().map(str::to_string));
        let has_name = text.lines().any(|l| l.starts_with("name:"));
        let docker = text.lines().any(|l| l.starts_with("services:"));
        if let (Some(dir), true, false) = (dir, files.len() > 1 && docker, has_name) {
            text = format!("name: {dir}\n{text}");
        }
        docs.push(text);
    }
    let mut out = compose::parse(&docs.join("\n---\n"))?;
    if let Some(d) = rehost {
        compose::rehost(&mut out, d);
    }
    for w in &out.warnings {
        eprintln!("warning: {w}");
    }
    Ok(out)
}

fn load(file: &PathBuf) -> Result<compose::ComposeOutput> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let out = compose::parse(&text)?;
    for w in &out.warnings {
        eprintln!("warning: {w}");
    }
    Ok(out)
}

/// Kubernetes only knows int32/int64; drop Rust-specific integer formats to avoid apply warnings.
fn strip_formats(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(o) => {
            if o.get("format").and_then(|f| f.as_str()).is_some_and(|f| f.starts_with("uint") || f == "int8" || f == "int16") {
                o.remove("format");
            }
            o.values_mut().for_each(strip_formats);
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(strip_formats),
        _ => {}
    }
}

fn split_addr(addr: &str, default_port: u16) -> (String, u16) {
    match addr.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse().unwrap()),
        _ => (addr.to_string(), default_port),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Crd => {
            let mut crd = serde_json::to_value(MinecraftServer::crd())?;
            strip_formats(&mut crd);
            print!("{}", serde_norway::to_string(&crd)?)
        }
        Cmd::Render { files, namespace, rehost } => print!("{}", load_many(&files, rehost.as_deref())?.to_yaml(Some(&namespace))?),
        Cmd::Resolve { file } => {
            let http = cubeharbor_core::sources::Http::from_env()?;
            for s in load(&file)?.servers {
                let r = cubeharbor_core::sources::resolve::resolve(&http, &s.spec).await?;
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "name": s.name, "resolved": r }))?);
            }
        }
        Cmd::Ping { address, host: vhost } => {
            let (host, port) = split_addr(&address, cubeharbor_core::crd::MINECRAFT_PORT);
            let vhost = vhost.unwrap_or_else(|| host.clone());
            let s = cubeharbor_core::protocol::ping(&format!("{host}:{port}"), &vhost, port).await?;
            let motd = cubeharbor_core::rcon::strip_formatting(&s.motd());
            let p = s.players.unwrap_or_default();
            let v = s.version.map(|v| format!("{} (protocol {})", v.name, v.protocol)).unwrap_or_default();
            println!("{vhost} — {v} — {}/{} players — {motd}", p.online, p.max);
        }
        Cmd::Rcon { address, password, command } => {
            let (host, port) = split_addr(&address, cubeharbor_core::crd::RCON_PORT);
            let mut r = cubeharbor_core::rcon::Rcon::connect(&format!("{host}:{port}"), &password).await?;
            println!("{}", cubeharbor_core::rcon::strip_formatting(&r.command(&command.join(" ")).await?));
        }
        Cmd::Apply { files, namespace, rehost } => {
            let out = load_many(&files, rehost.as_deref())?;
            let client = Client::try_default().await?;
            for m in out.manifests(Some(&namespace)) {
                let ns = m.namespace().unwrap_or_else(|| namespace.clone());
                let api: Api<MinecraftServer> = Api::namespaced(client.clone(), &ns);
                let mut v = serde_json::to_value(&m)?;
                v.as_object_mut().map(|o| o.remove("status"));
                api.patch(&m.name_any(), &PatchParams::apply("cubeharbor").force(), &Patch::Apply(&v)).await?;
                println!("minecraftserver.cubeharbor.dev/{} applied in {ns}", m.name_any());
            }
        }
    }
    Ok(())
}
