//! How failures reach the client: a status and a message without internals; the details go to
//! the log.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde_json::json;

use crate::views;

pub struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

/// An error whose message is meant for the user, with the status to answer it with.
#[derive(Debug)]
struct Public(StatusCode, String);

impl std::fmt::Display for Public {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

impl std::error::Error for Public {}

pub fn public(status: StatusCode, msg: impl Into<String>) -> anyhow::Error {
    Public(status, msg.into()).into()
}

const KUBE_FAILED: &str = "The Kubernetes API request failed (see the dashboard logs).";

/// What to tell the client about `e`: the status and a message without internals. Kubernetes
/// errors name the service account, the API server and the RBAC rules involved, so only the
/// ones about the request itself are passed on. The full error goes to the log.
pub fn classify(e: &anyhow::Error) -> (StatusCode, String) {
    if let Some(Public(status, msg)) = e.downcast_ref::<Public>() {
        return (*status, msg.clone());
    }
    for cause in e.chain() {
        match cause.downcast_ref::<kube::Error>() {
            Some(kube::Error::Api(s)) => {
                return match s.code {
                    404 => (StatusCode::NOT_FOUND, "Not found.".into()),
                    409 => (StatusCode::CONFLICT, "It was changed by someone else in the meantime. Reload and try again.".into()),
                    // Validation of what the user submitted.
                    400 | 422 => (StatusCode::BAD_REQUEST, s.message.clone()),
                    _ => (StatusCode::BAD_GATEWAY, KUBE_FAILED.into()),
                };
            }
            Some(_) => return (StatusCode::BAD_GATEWAY, KUBE_FAILED.into()),
            None => {}
        }
    }
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal error (see the dashboard logs).".into())
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::warn!("request failed: {:#}", self.0);
        let (status, msg) = classify(&self.0);
        (status, Html(views::error_page(&msg).into_string())).into_response()
    }
}

/// The JSON API's version of [`AppError`].
pub fn api_failure(e: anyhow::Error) -> Response {
    tracing::warn!("request failed: {e:#}");
    let (status, msg) = classify(&e);
    (status, Json(json!({ "error": msg }))).into_response()
}
