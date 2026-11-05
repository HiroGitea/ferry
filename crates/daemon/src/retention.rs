//! Lifecycle of SSD scratch workspaces: archive `/out` + log to the NAS (optional) and delete
//! workspaces older than `keep_days`, or oldest-first when total scratch exceeds `max_scratch`.

use std::path::Path;
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
        let mut archived = false;
        if let Some(dir) = &cfg.archive_dir {
            match archive(&ws, dir, &job.user_name, job.id) {
                Ok(()) => archived = true,
                Err(e) => {
                    warn!(job = job.id, "archive failed, keeping workspace: {e:#}");
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
        state.store.mark_workspace_deleted(job.id, archived).await?;
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

fn archive(ws: &Workspace, archive_dir: &Path, user: &str, job_id: i64) -> anyhow::Result<()> {
    let dest = archive_dir
        .join(sanitize_name(user))
        .join(job_id.to_string());
    std::fs::create_dir_all(&dest)?;
    if ws.out().exists() {
        copy_dir(&ws.out(), &dest.join("out"))?;
    }
    if ws.log().exists() {
        std::fs::copy(ws.log(), dest.join("job.log"))?;
    }
    Ok(())
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
