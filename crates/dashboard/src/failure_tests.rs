//! Failure paths of the handlers and the JSON API, and the role matrix for API tokens, against a
//! fake Kubernetes API that can be told to fail and records what reaches it.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use clap::Parser;
use cubeharbor_core::compose;
use cubeharbor_core::crd::MinecraftServer;
use k8s_openapi::ByteString;
use kube::Client;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::users::{self, Backend, Identity, Role, Store, Users, hash_password};
use crate::{AppState, COOKIE, handlers, new_state, ops, routes};

const NS: &str = "mc";
const SECRET: &str = "dash-users";
const ADMIN_PW: &str = "admin-password-1";
/// Part of every failure the fake API answers with; must never reach a client.
const INTERNAL: &str = "system:serviceaccount:cubeharbor:dashboard";

#[derive(Default)]
struct Fake {
    /// Answer every non-Secret request with this status.
    fail: Mutex<Option<u16>>,
    /// `METHOD path?query` of every non-Secret request.
    seen: Mutex<Vec<String>>,
    servers: Mutex<Vec<Value>>,
    secret: Mutex<BTreeMap<String, ByteString>>,
}

impl Fake {
    fn writes(&self) -> Vec<String> {
        self.seen.lock().unwrap().iter().filter(|r| !r.starts_with("GET ")).cloned().collect()
    }
}

fn client(fake: Arc<Fake>) -> Client {
    let svc = tower::service_fn(move |req: Request<kube::client::Body>| {
        let fake = fake.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = axum::body::to_bytes(Body::new(body), usize::MAX).await.unwrap();
            let path = parts.uri.path().to_string();
            let reply = |code: u16, v: Value| axum::http::Response::builder().status(code).header("content-type", "application/json").body(Body::from(v.to_string())).unwrap();
            let status = |code: u16, msg: &str| json!({ "kind": "Status", "apiVersion": "v1", "status": "Failure", "code": code, "message": msg });
            if path.ends_with(&format!("/secrets/{SECRET}")) {
                if parts.method == "PATCH" {
                    let patch: k8s_openapi::api::core::v1::Secret = serde_json::from_slice(&body).unwrap();
                    fake.secret.lock().unwrap().extend(patch.data.unwrap_or_default());
                }
                let meta = json!({ "name": SECRET, "namespace": NS });
                return Ok::<_, std::convert::Infallible>(reply(200, json!({ "apiVersion": "v1", "kind": "Secret", "metadata": meta })));
            }
            let target = parts.uri.path_and_query().map(|p| p.to_string()).unwrap_or_default();
            fake.seen.lock().unwrap().push(format!("{} {target}", parts.method));
            if let Some(code) = *fake.fail.lock().unwrap() {
                return Ok(reply(code, status(code, &format!("forbidden: User {INTERNAL} cannot do that"))));
            }
            let servers = fake.servers.lock().unwrap().clone();
            if path.ends_with("/minecraftservers") && parts.method == "GET" {
                return Ok(reply(200, json!({ "apiVersion": "cubeharbor.dev/v1alpha1", "kind": "MinecraftServerList", "metadata": {}, "items": servers })));
            }
            let named = servers.into_iter().find(|s| path.ends_with(&format!("/minecraftservers/{}", s["metadata"]["name"].as_str().unwrap())));
            Ok(match named {
                Some(s) if parts.method == "GET" => reply(200, s),
                _ => reply(404, status(404, &format!("{INTERNAL}: not found"))),
            })
        }
    });
    Client::new(svc, NS)
}

struct Fixture {
    app: Router,
    state: Arc<AppState>,
    fake: Arc<Fake>,
    admin: String,
    operator: String,
    viewer: String,
}

/// Users are built directly and sessions started without a login, so each test costs one hash.
fn fixture(extra: &[&str]) -> Fixture {
    let fake = Arc::new(Fake::default());
    let mut store = Store::default();
    store.add("admin", Role::Admin, hash_password(ADMIN_PW).unwrap()).unwrap();
    store.add("otto", Role::Operator, "unused".into()).unwrap();
    store.add("val", Role::Viewer, "unused".into()).unwrap();
    let kube = client(fake.clone());
    let backend = Backend { secrets: kube::Api::namespaced(kube.clone(), NS), name: SECRET.into() };
    let users = Users::new(store, Some(backend)).unwrap();
    let mut args = vec!["dashboard", "--default-namespace", NS, "--login-max-failures-total", "0"];
    args.extend_from_slice(extra);
    let cfg = crate::Config::try_parse_from(args).unwrap();
    let state = Arc::new(new_state(&cfg, kube, users).unwrap());
    let session = |u: &str| state.users.new_session(u).unwrap();
    let (admin, operator, viewer) = (session("admin"), session("otto"), session("val"));
    Fixture { app: routes::router(state.clone()), state, fake, admin, operator, viewer }
}

