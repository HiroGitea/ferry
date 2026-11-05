//! Long-lived tasks: one environment, many steps.
//!
//! * Every step starts from the task's HEAD snapshot (or the base image) and shares the task's
//!   `/work` and `/out` on the SSD.
//! * A successful step is saved with `commit` as a new snapshot ("存档") and becomes HEAD.
//! * A failed / cancelled / timed-out step discards its environment: the task rolls back to HEAD.
//! * With `keep_warm_secs > 0` the environment container stays up (idle, holding no scheduler
//!   resources) after a successful step, and the next step with the same image/resources/GPUs is
//!   exec'd into it directly.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use mlq_core::api::Event;
use mlq_core::{Job, JobState, SnapshotId, Task, TaskId, TaskState};
use tracing::{error, info, warn};

use crate::executor::EnvSpec;
use crate::sched::{Outcome, finish_job, outcome_state, publish_job, wait_job};
use crate::state::AppState;
use crate::store::WarmSpec;
use crate::workspace::{Workspace, sanitize_name};

/// Snapshots kept per task in addition to HEAD and pinned ones.
pub const KEEP_SNAPSHOTS: usize = 5;

pub fn env_name(task_id: TaskId) -> String {
    format!("mlq-task-{task_id}")
}

pub fn snapshot_image(task_id: TaskId, seq: i64) -> String {
    format!("localhost/mlq-task-{task_id}:{seq}")
}

pub fn publish_task(state: &AppState, task_id: TaskId) {
    state.events.publish(Event::TaskUpdated { task_id });
}

pub fn task_workspace(state: &AppState, task: &Task) -> Workspace {
    match &task.workspace_path {
        Some(p) => Workspace::open(p.into()),
        None => {
            let root = state
                .node_for_task(task)
                .map(|n| n.scratch_root())
                .unwrap_or_else(|_| state.config.scratch_root());
            Workspace::open(Workspace::task_path_for(&root, &task.user_name, task.id))
        }
    }
}

/// Create the task's workspace on its node. A task without a node yet is placed on `prefer`
/// (the node of its first step) or the primary node.
pub async fn ensure_workspace(state: &AppState, task: &Task) -> anyhow::Result<Workspace> {
    ensure_workspace_on(state, task, None).await
}

pub async fn ensure_workspace_on(
    state: &AppState,
    task: &Task,
    prefer: Option<&str>,
) -> anyhow::Result<Workspace> {
    let node = match (&task.node_id, prefer) {
        (Some(n), _) => state.node(Some(n))?,
        (None, Some(p)) => state.node(Some(p))?,
        (None, None) => state.node(None)?,
    };
    if task.node_id.is_none() {
        state.store.set_task_node(task.id, &node.id).await?;
    }
    let root = match &task.workspace_path {
        Some(p) => std::path::PathBuf::from(p),
        None => Workspace::task_path_for(&node.scratch_root(), &task.user_name, task.id),
    };
    node.files.ensure_workspace(&root).await?;
    if task.workspace_path.is_none() {
        state
            .store
            .set_task_workspace(task.id, &root.to_string_lossy())
            .await?;
    }
    Ok(Workspace::open(root))
}

/// Image the next step starts from, and the snapshot it corresponds to.
pub async fn head_image(state: &AppState, task: &Task) -> (String, Option<SnapshotId>) {
    if let Some(sid) = task.head_snapshot_id {
        if let Ok(Some(s)) = state.store.snapshot(sid).await {
            if !s.deleted {
                return (s.image, Some(sid));
            }
        }
    }
    (task.base_image.clone(), None)
}

