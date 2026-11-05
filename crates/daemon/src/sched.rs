//! Scheduler loop and job lifecycle.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use mlq_core::api::Event;
use mlq_core::scheduler::{Decision, SchedulerInput, schedule};
use mlq_core::{Inventory, Job, JobId, JobState, UserId};
use tracing::{error, info, warn};

use crate::executor::RunSpec;
use crate::state::AppState;
use crate::workspace::Workspace;

pub const NODE_ID: &str = "local";

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        if let Err(e) = recover(&state).await {
            error!("recovery failed: {e:#}");
        }
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = state.wake.notified() => {}
            }
            if let Err(e) = tick(&state).await {
                error!("scheduler tick failed: {e:#}");
            }
        }
    });
}

/// Host CPU count and total memory.
pub fn host_resources() -> (f64, u64) {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1) as f64;
    let mem = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
        })
        .map(|kb| kb * 1024)
        .unwrap_or(0);
    (cpus, mem)
}

/// The managed inventory: host resources minus reservations, GPUs filtered by `managed_gpus`.
pub fn inventory(state: &AppState) -> Inventory {
    let cfg = &state.config;
    let (host_cpus, host_mem) = host_resources();
    let cpus = cfg
        .max_cpus
        .unwrap_or((host_cpus - cfg.reserve_cpus).max(0.0));
    let mem = cfg
        .max_mem
        .as_deref()
        .and_then(|m| mlq_core::parse_bytes(m).ok())
        .unwrap_or(host_mem.saturating_sub(cfg.reserve_mem_bytes()));
    let detected: Vec<u32> = state.gpu.list().into_iter().map(|g| g.index).collect();
    let gpus = match &cfg.managed_gpus {
        Some(list) => list
            .iter()
            .copied()
            .filter(|g| detected.contains(g))
            .collect(),
        None => detected,
    };
    Inventory {
        cpus,
        mem_bytes: mem,
        gpus,
    }
}

/// Decayed GPU-seconds per user for fair-share ordering.
pub async fn fair_usage(state: &AppState, active: &[Job]) -> anyhow::Result<HashMap<UserId, f64>> {
    let mut usage: HashMap<UserId, f64> = HashMap::new();
    if !state.config.fair_share.enabled {
        return Ok(usage);
    }
    let now = Utc::now();
    let half_life = state.config.fair_share.half_life_secs;
    let since = now - chrono::Duration::seconds((half_life * 6.0) as i64);
    for j in state.store.finished_jobs_since(since).await? {
        if let Some(fin) = j.finished_at {
            *usage.entry(j.user_id).or_default() +=
                j.gpu_seconds(now) * mlq_core::fair_share::decay_weight(fin, now, half_life);
        }
    }
    for j in active.iter().filter(|j| j.state.is_active()) {
        *usage.entry(j.user_id).or_default() += j.gpu_seconds(now);
    }
    Ok(usage)
}

async fn recover(state: &Arc<AppState>) -> anyhow::Result<()> {
    for job in state.store.active_jobs().await? {
        if !job.state.is_active() {
            continue;
        }
        match &job.container_id {
            Some(h) if state.executor.is_alive(h).await.unwrap_or(false) => {
                info!(job = job.id, "re-attaching to running job");
                let st = state.clone();
                tokio::spawn(async move {
                    let handle = job.container_id.clone().unwrap();
                    let token = st.cancel_token(job.id);
                    let res = wait_job(&st, &job, &handle, token).await;
                    finalize(&st, job.id, &handle, res).await;
                });
            }
            _ => {
                warn!(
                    job = job.id,
                    "job was active before restart but is gone; marking failed"
                );
                state
                    .store
                    .set_finished(
                        job.id,
                        JobState::Failed,
                        None,
                        Some("daemon restarted while job was running"),
                    )
                    .await?;
            }
        }
    }
    Ok(())
}

