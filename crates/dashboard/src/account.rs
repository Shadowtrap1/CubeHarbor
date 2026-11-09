//! Logging in and out, the caller's own account, and user management.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Extension, Form, Path};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::Cookie;
use serde::Deserialize;

use crate::error::AppError;
use crate::handlers::html;
use crate::users::{Identity, Role};
use crate::{COOKIE, St, ops, views};

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

pub async fn login_page() -> Response {
    html(views::login(None))
}

#[derive(Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    username: String,
    password: String,
}

pub async fn login(
    st: St,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    jar: CookieJar,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Response {
    let client = st.client_ip(Some(peer.ip()), &headers);
    if st.throttled(client) {
        return (StatusCode::TOO_MANY_REQUESTS, html(views::login(Some("Too many failed attempts, try again in a minute")))).into_response();
    }
    let user = f.username.trim().to_lowercase();
    if st.check_login(&user, &f.password, client).await.is_none() {
        return (StatusCode::UNAUTHORIZED, html(views::login(Some("Wrong user name or password")))).into_response();
    }
    ops::bump(&st.stats.logins);
    tracing::info!("{user} logged in from {client}");
    let token = match st.users.new_session(&user) {
        Ok(t) => t,
        Err(e) => return AppError::from(e).into_response(),
    };
    let https = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("https"));
    let secure = if st.cfg.cookie_secure || https { "; Secure" } else { "" };
    let cookie = Cookie::parse(format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}", crate::SESSION_TTL.as_secs()))
        .expect("static cookie format");
    (jar.add(cookie), Redirect::to("/")).into_response()
}

pub async fn logout(st: St, jar: CookieJar) -> Response {
    if let Some(c) = jar.get(COOKIE) {
        st.users.end_session(c.value());
    }
    (jar.remove(Cookie::build(COOKIE).path("/")), Redirect::to("/login")).into_response()
}

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

/// Shortest password the dashboard accepts when it's changed from the UI.
pub const MIN_PASSWORD_LEN: usize = 12;

pub async fn account_page(st: St, Extension(me): Extension<Identity>) -> Response {
    html(views::account(&st, &me, None, None))
}

#[derive(Deserialize)]
pub struct PasswordForm {
    current: String,
    new: String,
    confirm: String,
}

/// Is `new` acceptable as the next password?
fn check_new_password(current: &str, new: &str, confirm: &str) -> Result<(), String> {
    if new.chars().count() < MIN_PASSWORD_LEN || new.trim().is_empty() {
        return Err(format!("The new password must be at least {MIN_PASSWORD_LEN} characters."));
    }
    if new != confirm {
        return Err("The new password and its confirmation don't match.".into());
    }
    if new == current {
        return Err("The new password is the same as the current one.".into());
    }
    Ok(())
}

/// Re-check the caller's own password before changing credentials, like a login (with the
/// failed-attempt limits), so a stolen session can't be turned into a password, a token or a
/// new admin.
async fn confirm_own_password(st: &St, me: &Identity, pw: &str, peer: SocketAddr, headers: &HeaderMap) -> Result<(), (StatusCode, &'static str)> {
    let client = st.client_ip(Some(peer.ip()), headers);
    if st.throttled(client) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "Too many failed attempts, try again in a minute."));
    }
    if st.check_login(&me.name, pw, client).await.is_none() {
        return Err((StatusCode::UNAUTHORIZED, "Your current password is wrong."));
    }
    Ok(())
}

