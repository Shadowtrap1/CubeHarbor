//! Dashboard users: named logins with a role, an argon2id password and API tokens.
//!
//! Users live in one JSON document (`users.json`) in a Secret the chart creates empty
//! (`USERS_SECRET`), so they survive restarts. On first start the store is empty and a user
//! `admin` is created with `DASHBOARD_PASSWORD`; after that, that password is only used to
//! recover (remove `users.json` and restart). Without `USERS_SECRET` there is just that one admin
//! and nothing can be changed from the dashboard.
//!
//! Sessions are random tokens kept in memory and mapped to a user name, so a role change takes
//! effect on the next request and removing a user ends their sessions and tokens.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::http::Method;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, Patch, PatchParams};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{SESSION_TTL, constant_eq, random_bytes, random_hex};

/// Key of the user store in `USERS_SECRET`.
pub const USERS_KEY: &str = "users.json";
/// Name of the user created from `DASHBOARD_PASSWORD` when the store is empty.
pub const FIRST_ADMIN: &str = "admin";
/// API tokens look like `chb_<id>_<secret>`; the id finds the token, the secret is checked.
const TOKEN_PREFIX: &str = "chb_";
/// Password hashes computed at once (each argon2id hash takes ~19 MiB and tens of ms).
const MAX_CONCURRENT_HASHES: usize = 2;
const MAX_TOKENS_PER_USER: usize = 20;

/// What a user may do. Ordered: each role can do everything the ones before it can.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Read-only: server list and pages, logs, compose export, their own account.
    Viewer,
    /// Also start, stop and restart servers and use the console.
    Operator,
    /// Everything: create, edit and delete servers, manage users.
    Admin,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Viewer, Role::Operator, Role::Admin];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.as_str() == s)
    }
}

