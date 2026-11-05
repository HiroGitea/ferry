use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use ferry_core::api::{NotifyInfo, NotifySettings};

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::notify::{Kind, Notification, deliver};
use crate::state::AppState;

fn info(state: &AppState, settings: NotifySettings) -> NotifyInfo {
    NotifyInfo {
        settings,
        telegram_available: state.config.notify.telegram_bot_token.is_some(),
    }
}

pub async fn get(State(state): State<Arc<AppState>>, user: AuthUser) -> ApiResult<NotifyInfo> {
    let s = state.store.notify_settings(user.0.id).await?;
    Ok(Json(info(&state, s)))
}

pub async fn put(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Json(mut s): Json<NotifySettings>,
) -> ApiResult<NotifyInfo> {
    s.webhook_url = s
        .webhook_url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty());
    s.telegram_chat_id = s
        .telegram_chat_id
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty());
    if let Some(u) = &s.webhook_url
        && !(u.starts_with("http://") || u.starts_with("https://"))
    {
        return Err(ApiError::BadRequest(
            "webhook URL must start with http:// or https://".into(),
        ));
    }
    if let Some(c) = &s.telegram_chat_id {
        let ok = c.starts_with('@') && c.len() > 1
            || c.strip_prefix('-')
                .unwrap_or(c)
                .chars()
                .all(|ch| ch.is_ascii_digit());
        if !ok {
            return Err(ApiError::BadRequest(
                "telegram chat id is a number (e.g. 123456789, -100…) or @channel".into(),
            ));
        }
        if state.config.notify.telegram_bot_token.is_none() {
            return Err(ApiError::BadRequest(
                "this server has no Telegram bot (notify.telegram_bot_token)".into(),
            ));
        }
    }
    state.store.set_notify_settings(user.0.id, &s).await?;
    Ok(Json(info(&state, s)))
}

/// Send a test message to the caller's channels and report per-channel errors.
pub async fn test(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
) -> ApiResult<serde_json::Value> {
    let s = state.store.notify_settings(user.0.id).await?;
    if s.webhook_url.is_none() && s.telegram_chat_id.is_none() {
        return Err(ApiError::BadRequest(
            "no notification channel configured".into(),
        ));
    }
    let n = Notification {
        kind: Kind::Test,
        title: format!("ferry: test notification for {}", user.0.name),
        body: "If you can read this, notifications work.".into(),
        job: None,
    };
    let errors = deliver(&state, &s, &n).await;
    if errors.is_empty() {
        Ok(Json(serde_json::json!({ "ok": true })))
    } else {
        Err(ApiError::BadRequest(errors.join("; ")))
    }
}
