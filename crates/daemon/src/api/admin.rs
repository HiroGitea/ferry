use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{Duration, Utc};
use ferry_core::api::{
    CreateInviteRequest, CreateTokenRequest, CreateUserRequest, DailyUsage, InviteCreated,
    InviteInfo, InvitePreview, RegisterRequest, TokenCreated, TokenInfo, UsageQuery, UserUsage,
};
use ferry_core::{JobState, Quota, User};

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::state::AppState;
use crate::store::{NewInvite, RegisterError};

fn valid_user_name(raw: &str) -> Result<&str, ApiError> {
    let name = raw.trim();
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(ApiError::BadRequest(
            "user name must be 1-64 chars of [A-Za-z0-9._-]".into(),
        ));
    }
    Ok(name)
}

pub async fn list_users(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
) -> ApiResult<Vec<User>> {
    user.require_admin()?;
    Ok(Json(state.store.list_users().await?))
}

pub async fn create_user(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Json(req): Json<CreateUserRequest>,
) -> ApiResult<TokenCreated> {
    user.require_admin()?;
    let name = valid_user_name(&req.name)?;
    if state.store.user_by_name(name).await?.is_some() {
        return Err(ApiError::Conflict(format!("user {name} exists")));
    }
    let u = state.store.create_user(name, req.role).await?;
    let (token, id) = state.store.create_token(u.id, Some("initial")).await?;
    Ok(Json(TokenCreated { id, user: u, token }))
}

pub async fn delete_user(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(name): Path<String>,
) -> ApiResult<serde_json::Value> {
    user.require_admin()?;
    let u = state
        .store
        .user_by_name(&name)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("user {name}")))?;
    if u.id == user.0.id {
        return Err(ApiError::BadRequest("cannot delete yourself".into()));
    }
    let active = state
        .store
        .count_user_jobs(
            u.id,
            &[
                JobState::Queued,
                JobState::Held,
                JobState::Starting,
                JobState::Pulling,
                JobState::Running,
            ],
        )
        .await?;
    if active > 0 {
        return Err(ApiError::Conflict(format!(
            "user {name} has {active} active job(s)"
        )));
    }
    state.store.delete_user(u.id).await?;
    Ok(Json(serde_json::json!({ "deleted": name })))
}

pub async fn get_quota(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(name): Path<String>,
) -> ApiResult<Quota> {
    let u = state
        .store
        .user_by_name(&name)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("user {name}")))?;
    if u.id != user.0.id {
        user.require_admin()?;
    }
    Ok(Json(state.store.quota(u.id).await?))
}

pub async fn set_quota(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(name): Path<String>,
    Json(q): Json<Quota>,
) -> ApiResult<Quota> {
    user.require_admin()?;
    let u = state
        .store
        .user_by_name(&name)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("user {name}")))?;
    state.store.set_quota(u.id, &q).await?;
    state.wake.notify_one();
    Ok(Json(q))
}

pub async fn list_tokens(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
) -> ApiResult<Vec<TokenInfo>> {
    let scope = if user.is_admin() {
        None
    } else {
        Some(user.0.id)
    };
    Ok(Json(state.store.list_tokens(scope).await?))
}

pub async fn create_token(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Json(req): Json<CreateTokenRequest>,
) -> ApiResult<TokenCreated> {
    let target = match &req.user {
        Some(name) if name != &user.0.name => {
            user.require_admin()?;
            state
                .store
                .user_by_name(name)
                .await?
                .ok_or_else(|| ApiError::NotFound(format!("user {name}")))?
        }
        _ => user.0.clone(),
    };
    let (token, id) = state
        .store
        .create_token(target.id, req.name.as_deref())
        .await?;
    Ok(Json(TokenCreated {
        id,
        user: target,
        token,
    }))
}

pub async fn delete_token(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    let scope = if user.is_admin() {
        None
    } else {
        Some(user.0.id)
    };
    if !state.store.delete_token(id, scope).await? {
        return Err(ApiError::NotFound(format!("token {id}")));
    }
    Ok(Json(serde_json::json!({ "deleted": id })))
}

pub async fn compute_usage(
    state: &AppState,
    q: &UsageQuery,
    scope_user: Option<&User>,
) -> anyhow::Result<Vec<UserUsage>> {
    let now = Utc::now();
    let from = q.from.unwrap_or(now - Duration::days(30));
    let to = q.to.unwrap_or(now);
    let users: Vec<User> = match scope_user {
        Some(u) => vec![u.clone()],
        None => state.store.list_users().await?,
    };
    let started = state
        .store
        .jobs_started_between(from, to, scope_user.map(|u| u.id))
        .await?;
    let active = state.store.active_jobs().await?;
    let fair = crate::sched::fair_usage(state, &active).await?;
    let quotas = state.store.all_quotas().await?;
    let mut out = Vec::new();
    for u in users {
        let mine: Vec<_> = started.iter().filter(|j| j.user_id == u.id).collect();
        let mut daily: BTreeMap<String, DailyUsage> = BTreeMap::new();
        let mut gpu_s = 0.0;
        let mut cpu_s = 0.0;
        for j in &mine {
            let rt = j.runtime_secs(now).unwrap_or(0) as f64;
            let g = rt * j.gpu_ids.len() as f64;
            let c = rt * j.resources.cpus;
            gpu_s += g;
            cpu_s += c;
            let day = j.started_at.unwrap_or(now).format("%Y-%m-%d").to_string();
            let d = daily.entry(day.clone()).or_insert(DailyUsage {
                day,
                gpu_seconds: 0.0,
                cpu_seconds: 0.0,
                jobs: 0,
            });
            d.gpu_seconds += g;
            d.cpu_seconds += c;
            d.jobs += 1;
        }
        let my_active: Vec<_> = active.iter().filter(|j| j.user_id == u.id).collect();
        out.push(UserUsage {
            quota: quotas.get(&u.id).cloned().unwrap_or_default(),
            jobs_total: mine.len() as u32,
            jobs_succeeded: mine
                .iter()
                .filter(|j| j.state == JobState::Succeeded)
                .count() as u32,
            jobs_failed: mine
                .iter()
                .filter(|j| matches!(j.state, JobState::Failed | JobState::Timeout))
                .count() as u32,
            running_jobs: my_active.iter().filter(|j| j.state.is_active()).count() as u32,
            running_gpus: my_active
                .iter()
                .filter(|j| j.state.is_active())
                .map(|j| j.gpu_ids.len() as u32)
                .sum(),
            queued_jobs: my_active
                .iter()
                .filter(|j| matches!(j.state, JobState::Queued | JobState::Held))
                .count() as u32,
            gpu_seconds: gpu_s,
            cpu_seconds: cpu_s,
            fair_share_usage: fair.get(&u.id).copied().unwrap_or(0.0),
            daily: daily.into_values().collect(),
            user: u,
        });
    }
    Ok(out)
}

