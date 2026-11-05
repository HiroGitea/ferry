//! Periodic backups of the control database to the NAS: `ferry-<timestamp>.db` (and optionally
//! `metrics-<timestamp>.db`) written with `VACUUM INTO` to a temporary name, then renamed, so a
//! listed backup is always complete. Restore = stop ferryd, copy the file over `ferry.db`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use tracing::{info, warn};

use crate::state::AppState;

fn backups(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".db"))
        })
        .collect();
    v.sort(); // timestamps sort lexically
    v
}

/// Write one backup now; returns the files written.
pub async fn run_once(state: &AppState) -> anyhow::Result<Vec<PathBuf>> {
    let dir = state
        .config
        .backup_dir()
        .context("backups are off: set backup.dir or retention.archive_dir")?;
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let stamp = Utc::now().format("%Y%m%d-%H%M%S");
    let mut written = vec![];
    let db = dir.join(format!("ferry-{stamp}.db"));
    let tmp = dir.join(format!(".ferry-{stamp}.db.part"));
    let _ = tokio::fs::remove_file(&tmp).await;
    state.store.backup_to(&tmp).await.context("VACUUM INTO")?;
    tokio::fs::rename(&tmp, &db).await?;
    written.push(db);
    if state.config.backup.include_metrics {
        let m = dir.join(format!("metrics-{stamp}.db"));
        let tmp = dir.join(format!(".metrics-{stamp}.db.part"));
        let _ = tokio::fs::remove_file(&tmp).await;
        if state.metrics.backup_to(&tmp).await? {
            tokio::fs::rename(&tmp, &m).await?;
            written.push(m);
        }
    }
    let keep = state.config.backup.keep.max(1);
    for prefix in ["ferry-", "metrics-"] {
        let list = backups(&dir, prefix);
        for old in list.iter().take(list.len().saturating_sub(keep)) {
            if let Err(e) = tokio::fs::remove_file(old).await {
                warn!(path = %old.display(), "removing old backup: {e}");
            }
        }
    }
    info!(files = ?written, "database backed up");
    Ok(written)
}

pub fn spawn(state: Arc<AppState>) {
    let Some(dir) = state.config.backup_dir() else {
        return;
    };
    let interval = ferry_core::parse_duration_secs(&state.config.backup.interval)
        .unwrap_or(86400)
        .max(60) as i64;
    tokio::spawn(async move {
        // First check shortly after start, then every hour (or the interval, if shorter).
        tokio::time::sleep(Duration::from_secs(60.min(interval as u64))).await;
        loop {
            let newest = backups(&dir, "ferry-")
                .pop()
                .and_then(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok());
            let age = newest
                .and_then(|t| t.elapsed().ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(i64::MAX);
            if age >= interval
                && let Err(e) = run_once(&state).await
            {
                warn!("backup failed: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(3600.min(interval as u64))).await;
        }
    });
}
