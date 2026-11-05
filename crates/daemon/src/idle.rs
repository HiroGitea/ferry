//! Idle GPU reclaim: a running job whose GPUs stayed at (near) 0% for `idle.warn_after_mins`
//! gets a warning notification; if they are still idle `idle.cancel_after_mins` later, the job
//! is cancelled and its owner told why.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use ferry_core::{Job, JobId, JobState};
use tracing::{info, warn};

use crate::notify::{Kind, Notification};
use crate::state::AppState;

/// Highest per-minute (or finer) GPU utilisation of the job in `[after, now]`; `None` if there
/// are no samples (metrics off, or the collector has not seen it yet).
async fn peak_util(state: &AppState, job: &Job, after: i64) -> Option<f32> {
    let key = match job.task_id {
        Some(t) => format!("task.{t}.gpu_util"),
        None => format!("job.{}.gpu_util", job.id),
    };
    let now = crate::metrics::now_secs();
    let points = ((now - after) / 10).clamp(1, 2000) as u32;
    let r = state.metrics.query(&[key], after, now, points).await.ok()?;
    let s = r.series.into_iter().next()?;
    let vals: Vec<f32> = s.max.iter().flatten().copied().collect();
    if vals.is_empty() {
        return None;
    }
    // Require samples over most of the window so a collector gap does not look idle.
    let covered = s.points.iter().filter(|p| p.1.is_some()).count();
    if covered * 10 < s.points.len() * 7 {
        return None;
    }
    Some(vals.into_iter().fold(0.0, f32::max))
}

pub fn spawn(state: Arc<AppState>) {
    let cfg = state.config.idle.clone();
    if !cfg.enabled || !state.config.metrics.enabled {
        return;
    }
    tokio::spawn(async move {
        // job -> when the warning was sent
        let mut warned: HashMap<JobId, DateTime<Utc>> = HashMap::new();
        let secs = |s: &str| ferry_core::parse_duration_secs(s).unwrap_or(1800).max(1) as i64;
        let warn_after = secs(&cfg.warn_after);
        let cancel_after = secs(&cfg.cancel_after);
        let human = |s: i64| {
            let s = s.max(0);
            match (s / 3600, s % 3600 / 60, s % 60) {
                (0, 0, sec) => format!("{sec}s"),
                (0, m, 0) => format!("{m} min"),
                (0, m, sec) => format!("{m} min {sec}s"),
                (h, m, _) => format!("{h} h {m} min"),
            }
        };
        let mut tick =
            tokio::time::interval(Duration::from_secs((warn_after / 3).clamp(5, 60) as u64));
        loop {
            tick.tick().await;
            let Ok(active) = state.store.active_jobs().await else {
                continue;
            };
            let running: Vec<Job> = active
                .into_iter()
                .filter(|j| j.state == JobState::Running && !j.gpu_ids.is_empty())
                .filter(|j| !cfg.exempt_users.contains(&j.user_name))
                .collect();
            warned.retain(|id, _| running.iter().any(|j| j.id == *id));
            let now = Utc::now();
            for job in running {
                let Some(started) = job.started_at else {
                    continue;
                };
                if (now - started).num_seconds() < warn_after {
                    continue;
                }
                match warned.get(&job.id).copied() {
                    None => {
                        let after = now.timestamp() - warn_after;
                        if peak_util(&state, &job, after)
                            .await
                            .is_some_and(|u| u <= cfg.util_threshold)
                        {
                            info!(job = job.id, user = %job.user_name, "GPUs idle; warning owner");
                            warned.insert(job.id, now);
                            let n = Notification::for_job(
                                Kind::IdleWarning,
                                &job,
                                format!(
                                    "Its GPUs ({:?}) have been at <= {}% for {}. It will be cancelled in {} unless they get busy.",
                                    job.gpu_ids,
                                    cfg.util_threshold,
                                    human(warn_after),
                                    human(cancel_after)
                                ),
                            );
                            crate::notify::send(state.clone(), job.user_id, n);
                        }
                    }
                    Some(at) => {
                        let since = at.timestamp() - warn_after.min(300);
                        match peak_util(&state, &job, since).await {
                            Some(u) if u > cfg.util_threshold => {
                                info!(job = job.id, "GPUs busy again; warning cleared");
                                warned.remove(&job.id);
                            }
                            Some(_) if (now - at).num_seconds() >= cancel_after => {
                                let idle = (now - at).num_seconds() + warn_after;
                                let reason = format!("cancelled: GPUs idle for {}", human(idle));
                                warn!(job = job.id, user = %job.user_name, "{reason}");
                                if state.request_cancel_because(job.id, reason.clone()) {
                                    let n =
                                        Notification::for_job(Kind::IdleCancelled, &job, reason);
                                    crate::notify::send(state.clone(), job.user_id, n);
                                }
                                warned.remove(&job.id);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    });
}
