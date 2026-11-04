//! Default values used across cubeharbor, collected in one place.
//!
//! Most of these can also be overridden at runtime (Helm values, environment variables, or a
//! field in the `MinecraftServer` spec). This file is the fallback used when nothing else is set.
//! After changing a spec default (memory, storage, idle minutes), run `make crd` so the CRD
//! shipped in the chart picks it up.

// ---------------------------------------------------------------------------
// Project
// ---------------------------------------------------------------------------

/// Project URL, taken from `repository` in the workspace `Cargo.toml`.
pub const PROJECT_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// Namespace used for servers that don't name one (CLI, dashboard, compose files).
pub const NAMESPACE: &str = "cubeharbor";

// ---------------------------------------------------------------------------
// MinecraftServer spec defaults
// ---------------------------------------------------------------------------

/// Container memory limit when a server sets none. The JVM heap is derived from it.
pub const MEMORY: &str = "4Gi";
/// World/data volume size when a server sets none.
pub const STORAGE: &str = "20Gi";
/// CPU request when a server sets none (`resources.cpu: "0"` removes the request).
pub const CPU_REQUEST: &str = "500m";
/// Idle minutes before an `autoSleep` server goes to sleep, when `idleMinutes` is not set.
pub const AUTO_SLEEP_IDLE_MINUTES: u32 = 15;

// ---------------------------------------------------------------------------
// Operator
// ---------------------------------------------------------------------------

/// Server image repository. The tag is `<tag>-java<N>`.
pub const SERVER_IMAGE: &str = "ghcr.io/shadowtrap1/cubeharbor-server";
/// Pool of public ports for voice chat, Bedrock and other extra ports.
pub const PORT_RANGE: &str = "25600-25699";
/// Fallback start of the range when `PORT_RANGE` can't be parsed.
pub const PORT_RANGE_FALLBACK_START: u16 = 25600;
/// Parallel downloads while installing modpacks.
pub const DOWNLOAD_CONCURRENCY: u32 = 16;

// ---------------------------------------------------------------------------
// Server pods
// ---------------------------------------------------------------------------

/// Where the world and server files live inside the server container.
pub const DATA_DIR: &str = "/data";
/// uid/gid the server container runs as.
pub const SERVER_UID: i64 = 1000;
/// Seconds the runner waits for `stop` to finish, as set by the operator.
pub const STOP_TIMEOUT_SECONDS: u64 = 100;
/// Seconds Kubernetes waits before killing the pod. Keep this above `STOP_TIMEOUT_SECONDS`.
pub const TERMINATION_GRACE_SECONDS: u64 = 120;
/// How long the first boot may take before the startup probe gives up (big modpacks are slow).
pub const STARTUP_PROBE_MINUTES: u32 = 60;

// ---------------------------------------------------------------------------
// Runner (JVM)
// ---------------------------------------------------------------------------

/// Memory limit assumed when neither `MEMORY_LIMIT_MIB` nor the cgroup limit is available.
pub const FALLBACK_MEMORY_LIMIT_MIB: u64 = 4096;
/// Seconds the runner waits for `stop` when `STOP_TIMEOUT_SECONDS` is not set (e.g. plain docker).
pub const FALLBACK_STOP_TIMEOUT_SECONDS: u64 = 90;
/// Without `heapPercent`, the heap is the limit minus max(`HEAP_RESERVE_MIN_MIB`, `HEAP_RESERVE_PERCENT`%).
pub const HEAP_RESERVE_PERCENT: u64 = 15;
pub const HEAP_RESERVE_MIN_MIB: u64 = 1024;
/// The heap is never smaller than this.
pub const HEAP_MIN_MIB: u64 = 512;
/// Heaps above this use Aikar's large-heap G1 settings.
pub const AIKAR_LARGE_HEAP_MIB: u64 = 12 * 1024;
