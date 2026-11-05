//! Wire types shared between daemon, client, CLI, MCP and the web dashboard.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::job::{Job, JobId, JobState};
use crate::resource::ResourceRequest;
use crate::user::{Quota, Role, User, UserId};

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SubmitJobRequest {
    pub name: Option<String>,
    /// Container image. Defaults to the daemon's `default_image`.
    pub image: Option<String>,
    /// Command and arguments executed inside the container, in `/work`.
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub resources: ResourceRequest,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub depends_on: Vec<JobId>,
    /// Array spec like "0-3" or "0,2,5". Expands into one job per index with `MLQ_ARRAY_INDEX` set.
    pub array: Option<String>,
    pub time_limit_secs: Option<u64>,
    /// Submit in `Held` state (release with `POST /jobs/{id}/release`), e.g. while uploading files.
    #[serde(default)]
    pub hold: bool,
    /// Small text files to write into `/work` before start: path -> content.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Validate and report a scheduling estimate without creating the job.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SubmitJobResponse {
    pub dry_run: bool,
    pub jobs: Vec<Job>,
    /// Human-readable scheduling estimate (dry-run or queued).
    pub estimate: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListJobsQuery {
    pub state: Option<JobState>,
    pub user: Option<String>,
    /// Include finished jobs (default: only non-terminal).
    #[serde(default)]
    pub all: bool,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogQuery {
    /// Return at most the last N lines (ignored when `offset` is set).
    pub tail: Option<usize>,
    /// Byte offset to read from (for following).
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LogChunk {
    pub job_id: JobId,
    pub content: String,
    /// Byte offset after this chunk; pass as `offset` to continue.
    pub next_offset: u64,
    pub state: JobState,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ArtifactEntry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobResult {
    pub job: Job,
    pub artifacts: Vec<ArtifactEntry>,
    pub log_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub uuid: String,
    pub mem_total: u64,
    pub mem_used: u64,
    pub util_percent: u32,
    pub temp_c: Option<u32>,
    /// Whether this GPU is in the scheduler's managed pool.
    pub managed: bool,
    /// Job currently assigned to this GPU by the scheduler, if any.
    pub job_id: Option<JobId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeInfo {
    pub id: String,
    pub hostname: String,
    pub cpus_total: f64,
    pub mem_total: u64,
    /// Managed pool (after reservations).
    pub cpus_managed: f64,
    pub mem_managed: u64,
    pub gpus_managed: Vec<u32>,
    pub runtime: String,
    pub default_image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClusterStatus {
    pub node: NodeInfo,
    pub gpus: Vec<GpuInfo>,
    pub queued_jobs: u32,
    pub running_jobs: u32,
    pub free_cpus: f64,
    pub free_mem: u64,
    pub free_gpus: Vec<u32>,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Me {
    pub user: User,
    pub quota: Quota,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateUserRequest {
    pub name: String,
    #[serde(default = "default_role")]
    pub role: Role,
}

fn default_role() -> Role {
    Role::User
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTokenRequest {
    pub name: Option<String>,
    /// Admin only: create a token for another user.
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenCreated {
    pub id: i64,
    pub user: User,
    /// Plaintext token, shown only once.
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    pub id: i64,
    pub user_id: UserId,
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageQuery {
    pub user: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UserUsage {
    pub user: User,
    pub quota: Quota,
    pub jobs_total: u32,
    pub jobs_succeeded: u32,
    pub jobs_failed: u32,
    pub running_jobs: u32,
    pub running_gpus: u32,
    pub queued_jobs: u32,
    pub gpu_seconds: f64,
    pub cpu_seconds: f64,
    /// Decayed GPU-seconds used for fair-share ordering (lower = higher priority).
    pub fair_share_usage: f64,
    /// Per-day GPU seconds within the query window, sorted by day ("YYYY-MM-DD").
    pub daily: Vec<DailyUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DailyUsage {
    pub day: String,
    pub gpu_seconds: f64,
    pub cpu_seconds: f64,
    pub jobs: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImageInfo {
    pub reference: String,
    pub id: String,
    pub size: u64,
    pub created: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullImageRequest {
    pub reference: String,
}

/// Server-sent events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    JobUpdated { job: Job },
    JobLog { job_id: JobId, line: String },
    GpusUpdated,
    Heartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}