impl Fixture {
    async fn request(&self, method: &str, path: &str, cookie: Option<&str>, body: &str, origin: Option<&str>) -> (StatusCode, String) {
        let mut b = Request::builder().method(method).uri(path).header(header::HOST, "dash.test");
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, format!("{COOKIE}={c}"));
        }
        if let Some(o) = origin {
            b = b.header(header::ORIGIN, o);
        }
        if method == "POST" && !path.contains("/compose?") && path != "/api/compose" && path != "/api/v1/compose" {
            b = b.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        let mut req = b.body(Body::from(body.to_string())).unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([192, 0, 2, 1], 1234))));
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let text = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&text).to_string())
    }

    async fn admin(&self, method: &str, path: &str, body: &str) -> (StatusCode, String) {
        self.request(method, path, Some(&self.admin), body, None).await
    }

    fn add_server(&self, name: &str, env: &[(&str, &str)]) {
        let yaml = format!("servers:\n  {name}:\n    loader: paper\n    version: 1.21.1\n    memory: 2Gi\n");
        let mut m: MinecraftServer = compose::parse(&yaml).unwrap().manifests(Some(NS)).remove(0);
        m.spec.env = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let mut v = serde_json::to_value(&m).unwrap();
        v["apiVersion"] = "cubeharbor.dev/v1alpha1".into();
        v["kind"] = "MinecraftServer".into();
        self.fake.servers.lock().unwrap().push(v);
    }
}

fn form_escape(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

#[tokio::test]
async fn role_matrix_is_the_same_for_api_tokens() {
    let f = fixture(&[]);
    let token_of = |user: &'static str| {
        let users = &f.state.users;
        async move { format!("Bearer {}", users.create_token(user, "t").await.unwrap()) }
    };
    let tokens = [(Role::Viewer, token_of("val").await), (Role::Operator, token_of("otto").await), (Role::Admin, token_of("admin").await)];
    let routes: &[(&str, &str, Role)] = &[
        ("GET", "/api/servers", Role::Viewer),
        ("GET", "/api/v1/servers?remotes=false", Role::Viewer),
        ("GET", "/api/v1/compose", Role::Viewer),
        ("POST", "/account/tokens/nope/revoke", Role::Viewer),
        ("POST", "/servers/mc/a/action/stop", Role::Operator),
        ("POST", "/servers/mc/a/console", Role::Operator),
        ("POST", "/servers/mc/a/action/delete", Role::Admin),
        ("POST", "/servers/mc/a/spec", Role::Admin),
        ("POST", "/api/v1/compose", Role::Admin),
        ("GET", "/users", Role::Admin),
    ];
    for (method, path, needed) in routes {
        for (role, token) in &tokens {
            let mut req = Request::builder().method(*method).uri(*path).header(header::AUTHORIZATION, token).body(Body::empty()).unwrap();
            req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([192, 0, 2, 1], 1))));
            let resp = f.app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status() == StatusCode::FORBIDDEN, role < needed, "{method} {path} as {role:?}: {}", resp.status());
        }
    }
    assert!(f.state.stats.forbidden.load(Ordering::Relaxed) > 0);
}