pub async fn tick(state: &Arc<AppState>) -> anyhow::Result<()> {
    let active = state.store.active_jobs().await?;
    if active.iter().all(|j| j.state != JobState::Queued) {
        return Ok(());
    }
    // Include terminal dependencies so the scheduler can evaluate them.
    let mut jobs = active.clone();
    let dep_ids: Vec<JobId> = active
        .iter()
        .flat_map(|j| j.depends_on.iter().copied())
        .filter(|d| !active.iter().any(|j| j.id == *d))
        .collect();
    jobs.extend(state.store.jobs_by_ids(&dep_ids).await?);

    let inv = inventory(state);
    let quotas = state.store.all_quotas().await?;
    let fair = fair_usage(state, &active).await?;
    let decisions = schedule(SchedulerInput {
        inventory: &inv,
        jobs: &jobs,
        quotas: &quotas,
        fair_usage: &fair,
    });

    for d in decisions {
        match d {
            Decision::Start(a) => {
                let Some(job) = jobs.iter().find(|j| j.id == a.job_id).cloned() else {
                    continue;
                };
                launch(state.clone(), job, a.gpu_ids).await;
            }
            Decision::Reject { job_id, reason } => {
                warn!(job = job_id, %reason, "rejecting job");
                state
                    .store
                    .set_finished(job_id, JobState::Failed, None, Some(&reason))
                    .await?;
                publish_job(state, job_id).await;
            }
            Decision::Wait { job_id, reason } => {
                if let Some(j) = jobs.iter().find(|j| j.id == job_id) {
                    if j.reason.as_deref() != Some(&reason) {
                        state.store.set_reason(job_id, Some(&reason)).await?;
                    }
                }
            }
        }
    }
    Ok(())
}

pub async fn publish_job(state: &AppState, id: JobId) {
    if let Ok(Some(job)) = state.store.job(id).await {
        state.events.publish(Event::JobUpdated { job });
    }
}

