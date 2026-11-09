//! cubeharbor-runner: PID 1 of the server container.
//!
//! Sizes the JVM from the container memory limit, starts the server with the command the
//! installer recorded, forwards stdin (for `kubectl attach`) and turns SIGTERM into a clean
//! `stop` so worlds are saved before the pod goes away.

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use cubeharbor_core::defaults;
use cubeharbor_core::plan::{self, LaunchSpec};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::signal::unix::{SignalKind, signal};

const AIKAR_FLAGS: &[&str] = &[
    "-XX:+UseG1GC",
    "-XX:+ParallelRefProcEnabled",
    "-XX:MaxGCPauseMillis=200",
    "-XX:+UnlockExperimentalVMOptions",
    "-XX:+DisableExplicitGC",
    "-XX:+AlwaysPreTouch",
    "-XX:G1HeapWastePercent=5",
    "-XX:G1MixedGCCountTarget=4",
    "-XX:InitiatingHeapOccupancyPercent=15",
    "-XX:G1MixedGCLiveThresholdPercent=90",
    "-XX:G1RSetUpdatingPauseTimePercent=5",
    "-XX:SurvivorRatio=32",
    "-XX:+PerfDisableSharedMem",
    "-XX:MaxTenuringThreshold=1",
];

/// Large heaps (>12G) get Aikar's adjusted region sizing.
const AIKAR_LARGE: &[&str] = &[
    "-XX:G1NewSizePercent=40",
    "-XX:G1MaxNewSizePercent=50",
    "-XX:G1HeapRegionSize=16M",
    "-XX:G1ReservePercent=15",
];
const AIKAR_SMALL: &[&str] = &[
    "-XX:G1NewSizePercent=30",
    "-XX:G1MaxNewSizePercent=40",
    "-XX:G1HeapRegionSize=8M",
    "-XX:G1ReservePercent=20",
];

#[tokio::main]
async fn main() {
    // `cubeharbor-runner probe`: the readiness probe. Vanilla opens its port before the world is
    // loaded, so an open port doesn't mean players can join; a status ping does.
    if std::env::args().nth(1).as_deref() == Some("probe") {
        let port = cubeharbor_core::crd::MINECRAFT_PORT;
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        // A server set up for PROXY protocol drops connections without the header.
        let prefix = if proxy_protocol() {
            cubeharbor_core::protocol::proxy_v2_header(addr, addr)
        } else {
            vec![]
        };
        match cubeharbor_core::protocol::ping_with_prefix(
            &addr.to_string(),
            "localhost",
            port,
            &prefix,
        )
        .await
        {
            Ok(_) => std::process::exit(0),
            Err(e) => {
                eprintln!("not ready: {e:#}");
                std::process::exit(1);
            }
        }
    }
    cubeharbor_core::logging::init("info", false);
    match run().await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            tracing::error!("{e:#}");
            std::process::exit(1);
        }
    }
}

/// Does the install plan say the server expects a PROXY protocol header (`routing.proxyProtocol`)?
fn proxy_protocol() -> bool {
    let path = env("CUBEHARBOR_PLAN").unwrap_or_else(|| plan::PLAN_PATH.into());
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<plan::InstallPlan>(&b).ok())
        .is_some_and(|p| p.proxy_protocol)
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Container memory limit from cgroup v2 (or v1), in MiB.
fn cgroup_limit_mib() -> Option<u64> {
    for p in [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ] {
        if let Ok(s) = std::fs::read_to_string(p)
            && let Ok(bytes) = s.trim().parse::<u64>()
            && bytes < (1 << 50)
        {
            return Some(bytes / 1024 / 1024);
        }
    }
    None
}

/// Heap size: explicit percentage, otherwise limit minus max(1 GiB, 15%) to leave room for
/// metaspace, threads and native buffers (modpacks use a lot of all three).
pub fn heap_mib(limit_mib: u64, percent: Option<u64>) -> u64 {
    let heap = match percent {
        Some(p) => limit_mib * p.clamp(10, 95) / 100,
        None => limit_mib.saturating_sub(
            (limit_mib * defaults::HEAP_RESERVE_PERCENT / 100).max(defaults::HEAP_RESERVE_MIN_MIB),
        ),
    };
    heap.max(defaults::HEAP_MIN_MIB)
}

