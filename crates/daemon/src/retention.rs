//! Lifecycle of SSD scratch workspaces: archive `/out` + log to the NAS (optional) and delete
//! workspaces older than `keep_days`, or oldest-first when total scratch exceeds `max_scratch`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tracing::{info, warn};

use crate::state::AppState;
use crate::workspace::{Workspace, sanitize_name};

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

/// Copy `/out` and the log of a finished job to `<archive_dir>/<user>/<job_id>/`, verify the copy
/// (file count and bytes), and record the location. Idempotent: re-copies if called again.
pub async fn archive_job(state: &AppState, job: &mlq_core::Job) -> anyhow::Result<PathBuf> {
    let Some(dir) = state.config.retention.archive_dir.clone() else {
        anyhow::bail!("no archive_dir configured");
    };
    let Some(p) = job.workspace_path.clone() else {
        anyhow::bail!("job has no workspace");
    };
    let ws = Workspace::open(p.into());
    let user = job.user_name.clone();
    let id = job.id;
    let dest = tokio::task::spawn_blocking(move || archive(&ws, &dir, &user, id)).await??;
    state
        .store
        .mark_archived(job.id, &dest.to_string_lossy())
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
    // Size every candidate once (blocking IO, but a sweep is rare).
    let mut sized: Vec<(mlq_core::Job, u64)> = Vec::new();
    for j in jobs {
        let Some(p) = j.workspace_path.as_deref() else {
            continue;
        };
        let ws = Workspace::open(p.into());
        let size = if ws.root.exists() { ws.total_size() } else { 0 };
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
        let ws = Workspace::open(job.workspace_path.clone().unwrap().into());
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
        if ws.root.exists() {
            if let Err(e) = std::fs::remove_dir_all(&ws.root) {
                warn!(job = job.id, "delete failed: {e}");
                continue;
            }
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

fn archive(ws: &Workspace, archive_dir: &Path, user: &str, job_id: i64) -> anyhow::Result<PathBuf> {
    let dest = archive_dir
        .join(sanitize_name(user))
        .join(job_id.to_string());
    std::fs::create_dir_all(&dest)?;
    if ws.out().exists() {
        copy_dir(&ws.out(), &dest.join("out"))?;
        // Verify: same number of files and bytes on both sides.
        let (n_src, b_src) = dir_stats(&ws.out());
        let (n_dst, b_dst) = dir_stats(&dest.join("out"));
        if n_src != n_dst || b_src != b_dst {
            anyhow::bail!(
                "archive verification failed: source {n_src} files/{b_src} bytes, archive {n_dst} files/{b_dst} bytes"
            );
        }
    }
    if ws.log().exists() {
        std::fs::copy(ws.log(), dest.join("job.log"))?;
    }
    Ok(dest)
}

fn dir_stats(dir: &Path) -> (u64, u64) {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .fold((0, 0), |(n, b), m| (n + 1, b + m.len()))
}

fn copy_dir(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in walkdir::WalkDir::new(src).min_depth(1) {
        let e = e?;
        let rel = e.path().strip_prefix(src)?;
        let target = dst.join(rel);
        if e.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if e.file_type().is_file() {
            std::fs::copy(e.path(), &target)?;
        }
    }
    Ok(())
}
