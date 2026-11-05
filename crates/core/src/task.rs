//! Long-lived tasks: an environment (container image + /work + /out) that several steps (jobs)
//! run in one after another. Each successful step is saved as a snapshot ("存档"); a failed
//! step rolls the environment back to the last snapshot.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::job::Job;
use crate::resource::ResourceRequest;
use crate::user::UserId;

pub type TaskId = i64;
pub type SnapshotId = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Open,
    Closed,
    Deleted,
}

impl TaskState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::Open => "open",
            TaskState::Closed => "closed",
            TaskState::Deleted => "deleted",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(TaskState::Open),
            "closed" => Some(TaskState::Closed),
            "deleted" => Some(TaskState::Deleted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Task {
    pub id: TaskId,
    pub user_id: UserId,
    pub user_name: String,
    pub name: String,
    /// Image the first step starts from.
    pub base_image: String,
    /// Default resources for steps that do not specify their own.
    pub resources: ResourceRequest,
    /// Keep the container alive this long after a successful step so the next step starts instantly.
    pub keep_warm_secs: u64,
    pub state: TaskState,
    /// Snapshot the next step starts from (`None` = `base_image`).
    pub head_snapshot_id: Option<SnapshotId>,
    /// Node holding the environment and workspace; set by the first step (or at creation with --node).
    #[serde(default)]
    pub node_id: Option<String>,
    pub workspace_path: Option<String>,
    pub archive_path: Option<String>,
    /// Container kept warm after the last step, and until when.
    pub warm_container: Option<String>,
    pub warm_until: Option<DateTime<Utc>>,
    pub warm_gpus: Vec<u32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Snapshot {
    pub id: SnapshotId,
    pub task_id: TaskId,
    pub parent_id: Option<SnapshotId>,
    /// Step that produced it.
    pub job_id: Option<i64>,
    /// Local image reference (e.g. `localhost/mlq-task-3:4`).
    pub image: String,
    pub size_bytes: Option<u64>,
    pub note: Option<String>,
    /// Pinned snapshots are never pruned.
    pub pinned: bool,
    /// Image removed by pruning (row kept for history).
    pub deleted: bool,
    pub created_at: DateTime<Utc>,
}

/// Summary row for task lists.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskSummary {
    pub task: Task,
    pub steps: u32,
    pub snapshots: u32,
    /// "running", "queued", "warm", "idle" or "closed".
    pub activity: String,
    pub last_step: Option<Job>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskDetail {
    pub summary: TaskSummary,
    pub steps: Vec<Job>,
    pub snapshots: Vec<Snapshot>,
    /// Image the next step will start from.
    pub head_image: String,
}
