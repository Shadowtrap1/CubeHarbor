//! The whole dashboard router against a fake Kubernetes API: an upgrade from a single-password
//! install, logins, user management, and the role check on every route.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use clap::Parser;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use tower::ServiceExt;

use super::*;

const NS: &str = "mc";
const USERS_SECRET: &str = "dash-users";
const OLD_PASSWORD: &str = "the-old-shared-password";

type SecretData = Arc<Mutex<BTreeMap<String, ByteString>>>;

/// A kube client whose API server knows one Secret (`USERS_SECRET`, get and merge patch) and an
/// empty list of MinecraftServers. Everything else is 404.
fn fake_kube(data: SecretData) -> Client {
    let svc = tower::service_fn(move |req: Request<kube::client::Body>| {
        let data = data.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = axum::body::to_bytes(Body::new(body), usize::MAX).await.unwrap();
            let path = parts.uri.path().to_string();
            let secret_path = format!("/api/v1/namespaces/{NS}/secrets/{USERS_SECRET}");
            let json = |code: u16, v: serde_json::Value| {
                axum::http::Response::builder().status(code).header("content-type", "application/json").body(Body::from(v.to_string())).unwrap()
            };
            let secret = |d: &BTreeMap<String, ByteString>| {
                let mut s = Secret::default();
                s.metadata.name = Some(USERS_SECRET.into());
                s.metadata.namespace = Some(NS.into());
                s.data = (!d.is_empty()).then(|| d.clone());
                let mut v = serde_json::to_value(s).unwrap();
                v["apiVersion"] = "v1".into();
                v["kind"] = "Secret".into();
                v
            };
            let resp = if path == secret_path && parts.method == "GET" {
                json(200, secret(&data.lock().unwrap()))
            } else if path == secret_path && parts.method == "PATCH" {
                let patch: Secret = serde_json::from_slice(&body).unwrap();
                let mut d = data.lock().unwrap();
                d.extend(patch.data.unwrap_or_default());
                json(200, secret(&d))
            } else if path.ends_with("/minecraftservers") && parts.method == "GET" {
                json(200, serde_json::json!({ "apiVersion": "cubeharbor.dev/v1alpha1", "kind": "MinecraftServerList", "metadata": {}, "items": [] }))
            } else {
                json(404, serde_json::json!({ "kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "NotFound", "code": 404, "message": "not found" }))
            };
            Ok::<_, std::convert::Infallible>(resp)
        }
    });
    Client::new(svc, NS)
}

async fn load(data: &SecretData, seed: &str) -> anyhow::Result<Users> {
    let backend = Backend { secrets: kube::Api::namespaced(fake_kube(data.clone()), NS), name: USERS_SECRET.into() };
    Users::load(Some(backend), Some(seed)).await
}

async fn app(data: &SecretData) -> Router {
    let cfg = Config::try_parse_from(["dashboard", "--default-namespace", NS, "--login-max-failures-total", "0"]).unwrap();
    let users = load(data, OLD_PASSWORD).await.unwrap();
    router(Arc::new(new_state(&cfg, fake_kube(data.clone()), users).unwrap()))
}

/// Send a request as a browser on the dashboard's own origin (or with a Bearer token).
async fn send(app: &Router, method: &str, path: &str, auth: Option<&str>, form: &str) -> (StatusCode, String, Option<String>) {
    let mut b = Request::builder().method(method).uri(path).header(header::HOST, "dash.test");
    match auth {
        Some(t) if t.starts_with("Bearer ") => b = b.header(header::AUTHORIZATION, t),
        Some(cookie) => b = b.header(header::COOKIE, format!("{COOKIE}={cookie}")),
        None => {}
    }
    if method == "POST" {
        b = b.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    }
    let mut req = b.body(Body::from(form.to_string())).unwrap();
    req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([192, 0, 2, 1], 1234))));
    let resp: Response = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let cookie = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| c.strip_prefix(&format!("{COOKIE}=")).map(|c| c.split(';').next().unwrap_or_default().to_string()))
        .filter(|c| !c.is_empty());
    // Streams (logs) never end: only read bodies that aren't event streams.
    let sse = resp.headers().get(header::CONTENT_TYPE).is_some_and(|v| v.to_str().unwrap_or_default().starts_with("text/event-stream"));
    let body = if sse { String::new() } else { String::from_utf8_lossy(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).to_string() };
    (status, body, cookie)
}

async fn login(app: &Router, user: &str, pw: &str) -> Option<String> {
    let (status, _, cookie) = send(app, "POST", "/login", None, &format!("username={user}&password={pw}")).await;
    if status == StatusCode::SEE_OTHER { cookie } else { None }
}

fn stored(data: &SecretData) -> users::Store {
    users::Store::parse(&data.lock().unwrap()[users::USERS_KEY].0).unwrap()
}

