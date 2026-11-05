use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use mlq_core::api::{
    ArtifactEntry, Event, JobResult, ListJobsQuery, LogChunk, LogQuery, SubmitJobRequest,
    SubmitJobResponse,
};
use mlq_core::{Job, JobId, JobState};
use serde::Deserialize;

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::sched::{inventory, publish_job};
use crate::state::AppState;
use crate::store::NewJob;
use crate::workspace::{Workspace, read_log};

/// Expand "0-3", "1,4,7", "0-2,5" into indices.
pub fn expand_array(spec: &str) -> Result<Vec<u32>, ApiError> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if let Some((a, b)) = part.split_once('-') {
            let a: u32 = a
                .trim()
                .parse()
                .map_err(|_| ApiError::BadRequest(format!("bad array spec '{spec}'")))?;
            let b: u32 = b
                .trim()
                .parse()
                .map_err(|_| ApiError::BadRequest(format!("bad array spec '{spec}'")))?;
            if b < a {
                return Err(ApiError::BadRequest(format!("bad array range '{part}'")));
            }
            out.extend(a..=b);
        } else {
            out.push(
                part.parse()
                    .map_err(|_| ApiError::BadRequest(format!("bad array spec '{spec}'")))?,
            );
        }
    }
    if out.is_empty() || out.len() > 1000 {
        return Err(ApiError::BadRequest(
            "array must have 1..=1000 indices".into(),
        ));
    }
    Ok(out)
}

fn validate(state: &AppState, req: &SubmitJobRequest) -> Result<(), ApiError> {
    if req.command.is_empty() {
        return Err(ApiError::BadRequest("command must not be empty".into()));
    }
    if req.resources.cpus <= 0.0 || req.resources.mem_bytes == 0 {
        return Err(ApiError::BadRequest(
            "cpus and mem_bytes must be positive".into(),
        ));
    }
    if let Some(img) = &req.image {
        if !state.config.image_allowed(img) {
            return Err(ApiError::Forbidden(format!(
                "image '{img}' is not in allowed_images"
            )));
        }
    }
    for (p, c) in &req.files {
        if c.len() > 4 * 1024 * 1024 {
            return Err(ApiError::BadRequest(format!(
                "inline file '{p}' exceeds 4 MiB; use the upload endpoint"
            )));
        }
    }
    Ok(())
}

fn estimate(state: &AppState, req: &SubmitJobRequest, active: &[Job]) -> String {
    let inv = inventory(state);
    if req.resources.cpus > inv.cpus
        || req.resources.mem_bytes > inv.mem_bytes
        || req.resources.gpus as usize > inv.gpus.len()
    {
        return format!(
            "will never run: request exceeds managed inventory ({} cpus, {} mem, {} gpus)",
            inv.cpus,
            mlq_core::resource::format_bytes(inv.mem_bytes),
            inv.gpus.len()
        );
    }
    let mut free_cpus = inv.cpus;
    let mut free_mem = inv.mem_bytes;
    let mut free_gpus = inv.gpus.len();
    let mut queued_ahead = 0;
    for j in active {
        if j.state.is_active() {
            free_cpus -= j.resources.cpus;
            free_mem = free_mem.saturating_sub(j.resources.mem_bytes);
            free_gpus = free_gpus.saturating_sub(j.gpu_ids.len());
        } else if j.state == JobState::Queued {
            queued_ahead += 1;
        }
    }
    let fits = req.resources.cpus <= free_cpus
        && req.resources.mem_bytes <= free_mem
        && req.resources.gpus as usize <= free_gpus;
    if fits && queued_ahead == 0 {
        "resources available: expected to start immediately".into()
    } else {
        format!(
            "will queue: {queued_ahead} job(s) ahead, free now = {free_cpus:.1} cpus, {} mem, {free_gpus} gpus",
            mlq_core::resource::format_bytes(free_mem)
        )
    }
}