fn jvm_args(launch: &LaunchSpec) -> Vec<String> {
    let mut args = vec![];
    let limit = env("MEMORY_LIMIT_MIB")
        .and_then(|s| s.parse().ok())
        .or_else(cgroup_limit_mib)
        .unwrap_or(defaults::FALLBACK_MEMORY_LIMIT_MIB);
    let heap = heap_mib(limit, env("HEAP_PERCENT").and_then(|s| s.parse().ok()));
    tracing::info!("memory limit {limit} MiB -> heap {heap} MiB");
    args.push(format!("-Xms{heap}M"));
    args.push(format!("-Xmx{heap}M"));
    if env("AIKAR_FLAGS").is_none_or(|v| v != "false") {
        args.extend(AIKAR_FLAGS.iter().map(|s| s.to_string()));
        let extra = if heap > defaults::AIKAR_LARGE_HEAP_MIB {
            AIKAR_LARGE
        } else {
            AIKAR_SMALL
        };
        args.extend(extra.iter().map(|s| s.to_string()));
        args.push("-Dusing.aikars.flags=https://mcflags.emc.gs".into());
        args.push("-Daikars.new.flags=true".into());
    }
    args.push("-Dlog4j2.formatMsgNoLookups=true".into());
    if let Some(extra) = env("JVM_EXTRA_ARGS") {
        match serde_json::from_str::<Vec<String>>(&extra) {
            Ok(v) => args.extend(v),
            Err(_) => args.extend(extra.split_whitespace().map(str::to_string)),
        }
    }
    args.extend(launch.java_args.iter().cloned());
    args.extend(launch.program_args.iter().cloned());
    args
}

async fn run() -> Result<i32> {
    let root = PathBuf::from(env("CUBEHARBOR_DATA").unwrap_or_else(|| defaults::DATA_DIR.into()));
    let launch = load_launch(&root)?;
    let java = env("JAVA_BIN").unwrap_or_else(|| "java".into());
    let args = jvm_args(&launch);
    tracing::info!("starting: {java} {}", args.join(" "));

    // Before the spawn: a SIGTERM with no handler is simply dropped for PID 1.
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let grace = Duration::from_secs(
        env("STOP_TIMEOUT_SECONDS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(defaults::FALLBACK_STOP_TIMEOUT_SECONDS),
    );

    let mut child = Command::new(&java)
        .args(&args)
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {java}"))?;
    let mut stdin = child.stdin.take().context("no stdin")?;

    // Writing happens in its own task: a server that stopped reading fills the pipe, and that
    // must never block signal handling or the shutdown timeout.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .is_err()
                || stdin.flush().await.is_err()
            {
                break;
            }
        }
    });
    // Forward our stdin (kubectl attach / docker -i) to the server console.
    let fwd = tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if fwd.send(line).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        status = child.wait() => {
            let status = status?;
            tracing::info!("server exited: {status}");
            return Ok(exit_code(status));
        }
        _ = term.recv() => {}
        _ = int.recv() => {}
    }

    // `stop` for most of the grace period, then SIGTERM (the JVM's shutdown hooks still save the
    // world if the console is stuck), then SIGKILL when the time is up.
    let (polite, last_resort) = (grace * 4 / 5, grace / 5);
    tracing::info!(
        "shutting down: sending `stop` (timeout {}s)",
        grace.as_secs()
    );
    let stop = async {
        let _ = tx.send("stop".into()).await;
        child.wait().await
    };
    if let Ok(status) = tokio::time::timeout(polite, stop).await {
        return Ok(exit_code(status?));
    }
    tracing::warn!("server ignored `stop`, sending SIGTERM");
    if let Some(pid) = child.id() {
        // SAFETY: `kill` has no memory-safety requirements, and the child isn't reaped yet.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
    if let Ok(status) = tokio::time::timeout(last_resort, child.wait()).await {
        return Ok(exit_code(status?));
    }
    tracing::warn!("server did not stop in time, killing");
    child.kill().await?;
    Ok(137)
}

/// The child's exit code, or the shell convention `128 + signal` when a signal killed it.
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1)
}

fn load_launch(root: &Path) -> Result<LaunchSpec> {
    let p = root.join(plan::LAUNCH_FILE);
    let data = std::fs::read(&p)
        .with_context(|| format!("{} missing — did the installer run?", p.display()))?;
    Ok(serde_json::from_slice(&data)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_sizing() {
        assert_eq!(heap_mib(4096, None), 3072);
        assert_eq!(heap_mib(16384, None), 13927);
        assert_eq!(heap_mib(8192, Some(50)), 4096);
        assert_eq!(heap_mib(600, None), 512);
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(ExitStatus::from_raw(0)), 0);
        assert_eq!(exit_code(ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(exit_code(ExitStatus::from_raw(9)), 137);
        assert_eq!(exit_code(ExitStatus::from_raw(15)), 143);
    }
}