/// Every route, with the least role it needs (`/api/modpacks/search` is left out: it would call
/// Modrinth; `users::tests::roles_per_route` covers it).
const ROUTES: &[(&str, &str, Role)] = &[
    ("GET", "/", Role::Viewer),
    ("GET", "/fragments/servers", Role::Viewer),
    ("GET", "/account", Role::Viewer),
    ("GET", "/servers/mc/a", Role::Viewer),
    ("GET", "/servers/mc/a/logs", Role::Viewer),
    ("GET", "/servers/mc/a/compose.yaml", Role::Viewer),
    ("GET", "/compose/export", Role::Viewer),
    ("GET", "/api/servers", Role::Viewer),
    ("GET", "/api/v1/servers?remotes=false", Role::Viewer),
    ("GET", "/api/v1/servers/mc/a/compose", Role::Viewer),
    ("GET", "/api/v1/compose", Role::Viewer),
    ("POST", "/account/tokens/nope/revoke", Role::Viewer),
    ("POST", "/servers/mc/a/action/start", Role::Operator),
    ("POST", "/servers/mc/a/action/stop", Role::Operator),
    ("POST", "/servers/mc/a/action/restart", Role::Operator),
    ("POST", "/servers/mc/a/console", Role::Operator),
    ("GET", "/new", Role::Admin),
    ("POST", "/new/quick", Role::Admin),
    ("POST", "/compose/preview", Role::Admin),
    ("POST", "/compose/apply", Role::Admin),
    ("POST", "/servers/mc/a/action/delete", Role::Admin),
    ("POST", "/servers/mc/a/action/delete-data", Role::Admin),
    ("POST", "/servers/mc/a/spec", Role::Admin),
    ("POST", "/api/compose", Role::Admin),
    ("POST", "/api/v1/compose", Role::Admin),
    ("GET", "/users", Role::Admin),
    ("POST", "/users", Role::Admin),
    ("POST", "/users/nobody/role", Role::Admin),
    ("POST", "/users/nobody/password", Role::Admin),
    ("POST", "/users/nobody/delete", Role::Admin),
];

fn refused_for_role(status: StatusCode, body: &str) -> bool {
    status == StatusCode::FORBIDDEN && body.contains("role")
}

