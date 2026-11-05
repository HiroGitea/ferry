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

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::job::{Job, JobId, JobState};
use crate::resource::Inventory;
use crate::user::{Quota, UserId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub job_id: JobId,
    pub node_id: String,
    pub gpu_ids: Vec<u32>,
}

/// One schedulable node and its managed resources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeInventory {
    pub id: String,
    pub inventory: Inventory,
    /// Offline nodes keep their running jobs' resources accounted but take no new work.
    pub online: bool,
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
    pub nodes: &'a [NodeInventory],
    /// All non-terminal jobs plus any terminal jobs referenced by `depends_on`.
    pub jobs: &'a [Job],
    pub quotas: &'a HashMap<UserId, Quota>,
    /// Decayed GPU-seconds per user (fair-share). Missing users count as 0.
    pub fair_usage: &'a HashMap<UserId, f64>,
    /// GPUs a task's warm container already has attached; steps of that task get them back
    /// when they are free so the warm container can be reused.
    pub gpu_affinity: &'a HashMap<i64, Vec<u32>>,
    /// Node each task's environment lives on (steps of that task must run there).
    pub task_nodes: &'a HashMap<i64, String>,
}

#[derive(Debug, Clone)]
struct Free {
    cpus: f64,
    mem: u64,
    gpus: Vec<u32>,
}

