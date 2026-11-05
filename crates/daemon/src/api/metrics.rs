//! `/api/v1/metrics`: netdata-style time series of the host, GPUs, scheduler, jobs and tasks.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures::Stream;
use futures::stream::StreamExt;
use mlq_core::api::{MetricInfo, MetricTick, MetricsQuery, MetricsResult};
use serde::Deserialize;
use tokio_stream::wrappers::BroadcastStream;

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::metrics::{expand_keys, now_secs, owner_scope};
use crate::state::AppState;

/// Owner of a `job.<id>` / `task.<id>` scope (cached by the collector, else from the database).
async fn scope_owner(state: &AppState, scope: &str) -> Option<i64> {
    if let Some(u) = state.metrics.owner(scope) {
        return Some(u);
    }
    let (kind, id) = scope.split_once('.')?;
    let id: i64 = id.parse().ok()?;
    let owner = match kind {
        "job" => state.store.job(id).await.ok().flatten().map(|j| j.user_id),
        "task" => state.store.task(id).await.ok().flatten().map(|t| t.user_id),
        _ => None,
    }?;
    state.metrics.set_owner(scope, owner);
    Some(owner)
}

pub async fn visible(state: &AppState, user: &AuthUser, key: &str) -> bool {
    if user.is_admin() {
        return true;
    }
    match owner_scope(key) {
        None => true,
        Some(scope) => scope_owner(state, &scope).await == Some(user.0.id),
    }
}

#[derive(Deserialize)]
pub struct KeysQuery {
    pub prefix: Option<String>,
}

pub async fn keys(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Query(q): Query<KeysQuery>,
) -> ApiResult<Vec<MetricInfo>> {
    let mut out = Vec::new();
    for k in state.metrics.keys().await? {
        if q.prefix
            .as_deref()
            .map(|p| k.key.starts_with(p))
            .unwrap_or(true)
            && visible(&state, &user, &k.key).await
        {
            out.push(k);
        }
    }
    Ok(Json(out))
}

fn resolve_time(v: Option<i64>, now: i64, default: i64) -> i64 {
    match v {
        None | Some(0) => default,
        Some(x) if x < 0 => now + x,
        Some(x) => x,
    }
}

pub async fn query(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Query(q): Query<MetricsQuery>,
) -> ApiResult<MetricsResult> {
    let now = now_secs();
    let before = resolve_time(q.before, now, now);
    let after = resolve_time(q.after, now, now - 600);
    if after >= before {
        return Err(ApiError::BadRequest(
            "after must be earlier than before".into(),
        ));
    }
    let known: Vec<String> = state
        .metrics
        .keys()
        .await?
        .into_iter()
        .map(|k| k.key)
        .collect();
    let mut keys = Vec::new();
    for k in expand_keys(&q.keys, &known) {
        if visible(&state, &user, &k).await {
            keys.push(k);
        }
    }
    Ok(Json(
        state
            .metrics
            .query(&keys, after, before, q.points.unwrap_or(300))
            .await?,
    ))
}

/// Live per-second values (`event: tick`), filtered to what the user may see.
pub async fn stream(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.metrics.subscribe();
    let st = state.clone();
    let stream = BroadcastStream::new(rx).filter_map(move |tick| {
        let st = st.clone();
        let user = user.clone();
        async move {
            let tick = tick.ok()?;
            let mut out = MetricTick {
                t: tick.t,
                values: Default::default(),
            };
            for (k, v) in tick.values {
                if visible(&st, &user, &k).await {
                    out.values.insert(k, v);
                }
            }
            Some(Ok(SseEvent::default()
                .event("tick")
                .json_data(out)
                .unwrap_or_default()))
        }
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}
