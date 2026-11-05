use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::resource::ResourceRequest;
use crate::user::UserId;

pub type JobId = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    /// Waiting for resources / dependencies.
    Queued,
    /// Submitted but explicitly held (e.g. while files are being uploaded).
    Held,
    /// Resources assigned; workspace being prepared / container starting.
    Starting,
    /// Container image is being pulled.
    Pulling,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Timeout,
}

impl JobState {
    pub const ALL: [JobState; 9] = [
        JobState::Queued,
        JobState::Held,
        JobState::Starting,
        JobState::Pulling,
        JobState::Running,
        JobState::Succeeded,
        JobState::Failed,
        JobState::Cancelled,
        JobState::Timeout,
    ];

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobState::Succeeded | JobState::Failed | JobState::Cancelled | JobState::Timeout
        )
    }

    /// States in which the job is occupying resources.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            JobState::Starting | JobState::Pulling | JobState::Running
        )
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Held => "held",
            JobState::Starting => "starting",
            JobState::Pulling => "pulling",
            JobState::Running => "running",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
            JobState::Timeout => "timeout",
        }
    }

    pub fn parse(s: &str) -> Option<JobState> {
        JobState::ALL.iter().copied().find(|st| st.as_str() == s)
    }
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Job {
    pub id: JobId,
    pub user_id: UserId,
    pub user_name: String,
    /// Node the job ran / will run on. `None` until scheduled. Single-node v1 always uses "local".
    pub node_id: Option<String>,
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub resources: ResourceRequest,
    pub state: JobState,
    pub priority: i32,
    pub depends_on: Vec<JobId>,
    pub array_index: Option<u32>,
    pub time_limit_secs: Option<u64>,
    pub submitted_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub gpu_ids: Vec<u32>,
    pub container_id: Option<String>,
    pub workspace_path: Option<String>,
    /// Human readable reason for failure / cancellation / pending.
    pub reason: Option<String>,
    /// Where `/out` and the log were copied after the job finished (e.g. on the NAS).
    #[serde(default)]
    pub archive_path: Option<String>,
    /// The SSD workspace has been removed by the retention sweep.
    #[serde(default)]
    pub workspace_deleted: bool,
    /// The long-lived task (environment) this job is a step of.
    #[serde(default)]
    pub task_id: Option<i64>,
    /// Snapshot the step's environment started from (`None` = the task's base image).
    #[serde(default)]
    pub base_snapshot_id: Option<i64>,
    /// Snapshot saved after this step succeeded.
    #[serde(default)]
    pub snapshot_id: Option<i64>,
    /// Node the user asked for (`--node`); `None` = any.
    #[serde(default)]
    pub requested_node: Option<String>,
}

impl Job {
    pub fn runtime_secs(&self, now: DateTime<Utc>) -> Option<u64> {
        let start = self.started_at?;
        let end = self.finished_at.unwrap_or(now);
        Some((end - start).num_seconds().max(0) as u64)
    }

    /// GPU-seconds consumed so far.
    pub fn gpu_seconds(&self, now: DateTime<Utc>) -> f64 {
        self.runtime_secs(now).unwrap_or(0) as f64 * self.gpu_ids.len() as f64
    }
}