pub fn schedule(input: SchedulerInput<'_>) -> Vec<Decision> {
    let SchedulerInput {
        nodes,
        jobs,
        quotas,
        fair_usage,
        gpu_affinity,
        task_nodes,
    } = input;
    let by_id: HashMap<JobId, &Job> = jobs.iter().map(|j| (j.id, j)).collect();

    // Free resources per node after subtracting active jobs.
    let mut free: Vec<(String, bool, Free)> = nodes
        .iter()
        .map(|n| {
            (
                n.id.clone(),
                n.online,
                Free {
                    cpus: n.inventory.cpus,
                    mem: n.inventory.mem_bytes,
                    gpus: n.inventory.gpus.clone(),
                },
            )
        })
        .collect();
    let mut running_jobs: HashMap<UserId, u32> = HashMap::new();
    let mut running_gpus: HashMap<UserId, u32> = HashMap::new();
    for j in jobs.iter().filter(|j| j.state.is_active()) {
        if let Some((_, _, f)) = free
            .iter_mut()
            .find(|(id, _, _)| Some(id.as_str()) == j.node_id.as_deref())
        {
            f.cpus -= j.resources.cpus;
            f.mem = f.mem.saturating_sub(j.resources.mem_bytes);
            f.gpus.retain(|g| !j.gpu_ids.contains(g));
        }
        *running_jobs.entry(j.user_id).or_default() += 1;
        *running_gpus.entry(j.user_id).or_default() += j.gpu_ids.len() as u32;
    }
    let max_inv =
        |pick: fn(&Inventory) -> f64| nodes.iter().map(|n| pick(&n.inventory)).fold(0.0, f64::max);
    let biggest_cpus = max_inv(|i| i.cpus);
    let biggest_mem = nodes
        .iter()
        .map(|n| n.inventory.mem_bytes)
        .max()
        .unwrap_or(0);
    let biggest_gpus = nodes
        .iter()
        .map(|n| n.inventory.gpus.len())
        .max()
        .unwrap_or(0);

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
        // Steps of one task share an environment: run them one at a time, in submission order.
        if let Some(t) = j.task_id {
            let earlier = jobs.iter().find(|k| {
                k.task_id == Some(t)
                    && k.id != j.id
                    && (k.state.is_active()
                        || (matches!(k.state, JobState::Queued | JobState::Held) && k.id < j.id))
            });
            if let Some(k) = earlier {
                decisions.push(Decision::Wait {
                    job_id: j.id,
                    reason: format!("waiting for step {} of task {t}", k.id),
                });
                continue;
            }
        }
        // Requests no node could ever satisfy.
        let pinned = j
            .task_id
            .and_then(|t| task_nodes.get(&t).cloned())
            .or_else(|| j.requested_node.clone());
        let (cap_cpus, cap_mem, cap_gpus) = match &pinned {
            Some(n) => match nodes.iter().find(|x| &x.id == n) {
                Some(x) => (
                    x.inventory.cpus,
                    x.inventory.mem_bytes,
                    x.inventory.gpus.len(),
                ),
                None => {
                    decisions.push(Decision::Reject {
                        job_id: j.id,
                        reason: format!("node {n} is not registered"),
                    });
                    continue;
                }
            },
            None => (biggest_cpus, biggest_mem, biggest_gpus),
        };
        if j.resources.cpus > cap_cpus
            || j.resources.mem_bytes > cap_mem
            || j.resources.gpus as usize > cap_gpus
        {
            decisions.push(Decision::Reject {
                job_id: j.id,
                reason: format!(
                    "request ({} cpus, {} bytes, {} gpus) exceeds {} ({} cpus, {} bytes, {} gpus)",
                    j.resources.cpus,
                    j.resources.mem_bytes,
                    j.resources.gpus,
                    pinned
                        .as_deref()
                        .map(|n| format!("node {n}"))
                        .unwrap_or_else(|| "every node".into()),
                    cap_cpus,
                    cap_mem,
                    cap_gpus
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
        if let Some(max) = quota.max_running_jobs
            && rj >= max
        {
            decisions.push(Decision::Wait {
                job_id: j.id,
                reason: format!("quota: {max} running jobs"),
            });
            continue;
        }
        if let Some(max) = quota.max_running_gpus
            && rg + j.resources.gpus > max
        {
            decisions.push(Decision::Wait {
                job_id: j.id,
                reason: format!("quota: {max} running gpus"),
            });
            continue;
        }
        let want = j.resources.gpus as usize;
        let pinned = j
            .task_id
            .and_then(|t| task_nodes.get(&t).cloned())
            .or_else(|| j.requested_node.clone());
        let pref = j.task_id.and_then(|t| gpu_affinity.get(&t));
        // First fit over online nodes (pinned node only, if any); nodes holding the task's warm
        // GPUs are tried first.
        let mut order: Vec<usize> = (0..free.len())
            .filter(|&i| free[i].1 && pinned.as_deref().map(|p| p == free[i].0).unwrap_or(true))
            .collect();
        if let Some(p) = pref {
            order.sort_by_key(|&i| !p.iter().all(|g| free[i].2.gpus.contains(g)));
        }
        let slot = order.into_iter().find(|&i| {
            let f = &free[i].2;
            j.resources.cpus <= f.cpus + 1e-9
                && j.resources.mem_bytes <= f.mem
                && want <= f.gpus.len()
        });
        let Some(i) = slot else {
            decisions.push(Decision::Wait {
                job_id: j.id,
                reason: match &pinned {
                    Some(n) if !free.iter().any(|(id, on, _)| id == n && *on) => {
                        format!("node {n} is offline")
                    }
                    Some(n) => format!("waiting for resources on node {n}"),
                    None => "waiting for resources".into(),
                },
            });
            continue;
        };
        let f = &mut free[i].2;
        let gpu_ids: Vec<u32> = match pref {
            Some(p) if want > 0 && p.len() == want && p.iter().all(|g| f.gpus.contains(g)) => {
                f.gpus.retain(|g| !p.contains(g));
                p.clone()
            }
            _ => f.gpus.drain(..want).collect(),
        };
        f.cpus -= j.resources.cpus;
        f.mem -= j.resources.mem_bytes;
        *running_jobs.entry(j.user_id).or_default() += 1;
        *running_gpus.entry(j.user_id).or_default() += j.resources.gpus;
        decisions.push(Decision::Start(Assignment {
            job_id: j.id,
            node_id: free[i].0.clone(),
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
            task_id: None,
            base_snapshot_id: None,
            snapshot_id: None,
            requested_node: None,
        }
    }

    fn one(inv: &Inventory) -> Vec<NodeInventory> {
        vec![NodeInventory {
            id: "local".into(),
            inventory: inv.clone(),
            online: true,
        }]
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
        let nodes = one(inv);
        schedule(SchedulerInput {
            nodes: &nodes,
            jobs,
            quotas: &quotas,
            fair_usage: &fair,
            gpu_affinity: &HashMap::new(),
            task_nodes: &HashMap::new(),
        })
    }

    #[test]
    fn task_steps_run_sequentially() {
        let mut a = job(1, 1, 1.0, 0, JobState::Running);
        a.task_id = Some(9);
        let mut b = job(2, 1, 1.0, 0, JobState::Queued);
        b.task_id = Some(9);
        let mut c = job(3, 1, 1.0, 0, JobState::Queued);
        c.task_id = Some(9);
        let mut other = job(4, 1, 1.0, 0, JobState::Queued);
        other.task_id = Some(10);
        let d = run(
            &inv(),
            &[a.clone(), b.clone(), c.clone(), other],
            Default::default(),
            Default::default(),
        );
        assert_eq!(starts(&d), vec![(4, vec![])]);
        assert!(d.iter().any(
            |x| matches!(x, Decision::Wait { job_id: 2, reason } if reason.contains("step 1"))
        ));
        assert!(d.iter().any(
            |x| matches!(x, Decision::Wait { job_id: 3, reason } if reason.contains("task 9"))
        ));
        // once step 1 finished, step 2 (not 3) starts
        a.state = JobState::Succeeded;
        let d = run(&inv(), &[a, b, c], Default::default(), Default::default());
        assert_eq!(starts(&d), vec![(2, vec![])]);
    }

    #[test]
    fn warm_gpu_affinity() {
        let mut j = job(1, 1, 1.0, 2, JobState::Queued);
        j.task_id = Some(5);
        let mut aff = HashMap::new();
        aff.insert(5, vec![2, 3]);
        let nodes = one(&inv());
        let d = schedule(SchedulerInput {
            nodes: &nodes,
            jobs: &[j.clone()],
            quotas: &HashMap::new(),
            fair_usage: &HashMap::new(),
            gpu_affinity: &aff,
            task_nodes: &HashMap::new(),
        });
        assert_eq!(starts(&d), vec![(1, vec![2, 3])]);
        // preferred GPU busy -> falls back to first fit
        let mut busy = job(2, 2, 1.0, 1, JobState::Running);
        busy.gpu_ids = vec![3];
        busy.node_id = Some("local".into());
        let d = schedule(SchedulerInput {
            nodes: &nodes,
            jobs: &[j, busy],
            quotas: &HashMap::new(),
            fair_usage: &HashMap::new(),
            gpu_affinity: &aff,
            task_nodes: &HashMap::new(),
        });
        assert_eq!(starts(&d), vec![(1, vec![0, 1])]);
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
    fn multi_node_placement() {
        let nodes = vec![
            NodeInventory {
                id: "a".into(),
                inventory: Inventory {
                    cpus: 4.0,
                    mem_bytes: 16 << 30,
                    gpus: vec![0],
                },
                online: true,
            },
            NodeInventory {
                id: "b".into(),
                inventory: Inventory {
                    cpus: 8.0,
                    mem_bytes: 64 << 30,
                    gpus: vec![0, 1],
                },
                online: true,
            },
            NodeInventory {
                id: "c".into(),
                inventory: Inventory {
                    cpus: 8.0,
                    mem_bytes: 64 << 30,
                    gpus: vec![0, 1],
                },
                online: false,
            },
        ];
        let mut want2 = job(1, 1, 1.0, 2, JobState::Queued); // only fits b
        let mut pinned_c = job(2, 1, 1.0, 0, JobState::Queued);
        pinned_c.requested_node = Some("c".into()); // offline
        let mut step = job(3, 1, 1.0, 1, JobState::Queued);
        step.task_id = Some(7); // task lives on a
        let mut tn = HashMap::new();
        tn.insert(7, "a".to_string());
        let mut too_big = job(4, 1, 1.0, 3, JobState::Queued);
        too_big.requested_node = None;
        want2.priority = 1;
        let d = schedule(SchedulerInput {
            nodes: &nodes,
            jobs: &[want2, pinned_c, step, too_big],
            quotas: &HashMap::new(),
            fair_usage: &HashMap::new(),
            gpu_affinity: &HashMap::new(),
            task_nodes: &tn,
        });
        let starts: Vec<(JobId, String, Vec<u32>)> = d
            .iter()
            .filter_map(|x| match x {
                Decision::Start(a) => Some((a.job_id, a.node_id.clone(), a.gpu_ids.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            vec![(1, "b".into(), vec![0, 1]), (3, "a".into(), vec![0])]
        );
        assert!(d.iter().any(
            |x| matches!(x, Decision::Wait { job_id: 2, reason } if reason.contains("offline"))
        ));
        assert!(
            d.iter()
                .any(|x| matches!(x, Decision::Reject { job_id: 4, .. }))
        );
    }

    #[test]
    fn running_jobs_consume_resources() {
        let mut running = job(1, 1, 4.0, 2, JobState::Running);
        running.gpu_ids = vec![1, 2];
        running.node_id = Some("local".into());
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
