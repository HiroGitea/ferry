//! REST API (`/api/v1`), SSE events, dashboard static files and MCP mount point.

pub mod admin;
pub mod info;
pub mod jobs;
pub mod metrics;
pub mod nodes;
pub mod notify;
pub mod sse;
pub mod tasks;
pub mod web;

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::auth::require_auth;
use crate::state::AppState;

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    Forbidden(String),
    NotFound(String),
    BadRequest(String),
    Conflict(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Internal(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "missing or invalid token".to_string(),
            ),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m),
            ApiError::Internal(e) => {
                tracing::error!("internal error: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
            }
        };
        (
            status,
            axum::Json(ferry_core::api::ErrorBody { error: msg }),
        )
            .into_response()
    }
}

pub type ApiResult<T> = Result<axum::Json<T>, ApiError>;

/// Turn a listen address like "0.0.0.0:7070" or "[::]:7070" into a loopback address with the same port.
pub fn loopback_addr(listen: &str) -> String {
    let port = listen.rsplit(':').next().unwrap_or("7070");
    if listen.starts_with('[') || listen.starts_with("::") {
        format!("[::1]:{port}")
    } else {
        format!("127.0.0.1:{port}")
    }
}

pub fn api_v1(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/me", get(info::me))
        .route("/me/notify", get(notify::get).put(notify::put))
        .route("/me/notify/test", post(notify::test))
        .route("/status", get(info::status))
        .route("/gpus", get(info::gpus))
        .route("/images", get(info::images))
        .route("/images/pull", post(info::pull_image))
        .route("/jobs", post(jobs::submit).get(jobs::list))
        .route("/jobs/{id}", get(jobs::get).delete(jobs::cancel))
        .route("/jobs/{id}/release", post(jobs::release))
        .route("/jobs/{id}/log", get(jobs::log))
        .route("/jobs/{id}/wait", get(jobs::wait))
        .route("/jobs/{id}/result", get(jobs::result))
        .route("/jobs/{id}/artifacts", get(jobs::artifacts))
        .route("/jobs/{id}/artifacts/{*path}", get(jobs::artifact))
        .route("/jobs/{id}/files", put(jobs::upload_files))
        .route("/users", get(admin::list_users).post(admin::create_user))
        .route("/users/{name}", delete(admin::delete_user))
        .route(
            "/users/{name}/quota",
            get(admin::get_quota).put(admin::set_quota),
        )
        .route("/tokens", get(admin::list_tokens).post(admin::create_token))
        .route("/tokens/{id}", delete(admin::delete_token))
        .route(
            "/invites",
            get(admin::list_invites).post(admin::create_invite),
        )
        .route("/invites/{id}", delete(admin::delete_invite))
        .route("/usage", get(admin::usage))
        .route("/retention/sweep", post(admin::sweep))
        .route("/events", get(sse::events))
        .route("/tasks", get(tasks::list).post(tasks::create))
        .route(
            "/tasks/{id}",
            get(tasks::get).patch(tasks::update).delete(tasks::delete),
        )
        .route("/tasks/{id}/checkout", post(tasks::checkout))
        .route("/tasks/{id}/close", post(tasks::close))
        .route("/tasks/{id}/export", post(tasks::export))
        .route("/archives", get(tasks::list_archives))
        .route("/archives/manifest", get(tasks::archive_manifest))
        .route("/archives/import", post(tasks::import))
        .route("/tasks/{id}/reopen", post(tasks::reopen))
        .route("/tasks/{id}/snapshots/{sid}/pin", post(tasks::pin))
        .route("/metrics/keys", get(metrics::keys))
        .route("/metrics/query", get(metrics::query))
        .route("/metrics/stream", get(metrics::stream))
        .route("/nodes", get(nodes::list))
        .route("/nodes/{id}", delete(nodes::delete))
        .layer(axum::middleware::from_fn_with_state(state, require_auth))
}

pub fn router(state: Arc<AppState>) -> Router {
    // The MCP endpoint reuses the bearer middleware; tools call back into /api/v1 over loopback
    // with the caller's token, so per-user permissions and quotas apply unchanged.
    let self_url = format!("http://{}", loopback_addr(&state.config.listen));
    let mcp = Router::new()
        .nest_service("/mcp", ferry_mcp::http_service(self_url))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ));
    // Agents register with the cluster node token (not a user token).
    let agents = Router::new()
        .route("/api/v1/nodes/register", post(nodes::register))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            nodes::require_node_token,
        ));
    // Registration with an invite code is the only unauthenticated API.
    let public = Router::new()
        .route("/api/v1/invite", get(admin::preview_invite))
        .route("/api/v1/register", post(admin::register));
    let mut app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(public)
        .merge(agents)
        .nest("/api/v1", api_v1(state.clone()))
        .merge(mcp);
    app = web::mount(app);
    app.layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024 * 1024))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
