use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use ferry_core::api::{
    ArtifactEntry, Event, JobResult, ListJobsQuery, LogChunk, LogQuery, SubmitJobRequest,
    SubmitJobResponse,
};
use ferry_core::{Job, JobId, JobState, ResourceRequest, TaskState};
use serde::Deserialize;

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::node::{Files, LocalFiles, Node};
use crate::sched::publish_job;
use crate::state::AppState;
use crate::store::NewJob;
use crate::workspace::Workspace;

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

fn validate(
    state: &AppState,
    req: &SubmitJobRequest,
    res: &ResourceRequest,
) -> Result<(), ApiError> {
    if req.command.is_empty() {
        return Err(ApiError::BadRequest("command must not be empty".into()));
    }
    if res.cpus <= 0.0 || res.mem_bytes == 0 {
        return Err(ApiError::BadRequest(
            "cpus and mem_bytes must be positive".into(),
        ));
    }
    if let Some(img) = &req.image
        && !state.config.image_allowed(img)
    {
        return Err(ApiError::Forbidden(format!(
            "image '{img}' is not in allowed_images"
        )));
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

fn estimate(state: &AppState, res: &ResourceRequest, active: &[Job]) -> String {
    // Capacity of the largest online node; free resources summed over online nodes.
    let nodes = state.nodes.all();
    let online: Vec<_> = nodes
        .iter()
        .filter(|n| n.is_online())
        .map(|n| n.facts().inventory)
        .collect();
    let max_cpus = online.iter().map(|i| i.cpus).fold(0.0, f64::max);
    let max_mem = online.iter().map(|i| i.mem_bytes).max().unwrap_or(0);
    let fits_gpu = |i: &ferry_core::Inventory| i.gpus_with_mem(res.gpu_mem_bytes);
    let max_gpus = online.iter().map(fits_gpu).max().unwrap_or(0);
    if res.cpus > max_cpus || res.mem_bytes > max_mem || res.gpus as usize > max_gpus {
        return format!(
            "will never run: request exceeds managed inventory ({} cpus, {} mem, {} gpus{})",
            max_cpus,
            ferry_core::resource::format_bytes(max_mem),
            max_gpus,
            if res.gpu_mem_bytes > 0 {
                format!(
                    " with >= {}",
                    ferry_core::resource::format_bytes(res.gpu_mem_bytes)
                )
            } else {
                String::new()
            }
        );
    }
    let mut free_cpus: f64 = online.iter().map(|i| i.cpus).sum();
    let mut free_mem: u64 = online.iter().map(|i| i.mem_bytes).sum();
    let mut free_gpus: usize = online.iter().map(fits_gpu).sum();
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
    let fits = res.cpus <= free_cpus && res.mem_bytes <= free_mem && res.gpus as usize <= free_gpus;
    if fits && queued_ahead == 0 {
        "resources available: expected to start immediately".into()
    } else {
        format!(
            "will queue: {queued_ahead} job(s) ahead, free now = {free_cpus:.1} cpus, {} mem, {free_gpus} gpus",
            ferry_core::resource::format_bytes(free_mem)
        )
    }
}

pub async fn submit(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Json(req): Json<SubmitJobRequest>,
) -> ApiResult<SubmitJobResponse> {
    // Steps of a task inherit the task's resources and environment.
    let task = match req.task_id {
        Some(tid) => {
            let t = state
                .store
                .task(tid)
                .await?
                .ok_or_else(|| ApiError::NotFound(format!("task {tid}")))?;
            if t.user_id != user.0.id && !user.is_admin() {
                return Err(ApiError::Forbidden("not your task".into()));
            }
            if t.state != TaskState::Open {
                return Err(ApiError::Conflict(format!(
                    "task {tid} is {}",
                    t.state.as_str()
                )));
            }
            if req.image.is_some() {
                return Err(ApiError::BadRequest(
                    "a task step runs in the task's environment; do not set image".into(),
                ));
            }
            Some(t)
        }
        None => None,
    };
    if let Some(n) = &req.node
        && state.nodes.get(n).is_none()
    {
        return Err(ApiError::BadRequest(format!(
            "node '{n}' is not registered"
        )));
    }
    let base = task
        .as_ref()
        .map(|t| t.resources.clone())
        .unwrap_or_default();
    let resources = req.resources.apply(&base);
    validate(&state, &req, &resources)?;
    let active = state.store.active_jobs().await?;
    let est = estimate(&state, &resources, &active);
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
    if req.preempt && !user.is_admin() {
        return Err(ApiError::Forbidden("only admins can preempt".into()));
    }
    // Who has to go so this request fits (admin --preempt). Decided before creating the job so a
    // dry run can show it.
    let victims = if req.preempt && !req.hold && req.array.is_none() {
        // Admins' jobs are never victims.
        let protected: std::collections::HashSet<i64> = state
            .store
            .list_users()
            .await?
            .into_iter()
            .filter(|u| u.role == ferry_core::Role::Admin)
            .map(|u| u.id)
            .collect();
        preemption_victims(&state, &resources, &active, req.node.as_deref(), &protected)
    } else {
        vec![]
    };
    let est = if victims.is_empty() {
        est
    } else {
        format!(
            "will preempt {} job(s): {}",
            victims.len(),
            victims
                .iter()
                .map(|v| format!("{} ({}, {})", v.id, v.name, v.user_name))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    if req.dry_run {
        return Ok(Json(SubmitJobResponse {
            dry_run: true,
            jobs: vec![],
            estimate: est,
        }));
    }
    // The preempting job must outrank its victims in the queue.
    let priority = if victims.is_empty() {
        req.priority
    } else {
        req.priority
            .max(victims.iter().map(|v| v.priority).max().unwrap_or(0) + 1)
    };
    let image = match &task {
        Some(t) => crate::tasks::head_image(&state, t).await.0,
        None => req
            .image
            .clone()
            .unwrap_or_else(|| state.config.default_image.clone()),
    };
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
                resources: resources.clone(),
                state: if req.hold {
                    JobState::Held
                } else {
                    JobState::Queued
                },
                priority,
                depends_on: req.depends_on.clone(),
                array_index: idx,
                time_limit_secs: req.time_limit_secs,
                task_id: req.task_id,
                requested_node: req.node.clone(),
            })
            .await?;
        for v in &victims {
            if state.request_preempt(v.id) {
                tracing::info!(victim = v.id, for_job = job.id, by = %user.0.name, "preempting");
            }
        }
        // Inline files are written now so they are present even if the job is held.
        if !req.files.is_empty() {
            match &task {
                Some(t) => {
                    let ws = crate::tasks::ensure_workspace(&state, t).await?;
                    let t = state.store.task(t.id).await?.unwrap();
                    state
                        .node_for_task(&t)?
                        .files
                        .write_files(&ws.root, &req.files)
                        .await?;
                }
                None => {
                    let node = staging_node(&state, &job).await?;
                    let root = Workspace::path_for(&node.scratch_root(), &user.0.name, job.id);
                    node.files.ensure_workspace(&root).await?;
                    node.files.write_files(&root, &req.files).await?;
                }
            }
        }
        state.events.publish(Event::JobUpdated { job: job.clone() });
        jobs.push(job);
    }
    if let Some(t) = &task {
        let _ = state.store.touch_task(t.id).await;
        crate::tasks::publish_task(&state, t.id);
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
                if let (Some(h), Ok(node)) = (&job.container_id, state.node_for_job(&job)) {
                    let _ = node.executor.kill(h).await;
                    let _ = node.executor.cleanup(h).await;
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

/// Where a job's files live: on its node, or in the control node's archive once the scratch
/// workspace was removed (archives have the same layout: out/, job.log or logs/<id>.log).
fn files_of(state: &AppState, job: &Job) -> Result<(Arc<dyn Files>, Workspace), ApiError> {
    if job.workspace_deleted
        && let Some(a) = &job.archive_path
    {
        return Ok((Arc::new(LocalFiles), Workspace::open(a.into())));
    }
    let node = state
        .node(job.node_id.as_deref().or(job.requested_node.as_deref()))
        .map_err(ApiError::Internal)?;
    let root = match (&job.workspace_path, job.task_id) {
        (Some(p), _) => std::path::PathBuf::from(p),
        (None, Some(t)) => Workspace::task_path_for(&node.scratch_root(), &job.user_name, t),
        (None, None) => Workspace::path_for(&node.scratch_root(), &job.user_name, job.id),
    };
    Ok((node.files.clone(), Workspace::open(root)))
}

/// Node that receives files for a job that has not started yet; pins the job to it.
async fn staging_node(state: &AppState, job: &Job) -> Result<Arc<Node>, ApiError> {
    let node = state
        .node(job.requested_node.as_deref())
        .map_err(ApiError::Internal)?;
    if job.requested_node.as_deref() != Some(&node.id) {
        state.store.set_requested_node(job.id, &node.id).await?;
    }
    Ok(node)
}

pub async fn log(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<JobId>,
    Query(q): Query<LogQuery>,
) -> ApiResult<LogChunk> {
    let job = load_job(&state, &user, id).await?;
    let (files, ws) = files_of(&state, &job)?;
    let (content, next) = files.read_log(&ws.log_for(&job), q.tail, q.offset).await?;
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
    let (files, ws) = files_of(&state, &job)?;
    Ok(Json(files.list_out(&ws.root).await?))
}

pub async fn result(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<JobId>,
) -> ApiResult<JobResult> {
    let job = load_job(&state, &user, id).await?;
    let (files, ws) = files_of(&state, &job)?;
    let artifacts = files.list_out(&ws.root).await?;
    let (log_tail, _) = files.read_log(&ws.log_for(&job), Some(100), None).await?;
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
    let (files, ws) = files_of(&state, &job)?;
    let file = ws
        .resolve_in(&ws.out(), &path)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let (len, stream) = files
        .open_file(&file)
        .await
        .map_err(|_| ApiError::NotFound(format!("artifact {path}")))?;
    let mime = mime_guess::from_path(&file).first_or_octet_stream();
    Ok((
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
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
    let (files, root) = match job.task_id {
        Some(tid) => {
            let t = state
                .store
                .task(tid)
                .await?
                .ok_or_else(|| ApiError::NotFound(format!("task {tid}")))?;
            let ws = crate::tasks::ensure_workspace(&state, &t).await?;
            let t = state.store.task(tid).await?.unwrap();
            (state.node_for_task(&t)?.files.clone(), ws.root)
        }
        None => {
            let node = staging_node(&state, &job).await?;
            let root = Workspace::path_for(&node.scratch_root(), &job.user_name, job.id);
            node.files.ensure_workspace(&root).await?;
            (node.files.clone(), root)
        }
    };
    let n = files.unpack_tar_gz(&root, body).await?;
    Ok(Json(serde_json::json!({ "job_id": id, "files": n })))
}

/// Admin preemption: the smallest set of running jobs on one node whose resources, added to
/// what is free there, fit `res`. Lowest priority first, then the youngest. Jobs of the admin
/// themselves and other admins' jobs are left alone.
fn preemption_victims(
    state: &AppState,
    res: &ResourceRequest,
    active: &[Job],
    pinned: Option<&str>,
    protected: &std::collections::HashSet<i64>,
) -> Vec<Job> {
    let mut best: Option<Vec<Job>> = None;
    for node in state.nodes.all().into_iter().filter(|n| n.is_online()) {
        if pinned.is_some_and(|p| p != node.id) {
            continue;
        }
        let inv = node.facts().inventory;
        let running: Vec<&Job> = active
            .iter()
            .filter(|j| j.state.is_active() && j.node_id.as_deref() == Some(node.id.as_str()))
            .collect();
        let mut cpus = inv.cpus - running.iter().map(|j| j.resources.cpus).sum::<f64>();
        let mut mem = inv
            .mem_bytes
            .saturating_sub(running.iter().map(|j| j.resources.mem_bytes).sum());
        let used: Vec<u32> = running.iter().flat_map(|j| j.gpu_ids.clone()).collect();
        let mut gpus: Vec<u32> = inv
            .gpus
            .iter()
            .copied()
            .filter(|g| !used.contains(g) && inv.gpu_mem_of(*g) >= res.gpu_mem_bytes)
            .collect();
        let fits = |c: f64, m: u64, g: &[u32]| {
            res.cpus <= c + 1e-9 && res.mem_bytes <= m && res.gpus as usize <= g.len()
        };
        if fits(cpus, mem, &gpus) {
            return vec![]; // it fits already; nobody has to go
        }
        let mut candidates: Vec<&Job> = running
            .iter()
            .copied()
            .filter(|j| !protected.contains(&j.user_id))
            .collect();
        candidates.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then(b.started_at.cmp(&a.started_at))
        });
        let mut chosen = vec![];
        for j in candidates {
            cpus += j.resources.cpus;
            mem += j.resources.mem_bytes;
            gpus.extend(
                j.gpu_ids
                    .iter()
                    .copied()
                    .filter(|g| inv.gpu_mem_of(*g) >= res.gpu_mem_bytes),
            );
            chosen.push(j.clone());
            if fits(cpus, mem, &gpus) {
                break;
            }
        }
        if fits(cpus, mem, &gpus) && best.as_ref().is_none_or(|b| chosen.len() < b.len()) {
            best = Some(chosen);
        }
    }
    best.unwrap_or_default()
}

/// Admin: kill a running job and put it back in the queue.
pub async fn preempt(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<JobId>,
) -> ApiResult<Job> {
    user.require_admin()?;
    let job = state
        .store
        .job(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("job {id}")))?;
    if !matches!(
        job.state,
        JobState::Running | JobState::Starting | JobState::Pulling
    ) {
        return Err(ApiError::Conflict(format!("job {id} is {}", job.state)));
    }
    if !state.request_preempt(id) {
        return Err(ApiError::Conflict(format!(
            "job {id} has no watcher; cancel it instead"
        )));
    }
    tracing::info!(job = id, by = %user.0.name, "preempting");
    publish_job(&state, id).await;
    Ok(Json(state.store.job(id).await?.unwrap()))
}

#[derive(serde::Deserialize)]
pub struct TrainQuery {
    /// File in /out (default metrics.jsonl).
    file: Option<String>,
    /// Max points per series (default 2000).
    max_points: Option<usize>,
}

/// Training curves from a JSON-lines file the job writes to /out (task steps share the task's /out).
pub async fn train_metrics(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<JobId>,
    Query(q): Query<TrainQuery>,
) -> ApiResult<ferry_core::api::TrainMetrics> {
    use futures::StreamExt;
    const CAP: usize = 32 << 20;
    let job = load_job(&state, &user, id).await?;
    let (files, ws) = files_of(&state, &job)?;
    let name = q.file.unwrap_or_else(|| "metrics.jsonl".into());
    let path = ws
        .resolve_in(&ws.out(), &name)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let (_, mut stream) = files.open_file(&path).await.map_err(|_| {
        ApiError::NotFound(format!(
            "/out/{name} not found (write one JSON object per line)"
        ))
    })?;
    // Keep only the last CAP bytes of very large files.
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ApiError::Internal(e.into()))?;
        buf.extend_from_slice(&chunk);
        if buf.len() > CAP * 2 {
            buf.drain(..buf.len() - CAP);
            truncated = true;
        }
    }
    if buf.len() > CAP {
        buf.drain(..buf.len() - CAP);
        truncated = true;
    }
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if truncated && let Some(nl) = text.find('\n') {
        text.drain(..=nl); // drop the partial first line
    }
    let mut m = ferry_core::api::TrainMetrics::parse(&name, &text, q.max_points.unwrap_or(2000));
    m.truncated = truncated;
    Ok(Json(m))
}

/// Interactive shell (WebSocket) inside a running job or task step. See `shell.rs` for the protocol.
pub async fn shell(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<JobId>,
    Query(q): Query<crate::shell::ShellQuery>,
    ws: axum::extract::WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let job = load_job(&state, &user, id).await?;
    if job.state != JobState::Running {
        return Err(ApiError::Conflict(format!(
            "job {id} is {}, not running",
            job.state
        )));
    }
    let handle = job
        .container_id
        .clone()
        .ok_or_else(|| ApiError::Conflict(format!("job {id} has no container yet")))?;
    let cmd = q
        .command()
        .map_err(|e| ApiError::BadRequest(format!("cmd: {e}")))?;
    let node = state.node_for_job(&job).map_err(ApiError::Internal)?;
    let work = job
        .workspace_path
        .as_ref()
        .map(|p| Workspace::open(p.into()).work())
        .unwrap_or_default();
    let (cols, rows) = (q.cols.unwrap_or(80), q.rows.unwrap_or(24));
    tracing::info!(job = id, user = %user.0.name, cmd = ?cmd, "shell opened");
    if let Some(remote) = &node.remote {
        let query = serde_urlencoded::to_string([
            ("handle", handle.as_str()),
            ("work", &work.to_string_lossy()),
            ("cmd", &serde_json::to_string(&cmd).unwrap_or_default()),
            ("cols", &cols.to_string()),
            ("rows", &rows.to_string()),
        ])
        .map_err(|e| ApiError::Internal(e.into()))?;
        let (url, token) = remote.agent.shell_url(&query);
        return Ok(ws.on_upgrade(move |socket| crate::shell::proxy_to_agent(socket, url, token)));
    }
    let target = node
        .executor
        .interactive(&handle, &work, &cmd)
        .ok_or_else(|| ApiError::BadRequest("this runtime has no interactive shell".into()))?;
    Ok(ws.on_upgrade(move |socket| crate::shell::bridge_pty(socket, target, cols, rows)))
}
