//! `/api/v1/tasks`: long-lived environments with steps and snapshots.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use mlq_core::api::{
    ArchiveInfo, ArchiveManifest, CheckoutRequest, CreateTaskRequest, ExportTaskRequest,
    ExportTaskResponse, ImportArchiveRequest, ListTasksQuery, PinSnapshotRequest,
    UpdateTaskRequest,
};
use mlq_core::{Job, JobState, Task, TaskDetail, TaskId, TaskState, TaskSummary};

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::state::AppState;
use crate::tasks::{self, publish_task};

async fn load(state: &AppState, user: &AuthUser, id: TaskId) -> Result<Task, ApiError> {
    let t = state
        .store
        .task(id)
        .await?
        .filter(|t| t.state != TaskState::Deleted)
        .ok_or_else(|| ApiError::NotFound(format!("task {id}")))?;
    if t.user_id != user.0.id && !user.is_admin() {
        return Err(ApiError::Forbidden("not your task".into()));
    }
    Ok(t)
}

pub async fn summarize(state: &AppState, task: Task, steps: &[Job]) -> anyhow::Result<TaskSummary> {
    let snapshots = state
        .store
        .snapshots(task.id)
        .await?
        .iter()
        .filter(|s| !s.deleted)
        .count() as u32;
    let activity = if task.state != TaskState::Open {
        "closed"
    } else if steps.iter().any(|j| j.state.is_active()) {
        "running"
    } else if steps
        .iter()
        .any(|j| matches!(j.state, JobState::Queued | JobState::Held))
    {
        "queued"
    } else if task.warm_container.is_some() {
        "warm"
    } else {
        "idle"
    };
    Ok(TaskSummary {
        steps: steps.len() as u32,
        snapshots,
        activity: activity.into(),
        last_step: steps.last().cloned(),
        task,
    })
}

pub async fn detail(state: &AppState, task: Task) -> anyhow::Result<TaskDetail> {
    let steps = state.store.task_steps(task.id).await?;
    let snapshots = state.store.snapshots(task.id).await?;
    let (head_image, _) = tasks::head_image(state, &task).await;
    Ok(TaskDetail {
        summary: summarize(state, task, &steps).await?,
        steps,
        snapshots,
        head_image,
    })
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Query(q): Query<ListTasksQuery>,
) -> ApiResult<Vec<TaskSummary>> {
    let scope = match q.user.as_deref() {
        Some("all") | Some("*") => {
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
            Some(u.id)
        }
        None => {
            if user.is_admin() {
                None
            } else {
                Some(user.0.id)
            }
        }
    };
    let mut out = Vec::new();
    for t in state.store.list_tasks(scope, q.all).await? {
        let steps = state.store.task_steps(t.id).await?;
        out.push(summarize(&state, t, &steps).await?);
    }
    Ok(Json(out))
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Json(req): Json<CreateTaskRequest>,
) -> ApiResult<TaskDetail> {
    let name = req.name.trim();
    if name.is_empty() || name.len() > 200 {
        return Err(ApiError::BadRequest(
            "task name must be 1-200 characters".into(),
        ));
    }
    let image = req
        .image
        .clone()
        .unwrap_or_else(|| state.config.default_image.clone());
    if !state.config.image_allowed(&image) {
        return Err(ApiError::Forbidden(format!(
            "image '{image}' is not in allowed_images"
        )));
    }
    let res = req.resources.apply(&Default::default());
    if res.cpus <= 0.0 || res.mem_bytes == 0 {
        return Err(ApiError::BadRequest(
            "cpus and mem_bytes must be positive".into(),
        ));
    }
    let keep = req.keep_warm_secs.unwrap_or(0).min(24 * 3600);
    if let Some(n) = &req.node {
        if state.nodes.get(n).is_none() {
            return Err(ApiError::BadRequest(format!(
                "node '{n}' is not registered"
            )));
        }
    }
    let t = state
        .store
        .create_task(user.0.id, name, &image, &res, keep, req.node.as_deref())
        .await?;
    tasks::ensure_workspace(&state, &t).await?;
    let t = state.store.task(t.id).await?.unwrap();
    publish_task(&state, t.id);
    Ok(Json(detail(&state, t).await?))
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<TaskId>,
) -> ApiResult<TaskDetail> {
    let t = load(&state, &user, id).await?;
    Ok(Json(detail(&state, t).await?))
}

pub async fn update(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<TaskId>,
    Json(req): Json<UpdateTaskRequest>,
) -> ApiResult<TaskDetail> {
    let t = load(&state, &user, id).await?;
    let res = (!req.resources.is_empty()).then(|| req.resources.apply(&t.resources));
    state
        .store
        .update_task(
            id,
            req.name.as_deref().map(str::trim),
            req.keep_warm_secs.map(|k| k.min(24 * 3600)),
            res.as_ref(),
        )
        .await?;
    if req.keep_warm_secs == Some(0) {
        let lock = state.task_lock(id);
        let _g = lock.lock().await;
        tasks::drop_warm(&state, &t).await;
    }
    publish_task(&state, id);
    let t = state.store.task(id).await?.unwrap();
    Ok(Json(detail(&state, t).await?))
}

fn has_active_step(steps: &[Job]) -> bool {
    steps.iter().any(|j| j.state.is_active())
}

