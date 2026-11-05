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
use crate::sched::{NODE_ID, Outcome, finish_job, outcome_state, publish_job, wait_job};
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
        None => Workspace::open(Workspace::task_path_for(
            &state.config.scratch_root(),
            &task.user_name,
            task.id,
        )),
    }
}

pub async fn ensure_workspace(state: &AppState, task: &Task) -> anyhow::Result<Workspace> {
    let ws = Workspace::create(task_workspace(state, task).root)?;
    std::fs::create_dir_all(ws.root.join("logs"))?;
    if task.workspace_path.is_none() {
        state
            .store
            .set_task_workspace(task.id, &ws.root.to_string_lossy())
            .await?;
    }
    Ok(ws)
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
    let ws = match ensure_workspace(&state, &task).await {
        Ok(ws) => ws,
        Err(e) => return fail_early(&state, job.id, &format!("workspace: {e}")).await,
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
        let alive = state.executor.is_alive(c).await.unwrap_or(false);
        if alive && spec.as_ref() == Some(&want) {
            info!(task = tid, job = job.id, "reusing warm environment");
            env = Some(c.clone());
            reused = true;
        } else {
            info!(
                task = tid,
                "warm environment does not match this step; replacing it"
            );
            let _ = state.executor.remove_env(c).await;
        }
        let _ = state.store.clear_task_warm(tid).await;
    }

    if env.is_none() {
        if base_snap.is_none() && !state.executor.image_present(&image).await.unwrap_or(false) {
            let _ = state
                .store
                .set_state(job.id, JobState::Pulling, Some("pulling image"))
                .await;
            publish_job(&state, job.id).await;
            tokio::select! {
                r = state.executor.pull_image(&image) => {
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
            mounts: state.config.mounts.clone(),
            env: BTreeMap::new(),
        };
        match state.executor.start_env(&spec).await {
            Ok(h) => env = Some(h),
            Err(e) => {
                error!(task = tid, job = job.id, "environment start failed: {e:#}");
                return fail_early(&state, job.id, &format!("environment start failed: {e}")).await;
            }
        }
    }
    let env = env.unwrap();
    let ws_str = ws.root.to_string_lossy().to_string();
    let _ = state.store.set_container(job.id, &env).await;
    let _ = state
        .store
        .set_started(job.id, &gpu_ids, NODE_ID, &ws_str)
        .await;
    let _ = state.store.touch_task(tid).await;
    info!(task = tid, job = job.id, reused, gpus = ?gpu_ids, "step started");
    publish_job(&state, job.id).await;
    publish_task(&state, tid);

    let vars = step_env(&task, &job, &gpu_ids);
    let exec = match state.executor.exec(&env, &job, &ws, &vars, &log_path).await {
        Ok(h) => h,
        Err(e) => {
            let _ = state.executor.remove_env(&env).await;
            return fail_early(&state, job.id, &format!("exec failed: {e}")).await;
        }
    };
    let outcome = wait_job(&state, &job, &exec, token).await;
    let _ = state.executor.cleanup(&exec).await;

    // Save or discard the environment.
    let mut produced: Option<SnapshotId> = None;
    let mut note: Option<String> = None;
    let task_now = state.store.task(tid).await.ok().flatten().unwrap_or(task);
    if matches!(outcome, Outcome::Exited(0)) {
        let mut next_image = image.clone();
        if state.executor.supports_snapshots() {
            match save_snapshot(&state, tid, base_snap, job.id, &env).await {
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
        let idle = state.executor.env_idle(&env).await.unwrap_or(false);
        if keep > 0 && idle && (produced.is_some() || !state.executor.supports_snapshots()) {
            let until = Utc::now() + chrono::Duration::seconds(keep as i64);
            let spec = WarmSpec {
                image: next_image,
                cpus: job.resources.cpus,
                mem_bytes: job.resources.mem_bytes,
                gpus: gpu_ids.clone(),
            };
            if let Err(e) = state.store.set_task_warm(tid, &env, until, &spec).await {
                warn!(task = tid, "recording warm container failed: {e:#}");
                let _ = state.executor.remove_env(&env).await;
            }
        } else {
            if keep > 0 && !idle {
                note = Some("background processes were still running after the step; environment not kept warm".into());
            }
            let _ = state.executor.remove_env(&env).await;
        }
    } else {
        // Roll back: discard everything since HEAD.
        let _ = state.executor.remove_env(&env).await;
        if state.executor.supports_snapshots() {
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
    finish_job(&state, job.id, st, code, reason.as_deref()).await;
    let _ = state.store.touch_task(tid).await;
    if state.config.retention.archive_dir.is_some() {
        if let Some(t) = state.store.task(tid).await.ok().flatten() {
            match crate::retention::archive_task(&state, &t).await {
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
    publish_task(&state, tid);
}

/// Commit the environment as the task's new HEAD and prune old snapshots.
async fn save_snapshot(
    state: &AppState,
    tid: TaskId,
    parent: Option<SnapshotId>,
    job_id: i64,
    env: &str,
) -> anyhow::Result<(SnapshotId, String)> {
    let seq = state.store.next_snapshot_seq(tid).await?;
    let tag = snapshot_image(tid, seq);
    let started = std::time::Instant::now();
    let size = state.executor.commit(&env.to_string(), &tag).await?;
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
        if let Err(e) = state.executor.remove_image(&s.image).await {
            warn!(snapshot = s.id, "removing image failed: {e:#}");
        }
        let _ = state.store.mark_snapshot_deleted(s.id).await;
    }
}

/// Stop a task's warm environment, if any.
pub async fn drop_warm(state: &AppState, task: &Task) {
    if let Some(c) = &task.warm_container {
        let _ = state.executor.remove_env(c).await;
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

/// After a restart: running steps cannot be re-attached (their exec sessions died with the
/// daemon), so they fail and their environments roll back. Warm environments that are still
/// within their window are kept.
pub async fn recover(state: &AppState) -> anyhow::Result<()> {
    for job in state.store.active_jobs().await? {
        if job.task_id.is_none() || !job.state.is_active() {
            continue;
        }
        warn!(
            job = job.id,
            "task step was running when the daemon stopped; failing it"
        );
        if let Some(c) = &job.container_id {
            let _ = state.executor.remove_env(c).await;
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
            && match &t.warm_container {
                Some(c) => state.executor.is_alive(c).await.unwrap_or(false),
                None => false,
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
    Ok(())
}

/// Delete a closed task: remove its snapshot images and SSD workspace (outputs stay in the
/// archive if one is configured).
pub async fn delete(state: &AppState, task: &Task) -> anyhow::Result<()> {
    drop_warm(state, task).await;
    for s in state.store.snapshots(task.id).await? {
        if !s.deleted {
            let _ = state.executor.remove_image(&s.image).await;
            let _ = state.store.mark_snapshot_deleted(s.id).await;
        }
    }
    if let Some(p) = &task.workspace_path {
        let archived_ok = state.config.retention.archive_dir.is_none()
            || crate::retention::archive_task(state, task).await.is_ok();
        if archived_ok {
            let _ = std::fs::remove_dir_all(p);
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
