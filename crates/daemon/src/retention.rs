//! Lifecycle of SSD scratch workspaces: archive `/out` + log to the NAS (optional) and delete
//! workspaces older than `keep_days`, or oldest-first when total scratch exceeds `max_scratch`.
//! Workspaces live on the node that ran the job; the archive lives on the control node.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tracing::{info, warn};

use crate::node::Node;
use crate::state::AppState;
use crate::workspace::sanitize_name;

pub fn spawn(state: Arc<AppState>) {
    let interval = state.config.retention.interval_secs.max(60);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval));
        loop {
            ticker.tick().await;
            if let Err(e) = sweep(&state).await {
                warn!("retention sweep failed: {e:#}");
            }
        }
    });
}

#[derive(Debug, Default, serde::Serialize)]
pub struct SweepReport {
    pub archived: u32,
    pub deleted: u32,
    pub freed_bytes: u64,
    pub scratch_bytes_after: u64,
    /// Expired workspaces that were kept because their archive copy failed.
    pub kept_unarchived: u32,
}

/// Copy `root/<subdirs>` from the node into `dest` and verify every `out/` file arrived with
/// its size (as listed by the node itself).
async fn pull_verified(
    node: &Node,
    root: &Path,
    subdirs: &[&str],
    dest: &Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest)?;
    node.files.copy_to_local(root, subdirs, dest).await?;
    if subdirs.contains(&"out") && node.files.exists(&root.join("out")).await? {
        for e in node
            .files
            .list_out(root)
            .await?
            .iter()
            .filter(|e| !e.is_dir)
        {
            let got = std::fs::metadata(dest.join("out").join(&e.path))
                .map(|m| m.len())
                .ok();
            if got != Some(e.size) {
                anyhow::bail!("archive verification failed for out/{}", e.path);
            }
        }
    }
    Ok(())
}

/// Copy `/out` and the log of a finished job to `<archive_dir>/<user>/<job_id>/` and record the
/// location. Idempotent: re-copies if called again.
pub async fn archive_job(state: &AppState, job: &mlq_core::Job) -> anyhow::Result<PathBuf> {
    let Some(dir) = state.config.retention.archive_dir.clone() else {
        anyhow::bail!("no archive_dir configured");
    };
    let Some(p) = job.workspace_path.clone() else {
        anyhow::bail!("job has no workspace");
    };
    let node = state.node_for_job(job)?;
    let root = PathBuf::from(p);
    let dest = dir
        .join(sanitize_name(&job.user_name))
        .join(job.id.to_string());
    pull_verified(&node, &root, &["out"], &dest).await?;
    let log = root.join("job.log");
    if node.files.exists(&log).await? {
        node.files.fetch_file(&log, &dest.join("job.log")).await?;
    }
    state
        .store
        .mark_archived(job.id, &dest.to_string_lossy())
        .await?;
    Ok(dest)
}

/// Copy a task's `/out` and step logs to `<archive_dir>/<user>/task-<id>/` and record it.
/// Called after every step, so the archive always mirrors the task's results.
pub async fn archive_task(state: &AppState, task: &mlq_core::Task) -> anyhow::Result<PathBuf> {
    let Some(root) = state.config.retention.archive_dir.clone() else {
        anyhow::bail!("no archive_dir configured");
    };
    let node = state.node_for_task(task)?;
    let ws = crate::tasks::task_workspace(state, task);
    let dest = crate::tasks::archive_dir_for(&root, task);
    pull_verified(&node, &ws.root, &["out", "logs"], &dest).await?;
    state
        .store
        .set_task_archive(task.id, &dest.to_string_lossy())
        .await?;
    Ok(dest)
}

pub async fn sweep(state: &AppState) -> anyhow::Result<SweepReport> {
    let cfg = &state.config.retention;
    let keep = chrono::Duration::days(cfg.keep_days as i64);
    let now = Utc::now();
    let max_scratch = cfg
        .max_scratch
        .as_deref()
        .and_then(|s| mlq_core::parse_bytes(s).ok());

    let jobs = state.store.finished_jobs_with_workspace().await?;
    let mut sized: Vec<(mlq_core::Job, u64)> = Vec::new();
    for j in jobs {
        let Some(p) = j.workspace_path.as_deref() else {
            continue;
        };
        let Ok(node) = state.node_for_job(&j) else {
            continue;
        };
        if !node.is_online() {
            continue;
        }
        let size = node.files.dir_size(Path::new(p)).await.unwrap_or(0);
        sized.push((j, size));
    }
    let mut total: u64 = sized.iter().map(|(_, s)| s).sum();
    let mut report = SweepReport::default();

    for (job, size) in sized {
        let finished = job.finished_at.unwrap_or(now);
        let expired = now - finished > keep;
        let over = max_scratch.map(|m| total > m).unwrap_or(false);
        if !(expired || over) {
            continue;
        }
        let root = PathBuf::from(job.workspace_path.clone().unwrap());
        let mut archived = job.archive_path.is_some();
        if cfg.archive_dir.is_some() && !archived {
            // Never delete results that have not reached the archive.
            match archive_job(state, &job).await {
                Ok(_) => archived = true,
                Err(e) => {
                    warn!(job = job.id, "archive failed, keeping workspace: {e:#}");
                    report.kept_unarchived += 1;
                    continue;
                }
            }
        }
        let node = state.node_for_job(&job)?;
        if let Err(e) = node.files.remove_dir(&root).await {
            warn!(job = job.id, "delete failed: {e}");
            continue;
        }
        state.store.mark_workspace_deleted(job.id).await?;
        crate::sched::publish_job(state, job.id).await;
        total = total.saturating_sub(size);
        report.deleted += 1;
        report.freed_bytes += size;
        if archived {
            report.archived += 1;
        }
        info!(job = job.id, archived, size, "workspace removed");
    }
    report.scratch_bytes_after = total;
    Ok(report)
}
