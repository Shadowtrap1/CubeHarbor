//! Every route of the dashboard and who may use it, in one table.
//!
//! A route is registered together with its [`Access`], so a new one can't forget authorization:
//! the `auth` middleware looks the rule up by matched route, and the handlers do no role checks.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Extension;
use axum::Router;
use axum::handler::Handler;
use axum::http::Method;
use axum::middleware;
use axum::routing::{MethodRouter, get, post};

use crate::users::Role;
use crate::{AppState, account, api, auth, handlers, security_headers};

/// Who may use a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// No login (the login page itself, liveness).
    Public,
    /// This role or a higher one.
    Min(Role),
    /// `/servers/{ns}/{name}/action/{action}`: running servers is for operators; anything else,
    /// such as deleting them, is for admins.
    ServerAction,
    /// The server list: public with `PUBLIC_SERVER_LIST`, viewers otherwise.
    ServerList,
}

impl Access {
    /// The least role needed for `path` (the raw request path), or None when no login is needed.
    pub fn required_role(self, path: &str, public_server_list: bool) -> Option<Role> {
        match self {
            Access::Public => None,
            Access::Min(role) => Some(role),
            Access::ServerAction if matches!(path.rsplit('/').next(), Some("start" | "stop" | "restart")) => Some(Role::Operator),
            Access::ServerAction => Some(Role::Admin),
            Access::ServerList if public_server_list => None,
            Access::ServerList => Some(Role::Viewer),
        }
    }
}

type S = Arc<AppState>;

struct Entry {
    method: Method,
    path: &'static str,
    access: Access,
    handler: MethodRouter<S>,
}

fn g<H: Handler<T, S>, T: 'static>(path: &'static str, access: Access, h: H) -> Entry {
    Entry { method: Method::GET, path, access, handler: get(h) }
}

fn p<H: Handler<T, S>, T: 'static>(path: &'static str, access: Access, h: H) -> Entry {
    Entry { method: Method::POST, path, access, handler: post(h) }
}

/// Add routes here. Reads are usually `VIEWER`; anything that changes what runs needs `ADMIN`.
fn table() -> Vec<Entry> {
    use Access::{Public, ServerAction, ServerList};
    use account::*;
    use handlers::*;
    const VIEWER: Access = Access::Min(Role::Viewer);
    const OPERATOR: Access = Access::Min(Role::Operator);
    const ADMIN: Access = Access::Min(Role::Admin);
    vec![
        g("/healthz", Public, || async { "ok" }),
        g("/login", Public, login_page),
        p("/login", Public, login),
        p("/logout", VIEWER, logout),
        g("/", VIEWER, index),
        g("/fragments/servers", VIEWER, servers_fragment),
        g("/account", VIEWER, account_page),
        p("/account/password", VIEWER, change_password),
        p("/account/tokens", VIEWER, create_token),
        p("/account/tokens/{id}/revoke", VIEWER, revoke_token),
        g("/users", ADMIN, users_page),
        p("/users", ADMIN, add_user),
        p("/users/{name}/role", ADMIN, set_role),
        p("/users/{name}/password", ADMIN, reset_password),
        p("/users/{name}/delete", ADMIN, remove_user),
        g("/new", ADMIN, new_page),
        p("/new/quick", ADMIN, quick_create),
        p("/compose/preview", ADMIN, compose_preview),
        p("/compose/apply", ADMIN, compose_apply),
        g("/compose/export", VIEWER, api::compose_export),
        g("/servers/{ns}/{name}", VIEWER, server_page),
        p("/servers/{ns}/{name}/action/{action}", ServerAction, server_action),
        p("/servers/{ns}/{name}/spec", ADMIN, update_spec),
        p("/servers/{ns}/{name}/console", OPERATOR, console),
        g("/servers/{ns}/{name}/logs", VIEWER, logs),
        g("/servers/{ns}/{name}/compose.yaml", VIEWER, api::server_compose),
        g("/api/servers", VIEWER, api_servers),
        p("/api/compose", ADMIN, api_compose),
        g("/api/modpacks/search", VIEWER, api_search),
        g("/api/v1/servers", ServerList, api::servers),
        g("/api/v1/servers/{ns}/{name}/compose", VIEWER, api::server_compose),
        g("/api/v1/compose", VIEWER, api::compose_export),
        p("/api/v1/compose", ADMIN, api::compose_apply),
    ]
}

/// The table's access rules, by route pattern.
#[derive(Default)]
pub struct Policy(HashMap<&'static str, Vec<(Method, Access)>>);

pub type SharedPolicy = Arc<Policy>;

impl Policy {
    /// The rule for a matched route. `GET` routes also answer `HEAD`.
    pub fn lookup(&self, method: &Method, pattern: &str) -> Option<Access> {
        let method = if method == Method::HEAD { &Method::GET } else { method };
        self.0.get(pattern)?.iter().find(|(m, _)| m == method).map(|(_, a)| *a)
    }
}

pub fn router(state: S) -> Router {
    let mut policy = Policy::default();
    let mut router = Router::new();
    for e in table() {
        policy.0.entry(e.path).or_default().push((e.method, e.access));
        router = router.route(e.path, e.handler);
    }
    router
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(Extension(SharedPolicy::new(policy)))
        .layer(middleware::from_fn_with_state(state.clone(), api::cors))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_has_one_rule() {
        let mut seen = std::collections::HashSet::new();
        for e in table() {
            assert!(seen.insert((e.method.clone(), e.path)), "{} {} is listed twice", e.method, e.path);
        }
    }

    #[test]
    fn only_login_and_liveness_are_always_public() {
        let public: Vec<&str> = table().iter().filter(|e| e.access == Access::Public).map(|e| e.path).collect();
        assert_eq!(public, ["/healthz", "/login", "/login"]);
    }

    #[test]
    fn writes_need_at_least_an_operator_unless_they_are_about_your_own_login() {
        let own = ["/logout", "/login"];
        for e in table().iter().filter(|e| e.method != Method::GET) {
            if own.contains(&e.path) || e.path.starts_with("/account") {
                continue;
            }
            match e.access.required_role("/x", false) {
                Some(r) => assert!(r >= Role::Operator, "{} is open to {r:?}", e.path),
                None => panic!("{} is public", e.path),
            }
        }
    }

    #[test]
    fn server_actions_by_role() {
        use Role::*;
        let role = |a: &str| Access::ServerAction.required_role(&format!("/servers/mc/a/action/{a}"), false);
        for a in ["start", "stop", "restart"] {
            assert_eq!(role(a), Some(Operator));
        }
        for a in ["delete", "delete-data", "other", "sto%70"] {
            assert_eq!(role(a), Some(Admin), "{a}");
        }
    }

    #[test]
    fn public_list_only_when_configured() {
        assert_eq!(Access::ServerList.required_role("/api/v1/servers", true), None);
        assert_eq!(Access::ServerList.required_role("/api/v1/servers", false), Some(Role::Viewer));
    }

    #[test]
    fn head_follows_get_and_unknown_methods_have_no_rule() {
        let mut p = Policy::default();
        p.0.insert("/x", vec![(Method::GET, Access::Min(Role::Admin))]);
        assert_eq!(p.lookup(&Method::HEAD, "/x"), Some(Access::Min(Role::Admin)));
        assert_eq!(p.lookup(&Method::POST, "/x"), None);
        assert_eq!(p.lookup(&Method::GET, "/y"), None);
    }
}
