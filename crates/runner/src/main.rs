//! cubeharbor-runner: PID 1 of the server container.
//!
//! Sizes the JVM from the container memory limit, starts the server with the command the
//! installer recorded, forwards stdin (for `kubectl attach`) and turns SIGTERM into a clean
//! `stop` so worlds are saved before the pod goes away.

use std::path::{Path, PathBuf};
use std::process::Stdio;
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
const AIKAR_LARGE: &[&str] = &["-XX:G1NewSizePercent=40", "-XX:G1MaxNewSizePercent=50", "-XX:G1HeapRegionSize=16M", "-XX:G1ReservePercent=15"];
const AIKAR_SMALL: &[&str] = &["-XX:G1NewSizePercent=30", "-XX:G1MaxNewSizePercent=40", "-XX:G1HeapRegionSize=8M", "-XX:G1ReservePercent=20"];

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();
    match run().await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            tracing::error!("{e:#}");
            std::process::exit(1);
        }
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Container memory limit from cgroup v2 (or v1), in MiB.
fn cgroup_limit_mib() -> Option<u64> {
    for p in ["/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"] {
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
        None => limit_mib.saturating_sub((limit_mib * defaults::HEAP_RESERVE_PERCENT / 100).max(defaults::HEAP_RESERVE_MIN_MIB)),
    };
    heap.max(defaults::HEAP_MIN_MIB)
}

fn jvm_args(launch: &LaunchSpec) -> Vec<String> {
    let mut args = vec![];
    let limit = env("MEMORY_LIMIT_MIB").and_then(|s| s.parse().ok()).or_else(cgroup_limit_mib).unwrap_or(defaults::FALLBACK_MEMORY_LIMIT_MIB);
    let heap = heap_mib(limit, env("HEAP_PERCENT").and_then(|s| s.parse().ok()));
    tracing::info!("memory limit {limit} MiB -> heap {heap} MiB");
    args.push(format!("-Xms{heap}M"));
    args.push(format!("-Xmx{heap}M"));
    if env("AIKAR_FLAGS").is_none_or(|v| v != "false") {
        args.extend(AIKAR_FLAGS.iter().map(|s| s.to_string()));
        let extra = if heap > defaults::AIKAR_LARGE_HEAP_MIB { AIKAR_LARGE } else { AIKAR_SMALL };
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

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
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

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let grace = Duration::from_secs(env("STOP_TIMEOUT_SECONDS").and_then(|s| s.parse().ok()).unwrap_or(defaults::FALLBACK_STOP_TIMEOUT_SECONDS));

    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                tracing::info!("server exited: {status}");
                return Ok(status.code().unwrap_or(1));
            }
            Some(line) = rx.recv() => {
                let _ = stdin.write_all(format!("{line}\n").as_bytes()).await;
            }
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }

    tracing::info!("shutting down: sending `stop` (timeout {}s)", grace.as_secs());
    let _ = stdin.write_all(b"stop\n").await;
    let _ = stdin.flush().await;
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(status) => Ok(status?.code().unwrap_or(0)),
        Err(_) => {
            tracing::warn!("server did not stop in time, killing");
            child.kill().await?;
            Ok(137)
        }
    }
}

fn load_launch(root: &Path) -> Result<LaunchSpec> {
    let p = root.join(plan::LAUNCH_FILE);
    let data = std::fs::read(&p).with_context(|| format!("{} missing — did the installer run?", p.display()))?;
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
}