pub async fn submit(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Json(req): Json<SubmitJobRequest>,
) -> ApiResult<SubmitJobResponse> {
    validate(&state, &req)?;
    let active = state.store.active_jobs().await?;
    let est = estimate(&state, &req, &active);
    let indices: Vec<Option<u32>> = match &req.array {
        Some(spec) => expand_array(spec)?.into_iter().map(Some).collect(),
        None => vec![None],
    };
    // Queue quota.
    let quota = state.store.quota(user.0.id).await?;
    if let Some(max) = quota.max_queued_jobs {
        let queued = state
            .store
            .count_user_jobs(user.0.id, &[JobState::Queued, JobState::Held])
            .await?;
        if queued + indices.len() as u32 > max {
            return Err(ApiError::Forbidden(format!(
                "quota: at most {max} queued jobs"
            )));
        }
    }
    // Dependencies must exist and belong to the user (admins may depend on anything).
    for dep in &req.depends_on {
        match state.store.job(*dep).await? {
            Some(j) if j.user_id == user.0.id || user.is_admin() => {}
            Some(_) => {
                return Err(ApiError::Forbidden(format!(
                    "dependency {dep} belongs to another user"
                )));
            }
            None => return Err(ApiError::BadRequest(format!("dependency {dep} not found"))),
        }
    }
    if req.dry_run {
        return Ok(Json(SubmitJobResponse {
            dry_run: true,
            jobs: vec![],
            estimate: est,
        }));
    }
    let image = req
        .image
        .clone()
        .unwrap_or_else(|| state.config.default_image.clone());
    let base_name = req
        .name
        .clone()
        .unwrap_or_else(|| req.command.first().cloned().unwrap_or_else(|| "job".into()));
    let mut jobs = Vec::new();
    for idx in indices {
        let name = match idx {
            Some(i) => format!("{base_name}[{i}]"),
            None => base_name.clone(),
        };
        let job = state
            .store
            .create_job(&NewJob {
                user_id: user.0.id,
                name,
                image: image.clone(),
                command: req.command.clone(),
                env: req.env.clone(),
                resources: req.resources.clone(),
                state: if req.hold {
                    JobState::Held
                } else {
                    JobState::Queued
                },
                priority: req.priority,
                depends_on: req.depends_on.clone(),
                array_index: idx,
                time_limit_secs: req.time_limit_secs,
            })
            .await?;
        // Inline files are written now so they are present even if the job is held.
        if !req.files.is_empty() {
            let ws = Workspace::create(Workspace::path_for(
                &state.config.scratch_root(),
                &user.0.name,
                job.id,
            ))?;
            ws.write_files(&req.files)?;
        }
        state.events.publish(Event::JobUpdated { job: job.clone() });
        jobs.push(job);
    }
    state.wake.notify_one();
    Ok(Json(SubmitJobResponse {
        dry_run: false,
        jobs,
        estimate: est,
    }))
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Query(q): Query<ListJobsQuery>,
) -> ApiResult<Vec<Job>> {
    let user_id = match &q.user {
        Some(name) if name == "*" || name == "all" => {
            user.require_admin()?;
            None
        }
        Some(name) => Some(
            state
                .store
                .user_by_name(name)
                .await?
                .ok_or_else(|| ApiError::NotFound(format!("user {name}")))?
                .id,
        ),
        None => {
            if user.is_admin() {
                None
            } else {
                Some(user.0.id)
            }
        }
    };
    Ok(Json(
        state
            .store
            .list_jobs(q.state, user_id, q.all, q.limit.unwrap_or(200).min(5000))
            .await?,
    ))
}

async fn load_job(state: &AppState, user: &AuthUser, id: JobId) -> Result<Job, ApiError> {
    let job = state
        .store
        .job(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("job {id}")))?;
    if job.user_id != user.0.id && !user.is_admin() {
        return Err(ApiError::Forbidden("not your job".into()));
    }
    Ok(job)
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<JobId>,
) -> ApiResult<Job> {
    Ok(Json(load_job(&state, &user, id).await?))
}

pub async fn cancel(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<JobId>,
) -> ApiResult<Job> {
    let job = load_job(&state, &user, id).await?;
    match job.state {
        JobState::Queued | JobState::Held => {
            state
                .store
                .set_finished(id, JobState::Cancelled, None, Some("cancelled by user"))
                .await?;
        }
        JobState::Starting | JobState::Pulling | JobState::Running => {
            if !state.request_cancel(id) {
                // No watcher task (should not happen); force the state.
                if let Some(h) = &job.container_id {
                    let _ = state.executor.kill(h).await;
                    let _ = state.executor.cleanup(h).await;
                }
                state
                    .store
                    .set_finished(id, JobState::Cancelled, None, Some("cancelled by user"))
                    .await?;
            }
        }
        _ => {
            return Err(ApiError::Conflict(format!(
                "job {id} already {}",
                job.state
            )));
        }
    }
    publish_job(&state, id).await;
    state.wake.notify_one();
    Ok(Json(state.store.job(id).await?.unwrap()))
}