#[tokio::test]
async fn unknown_methods_and_paths_run_no_handler() {
    let f = fixture(&[]);
    assert_eq!(f.request("GET", "/nope", None, "", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(f.request("DELETE", "/users", None, "", None).await.0, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn public_list_opens_only_the_list() {
    let f = fixture(&["--public-server-list", "--remotes", "internal=http://10.11.12.13:1/api"]);
    let (status, body) = f.request("GET", "/api/v1/servers", None, "", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("10.11.12.13"), "a remote's address stays private: {body}");
    assert!(body.contains("unreachable"), "{body}");
    assert_eq!(f.request("GET", "/api/servers", None, "", None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(f.request("GET", "/api/v1/compose", None, "", None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn encoded_names_never_reach_kubernetes() {
    let f = fixture(&["--allowed-namespaces", "*"]);
    for path in ["/servers/mc/x%2F..%2F..%2Fsecrets%2Fy", "/servers/a%2Fb/x", "/servers/mc/x%3Fa=b", "/api/v1/servers/mc/%2E%2E/compose", "/servers/mc/UPPER"] {
        let (status, body) = f.admin("GET", path, "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
    }
    let (status, _) = f.request("POST", "/servers/mc/a%2Fb/console", Some(&f.operator), "command=list", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(f.admin("GET", "/compose/export?namespace=a%2Fb", "").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.admin("GET", "/compose/export?project=a,b%3Dc", "").await.0, StatusCode::BAD_REQUEST);
    assert!(f.fake.seen.lock().unwrap().is_empty(), "{:?}", f.fake.seen.lock().unwrap());
}

#[tokio::test]
async fn namespaces_outside_the_list_are_refused() {
    let f = fixture(&[]);
    assert_eq!(f.admin("GET", "/servers/kube-system/x", "").await.0, StatusCode::FORBIDDEN);
    assert_eq!(f.admin("GET", "/compose/export?namespace=kube-system", "").await.0, StatusCode::FORBIDDEN);
    assert!(f.fake.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn kubernetes_errors_stay_in_the_log() {
    let f = fixture(&[]);
    *f.fake.fail.lock().unwrap() = Some(403);
    let compose = "servers:\n  a:\n    loader: paper\n    version: 1.21.1\n    memory: 2Gi\n";
    let form = format!("compose={}", form_escape(compose));
    for (method, path, body) in [
        ("GET", "/", ""),
        ("GET", "/servers/mc/a", ""),
        ("GET", "/api/servers", ""),
        ("GET", "/api/v1/servers?remotes=false", ""),
        ("GET", "/api/v1/compose", ""),
        ("GET", "/api/v1/servers/mc/a/compose", ""),
        ("POST", "/api/v1/compose", compose),
        ("POST", "/api/compose", compose),
        ("POST", "/compose/apply", &form),
        ("POST", "/servers/mc/a/action/stop", ""),
        ("POST", "/servers/mc/a/console", "command=list"),
    ] {
        let (status, text) = f.admin(method, path, body).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{method} {path}: {text}");
        assert!(!text.contains(INTERNAL), "{method} {path} leaked: {text}");
    }
    *f.fake.fail.lock().unwrap() = Some(500);
    assert_eq!(f.admin("GET", "/api/v1/servers?remotes=false", "").await.0, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn missing_servers_are_404() {
    let f = fixture(&[]);
    let (status, body) = f.admin("GET", "/servers/mc/nope", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!body.contains(INTERNAL), "{body}");
    let (status, body) = f.admin("GET", "/api/v1/servers/mc/nope/compose", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(serde_json::from_str::<Value>(&body).unwrap()["error"].is_string());
    assert_eq!(f.admin("POST", "/servers/mc/nope/action/stop", "").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bad_input_is_400() {
    let f = fixture(&[]);
    f.add_server("a", &[]);
    assert_eq!(f.admin("POST", "/servers/mc/a/action/explode", "").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.admin("POST", "/compose/apply", "compose=%7Bnot%20yaml").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.admin("POST", "/servers/mc/a/spec", "spec=%5B1%2C2%5D").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.admin("POST", "/api/v1/compose", "{not yaml").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.admin("POST", "/api/compose", "").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.admin("POST", "/servers/mc/a/console", "command=%20").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.admin("POST", "/servers/mc/a/console", &format!("command={}", "x".repeat(1447))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.admin("GET", &format!("/api/modpacks/search?q={}", "x".repeat(201)), "").await.0, StatusCode::BAD_REQUEST);
    assert!(f.fake.writes().is_empty(), "{:?}", f.fake.writes());
}

#[tokio::test]
async fn a_refused_namespace_applies_nothing() {
    let f = fixture(&[]);
    let two = "servers:\n  fine:\n    loader: paper\n    version: 1.21.1\n    memory: 2Gi\n  bad:\n    namespace: kube-system\n    loader: paper\n    version: 1.21.1\n    memory: 2Gi\n";
    let (status, body) = f.admin("POST", "/api/v1/compose", two).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let form = format!("compose={}", form_escape(two));
    assert_eq!(f.admin("POST", "/compose/apply", &form).await.0, StatusCode::FORBIDDEN);
    assert!(f.fake.writes().is_empty(), "{:?}", f.fake.writes());
    // Nor can a compose file put a path into a namespace when every namespace is allowed.
    let f = fixture(&["--allowed-namespaces", "*"]);
    let evil = "namespace: a/../b\nservers:\n  fine:\n    loader: paper\n    version: 1.21.1\n    memory: 2Gi\n";
    assert_eq!(f.admin("POST", "/api/v1/compose", evil).await.0, StatusCode::BAD_REQUEST);
    assert!(f.fake.writes().is_empty());
}

#[tokio::test]
async fn env_is_masked_below_admin() {
    let f = fixture(&[]);
    f.add_server("a", &[("MOD_API_KEY", "hunter2-secret")]);
    for path in ["/servers/mc/a", "/api/servers", "/api/v1/compose", "/compose/export", "/api/v1/servers/mc/a/compose", "/servers/mc/a/compose.yaml"] {
        for who in [&f.viewer, &f.operator] {
            let (status, body) = f.request("GET", path, Some(who), "", None).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            assert!(!body.contains("hunter2-secret"), "{path}: {body}");
        }
        let (status, body) = f.admin("GET", path, "").await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(body.contains("hunter2-secret"), "admins edit the spec, so they see it: {path}");
    }
}

#[tokio::test]
async fn login_is_csrf_protected_and_guesses_are_limited_up_front() {
    let f = fixture(&[]);
    let form = format!("username=admin&password={ADMIN_PW}");
    let (status, _) = f.request("POST", "/login", None, &form, Some("https://evil.test")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(f.state.stats.csrf_refused.load(Ordering::Relaxed), 1);

    // All at once, so no check has finished when the later ones arrive.
    let attempts = (0..25).map(|_| f.request("POST", "/login", None, "username=admin&password=wrong", None));
    let results = futures::future::join_all(attempts).await;
    let checked = results.iter().filter(|(s, _)| *s == StatusCode::UNAUTHORIZED).count();
    let refused = results.iter().filter(|(s, _)| *s == StatusCode::TOO_MANY_REQUESTS).count();
    assert_eq!((checked, refused), (10, 15));
}

#[tokio::test]
async fn role_changes_need_the_admins_password() {
    let f = fixture(&[]);
    let (status, _) = f.admin("POST", "/users/val/role", "role=admin&current=wrong").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(f.state.users.role_of("val"), Some(Role::Viewer));
    let (status, body) = f.admin("POST", "/users/val/role", &format!("role=admin&current={ADMIN_PW}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(f.state.users.role_of("val"), Some(Role::Admin));
    let (status, _) = f.admin("POST", "/users/val/role", &format!("role=root&current={ADMIN_PW}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // The login itself is confirmed before anything else, for a missing user as well.
    let (status, _) = f.admin("POST", "/users/ghost/role", &format!("role=viewer&current={ADMIN_PW}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(users::USERS_KEY == "users.json" && f.fake.secret.lock().unwrap().contains_key(users::USERS_KEY));
}

#[tokio::test]
async fn logs_are_capped_and_end_with_the_login() {
    let f = fixture(&[]);
    let id = Identity { name: "val".into(), role: Role::Viewer, session: Some(f.viewer.clone()) };
    assert!(handlers::still_logged_in(&f.state, &id));
    let (status, _) = f.request("GET", "/servers/mc/a/logs", Some(&f.viewer), "", None).await;
    assert_eq!(status, StatusCode::OK, "the stream's error event says what went wrong");
    f.state.users.end_sessions_of("val", None);
    assert!(!handlers::still_logged_in(&f.state, &id));
    let token_user = Identity { name: "gone".into(), role: Role::Viewer, session: None };
    assert!(!handlers::still_logged_in(&f.state, &token_user));

    let held = f.state.log_streams.clone().acquire_many_owned(crate::MAX_LOG_STREAMS as u32).await.unwrap();
    let (status, _) = f.request("GET", "/servers/mc/a/logs", Some(&f.operator), "", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    drop(held);
}

#[tokio::test]
async fn readiness_and_metrics_follow_the_shutdown() {
    let f = fixture(&[]);
    let (status, _) = f.request("GET", "/users", Some(&f.viewer), "", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(ops::endpoint(&f.state, "/readyz").status, 200);
    let m = ops::endpoint(&f.state, "/metrics");
    assert_eq!(m.status, 200);
    assert!(m.body.contains("cubeharbor_dashboard_forbidden_total 1"), "{}", m.body);
    assert!(m.body.contains("cubeharbor_dashboard_users 3"), "{}", m.body);
    assert!(m.body.contains("cubeharbor_dashboard_sessions 3"), "{}", m.body);
    assert!(!m.body.contains("otto"), "counts only");
    f.state.draining.store(true, Ordering::Relaxed);
    assert_eq!(ops::endpoint(&f.state, "/readyz").status, 503);
    assert_eq!(ops::endpoint(&f.state, "/healthz").status, 200, "liveness stays up while draining");
    assert_eq!(ops::endpoint(&f.state, "/nope").status, 404);
}