/// The least role a request needs. Every route goes through this (in the `auth` middleware), so
/// hiding a button is never the only check. Anything not listed that changes state needs admin.
pub fn required_role(method: &Method, path: &str) -> Role {
    let read = matches!(*method, Method::GET | Method::HEAD);
    let seg: Vec<&str> = path.strip_prefix('/').unwrap_or(path).split('/').collect();
    match (read, seg.as_slice()) {
        (_, ["users", ..]) => Role::Admin,
        // Creating servers, including the forms and previews that lead to it.
        (_, ["new", ..]) | (_, ["compose", "preview" | "apply"]) => Role::Admin,
        (false, ["logout"]) | (false, ["account", ..]) => Role::Viewer,
        (false, ["servers", _, _, "action", "start" | "stop" | "restart"]) => Role::Operator,
        (false, ["servers", _, _, "console"]) => Role::Operator,
        (false, _) => Role::Admin,
        (true, _) => Role::Viewer,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Token {
    pub id: String,
    pub name: String,
    /// SHA-256 of the secret part, hex. The secret is 256 random bits, so a slow hash adds nothing.
    pub hash: String,
    /// Unix seconds.
    pub created: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct User {
    pub name: String,
    pub role: Role,
    /// argon2id PHC string (includes the salt and parameters).
    pub password: String,
    #[serde(default)]
    pub tokens: Vec<Token>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Store {
    pub users: Vec<User>,
}

impl Store {
    pub fn parse(json: &[u8]) -> anyhow::Result<Store> {
        let s: Store = serde_json::from_slice(json)?;
        s.validate()?;
        Ok(s)
    }

    pub fn to_json(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    pub fn get(&self, name: &str) -> Option<&User> {
        self.users.iter().find(|u| u.name == name)
    }

    pub fn get_mut(&mut self, name: &str) -> anyhow::Result<&mut User> {
        self.users.iter_mut().find(|u| u.name == name).ok_or_else(|| anyhow::anyhow!("There is no user {name}."))
    }

    /// Rules every saved store keeps: valid unique names and at least one admin.
    pub fn validate(&self) -> anyhow::Result<()> {
        for (i, u) in self.users.iter().enumerate() {
            check_name(&u.name)?;
            anyhow::ensure!(!self.users[..i].iter().any(|o| o.name == u.name), "user {} exists twice", u.name);
        }
        anyhow::ensure!(self.users.iter().any(|u| u.role == Role::Admin), "There must always be at least one admin.");
        Ok(())
    }

    pub fn add(&mut self, name: &str, role: Role, password_hash: String) -> anyhow::Result<()> {
        check_name(name)?;
        anyhow::ensure!(self.get(name).is_none(), "A user named {name} already exists.");
        self.users.push(User { name: name.into(), role, password: password_hash, tokens: vec![] });
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> anyhow::Result<()> {
        let before = self.users.len();
        self.users.retain(|u| u.name != name);
        anyhow::ensure!(self.users.len() < before, "There is no user {name}.");
        Ok(())
    }

    /// Find the user and token for a presented `chb_<id>_<secret>` token.
    fn token_user(&self, presented: &str) -> Option<&User> {
        let (id, secret) = presented.strip_prefix(TOKEN_PREFIX)?.split_once('_')?;
        let digest = hex(&Sha256::digest(secret.as_bytes()));
        self.users.iter().find(|u| u.tokens.iter().any(|t| t.id == id && constant_eq(t.hash.as_bytes(), digest.as_bytes())))
    }
}

/// User names: 1-32 of `a-z 0-9 . _ -`, starting with a letter or digit.
pub fn check_name(name: &str) -> anyhow::Result<()> {
    let ok = (1..=32).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    anyhow::ensure!(ok, "User names are 1-32 lowercase letters, digits, '.', '_' or '-', starting with a letter or digit.");
    Ok(())
}

/// Salted argon2id hash (default parameters: 19 MiB, 2 passes) as a PHC string.
pub fn hash_password(pw: &str) -> anyhow::Result<String> {
    let salt = SaltString::encode_b64(&random_bytes(16)?).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
    Ok(Argon2::default().hash_password(pw.as_bytes(), &salt).map_err(|e| anyhow::anyhow!("hashing password: {e}"))?.to_string())
}

pub fn verify_password(phc: &str, pw: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Who made a request, and how.
#[derive(Clone, Debug)]
pub struct Identity {
    pub name: String,
    pub role: Role,
    /// The session cookie, for browser logins (None for API tokens).
    pub session: Option<String>,
}

impl Identity {
    pub fn can(&self, role: Role) -> bool {
        self.role >= role
    }
}

struct Session {
    user: String,
    expires: Instant,
}

/// Where the store is saved: a Secret, or nowhere (single admin from `DASHBOARD_PASSWORD`).
pub struct Backend {
    pub secrets: Api<Secret>,
    pub name: String,
}

pub struct Users {
    store: Mutex<Store>,
    /// Serializes changes, so two edits can't save over each other.
    write: tokio::sync::Mutex<()>,
    sessions: Mutex<HashMap<String, Session>>,
    backend: Option<Backend>,
    hashing: tokio::sync::Semaphore,
    /// Checked for unknown user names, so they take as long as wrong passwords.
    dummy_hash: String,
}

impl Users {
    pub fn new(store: Store, backend: Option<Backend>) -> anyhow::Result<Users> {
        Ok(Users {
            store: Mutex::new(store),
            write: tokio::sync::Mutex::new(()),
            sessions: Mutex::new(HashMap::new()),
            backend,
            hashing: tokio::sync::Semaphore::new(MAX_CONCURRENT_HASHES),
            dummy_hash: hash_password(&random_hex(16)?)?,
        })
    }

    /// Load the store from the Secret, creating the first admin from `seed` when it's empty.
    pub async fn load(backend: Option<Backend>, seed: Option<&str>) -> anyhow::Result<Users> {
        let seed = seed.filter(|s| !s.trim().is_empty());
        let first_admin = |seed: Option<&str>| -> anyhow::Result<Store> {
            let pw = seed.ok_or_else(|| anyhow::anyhow!("DASHBOARD_PASSWORD is empty and there are no users yet: refusing to start with no login"))?;
            if pw.chars().count() < 12 {
                tracing::warn!("DASHBOARD_PASSWORD is shorter than 12 characters; change the admin password in the dashboard");
            }
            let mut s = Store::default();
            s.add(FIRST_ADMIN, Role::Admin, hash_password(pw)?)?;
            Ok(s)
        };
        let Some(b) = backend else {
            tracing::info!("USERS_SECRET not set: one user ({FIRST_ADMIN}) with DASHBOARD_PASSWORD; users can't be changed");
            return Users::new(first_admin(seed)?, None);
        };
        let secret = b.secrets.get_opt(&b.name).await?.ok_or_else(|| anyhow::anyhow!("users Secret {} not found", b.name))?;
        let stored = secret.data.as_ref().and_then(|d| d.get(USERS_KEY)).filter(|v| !v.0.is_empty());
        match stored {
            Some(v) => {
                let s = Store::parse(&v.0).map_err(|e| anyhow::anyhow!("{USERS_KEY} in Secret {} is invalid: {e:#}", b.name))?;
                tracing::info!("loaded {} dashboard users from Secret {}", s.users.len(), b.name);
                Users::new(s, Some(b))
            }
            None => {
                let s = first_admin(seed)?;
                let users = Users::new(Store::default(), Some(b))?;
                users.save(&s).await?;
                *users.store.lock().unwrap() = s;
                tracing::info!("no dashboard users yet: created {FIRST_ADMIN} with DASHBOARD_PASSWORD");
                Ok(users)
            }
        }
    }

    /// Can users be added and passwords changed (is there somewhere to save them)?
    pub fn editable(&self) -> bool {
        self.backend.is_some()
    }

    pub fn snapshot(&self) -> Store {
        self.store.lock().unwrap().clone()
    }

    pub fn role_of(&self, name: &str) -> Option<Role> {
        self.store.lock().unwrap().get(name).map(|u| u.role)
    }

    /// Hash off the async runtime, a few at a time (each hash takes ~19 MiB).
    pub async fn hash(&self, pw: &str) -> anyhow::Result<String> {
        let _permit = self.hashing.acquire().await?;
        let pw = pw.to_string();
        tokio::task::spawn_blocking(move || hash_password(&pw)).await?
    }

    /// Check a user's password. Unknown users cost the same as a wrong password.
    pub async fn verify(&self, name: &str, pw: &str) -> Option<Role> {
        let found = self.store.lock().unwrap().get(name).map(|u| (u.password.clone(), u.role));
        let phc = found.as_ref().map_or(self.dummy_hash.clone(), |(p, _)| p.clone());
        let _permit = self.hashing.acquire().await.ok()?;
        let pw = pw.to_string();
        let ok = tokio::task::spawn_blocking(move || verify_password(&phc, &pw)).await.unwrap_or(false);
        // Look the role up again: it may have changed while hashing.
        if ok && found.is_some() { self.role_of(name) } else { None }
    }

    /// The user an API token belongs to.
    pub fn check_token(&self, presented: &str) -> Option<(String, Role)> {
        self.store.lock().unwrap().token_user(presented).map(|u| (u.name.clone(), u.role))
    }

    /// Apply `f` to a copy of the store, save it, then use it. Nothing changes if `f` or saving fails.
    pub async fn update<T>(&self, f: impl FnOnce(&mut Store) -> anyhow::Result<T>) -> anyhow::Result<T> {
        anyhow::ensure!(self.editable(), "Users can't be changed here: the dashboard has nowhere to save them (USERS_SECRET).");
        let _w = self.write.lock().await;
        let mut next = self.snapshot();
        let out = f(&mut next)?;
        next.validate()?;
        self.save(&next).await?;
        *self.store.lock().unwrap() = next;
        Ok(out)
    }

    async fn save(&self, s: &Store) -> anyhow::Result<()> {
        let Some(b) = &self.backend else { return Ok(()) };
        let patch = serde_json::json!({ "data": { USERS_KEY: k8s_openapi::ByteString(s.to_json()?) } });
        b.secrets
            .patch(&b.name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map_err(|e| anyhow::anyhow!("could not save users to Secret {}: {e}", b.name))?;
        Ok(())
    }

    /// Create an API token for `user`. Returns the token, which is shown once and never stored.
    pub async fn create_token(&self, user: &str, label: &str) -> anyhow::Result<String> {
        let label = label.trim();
        anyhow::ensure!((1..=64).contains(&label.chars().count()), "Give the token a name (up to 64 characters).");
        let id = random_hex(8)?;
        let secret = random_hex(32)?;
        let hash = hex(&Sha256::digest(secret.as_bytes()));
        self.update(|s| {
            let u = s.get_mut(user)?;
            anyhow::ensure!(u.tokens.len() < MAX_TOKENS_PER_USER, "You already have {MAX_TOKENS_PER_USER} tokens; revoke one first.");
            u.tokens.push(Token { id: id.clone(), name: label.into(), hash, created: now() });
            Ok(())
        })
        .await?;
        Ok(format!("{TOKEN_PREFIX}{id}_{secret}"))
    }

    pub async fn revoke_token(&self, user: &str, id: &str) -> anyhow::Result<()> {
        self.update(|s| {
            let u = s.get_mut(user)?;
            let before = u.tokens.len();
            u.tokens.retain(|t| t.id != id);
            anyhow::ensure!(u.tokens.len() < before, "No such token.");
            Ok(())
        })
        .await
    }

    /// Start a session for `user`: a random token known only to this process.
    pub fn new_session(&self, user: &str) -> anyhow::Result<String> {
        let token = random_hex(32)?;
        let mut s = self.sessions.lock().unwrap();
        s.retain(|_, v| v.expires > Instant::now());
        s.insert(token.clone(), Session { user: user.into(), expires: Instant::now() + SESSION_TTL });
        Ok(token)
    }

    /// The user and current role of a session (None once it expired or the user was removed).
    pub fn session(&self, token: &str) -> Option<Identity> {
        let user = self.sessions.lock().unwrap().get(token).filter(|v| v.expires > Instant::now()).map(|v| v.user.clone())?;
        let role = self.role_of(&user)?;
        Some(Identity { name: user, role, session: Some(token.into()) })
    }

    pub fn end_session(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }

    /// Log `user` out everywhere except the session `keep`.
    pub fn end_sessions_of(&self, user: &str, keep: Option<&str>) {
        self.sessions.lock().unwrap().retain(|t, v| v.user != user || Some(t.as_str()) == keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        let mut s = Store::default();
        s.add("admin", Role::Admin, hash_password("admin password 1").unwrap()).unwrap();
        s.add("ops", Role::Operator, hash_password("ops password 12").unwrap()).unwrap();
        s.add("guest", Role::Viewer, hash_password("guest password 1").unwrap()).unwrap();
        s
    }

    #[test]
    fn passwords_are_salted_argon2id() {
        let a = hash_password("correct horse battery").unwrap();
        let b = hash_password("correct horse battery").unwrap();
        assert!(a.starts_with("$argon2id$"), "{a}");
        assert_ne!(a, b, "a random salt per hash");
        assert!(verify_password(&a, "correct horse battery"));
        assert!(!verify_password(&a, "correct horse batterY"));
        assert!(!verify_password("not a hash", "x"));
    }

    #[test]
    fn store_round_trips() {
        let mut s = store();
        s.get_mut("ops").unwrap().tokens.push(Token { id: "ab12".into(), name: "ci".into(), hash: "00".into(), created: 1 });
        let back = Store::parse(&s.to_json().unwrap()).unwrap();
        assert_eq!(back, s);
        assert!(verify_password(&back.get("guest").unwrap().password, "guest password 1"));
        // Older stores without tokens still load.
        let old = br#"{"users":[{"name":"admin","role":"admin","password":"$argon2id$x"}]}"#;
        assert!(Store::parse(old).unwrap().users[0].tokens.is_empty());
    }

    #[test]
    fn store_rules() {
        let mut s = store();
        assert!(s.add("admin", Role::Viewer, "h".into()).is_err(), "duplicate");
        assert!(s.add("Bob", Role::Viewer, "h".into()).is_err(), "upper case");
        assert!(s.add("-bob", Role::Viewer, "h".into()).is_err());
        assert!(s.add(&"b".repeat(33), Role::Viewer, "h".into()).is_err());
        assert!(s.add("bob.smith_2-x", Role::Viewer, "h".into()).is_ok());
        assert!(s.remove("nobody").is_err());
        // The last admin can't be removed or demoted.
        let mut t = s.clone();
        t.remove("admin").unwrap();
        assert!(t.validate().is_err());
        let mut t = s.clone();
        t.get_mut("admin").unwrap().role = Role::Operator;
        assert!(t.validate().is_err());
        // With a second admin it's fine.
        s.get_mut("ops").unwrap().role = Role::Admin;
        s.remove("admin").unwrap();
        assert!(s.validate().is_ok());
        assert!(Store::parse(br#"{"users":[]}"#).is_err(), "a store without admins is refused");
    }

    #[test]
    fn tokens_match_only_their_secret() {
        let mut s = store();
        let secret = "s3cr3t";
        s.get_mut("ops").unwrap().tokens.push(Token { id: "id1".into(), name: "ci".into(), hash: hex(&Sha256::digest(secret)), created: 0 });
        assert_eq!(s.token_user("chb_id1_s3cr3t").map(|u| u.name.as_str()), Some("ops"));
        assert!(s.token_user("chb_id1_wrong").is_none());
        assert!(s.token_user("chb_id2_s3cr3t").is_none());
        assert!(s.token_user("id1_s3cr3t").is_none());
        assert!(s.token_user("ops password 12").is_none(), "passwords aren't tokens");
    }

    #[test]
    fn roles_per_route() {
        use Role::*;
        let get = |p| required_role(&Method::GET, p);
        let post = |p| required_role(&Method::POST, p);
        // Reading.
        for p in ["/", "/fragments/servers", "/servers/mc/a", "/servers/mc/a/logs", "/servers/mc/a/compose.yaml", "/compose/export",
            "/account", "/api/servers", "/api/v1/servers", "/api/v1/compose", "/api/v1/servers/mc/a/compose", "/api/modpacks/search"] {
            assert_eq!(get(p), Viewer, "{p}");
        }
        assert_eq!(post("/logout"), Viewer);
        assert_eq!(post("/account/password"), Viewer);
        assert_eq!(post("/account/tokens"), Viewer);
        assert_eq!(post("/account/tokens/ab/revoke"), Viewer);
        // Running servers.
        for a in ["start", "stop", "restart"] {
            assert_eq!(post(&format!("/servers/mc/a/action/{a}")), Operator);
        }
        assert_eq!(post("/servers/mc/a/console"), Operator);
        // Changing what runs, and users.
        for p in ["/servers/mc/a/action/delete", "/servers/mc/a/action/delete-data", "/servers/mc/a/action/other", "/servers/mc/a/spec",
            "/new/quick", "/compose/preview", "/compose/apply", "/api/compose", "/api/v1/compose", "/users", "/users/bob/role",
            "/users/bob/password", "/users/bob/delete", "/anything-new"] {
            assert_eq!(post(p), Admin, "{p}");
        }
        assert_eq!(get("/users"), Admin);
        assert_eq!(get("/new"), Admin);
        assert_eq!(required_role(&Method::DELETE, "/servers/mc/a"), Admin);
        assert_eq!(required_role(&Method::PUT, "/account/password"), Viewer, "own account, any method");
        assert!(Admin > Operator && Operator > Viewer);
    }

    #[tokio::test]
    async fn sessions_follow_the_user() {
        let users = Users::new(store(), None).unwrap();
        assert_eq!(users.verify("ops", "ops password 12").await, Some(Role::Operator));
        assert_eq!(users.verify("ops", "wrong").await, None);
        assert_eq!(users.verify("nobody", "ops password 12").await, None);
        let a = users.new_session("ops").unwrap();
        let b = users.new_session("ops").unwrap();
        let g = users.new_session("guest").unwrap();
        assert_eq!(users.session(&a).unwrap().role, Role::Operator);
        // A role change applies to existing sessions.
        users.store.lock().unwrap().get_mut("ops").unwrap().role = Role::Viewer;
        assert_eq!(users.session(&a).unwrap().role, Role::Viewer);
        // Changing a password keeps only the caller's session, and only that user's.
        users.end_sessions_of("ops", Some(&a));
        assert!(users.session(&a).is_some());
        assert!(users.session(&b).is_none());
        assert!(users.session(&g).is_some());
        // Removed users are logged out.
        users.store.lock().unwrap().remove("ops").unwrap();
        assert!(users.session(&a).is_none());
        // Without a Secret nothing can be changed.
        assert!(users.update(|_| Ok(())).await.is_err());
    }
}