pub async fn checkout(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<TaskId>,
    Json(req): Json<CheckoutRequest>,
) -> ApiResult<TaskDetail> {
    let t = load(&state, &user, id).await?;
    if let Some(sid) = req.snapshot_id {
        match state.store.snapshot(sid).await? {
            Some(s) if s.task_id == id && !s.deleted => {}
            Some(s) if s.task_id == id => {
                return Err(ApiError::Conflict(format!("snapshot {sid} was pruned")));
            }
            _ => return Err(ApiError::NotFound(format!("snapshot {sid} in task {id}"))),
        }
    }
    let lock = state.task_lock(id);
    let _g = lock.lock().await;
    if has_active_step(&state.store.task_steps(id).await?) {
        return Err(ApiError::Conflict(
            "a step is running; wait for it or cancel it first".into(),
        ));
    }
    tasks::drop_warm(&state, &t).await;
    state.store.set_task_head(id, req.snapshot_id).await?;
    publish_task(&state, id);
    let t = state.store.task(id).await?.unwrap();
    Ok(Json(detail(&state, t).await?))
}

pub async fn pin(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, sid)): Path<(TaskId, i64)>,
    Json(req): Json<PinSnapshotRequest>,
) -> ApiResult<TaskDetail> {
    let t = load(&state, &user, id).await?;
    match state.store.snapshot(sid).await? {
        Some(s) if s.task_id == id => {}
        _ => return Err(ApiError::NotFound(format!("snapshot {sid} in task {id}"))),
    }
    state
        .store
        .pin_snapshot(sid, req.pinned, req.note.as_deref())
        .await?;
    publish_task(&state, id);
    Ok(Json(detail(&state, t).await?))
}

pub async fn close(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<TaskId>,
) -> ApiResult<TaskDetail> {
    let t = load(&state, &user, id).await?;
    let lock = state.task_lock(id);
    let _g = lock.lock().await;
    if has_active_step(&state.store.task_steps(id).await?) {
        return Err(ApiError::Conflict(
            "a step is running; cancel it before closing the task".into(),
        ));
    }
    tasks::close(&state, &t).await?;
    publish_task(&state, id);
    let t = state.store.task(id).await?.unwrap();
    Ok(Json(detail(&state, t).await?))
}

pub async fn reopen(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<TaskId>,
) -> ApiResult<TaskDetail> {
    let t = load(&state, &user, id).await?;
    if t.state != TaskState::Closed {
        return Err(ApiError::Conflict("task is not closed".into()));
    }
    state.store.set_task_state(id, TaskState::Open).await?;
    publish_task(&state, id);
    let t = state.store.task(id).await?.unwrap();
    Ok(Json(detail(&state, t).await?))
}

pub async fn delete(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<TaskId>,
) -> ApiResult<serde_json::Value> {
    let t = load(&state, &user, id).await?;
    if t.state != TaskState::Closed {
        return Err(ApiError::Conflict(
            "close the task before deleting it".into(),
        ));
    }
    let lock = state.task_lock(id);
    let _g = lock.lock().await;
    tasks::delete(&state, &t).await?;
    publish_task(&state, id);
    Ok(Json(serde_json::json!({ "deleted": id })))
}

// ---- archives (.7z on the NAS)

pub async fn export(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<TaskId>,
    Json(req): Json<ExportTaskRequest>,
) -> ApiResult<ExportTaskResponse> {
    let t = load(&state, &user, id).await?;
    let policy = crate::archive::ImagePolicy::parse(
        req.images
            .as_deref()
            .unwrap_or(&state.config.archive.images),
    )
    .ok_or_else(|| ApiError::BadRequest("images must be none, head, pinned or all".into()))?;
    let lock = state.task_lock(id);
    let _g = lock.lock().await;
    if has_active_step(&state.store.task_steps(id).await?) {
        return Err(ApiError::Conflict(
            "a step is running; wait for it before exporting".into(),
        ));
    }
    tasks::drop_warm(&state, &t).await;
    let mut r = crate::archive::export_task(&state, &t, policy).await?;
    state.store.set_task_archive(id, &r.path).await?;
    if req.offload {
        let t = state.store.task(id).await?.unwrap();
        crate::archive::offload(&state, &t, &r.path).await?;
        r.offloaded = true;
    }
    publish_task(&state, id);
    Ok(Json(r))
}

pub async fn list_archives(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
) -> ApiResult<Vec<ArchiveInfo>> {
    let scope = if user.is_admin() {
        None
    } else {
        Some(user.0.name.as_str())
    };
    Ok(Json(crate::archive::list(&state, scope).await?))
}

#[derive(serde::Deserialize)]
pub struct ArchiveNameQuery {
    pub name: String,
}

fn check_archive_access(user: &AuthUser, name: &str) -> Result<(), ApiError> {
    let owner = name.split('/').next().unwrap_or("");
    if !user.is_admin() && owner != crate::workspace::sanitize_name(&user.0.name) {
        return Err(ApiError::Forbidden("not your archive".into()));
    }
    Ok(())
}

pub async fn archive_manifest(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Query(q): Query<ArchiveNameQuery>,
) -> ApiResult<ArchiveManifest> {
    check_archive_access(&user, &q.name)?;
    let path =
        crate::archive::resolve(&state, &q.name).map_err(|e| ApiError::NotFound(e.to_string()))?;
    Ok(Json(crate::archive::read_manifest(&state, &path).await?))
}

pub async fn import(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Json(req): Json<ImportArchiveRequest>,
) -> ApiResult<TaskDetail> {
    check_archive_access(&user, &req.name)?;
    let path = crate::archive::resolve(&state, &req.name)
        .map_err(|e| ApiError::NotFound(e.to_string()))?;
    if let Some(n) = &req.node {
        if state.nodes.get(n).is_none() {
            return Err(ApiError::BadRequest(format!(
                "node '{n}' is not registered"
            )));
        }
    }
    let t = crate::archive::import_task(
        &state,
        user.0.id,
        &path,
        req.task_name.as_deref(),
        req.node.clone(),
    )
    .await?;
    publish_task(&state, t.id);
    Ok(Json(detail(&state, t).await?))
}