#[tokio::test]
async fn upgrade_roles_and_user_management() {
    // An install from before users: the users Secret exists (the chart creates it) but is empty,
    // and DASHBOARD_PASSWORD still holds the old shared password.
    let data: SecretData = Arc::default();
    let app = app(&data).await;

    // The old password became user `admin`, stored only as an argon2id hash.
    let s = stored(&data);
    assert_eq!(s.users.len(), 1);
    assert_eq!((s.users[0].name.as_str(), s.users[0].role), ("admin", Role::Admin));
    assert!(s.users[0].password.starts_with("$argon2id$"));
    assert!(!String::from_utf8_lossy(&data.lock().unwrap()[users::USERS_KEY].0).contains(OLD_PASSWORD));

    // Old scripts sending the password as a Bearer token get a clear 401.
    let (status, body, _) = send(&app, "GET", "/api/v1/servers", Some(&format!("Bearer {OLD_PASSWORD}")), "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("no longer accepted"), "{body}");
    assert!(login(&app, "admin", "wrong password!").await.is_none());
    assert!(login(&app, "nobody", OLD_PASSWORD).await.is_none());
    let (status, _, _) = send(&app, "GET", "/", None, "").await;
    assert_eq!(status, StatusCode::SEE_OTHER, "not logged in: to /login");

    let admin = login(&app, "admin", OLD_PASSWORD).await.expect("old password logs in as admin");

    // Adding users needs the admin's own password.
    let (status, _, _) = send(&app, "POST", "/users", Some(&admin), "name=otto&role=operator&password=operator-pass-1&current=wrong").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for (name, role) in [("otto", "operator"), ("val", "viewer")] {
        let form = format!("name={name}&role={role}&password={name}-password-1&current={OLD_PASSWORD}");
        let (status, body, _) = send(&app, "POST", "/users", Some(&admin), &form).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, _, _) = send(&app, "POST", "/users", Some(&admin), &format!("name=short&role=viewer&password=short&current={OLD_PASSWORD}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "password too short");
    let operator = login(&app, "otto", "otto-password-1").await.unwrap();
    let viewer = login(&app, "val", "val-password-1").await.unwrap();

    // A viewer's API token.
    let (status, body, _) = send(&app, "POST", "/account/tokens", Some(&viewer), "name=status+page&current=val-password-1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = body.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).find(|w| w.starts_with("chb_")).unwrap().to_string();
    let secret_part = token.rsplit('_').next().unwrap();
    assert!(!String::from_utf8_lossy(&data.lock().unwrap()[users::USERS_KEY].0).contains(secret_part), "only the hash is stored");
    let viewer_token = format!("Bearer {token}");

    // Every route, as every role. Allowed requests may still fail later (the fake API has no
    // servers), but never with the role check.
    let callers = [(Role::Viewer, &viewer), (Role::Viewer, &viewer_token), (Role::Operator, &operator), (Role::Admin, &admin)];
    for (method, path, needed) in ROUTES {
        for (role, auth) in &callers {
            let (status, body, _) = send(&app, method, path, Some(auth), "").await;
            assert_eq!(refused_for_role(status, &body), role < needed, "{method} {path} as {role:?} ({}): {status} {body}", &auth[..8]);
        }
        let (status, _, _) = send(&app, method, path, None, "").await;
        assert!(matches!(status, StatusCode::UNAUTHORIZED | StatusCode::SEE_OTHER), "{method} {path} without login: {status}");
    }
    // Pages leave out what the role can't use.
    let (_, page, _) = send(&app, "GET", "/", Some(&viewer), "").await;
    assert!(!page.contains(r#"href="/new""#) && !page.contains(r#"href="/users""#));
    let (_, page, _) = send(&app, "GET", "/", Some(&admin), "").await;
    assert!(page.contains(r#"href="/new""#) && page.contains(r#"href="/users""#));

    // Cross-site form posts are refused for cookie sessions.
    let mut req = Request::builder().method("POST").uri("/servers/mc/a/action/stop").header(header::HOST, "dash.test")
        .header(header::ORIGIN, "https://evil.test").header(header::COOKIE, format!("{COOKIE}={operator}")).body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([192, 0, 2, 1], 1))));
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Role changes apply to existing sessions.
    let (status, _, _) = send(&app, "POST", "/users/otto/role", Some(&admin), "role=viewer").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body, _) = send(&app, "POST", "/servers/mc/a/action/stop", Some(&operator), "").await;
    assert!(refused_for_role(status, &body));

    // The last admin can't be removed or demoted.
    let (status, body, _) = send(&app, "POST", "/users/admin/role", Some(&admin), "role=viewer").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _, _) = send(&app, "POST", "/users/admin/delete", Some(&admin), "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(stored(&data).get("admin").unwrap().role, Role::Admin);

    // Changing your own password logs out your other sessions, not other people's.
    let admin2 = login(&app, "admin", OLD_PASSWORD).await.unwrap();
    let form = format!("current={OLD_PASSWORD}&new=brand-new-admin-pw&confirm=brand-new-admin-pw");
    let (status, body, _) = send(&app, "POST", "/account/password", Some(&admin), &form).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(send(&app, "GET", "/account", Some(&admin), "").await.0, StatusCode::OK);
    assert_eq!(send(&app, "GET", "/account", Some(&admin2), "").await.0, StatusCode::SEE_OTHER);
    assert_eq!(send(&app, "GET", "/account", Some(&viewer), "").await.0, StatusCode::OK);

    // An admin reset logs the user out and revokes their tokens.
    let form = "password=reset-by-admin-1&current=brand-new-admin-pw";
    let (status, body, _) = send(&app, "POST", "/users/val/password", Some(&admin), form).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(send(&app, "GET", "/account", Some(&viewer), "").await.0, StatusCode::SEE_OTHER);
    assert_eq!(send(&app, "GET", "/api/v1/servers", Some(&viewer_token), "").await.0, StatusCode::UNAUTHORIZED);

    // A restart keeps everyone (DASHBOARD_PASSWORD no longer matters), and the old password is gone.
    let again = load(&data, "some other seed password").await.unwrap();
    assert_eq!(again.verify("admin", "brand-new-admin-pw").await, Some(Role::Admin));
    assert_eq!(again.verify("admin", OLD_PASSWORD).await, None);
    assert_eq!(again.verify("val", "reset-by-admin-1").await, Some(Role::Viewer));
    assert_eq!(again.verify("otto", "otto-password-1").await, Some(Role::Viewer));
}

#[tokio::test]
async fn recovery_and_bad_stores() {
    // Removing users.json recreates `admin` from DASHBOARD_PASSWORD.
    let data: SecretData = Arc::default();
    load(&data, "first-password-1").await.unwrap();
    data.lock().unwrap().remove(users::USERS_KEY);
    let users = load(&data, "recovery-password").await.unwrap();
    assert_eq!(users.verify("admin", "recovery-password").await, Some(Role::Admin));
    // No users and no password: refuse to start rather than run without a login.
    let empty: SecretData = Arc::default();
    assert!(load(&empty, "").await.is_err());
    // A broken store is an error, not a silent reset to the seed password.
    data.lock().unwrap().insert(users::USERS_KEY.into(), ByteString(b"{not json".to_vec()));
    assert!(load(&data, "recovery-password").await.is_err());
}