pub async fn change_password(
    st: St,
    Extension(me): Extension<Identity>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(f): Form<PasswordForm>,
) -> Response {
    let page = |status: StatusCode, msg: Result<&str, &str>| (status, html(views::account(&st, &me, Some(msg), None))).into_response();
    if !st.users.editable() {
        return page(StatusCode::BAD_REQUEST, Err("Changing passwords here is turned off (dashboard.manageUsers)."));
    }
    if let Err((code, e)) = confirm_own_password(&st, &me, &f.current, peer, &headers).await {
        return page(code, Err(e));
    }
    if let Err(e) = check_new_password(&f.current, &f.new, &f.confirm) {
        return page(StatusCode::BAD_REQUEST, Err(&e));
    }
    let saved = async {
        let hash = st.users.hash(&f.new).await?;
        st.users.update(|s| { s.get_mut(&me.name)?.password = hash; Ok(()) }).await
    };
    if let Err(e) = saved.await {
        tracing::warn!("could not change the password of {}: {e:#}", me.name);
        return page(StatusCode::BAD_GATEWAY, Err("Could not save the new password (see the dashboard logs). Nothing was changed."));
    }
    // Keeps only the caller's session. With a token there is no cookie, so every session ends.
    st.users.end_sessions_of(&me.name, me.session.as_deref());
    tracing::info!("{} changed their password; their other sessions were logged out", me.name);
    page(StatusCode::OK, Ok("Password changed. Your other sessions have been logged out."))
}

#[derive(Deserialize)]
pub struct TokenForm {
    name: String,
    current: String,
}

pub async fn create_token(
    st: St,
    Extension(me): Extension<Identity>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(f): Form<TokenForm>,
) -> Response {
    let page = |status: StatusCode, msg: Result<&str, &str>, token: Option<&str>| (status, html(views::account(&st, &me, Some(msg), token))).into_response();
    if let Err((code, e)) = confirm_own_password(&st, &me, &f.current, peer, &headers).await {
        return page(code, Err(e), None);
    }
    match st.users.create_token(&me.name, &f.name).await {
        Ok(token) => {
            tracing::info!("{} created API token {:?}", me.name, f.name.trim());
            page(StatusCode::OK, Ok("Token created. Copy it now: it is not shown again."), Some(&token))
        }
        Err(e) => page(StatusCode::BAD_REQUEST, Err(&format!("{e:#}")), None),
    }
}

pub async fn revoke_token(st: St, Extension(me): Extension<Identity>, Path(id): Path<String>) -> Response {
    let msg = st.users.revoke_token(&me.name, &id).await.map(|_| "Token revoked.").map_err(|e| format!("{e:#}"));
    let status = if msg.is_ok() { StatusCode::OK } else { StatusCode::BAD_REQUEST };
    (status, html(views::account(&st, &me, Some(msg.as_deref().map_err(String::as_str)), None))).into_response()
}

// ---------------------------------------------------------------------------
// Users (admin only, enforced by `users::required_role`)
// ---------------------------------------------------------------------------

fn users_page_with(st: &St, me: &Identity, status: StatusCode, msg: Result<&str, &str>) -> Response {
    (status, html(views::users(st, me, Some(msg)))).into_response()
}

pub async fn users_page(st: St, Extension(me): Extension<Identity>) -> Response {
    html(views::users(&st, &me, None))
}

#[derive(Deserialize)]
pub struct AddUserForm {
    name: String,
    role: String,
    password: String,
    current: String,
}