pub async fn usage(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Vec<UserUsage>> {
    let scope = match &q.user {
        Some(name) if name == "*" || name == "all" => {
            user.require_admin()?;
            None
        }
        Some(name) => {
            let u = state
                .store
                .user_by_name(name)
                .await?
                .ok_or_else(|| ApiError::NotFound(format!("user {name}")))?;
            if u.id != user.0.id {
                user.require_admin()?;
            }
            Some(u)
        }
        None => {
            if user.is_admin() {
                None
            } else {
                Some(user.0.clone())
            }
        }
    };
    Ok(Json(compute_usage(&state, &q, scope.as_ref()).await?))
}

pub async fn sweep(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
) -> ApiResult<crate::retention::SweepReport> {
    user.require_admin()?;
    Ok(Json(crate::retention::sweep(&state).await?))
}

// ---- invites ----

const DEFAULT_INVITE_TTL: u64 = 7 * 24 * 3600;
const MAX_INVITE_TTL: u64 = 90 * 24 * 3600;

pub async fn list_invites(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
) -> ApiResult<Vec<InviteInfo>> {
    user.require_admin()?;
    Ok(Json(state.store.list_invites().await?))
}

pub async fn create_invite(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Json(req): Json<CreateInviteRequest>,
) -> ApiResult<InviteCreated> {
    user.require_admin()?;
    let name = match req.name.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(n) => {
            let n = valid_user_name(n)?;
            if state.store.user_by_name(n).await?.is_some() {
                return Err(ApiError::Conflict(format!("user {n} exists")));
            }
            Some(n)
        }
    };
    let max_uses = req.max_uses.unwrap_or(1);
    if !(1..=1000).contains(&max_uses) || (name.is_some() && max_uses != 1) {
        return Err(ApiError::BadRequest(
            "max_uses must be 1-1000 (and 1 for an invite locked to a name)".into(),
        ));
    }
    let ttl = req.expires_in_secs.unwrap_or(DEFAULT_INVITE_TTL);
    if ttl == 0 || ttl > MAX_INVITE_TTL {
        return Err(ApiError::BadRequest(
            "expiry must be between 1s and 90 days".into(),
        ));
    }
    let note = req.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let (code, invite) = state
        .store
        .create_invite(&NewInvite {
            role: req.role.unwrap_or(ferry_core::Role::User),
            name,
            note,
            max_uses,
            expires_at: Utc::now() + Duration::seconds(ttl as i64),
            created_by: Some(user.0.id),
        })
        .await?;
    Ok(Json(InviteCreated { invite, code }))
}

pub async fn delete_invite(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    user.require_admin()?;
    if !state.store.delete_invite(id).await? {
        return Err(ApiError::NotFound(format!("invite {id}")));
    }
    Ok(Json(serde_json::json!({ "deleted": id })))
}

#[derive(serde::Deserialize)]
pub struct CodeQuery {
    code: String,
}

/// Public (no token): lets the registration page show what an invite grants.
pub async fn preview_invite(
    State(state): State<Arc<AppState>>,
    Query(q): Query<CodeQuery>,
) -> ApiResult<InvitePreview> {
    state
        .store
        .preview_invite(q.code.trim())
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("invite is invalid, expired or used up".into()))
}

/// Public (no token): create an account with an invite code and return its first token.
pub async fn register(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterRequest>,
) -> ApiResult<TokenCreated> {
    let name = valid_user_name(&req.name)?;
    let token_name = req
        .token_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("registration");
    match state
        .store
        .register_with_invite(req.code.trim(), name, token_name)
        .await?
    {
        Ok((user, token, id)) => {
            tracing::info!(user = %user.name, role = user.role.as_str(), "user registered with invite");
            Ok(Json(TokenCreated { id, user, token }))
        }
        Err(RegisterError::InvalidInvite) => Err(ApiError::NotFound(
            "invite is invalid, expired or used up".into(),
        )),
        Err(RegisterError::NameTaken) => {
            Err(ApiError::Conflict(format!("user name {name} is taken")))
        }
        Err(RegisterError::NameMismatch(n)) => Err(ApiError::BadRequest(format!(
            "this invite is for user name {n}"
        ))),
    }
}

/// Admin: back up the database now.
pub async fn backup(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
) -> ApiResult<serde_json::Value> {
    user.require_admin()?;
    let files = crate::backup::run_once(&state)
        .await
        .map_err(|e| ApiError::BadRequest(format!("{e:#}")))?;
    Ok(Json(serde_json::json!({ "files": files })))
}
