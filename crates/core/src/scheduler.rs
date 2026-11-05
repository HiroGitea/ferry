//! Pure scheduling function: given the managed inventory and the current jobs, decide what to start.
//!
//! Policy (v1):
//! 1. Only `Queued` jobs whose dependencies have all `Succeeded` are candidates.
//!    A dependency that ended in any other terminal state rejects the job.
//! 2. Candidates are ordered by priority (desc), then decayed fair-share usage of the user (asc),
//!    then submission time, then id.
//! 3. First-fit: walk the ordered list and start every job that fits in the remaining
//!    free resources and the user's quota. Smaller jobs may therefore run ahead of a large
//!    job that is waiting (simple backfill, no reservations).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::job::{Job, JobId, JobState};
use crate::resource::Inventory;
use crate::user::{Quota, UserId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub job_id: JobId,
    pub gpu_ids: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Decision {
    Start(Assignment),
    /// The job can never run (bad dependency, request larger than inventory). The daemon fails it.
    Reject {
        job_id: JobId,
        reason: String,
    },
    /// Still waiting; `reason` is informational for the UI.
    Wait {
        job_id: JobId,
        reason: String,
    },
}

pub struct SchedulerInput<'a> {
    pub inventory: &'a Inventory,
    /// All non-terminal jobs plus any terminal jobs referenced by `depends_on`.
    pub jobs: &'a [Job],
    pub quotas: &'a HashMap<UserId, Quota>,
    /// Decayed GPU-seconds per user (fair-share). Missing users count as 0.
    pub fair_usage: &'a HashMap<UserId, f64>,
}

#[derive(Debug, Clone)]
struct Free {
    cpus: f64,
    mem: u64,
    gpus: Vec<u32>,
}

