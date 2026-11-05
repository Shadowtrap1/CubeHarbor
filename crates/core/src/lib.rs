//! Shared types and clients for the cubeharbor Minecraft platform.

pub mod cidr;
pub mod compose;
pub mod crd;
pub mod defaults;
pub mod java;
pub mod plan;
pub mod protocol;
pub mod rcon;
pub mod sources;

pub use crd::{Loader, MinecraftServer, MinecraftServerSpec, MinecraftServerStatus};
pub use reqwest;

/// Well-known labels and annotations.
pub mod labels {
    pub const MANAGED_BY: &str = "app.kubernetes.io/managed-by";
    pub const MANAGER: &str = "cubeharbor";
    pub const SERVER: &str = "cubeharbor.dev/server";
    pub const COMPONENT: &str = "app.kubernetes.io/component";
    /// Set by the gateway when a player tries to join a sleeping server.
    pub const WAKE_ANNOTATION: &str = "cubeharbor.dev/wake-requested";
    /// Changing this restarts the server pod.
    pub const RESTART_ANNOTATION: &str = "cubeharbor.dev/restarted-at";
    pub const PLAN_HASH_ANNOTATION: &str = "cubeharbor.dev/plan-hash";
    pub const FIELD_MANAGER: &str = "cubeharbor-operator";
}