pub async fn add_user(
    st: St,
    Extension(me): Extension<Identity>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(f): Form<AddUserForm>,
) -> Response {
    if let Err((code, e)) = confirm_own_password(&st, &me, &f.current, peer, &headers).await {
        return users_page_with(&st, &me, code, Err(e));
    }
    let name = f.name.trim().to_lowercase();
    let added = async {
        let role = Role::parse(&f.role).ok_or_else(|| anyhow::anyhow!("Unknown role {}.", f.role))?;
        check_new_password("", &f.password, &f.password).map_err(anyhow::Error::msg)?;
        crate::users::check_name(&name)?;
        let hash = st.users.hash(&f.password).await?;
        st.users.update(|s| s.add(&name, role, hash)).await?;
        tracing::info!("{} added user {name} ({})", me.name, role.as_str());
        anyhow::Ok(())
    };
    match added.await {
        Ok(()) => users_page_with(&st, &me, StatusCode::OK, Ok(&format!("Added {name}. Give them their password; they can change it on their Account page."))),
        Err(e) => users_page_with(&st, &me, StatusCode::BAD_REQUEST, Err(&format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
pub struct RoleForm {
    role: String,
    current: String,
}

pub async fn set_role(
    st: St,
    Extension(me): Extension<Identity>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(f): Form<RoleForm>,
) -> Response {
    if let Err((code, e)) = confirm_own_password(&st, &me, &f.current, peer, &headers).await {
        return users_page_with(&st, &me, code, Err(e));
    }
    let changed = async {
        let role = Role::parse(&f.role).ok_or_else(|| anyhow::anyhow!("Unknown role {}.", f.role))?;
        st.users.update(|s| { s.get_mut(&name)?.role = role; Ok(()) }).await?;
        tracing::info!("{} made {name} {}", me.name, role.as_str());
        anyhow::Ok(role)
    };
    match changed.await {
        // Demoting yourself: the users page is no longer yours to see.
        Ok(_) if name == me.name && st.users.role_of(&me.name) != Some(Role::Admin) => Redirect::to("/account").into_response(),
        Ok(role) => users_page_with(&st, &me, StatusCode::OK, Ok(&format!("{name} is now {}.", role.as_str()))),
        Err(e) => users_page_with(&st, &me, StatusCode::BAD_REQUEST, Err(&format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
pub struct ResetForm {
    password: String,
    current: String,
}

/// Set someone else's password. Ends all their sessions and revokes their API tokens, since a
/// reset usually means the account can't be trusted any more.
pub async fn reset_password(
    st: St,
    Extension(me): Extension<Identity>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(f): Form<ResetForm>,
) -> Response {
    if name == me.name {
        return users_page_with(&st, &me, StatusCode::BAD_REQUEST, Err("Change your own password on the Account page."));
    }
    if let Err((code, e)) = confirm_own_password(&st, &me, &f.current, peer, &headers).await {
        return users_page_with(&st, &me, code, Err(e));
    }
    let reset = async {
        check_new_password("", &f.password, &f.password).map_err(anyhow::Error::msg)?;
        let hash = st.users.hash(&f.password).await?;
        st.users.update(|s| {
            let u = s.get_mut(&name)?;
            u.password = hash;
            u.tokens.clear();
            Ok(())
        })
        .await
    };
    match reset.await {
        Ok(()) => {
            st.users.end_sessions_of(&name, None);
            tracing::info!("{} reset the password of {name}; their sessions and tokens were revoked", me.name);
            users_page_with(&st, &me, StatusCode::OK, Ok(&format!("New password set for {name}. Their sessions and API tokens were revoked.")))
        }
        Err(e) => users_page_with(&st, &me, StatusCode::BAD_REQUEST, Err(&format!("{e:#}"))),
    }
}

pub async fn remove_user(st: St, Extension(me): Extension<Identity>, Path(name): Path<String>) -> Response {
    match st.users.update(|s| s.remove(&name)).await {
        Ok(()) => {
            st.users.end_sessions_of(&name, None);
            tracing::info!("{} removed user {name}", me.name);
            if name == me.name {
                return Redirect::to("/login").into_response();
            }
            users_page_with(&st, &me, StatusCode::OK, Ok(&format!("Removed {name}.")))
        }
        Err(e) => users_page_with(&st, &me, StatusCode::BAD_REQUEST, Err(&format!("{e:#}"))),
    }
}

#[cfg(test)]
mod account_tests {
    use super::*;

    #[test]
    fn new_password_rules() {
        assert!(check_new_password("old-password-1", "correct horse battery", "correct horse battery").is_ok());
        assert!(check_new_password("old", "short", "short").is_err());
        assert!(check_new_password("old", "            ", "            ").is_err(), "whitespace only");
        assert!(check_new_password("old", "correct horse battery", "correct horse batterY").is_err());
        assert!(check_new_password("same-password-1", "same-password-1", "same-password-1").is_err());
        // Length counts characters, not bytes.
        assert!(check_new_password("old", "ééééééééééé", "ééééééééééé").is_err());
    }
}
