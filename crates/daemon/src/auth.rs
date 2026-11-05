//! Bearer-token authentication. Tokens are looked up by SHA-256 hash in SQLite.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use ferry_core::{Role, User};

use crate::api::ApiError;
use crate::state::AppState;

#[derive(Debug, Clone)]
pub struct AuthUser(pub User);

impl AuthUser {
    pub fn is_admin(&self) -> bool {
        self.0.role == Role::Admin
    }
    pub fn require_admin(&self) -> Result<(), ApiError> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(ApiError::Forbidden("admin role required".into()))
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for AuthUser {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthUser>()
            .cloned()
            .ok_or(ApiError::Unauthorized)
    }
}

fn extract_token(req: &Request<Body>) -> Option<String> {
    if let Some(v) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some(t) = v.strip_prefix("Bearer ")
    {
        return Some(t.trim().to_string());
    }
    // EventSource cannot set headers: allow ?token= on the query string.
    req.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("token=").map(|t| t.to_string()))
    })
}

pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    let Some(token) = extract_token(&req) else {
        return Err(ApiError::Unauthorized);
    };
    match state.store.user_for_token(&token).await {
        Ok(Some(user)) => {
            req.extensions_mut().insert(AuthUser(user));
            Ok(next.run(req).await)
        }
        Ok(None) => Err(ApiError::Unauthorized),
        Err(e) => Err(ApiError::Internal(e)),
    }
}

pub fn status_for_unauthorized() -> StatusCode {
    StatusCode::UNAUTHORIZED
}