fn step_env(task: &Task, job: &Job, gpu_ids: &[u32]) -> BTreeMap<String, String> {
    let mut env = job.env.clone();
    env.insert("MLQ_JOB_ID".into(), job.id.to_string());
    env.insert("MLQ_JOB_NAME".into(), job.name.clone());
    env.insert("MLQ_TASK_ID".into(), task.id.to_string());
    env.insert("MLQ_TASK_NAME".into(), task.name.clone());
    env.insert("MLQ_USER".into(), job.user_name.clone());
    env.insert("MLQ_OUT".into(), "/out".into());
    if let Some(i) = job.array_index {
        env.insert("MLQ_ARRAY_INDEX".into(), i.to_string());
    }
    if !gpu_ids.is_empty() {
        env.insert(
            "MLQ_HOST_GPUS".into(),
            gpu_ids
                .iter()
                .map(|g| g.to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    env
}

async fn fail_early(state: &AppState, job_id: i64, reason: &str) {
    let _ = state
        .store
        .set_finished(job_id, JobState::Failed, None, Some(reason))
        .await;
    state.remove_cancel(job_id);
    publish_job(state, job_id).await;
    state.wake.notify_one();
}

/// Run one step of a task. Resources (`gpu_ids`) were already claimed by the scheduler.
pub async fn run_step(state: Arc<AppState>, mut job: Job, gpu_ids: Vec<u32>) {
    let Some(tid) = job.task_id else { return };
    let lock = state.task_lock(tid);
    let _guard = lock.lock().await;
    let token = state.cancel_token(job.id);

    let task = match state.store.task(tid).await {
        Ok(Some(t)) if t.state == TaskState::Open => t,
        Ok(Some(_)) => return fail_early(&state, job.id, "task is closed").await,
        _ => return fail_early(&state, job.id, "task not found").await,
    };
    let ws = match ensure_workspace_on(&state, &task, job.node_id.as_deref()).await {
        Ok(ws) => ws,
        Err(e) => return fail_early(&state, job.id, &format!("workspace: {e}")).await,
    };
    let task = state.store.task(tid).await.ok().flatten().unwrap_or(task);
    let node = match state.node_for_task(&task) {
        Ok(n) => n,
        Err(e) => return fail_early(&state, job.id, &e.to_string()).await,
    };
    let log_path = ws.log_for(&job);
    let (image, base_snap) = head_image(&state, &task).await;
    let _ = state.store.set_job_image(job.id, &image).await;
    let _ = state.store.set_job_snapshots(job.id, base_snap, None).await;
    job.image = image.clone();
    job.gpu_ids = gpu_ids.clone();

    // Reuse the warm environment if it was started with exactly this setup.
    let want = WarmSpec {
        image: image.clone(),
        cpus: job.resources.cpus,
        mem_bytes: job.resources.mem_bytes,
        gpus: gpu_ids.clone(),
    };
    let mut env = None;
    let mut reused = false;
    if let Some(c) = &task.warm_container {
        let spec = state.store.task_warm_spec(tid).await.ok().flatten();
        let alive = node.executor.is_alive(c).await.unwrap_or(false);
        if alive && spec.as_ref() == Some(&want) {
            info!(task = tid, job = job.id, "reusing warm environment");
            env = Some(c.clone());
            reused = true;
        } else {
            info!(
                task = tid,
                "warm environment does not match this step; replacing it"
            );
            let _ = node.executor.remove_env(c).await;
        }
        let _ = state.store.clear_task_warm(tid).await;
    }

    if env.is_none() {
        if base_snap.is_none() && !node.executor.image_present(&image).await.unwrap_or(false) {
            let _ = state
                .store
                .set_state(job.id, JobState::Pulling, Some("pulling image"))
                .await;
            publish_job(&state, job.id).await;
            tokio::select! {
                r = node.executor.pull_image(&image) => {
                    if let Err(e) = r {
                        return fail_early(&state, job.id, &format!("image pull failed: {e}")).await;
                    }
                }
                _ = token.cancelled() => {
                    let _ = state.store.set_finished(job.id, JobState::Cancelled, None, Some("cancelled during image pull")).await;
                    state.remove_cancel(job.id);
                    publish_job(&state, job.id).await;
                    return;
                }
            }
        }
        let spec = EnvSpec {
            name: env_name(tid),
            labels: vec![
                ("mlq.task".into(), tid.to_string()),
                ("mlq.user".into(), job.user_name.clone()),
            ],
            image: image.clone(),
            user_name: job.user_name.clone(),
            resources: job.resources.clone(),
            gpu_ids: gpu_ids.clone(),
            workspace: ws.clone(),
            mounts: node.mounts(),
            env: BTreeMap::new(),
        };
        match node.executor.start_env(&spec).await {
            Ok(h) => env = Some(h),
            Err(e) => {
                error!(task = tid, job = job.id, "environment start failed: {e:#}");
                return fail_early(&state, job.id, &format!("environment start failed: {e}")).await;
            }
        }
    }
    let env = env.unwrap();
    let vars = step_env(&task, &job, &gpu_ids);
    let exec = match node.executor.exec(&env, &job, &ws, &vars, &log_path).await {
        Ok(h) => h,
        Err(e) => {
            let _ = node.executor.remove_env(&env).await;
            return fail_early(&state, job.id, &format!("exec failed: {e}")).await;
        }
    };
    // The exec handle (environment + exit file) is persisted so a restarted daemon can re-wait.
    let ws_str = ws.root.to_string_lossy().to_string();
    let _ = state.store.set_container(job.id, &exec).await;
    let _ = state
        .store
        .set_started(job.id, &gpu_ids, &node.id, &ws_str)
        .await;
    let _ = state.store.touch_task(tid).await;
    info!(task = tid, job = job.id, reused, gpus = ?gpu_ids, "step started");
    publish_job(&state, job.id).await;
    publish_task(&state, tid);
    job.container_id = Some(exec.clone());
    job.started_at = Some(Utc::now());
    job.base_snapshot_id = base_snap;
    watch_step(&state, job, env, exec, token).await;
}

/// Wait for a started step, then save (success) or discard (anything else) the environment.
/// Also used to re-attach to a step after a daemon restart; the task lock must be held.
async fn watch_step(
    state: &Arc<AppState>,
    job: Job,
    env: String,
    exec: String,
    token: tokio_util::sync::CancellationToken,
) {
    let tid = job.task_id.unwrap_or_default();
    let base_snap = job.base_snapshot_id;
    let image = job.image.clone();
    let gpu_ids = job.gpu_ids.clone();
    let Ok(node) = state.node_for_job(&job) else {
        return fail_early(state, job.id, "node not found").await;
    };
    let outcome = wait_job(state, &job, &exec, token).await;
    let _ = node.executor.cleanup(&exec).await;

    // Save or discard the environment.
    let mut produced: Option<SnapshotId> = None;
    let mut note: Option<String> = None;
    let Some(task_now) = state.store.task(tid).await.ok().flatten() else {
        let _ = node.executor.remove_env(&env).await;
        return fail_early(state, job.id, "task vanished").await;
    };
    if matches!(outcome, Outcome::Exited(0)) {
        let mut next_image = image.clone();
        if node.executor.supports_snapshots() {
            match save_snapshot(state, &node, tid, base_snap, job.id, &env).await {
                Ok((sid, tag)) => {
                    produced = Some(sid);
                    next_image = tag;
                }
                Err(e) => {
                    warn!(task = tid, "snapshot failed: {e:#}");
                    note = Some(format!(
                        "step succeeded but the snapshot failed ({e}); HEAD unchanged"
                    ));
                }
            }
        }
        let keep = task_now.keep_warm_secs;
        let idle = node.executor.env_idle(&env).await.unwrap_or(false);
        if keep > 0 && idle && (produced.is_some() || !node.executor.supports_snapshots()) {
            let until = Utc::now() + chrono::Duration::seconds(keep as i64);
            let spec = WarmSpec {
                image: next_image,
                cpus: job.resources.cpus,
                mem_bytes: job.resources.mem_bytes,
                gpus: gpu_ids.clone(),
            };
            if let Err(e) = state.store.set_task_warm(tid, &env, until, &spec).await {
                warn!(task = tid, "recording warm container failed: {e:#}");
                let _ = node.executor.remove_env(&env).await;
            }
        } else {
            if keep > 0 && !idle {
                note = Some("background processes were still running after the step; environment not kept warm".into());
            }
            let _ = node.executor.remove_env(&env).await;
        }
    } else {
        // Roll back: discard everything since HEAD.
        let _ = node.executor.remove_env(&env).await;
        if node.executor.supports_snapshots() {
            note = Some(match base_snap {
                Some(s) => format!("environment rolled back to snapshot {s}"),
                None => "environment rolled back to the base image".into(),
            });
        }
    }

    let (st, code, mut reason) = outcome_state(outcome);
    if let Some(n) = note {
        reason = Some(match reason {
            Some(r) => format!("{r}; {n}"),
            None => n,
        });
    }
    let _ = state
        .store
        .set_job_snapshots(job.id, base_snap, produced)
        .await;
    finish_job(state, job.id, st, code, reason.as_deref()).await;
    let _ = state.store.touch_task(tid).await;
    if state.config.retention.archive_dir.is_some() {
        if let Some(t) = state.store.task(tid).await.ok().flatten() {
            match crate::retention::archive_task(state, &t).await {
                Ok(p) => {
                    let _ = state
                        .store
                        .mark_archived(job.id, &p.to_string_lossy())
                        .await;
                }
                Err(e) => warn!(task = tid, "archive failed: {e:#}"),
            }
        }
    }
    publish_task(state, tid);
}

/// Commit the environment as the task's new HEAD and prune old snapshots.
async fn save_snapshot(
    state: &AppState,
    node: &crate::node::Node,
    tid: TaskId,
    parent: Option<SnapshotId>,
    job_id: i64,
    env: &str,
) -> anyhow::Result<(SnapshotId, String)> {
    let seq = state.store.next_snapshot_seq(tid).await?;
    let tag = snapshot_image(tid, seq);
    let started = std::time::Instant::now();
    let size = node.executor.commit(&env.to_string(), &tag).await?;
    let snap = state
        .store
        .create_snapshot(tid, parent, Some(job_id), &tag, size)
        .await?;
    state.store.set_task_head(tid, Some(snap.id)).await?;
    info!(task = tid, snapshot = snap.id, image = %tag, ms = started.elapsed().as_millis() as u64, "snapshot saved");
    prune_snapshots(state, tid).await;
    Ok((snap.id, tag))
}

/// Remove images of old snapshots beyond `KEEP_SNAPSHOTS` (never HEAD or pinned ones).
pub async fn prune_snapshots(state: &AppState, tid: TaskId) {
    let Ok(Some(task)) = state.store.task(tid).await else {
        return;
    };
    let Ok(node) = state.node_for_task(&task) else {
        return;
    };
    let Ok(snaps) = state.store.snapshots(tid).await else {
        return;
    };
    let candidates: Vec<_> = snaps
        .iter()
        .filter(|s| !s.deleted && !s.pinned && Some(s.id) != task.head_snapshot_id)
        .collect();
    if candidates.len() <= KEEP_SNAPSHOTS {
        return;
    }
    for s in &candidates[..candidates.len() - KEEP_SNAPSHOTS] {
        if let Err(e) = node.executor.remove_image(&s.image).await {
            warn!(snapshot = s.id, "removing image failed: {e:#}");
        }
        let _ = state.store.mark_snapshot_deleted(s.id).await;
    }
}

/// Stop a task's warm environment, if any.
pub async fn drop_warm(state: &AppState, task: &Task) {
    if let Some(c) = &task.warm_container {
        if let Ok(node) = state.node_for_task(task) {
            let _ = node.executor.remove_env(c).await;
        }
        let _ = state.store.clear_task_warm(task.id).await;
    }
}

/// Background loop: stop warm environments whose keep-warm window has passed.
pub fn spawn_warm_reaper(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        loop {
            tick.tick().await;
            let Ok(tasks) = state.store.warm_tasks().await else {
                continue;
            };
            for t in tasks {
                if t.warm_until.map(|u| u < Utc::now()).unwrap_or(true) {
                    let lock = state.task_lock(t.id);
                    let Ok(_g) = lock.try_lock() else { continue };
                    // Re-read under the lock: a step may have just started using it.
                    if let Ok(Some(t)) = state.store.task(t.id).await {
                        if t.warm_until.map(|u| u < Utc::now()).unwrap_or(true) {
                            info!(task = t.id, "warm window over; stopping environment");
                            drop_warm(&state, &t).await;
                            publish_task(&state, t.id);
                        }
                    }
                }
            }
        }
    });
}

/// After a restart: steps whose environment is still running are re-attached (they run
/// detached inside the container and leave an exit file), the rest fail and roll back. Warm
/// environments still within their window are kept.
pub async fn recover(state: &Arc<AppState>) -> anyhow::Result<()> {
    for job in state.store.active_jobs().await? {
        let Some(tid) = job.task_id else { continue };
        if !job.state.is_active() {
            continue;
        }
        let exec = job.container_id.clone().unwrap_or_default();
        let Ok(node) = state.node_for_job(&job) else {
            continue;
        };
        let env = node.executor.env_of(&exec);
        let alive = job.state == JobState::Running
            && env.is_some()
            && node.executor.is_alive(&exec).await.unwrap_or(false);
        if alive {
            info!(job = job.id, task = tid, "re-attaching to running step");
            let st = state.clone();
            let env = env.unwrap();
            tokio::spawn(async move {
                let lock = st.task_lock(tid);
                let _g = lock.lock().await;
                let token = st.cancel_token(job.id);
                watch_step(&st, job, env, exec, token).await;
            });
            continue;
        }
        warn!(job = job.id, "task step cannot be re-attached; failing it");
        if let Some(e) = env.or(job.container_id.clone()) {
            let _ = node.executor.remove_env(&e).await;
        }
        state
            .store
            .set_finished(
                job.id,
                JobState::Failed,
                None,
                Some("daemon restarted during the step; environment rolled back"),
            )
            .await?;
    }
    for t in state.store.warm_tasks().await? {
        let keep = t.warm_until.map(|u| u > Utc::now()).unwrap_or(false)
            && match (&t.warm_container, state.node_for_task(&t)) {
                (Some(c), Ok(node)) => node.executor.is_alive(c).await.unwrap_or(false),
                _ => false,
            };
        if !keep {
            drop_warm(state, &t).await;
        }
    }
    Ok(())
}

/// Close a task: cancel queued steps, stop the warm environment, archive outputs.
pub async fn close(state: &AppState, task: &Task) -> anyhow::Result<()> {
    for j in state.store.task_steps(task.id).await? {
        if matches!(j.state, JobState::Queued | JobState::Held) {
            state
                .store
                .set_finished(j.id, JobState::Cancelled, None, Some("task closed"))
                .await?;
            publish_job(state, j.id).await;
        }
    }
    drop_warm(state, task).await;
    if state.config.retention.archive_dir.is_some() && task.workspace_path.is_some() {
        if let Err(e) = crate::retention::archive_task(state, task).await {
            warn!(task = task.id, "archive on close failed: {e:#}");
        }
    }
    state
        .store
        .set_task_state(task.id, TaskState::Closed)
        .await?;
    // Self-contained .7z on the NAS; the per-step mirror directory is then redundant.
    if state.config.archive.on_close && state.config.retention.archive_dir.is_some() {
        let policy = crate::archive::ImagePolicy::parse(&state.config.archive.images)
            .unwrap_or(crate::archive::ImagePolicy::Head);
        if let Some(t) = state.store.task(task.id).await? {
            match crate::archive::export_task(state, &t, policy).await {
                Ok(r) => {
                    state.store.set_task_archive(task.id, &r.path).await?;
                    if let Some(root) = &state.config.retention.archive_dir {
                        let mirror = archive_dir_for(root, &t);
                        if mirror.is_dir() {
                            let _ = std::fs::remove_dir_all(&mirror);
                        }
                    }
                }
                Err(e) => warn!(
                    task = task.id,
                    "7z export on close failed (mirror directory kept): {e:#}"
                ),
            }
        }
    }
    Ok(())
}

/// Delete a closed task: remove its snapshot images and SSD workspace (outputs stay in the
/// archive if one is configured).
pub async fn delete(state: &AppState, task: &Task) -> anyhow::Result<()> {
    drop_warm(state, task).await;
    let node = state.node_for_task(task)?;
    for s in state.store.snapshots(task.id).await? {
        if !s.deleted {
            let _ = node.executor.remove_image(&s.image).await;
            let _ = state.store.mark_snapshot_deleted(s.id).await;
        }
    }
    if let Some(p) = &task.workspace_path {
        let archived_ok = state.config.retention.archive_dir.is_none()
            || crate::retention::archive_task(state, task).await.is_ok();
        if archived_ok {
            let _ = node.files.remove_dir(std::path::Path::new(p)).await;
        } else {
            anyhow::bail!("archiving the task failed; workspace kept at {p}");
        }
    }
    state
        .store
        .set_task_state(task.id, TaskState::Deleted)
        .await?;
    Ok(())
}

/// Archive directory for a task: `<archive_dir>/<user>/task-<id>`.
pub fn archive_dir_for(archive_root: &std::path::Path, task: &Task) -> PathBuf {
    archive_root
        .join(sanitize_name(&task.user_name))
        .join(format!("task-{}", task.id))
}