pub async fn release(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<JobId>,
) -> ApiResult<Job> {
    let job = load_job(&state, &user, id).await?;
    if job.state != JobState::Held {
        return Err(ApiError::Conflict(format!(
            "job {id} is {}, not held",
            job.state
        )));
    }
    state.store.set_state(id, JobState::Queued, None).await?;
    publish_job(&state, id).await;
    state.wake.notify_one();
    Ok(Json(state.store.job(id).await?.unwrap()))
}

fn workspace_of(state: &AppState, job: &Job) -> Workspace {
    if job.workspace_deleted {
        if let Some(a) = &job.archive_path {
            // Archive has the same layout (out/, job.log) as a workspace.
            return Workspace::open(a.into());
        }
    }
    match &job.workspace_path {
        Some(p) => Workspace::open(p.into()),
        None => Workspace::open(Workspace::path_for(
            &state.config.scratch_root(),
            &job.user_name,
            job.id,
        )),
    }
}

pub async fn log(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<JobId>,
    Query(q): Query<LogQuery>,
) -> ApiResult<LogChunk> {
    let job = load_job(&state, &user, id).await?;
    let ws = workspace_of(&state, &job);
    let (content, next) = read_log(&ws.log(), q.tail, q.offset)?;
    Ok(Json(LogChunk {
        job_id: id,
        content,
        next_offset: next,
        state: job.state,
    }))
}

#[derive(Deserialize)]
pub struct WaitQuery {
    /// Seconds to wait before returning the current state (max 600).
    pub timeout: Option<u64>,
}

/// Long-poll until the job reaches a terminal state or the timeout elapses.
pub async fn wait(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<JobId>,
    Query(q): Query<WaitQuery>,
) -> ApiResult<Job> {
    let mut rx = state.events.subscribe();
    let job = load_job(&state, &user, id).await?;
    if job.state.is_terminal() {
        return Ok(Json(job));
    }
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(q.timeout.unwrap_or(60).min(600));
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(Event::JobUpdated { job })) if job.id == id && job.state.is_terminal() => {
                return Ok(Json(job));
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => break, // lagged / closed
            Err(_) => break,
        }
    }
    Ok(Json(state.store.job(id).await?.unwrap()))
}

pub async fn artifacts(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<JobId>,
) -> ApiResult<Vec<ArtifactEntry>> {
    let job = load_job(&state, &user, id).await?;
    Ok(Json(workspace_of(&state, &job).list_out()?))
}

pub async fn result(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<JobId>,
) -> ApiResult<JobResult> {
    let job = load_job(&state, &user, id).await?;
    let ws = workspace_of(&state, &job);
    let artifacts = ws.list_out()?;
    let (log_tail, _) = read_log(&ws.log(), Some(100), None)?;
    Ok(Json(JobResult {
        job,
        artifacts,
        log_tail,
    }))
}

pub async fn artifact(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, path)): Path<(JobId, String)>,
) -> Result<Response, ApiError> {
    let job = load_job(&state, &user, id).await?;
    let ws = workspace_of(&state, &job);
    let file = ws
        .resolve_in(&ws.out(), &path)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let md = tokio::fs::metadata(&file)
        .await
        .map_err(|_| ApiError::NotFound(format!("artifact {path}")))?;
    if !md.is_file() {
        return Err(ApiError::BadRequest("not a file".into()));
    }
    let f = tokio::fs::File::open(&file)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let stream = tokio_util::io::ReaderStream::new(f);
    let mime = mime_guess::from_path(&file).first_or_octet_stream();
    Ok((
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CONTENT_LENGTH, md.len().to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

/// Upload a gzipped tar archive into the job's `work/` directory (job must be held or queued).
pub async fn upload_files(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<JobId>,
    body: Bytes,
) -> ApiResult<serde_json::Value> {
    let job = load_job(&state, &user, id).await?;
    if !matches!(job.state, JobState::Held | JobState::Queued) {
        return Err(ApiError::Conflict(format!(
            "job {id} is {}; files can only be uploaded before it starts",
            job.state
        )));
    }
    let ws = Workspace::create(Workspace::path_for(
        &state.config.scratch_root(),
        &job.user_name,
        job.id,
    ))?;
    let n = tokio::task::spawn_blocking(move || ws.unpack_tar_gz(&body))
        .await
        .map_err(|e| ApiError::Internal(e.into()))??;
    Ok(Json(serde_json::json!({ "job_id": id, "files": n })))
}