pub fn schedule(input: SchedulerInput<'_>) -> Vec<Decision> {
    let SchedulerInput {
        inventory,
        jobs,
        quotas,
        fair_usage,
    } = input;
    let by_id: HashMap<JobId, &Job> = jobs.iter().map(|j| (j.id, j)).collect();

    // Resources currently in use by active jobs.
    let mut free = Free {
        cpus: inventory.cpus,
        mem: inventory.mem_bytes,
        gpus: inventory.gpus.clone(),
    };
    let mut running_jobs: HashMap<UserId, u32> = HashMap::new();
    let mut running_gpus: HashMap<UserId, u32> = HashMap::new();
    let mut used_gpus: HashSet<u32> = HashSet::new();
    for j in jobs.iter().filter(|j| j.state.is_active()) {
        free.cpus -= j.resources.cpus;
        free.mem = free.mem.saturating_sub(j.resources.mem_bytes);
        used_gpus.extend(j.gpu_ids.iter().copied());
        *running_jobs.entry(j.user_id).or_default() += 1;
        *running_gpus.entry(j.user_id).or_default() += j.gpu_ids.len() as u32;
    }
    free.gpus.retain(|g| !used_gpus.contains(g));

    let mut decisions = Vec::new();
    let mut candidates: Vec<&Job> = Vec::new();
    for j in jobs.iter().filter(|j| j.state == JobState::Queued) {
        // Dependency check.
        let mut blocked = None;
        let mut rejected = None;
        for dep in &j.depends_on {
            match by_id.get(dep).map(|d| d.state) {
                Some(JobState::Succeeded) => {}
                Some(s) if s.is_terminal() => {
                    rejected = Some(format!("dependency {dep} ended with state {s}"));
                    break;
                }
                Some(_) => blocked = Some(format!("waiting for dependency {dep}")),
                None => {
                    rejected = Some(format!("dependency {dep} not found"));
                    break;
                }
            }
        }
        if let Some(reason) = rejected {
            decisions.push(Decision::Reject {
                job_id: j.id,
                reason,
            });
            continue;
        }
        if let Some(reason) = blocked {
            decisions.push(Decision::Wait {
                job_id: j.id,
                reason,
            });
            continue;
        }
        // Impossible requests.
        if j.resources.cpus > inventory.cpus
            || j.resources.mem_bytes > inventory.mem_bytes
            || j.resources.gpus as usize > inventory.gpus.len()
        {
            decisions.push(Decision::Reject {
                job_id: j.id,
                reason: format!(
                    "request ({} cpus, {} bytes, {} gpus) exceeds managed inventory ({} cpus, {} bytes, {} gpus)",
                    j.resources.cpus,
                    j.resources.mem_bytes,
                    j.resources.gpus,
                    inventory.cpus,
                    inventory.mem_bytes,
                    inventory.gpus.len()
                ),
            });
            continue;
        }
        candidates.push(j);
    }

    candidates.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| {
                let ua = fair_usage.get(&a.user_id).copied().unwrap_or(0.0);
                let ub = fair_usage.get(&b.user_id).copied().unwrap_or(0.0);
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.submitted_at.cmp(&b.submitted_at))
            .then_with(|| a.id.cmp(&b.id))
    });

    for j in candidates {
        let quota = quotas.get(&j.user_id).cloned().unwrap_or_default();
        let rj = running_jobs.get(&j.user_id).copied().unwrap_or(0);
        let rg = running_gpus.get(&j.user_id).copied().unwrap_or(0);
        if let Some(max) = quota.max_running_jobs {
            if rj >= max {
                decisions.push(Decision::Wait {
                    job_id: j.id,
                    reason: format!("quota: {max} running jobs"),
                });
                continue;
            }
        }
        if let Some(max) = quota.max_running_gpus {
            if rg + j.resources.gpus > max {
                decisions.push(Decision::Wait {
                    job_id: j.id,
                    reason: format!("quota: {max} running gpus"),
                });
                continue;
            }
        }
        let fits = j.resources.cpus <= free.cpus + 1e-9
            && j.resources.mem_bytes <= free.mem
            && (j.resources.gpus as usize) <= free.gpus.len();
        if !fits {
            decisions.push(Decision::Wait {
                job_id: j.id,
                reason: "waiting for resources".into(),
            });
            continue;
        }
        let gpu_ids: Vec<u32> = free.gpus.drain(..j.resources.gpus as usize).collect();
        free.cpus -= j.resources.cpus;
        free.mem -= j.resources.mem_bytes;
        *running_jobs.entry(j.user_id).or_default() += 1;
        *running_gpus.entry(j.user_id).or_default() += j.resources.gpus;
        decisions.push(Decision::Start(Assignment {
            job_id: j.id,
            gpu_ids,
        }));
    }
    decisions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ResourceRequest;
    use chrono::{Duration, Utc};

    fn job(id: JobId, user: UserId, cpus: f64, gpus: u32, state: JobState) -> Job {
        Job {
            id,
            user_id: user,
            user_name: format!("u{user}"),
            node_id: None,
            name: format!("job{id}"),
            image: "img".into(),
            command: vec!["true".into()],
            env: Default::default(),
            resources: ResourceRequest {
                cpus,
                mem_bytes: 1 << 30,
                gpus,
            },
            state,
            priority: 0,
            depends_on: vec![],
            array_index: None,
            time_limit_secs: None,
            submitted_at: Utc::now() + Duration::milliseconds(id),
            started_at: None,
            finished_at: None,
            exit_code: None,
            gpu_ids: vec![],
            container_id: None,
            workspace_path: None,
            reason: None,
            archive_path: None,
            workspace_deleted: false,
        }
    }

    fn inv() -> Inventory {
        Inventory {
            cpus: 8.0,
            mem_bytes: 64 << 30,
            gpus: vec![0, 1, 2, 3],
        }
    }

    fn run(
        inv: &Inventory,
        jobs: &[Job],
        quotas: HashMap<UserId, Quota>,
        fair: HashMap<UserId, f64>,
    ) -> Vec<Decision> {
        schedule(SchedulerInput {
            inventory: inv,
            jobs,
            quotas: &quotas,
            fair_usage: &fair,
        })
    }

    fn starts(d: &[Decision]) -> Vec<(JobId, Vec<u32>)> {
        d.iter()
            .filter_map(|d| match d {
                Decision::Start(a) => Some((a.job_id, a.gpu_ids.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn fifo_and_gpu_assignment() {
        let jobs = vec![
            job(1, 1, 2.0, 2, JobState::Queued),
            job(2, 1, 2.0, 2, JobState::Queued),
            job(3, 1, 2.0, 1, JobState::Queued),
        ];
        let d = run(&inv(), &jobs, Default::default(), Default::default());
        assert_eq!(starts(&d), vec![(1, vec![0, 1]), (2, vec![2, 3])]);
        assert!(matches!(d[2], Decision::Wait { job_id: 3, .. }));
    }

    #[test]
    fn running_jobs_consume_resources() {
        let mut running = job(1, 1, 4.0, 2, JobState::Running);
        running.gpu_ids = vec![1, 2];
        let jobs = vec![
            running,
            job(2, 2, 4.0, 2, JobState::Queued),
            job(3, 2, 1.0, 1, JobState::Queued),
        ];
        let d = run(&inv(), &jobs, Default::default(), Default::default());
        assert_eq!(starts(&d), vec![(2, vec![0, 3])]);
    }

    #[test]
    fn priority_then_fair_share() {
        let mut a = job(1, 1, 1.0, 0, JobState::Queued);
        let mut b = job(2, 2, 1.0, 0, JobState::Queued);
        let mut c = job(3, 3, 1.0, 0, JobState::Queued);
        a.priority = 0;
        b.priority = 0;
        c.priority = 10;
        let mut fair = HashMap::new();
        fair.insert(1, 1000.0); // user 1 used a lot
        fair.insert(2, 10.0);
        let small = Inventory {
            cpus: 1.0,
            mem_bytes: 64 << 30,
            gpus: vec![],
        };
        let d = run(&small, &[a, b, c], Default::default(), fair);
        assert_eq!(starts(&d), vec![(3, vec![])]); // highest priority first
        let small2 = Inventory {
            cpus: 2.0,
            mem_bytes: 64 << 30,
            gpus: vec![],
        };
        let jobs = vec![
            job(1, 1, 1.0, 0, JobState::Queued),
            job(2, 2, 1.0, 0, JobState::Queued),
            job(3, 3, 1.0, 0, JobState::Queued),
        ];
        let mut fair = HashMap::new();
        fair.insert(1, 1000.0);
        let d = run(&small2, &jobs, Default::default(), fair);
        // users 2 and 3 (usage 0) go before user 1 (usage 1000)
        assert_eq!(
            starts(&d).iter().map(|s| s.0).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn quotas() {
        let jobs = vec![
            job(1, 1, 1.0, 1, JobState::Queued),
            job(2, 1, 1.0, 1, JobState::Queued),
            job(3, 1, 1.0, 1, JobState::Queued),
        ];
        let mut q = HashMap::new();
        q.insert(
            1,
            Quota {
                max_running_jobs: None,
                max_running_gpus: Some(2),
                max_queued_jobs: None,
            },
        );
        let d = run(&inv(), &jobs, q, Default::default());
        assert_eq!(starts(&d).len(), 2);
        assert!(matches!(&d[2], Decision::Wait { job_id: 3, reason } if reason.contains("quota")));
    }

    #[test]
    fn dependencies() {
        let mut dep_ok = job(1, 1, 1.0, 0, JobState::Succeeded);
        dep_ok.finished_at = Some(Utc::now());
        let dep_bad = job(2, 1, 1.0, 0, JobState::Failed);
        let dep_running = job(3, 1, 1.0, 0, JobState::Running);
        let mut a = job(4, 1, 1.0, 0, JobState::Queued);
        a.depends_on = vec![1];
        let mut b = job(5, 1, 1.0, 0, JobState::Queued);
        b.depends_on = vec![2];
        let mut c = job(6, 1, 1.0, 0, JobState::Queued);
        c.depends_on = vec![3];
        let mut dmiss = job(7, 1, 1.0, 0, JobState::Queued);
        dmiss.depends_on = vec![99];
        let d = run(
            &inv(),
            &[dep_ok, dep_bad, dep_running, a, b, c, dmiss],
            Default::default(),
            Default::default(),
        );
        assert!(
            d.iter()
                .any(|x| matches!(x, Decision::Start(a) if a.job_id == 4))
        );
        assert!(
            d.iter()
                .any(|x| matches!(x, Decision::Reject { job_id: 5, .. }))
        );
        assert!(
            d.iter()
                .any(|x| matches!(x, Decision::Wait { job_id: 6, .. }))
        );
        assert!(
            d.iter()
                .any(|x| matches!(x, Decision::Reject { job_id: 7, .. }))
        );
    }

    #[test]
    fn rejects_impossible_request() {
        let jobs = vec![job(1, 1, 1.0, 8, JobState::Queued)];
        let d = run(&inv(), &jobs, Default::default(), Default::default());
        assert!(matches!(&d[0], Decision::Reject { job_id: 1, .. }));
    }
}
