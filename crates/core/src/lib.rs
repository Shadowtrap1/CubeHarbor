//! Shared types and clients for the cubeharbor Minecraft platform.

pub mod cidr;
pub mod compose;
pub mod crd;
pub mod defaults;
pub mod java;
pub mod logging;
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

/// Wake requests from the gateway. The gateway faces the internet, so instead of patching
/// MinecraftServers it may only write one ConfigMap (`<namespace>_<name>` -> unix time); the
/// operator turns each entry into a [`labels::WAKE_ANNOTATION`] and removes it.
pub mod wake {
    pub const DEFAULT_CONFIGMAP: &str = "cubeharbor-wake";

    /// ConfigMap key for a server. `_` can't appear in namespace or object names, so it splits
    /// unambiguously (and is a valid ConfigMap key character).
    pub fn key(namespace: &str, name: &str) -> String {
        format!("{namespace}_{name}")
    }

    pub fn parse_key(key: &str) -> Option<(&str, &str)> {
        key.split_once('_').filter(|(ns, name)| !ns.is_empty() && !name.is_empty())
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn keys_round_trip() {
            assert_eq!(super::key("mc", "lobby"), "mc_lobby");
            assert_eq!(super::parse_key("mc_lobby"), Some(("mc", "lobby")));
            assert_eq!(super::parse_key("mc-prod_atm-10"), Some(("mc-prod", "atm-10")));
            assert_eq!(super::parse_key("nounderscore"), None);
            assert_eq!(super::parse_key("_x"), None);
        }
    }
}