/// Prepare the workspace, pull the image if needed, start the container and watch it.
async fn launch(state: Arc<AppState>, job: Job, gpu_ids: Vec<u32>) {
    let ws_path = Workspace::path_for(&state.config.scratch_root(), &job.user_name, job.id);
    let ws = match Workspace::create(ws_path.clone()) {
        Ok(ws) => ws,
        Err(e) => {
            let _ = state
                .store
                .set_finished(
                    job.id,
                    JobState::Failed,
                    None,
                    Some(&format!("workspace: {e}")),
                )
                .await;
            publish_job(&state, job.id).await;
            return;
        }
    };
    let ws_str = ws_path.to_string_lossy().to_string();
    // Claim the resources immediately so the next tick sees them as taken.
    if let Err(e) = state
        .store
        .set_assigned(job.id, &gpu_ids, NODE_ID, &ws_str)
        .await
    {
        error!(job = job.id, "db: {e:#}");
        return;
    }
    publish_job(&state, job.id).await;

    tokio::spawn(async move {
        let token = state.cancel_token(job.id);
        // Image pull.
        let present = state
            .executor
            .image_present(&job.image)
            .await
            .unwrap_or(false);
        if !present {
            info!(job = job.id, image = %job.image, "pulling image");
            let _ = state
                .store
                .set_state(job.id, JobState::Pulling, Some("pulling image"))
                .await;
            publish_job(&state, job.id).await;
            let pull = state.executor.pull_image(&job.image);
            tokio::select! {
                r = pull => {
                    if let Err(e) = r {
                        let _ = state.store.set_finished(job.id, JobState::Failed, None, Some(&format!("image pull failed: {e}"))).await;
                        state.remove_cancel(job.id);
                        publish_job(&state, job.id).await;
                        state.wake.notify_one();
                        return;
                    }
                }
                _ = token.cancelled() => {
                    let _ = state.store.set_finished(job.id, JobState::Cancelled, None, Some("cancelled during image pull")).await;
                    state.remove_cancel(job.id);
                    publish_job(&state, job.id).await;
                    state.wake.notify_one();
                    return;
                }
            }
        }

        let mut env: BTreeMap<String, String> = job.env.clone();
        env.insert("MLQ_JOB_ID".into(), job.id.to_string());
        env.insert("MLQ_JOB_NAME".into(), job.name.clone());
        env.insert("MLQ_USER".into(), job.user_name.clone());
        env.insert("MLQ_OUT".into(), "/out".into());
        if let Some(i) = job.array_index {
            env.insert("MLQ_ARRAY_INDEX".into(), i.to_string());
        }
        if !gpu_ids.is_empty() {
            // Inside the container the assigned GPUs are renumbered from 0 by CDI; expose the
            // host indices for logging purposes only.
            env.insert(
                "MLQ_HOST_GPUS".into(),
                gpu_ids
                    .iter()
                    .map(|g| g.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        let spec = RunSpec {
            job: job.clone(),
            workspace: ws.clone(),
            gpu_ids: gpu_ids.clone(),
            mounts: state.config.mounts.clone(),
            env,
            log_path: ws.log(),
        };
        let handle = match state.executor.start(&spec).await {
            Ok(h) => h,
            Err(e) => {
                error!(job = job.id, "start failed: {e:#}");
                let _ = state
                    .store
                    .set_finished(
                        job.id,
                        JobState::Failed,
                        None,
                        Some(&format!("start failed: {e}")),
                    )
                    .await;
                state.remove_cancel(job.id);
                publish_job(&state, job.id).await;
                state.wake.notify_one();
                return;
            }
        };
        let _ = state.store.set_container(job.id, &handle).await;
        let _ = state
            .store
            .set_started(job.id, &gpu_ids, NODE_ID, &ws_str)
            .await;
        info!(job = job.id, user = %job.user_name, gpus = ?gpu_ids, "job started");
        publish_job(&state, job.id).await;
        let res = wait_job(&state, &job, &handle, token).await;
        finalize(&state, job.id, &handle, res).await;
    });
}

enum Outcome {
    Exited(i32),
    Cancelled,
    Timeout,
    Error(String),
}

async fn wait_job(
    state: &AppState,
    job: &Job,
    handle: &str,
    token: tokio_util::sync::CancellationToken,
) -> Outcome {
    let handle = handle.to_string();
    let limit = job.time_limit_secs.map(|s| {
        // Account for time already spent if we are re-attaching after a restart.
        let elapsed = job
            .started_at
            .map(|st| (Utc::now() - st).num_seconds().max(0) as u64)
            .unwrap_or(0);
        Duration::from_secs(s.saturating_sub(elapsed).max(1))
    });
    let wait = state.executor.wait(&handle);
    tokio::pin!(wait);
    let timeout = async {
        match limit {
            Some(d) => tokio::time::sleep(d).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        r = &mut wait => match r {
            Ok(code) => Outcome::Exited(code),
            Err(e) => Outcome::Error(e.to_string()),
        },
        _ = token.cancelled() => {
            let _ = state.executor.kill(&handle).await;
            let _ = wait.await;
            Outcome::Cancelled
        }
        _ = timeout => {
            let _ = state.executor.kill(&handle).await;
            let _ = wait.await;
            Outcome::Timeout
        }
    }
}

async fn finalize(state: &AppState, job_id: JobId, handle: &str, outcome: Outcome) {
    let (st, code, reason) = match outcome {
        Outcome::Exited(0) => (JobState::Succeeded, Some(0), None),
        Outcome::Exited(c) => (JobState::Failed, Some(c), Some(format!("exit code {c}"))),
        Outcome::Cancelled => (
            JobState::Cancelled,
            None,
            Some("cancelled by user".to_string()),
        ),
        Outcome::Timeout => (
            JobState::Timeout,
            None,
            Some("time limit exceeded".to_string()),
        ),
        Outcome::Error(e) => (JobState::Failed, None, Some(e)),
    };
    let _ = state.executor.cleanup(&handle.to_string()).await;
    if let Err(e) = state
        .store
        .set_finished(job_id, st, code, reason.as_deref())
        .await
    {
        error!(job = job_id, "db: {e:#}");
    }
    state.remove_cancel(job_id);
    info!(job = job_id, state = %st, "job finished");
    publish_job(state, job_id).await;
    state.wake.notify_one();
    // Copy results to the archive (NAS) right away so the SSD copy is only a cache.
    if state.config.retention.archive_dir.is_some() {
        if let Ok(Some(job)) = state.store.job(job_id).await {
            match crate::retention::archive_job(state, &job).await {
                Ok(p) => {
                    info!(job = job_id, path = %p.display(), "results archived");
                    publish_job(state, job_id).await;
                }
                Err(e) => warn!(
                    job = job_id,
                    "archive failed (will retry in retention sweep): {e:#}"
                ),
            }
        }
    }
}
