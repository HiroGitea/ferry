//! Wire types shared between daemon, client, CLI, MCP and the web dashboard.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::job::{Job, JobId, JobState};
use crate::resource::ResourceOverrides;
use crate::task::{SnapshotId, TaskId};
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
    /// Unset fields come from the task's defaults (for task steps) or the global default
    /// (2 CPUs, 8G, 0 GPUs).
    #[serde(default)]
    pub resources: ResourceOverrides,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub depends_on: Vec<JobId>,
    /// Array spec like "0-3" or "0,2,5". Expands into one job per index with `FERRY_ARRAY_INDEX` set.
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
    /// Run as the next step of this task: starts from the task's latest snapshot, shares its
    /// /work and /out. `image` must not be set.
    #[serde(default)]
    pub task_id: Option<TaskId>,
    /// Run on this node only (default: any node with free resources).
    #[serde(default)]
    pub node: Option<String>,
    /// Admin only: if the job does not fit right now, preempt the lowest-priority running jobs
    /// (they are killed and requeued) until it does.
    #[serde(default)]
    pub preempt: bool,
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
    #[serde(default)]
    pub node_id: String,
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

/// One node as seen by the control daemon.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeStatus {
    pub info: NodeInfo,
    pub online: bool,
    pub local: bool,
    pub url: Option<String>,
    pub last_seen: DateTime<Utc>,
    pub gpus: Vec<GpuInfo>,
    pub running_jobs: u32,
    pub free_cpus: f64,
    pub free_mem: u64,
    pub free_gpus: Vec<u32>,
    /// Container paths of the mounts this node offers (e.g. "/data").
    pub mounts: Vec<String>,
    pub supports_snapshots: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClusterStatus {
    /// The node the API was asked on (local node, or the first node for a pure control daemon).
    pub node: NodeInfo,
    pub nodes: Vec<NodeStatus>,
    /// All GPUs of all nodes (`node_id` tells them apart).
    pub gpus: Vec<GpuInfo>,
    pub queued_jobs: u32,
    pub running_jobs: u32,
    pub free_cpus: f64,
    pub free_mem: u64,
    /// Free GPUs on all nodes, as "node:index" when there is more than one node.
    pub free_gpus: Vec<u32>,
    pub free_gpu_count: u32,
    pub managed_gpu_count: u32,
    pub version: String,
}

/// What an agent reports when registering with the control daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRegistration {
    pub id: String,
    pub url: String,
    pub hostname: String,
    pub runtime: String,
    pub version: String,
    pub cpus_total: f64,
    pub mem_total: u64,
    pub cpus_managed: f64,
    pub mem_managed: u64,
    pub gpus_managed: Vec<u32>,
    /// Memory of each managed GPU (agents older than this field report none).
    #[serde(default)]
    pub gpu_mem: std::collections::BTreeMap<u32, u64>,
    pub scratch_root: String,
    pub default_image: String,
    pub mounts: Vec<String>,
    pub supports_snapshots: bool,
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

/// How a webhook body is shaped. `json` is ferry's own payload; the others match the chat bots'
/// incoming-webhook formats so the URL can point straight at them.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum WebhookFormat {
    #[default]
    Json,
    Slack,
    Feishu,
    Dingtalk,
    Wecom,
}

impl WebhookFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            WebhookFormat::Json => "json",
            WebhookFormat::Slack => "slack",
            WebhookFormat::Feishu => "feishu",
            WebhookFormat::Dingtalk => "dingtalk",
            WebhookFormat::Wecom => "wecom",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "json" => WebhookFormat::Json,
            "slack" => WebhookFormat::Slack,
            "feishu" | "lark" => WebhookFormat::Feishu,
            "dingtalk" => WebhookFormat::Dingtalk,
            "wecom" => WebhookFormat::Wecom,
            _ => return None,
        })
    }
}

/// A user's notification channels and which events they want.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifySettings {
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub webhook_format: WebhookFormat,
    #[serde(default)]
    pub telegram_chat_id: Option<String>,
    /// Job (or task step, see `task_steps`) succeeded.
    #[serde(default = "yes")]
    pub on_success: bool,
    /// Failed or timed out. Idle warnings and admin actions are always sent.
    #[serde(default = "yes")]
    pub on_failure: bool,
    /// Also notify for every task step, not only plain jobs.
    #[serde(default)]
    pub task_steps: bool,
}

fn yes() -> bool {
    true
}

impl Default for NotifySettings {
    fn default() -> Self {
        Self {
            webhook_url: None,
            webhook_format: WebhookFormat::Json,
            telegram_chat_id: None,
            on_success: true,
            on_failure: true,
            task_steps: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyInfo {
    pub settings: NotifySettings,
    /// The server has a Telegram bot configured.
    pub telegram_available: bool,
}

/// Admin: create an invite link.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateInviteRequest {
    /// Role of the users who register with it (default `user`).
    #[serde(default)]
    pub role: Option<Role>,
    /// Lock the invite to this user name.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// How many accounts it can create (default 1).
    #[serde(default)]
    pub max_uses: Option<u32>,
    /// Lifetime in seconds (default 7 days).
    #[serde(default)]
    pub expires_in_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteInfo {
    pub id: i64,
    pub role: Role,
    pub name: Option<String>,
    pub note: Option<String>,
    pub max_uses: u32,
    pub uses: u32,
    pub expires_at: DateTime<Utc>,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Users who registered with this invite.
    #[serde(default)]
    pub used_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteCreated {
    pub invite: InviteInfo,
    /// Plaintext code, shown only once. The link is `<server>/register?code=<code>`.
    pub code: String,
}

/// Public: what an invite code allows, shown on the registration page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitePreview {
    pub role: Role,
    pub name: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub remaining_uses: u32,
}

/// Public: register with an invite code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub code: String,
    pub name: String,
    /// Name of the first token (default "registration").
    #[serde(default)]
    pub token_name: Option<String>,
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

/// Server-sent events. Short-lived and cloned per subscriber, so the unboxed `Job` is fine.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    JobUpdated { job: Job },
    TaskUpdated { task_id: TaskId },
    JobLog { job_id: JobId, line: String },
    GpusUpdated,
    Heartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

// ---------- tasks ----------

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateTaskRequest {
    pub name: String,
    /// Base image (default: the daemon's default_image).
    pub image: Option<String>,
    /// Default resources for steps.
    #[serde(default)]
    pub resources: ResourceOverrides,
    /// Keep the container running this long after a successful step (0 = stop immediately).
    pub keep_warm_secs: Option<u64>,
    /// Pin the task to this node (default: the node of the first step).
    pub node: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UpdateTaskRequest {
    pub name: Option<String>,
    pub keep_warm_secs: Option<u64>,
    #[serde(default)]
    pub resources: ResourceOverrides,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CheckoutRequest {
    /// Snapshot to make the task's HEAD; `None` resets to the base image.
    pub snapshot_id: Option<SnapshotId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PinSnapshotRequest {
    pub pinned: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListTasksQuery {
    /// Include closed tasks.
    #[serde(default)]
    pub all: bool,
    /// User name, or "all" (admin).
    pub user: Option<String>,
}

// ---------- metrics ----------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsQuery {
    /// Comma separated keys; a trailing `*` matches a prefix (e.g. `host.*,gpu.0.*`).
    pub keys: String,
    /// Unix seconds, or negative = relative to now (e.g. -600).
    pub after: Option<i64>,
    /// Unix seconds, 0/None = now, negative = relative to now.
    pub before: Option<i64>,
    /// Desired number of points per series (default 300).
    pub points: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MetricSeries {
    pub key: String,
    pub unit: String,
    /// `[t, avg]` per bucket; `None` = no data.
    pub points: Vec<(i64, Option<f32>)>,
    /// Bucket maxima (same timestamps), so peaks survive downsampling.
    pub max: Vec<Option<f32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MetricsResult {
    pub after: i64,
    pub before: i64,
    /// Seconds per point.
    pub step: i64,
    /// 0 = per-second, 1 = per-minute, 2 = per-hour.
    pub tier: u8,
    pub series: Vec<MetricSeries>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MetricInfo {
    pub key: String,
    pub unit: String,
    pub last_t: Option<i64>,
}

/// One collection tick pushed over `/metrics/stream`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricTick {
    pub t: i64,
    pub values: std::collections::BTreeMap<String, f32>,
}

// ---------- task archives (.7z on the NAS) ----------

/// `manifest.json` inside a task archive.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ArchiveManifest {
    /// Archive format version.
    pub format: u32,
    pub created_at: DateTime<Utc>,
    pub ferry_version: String,
    pub hostname: String,
    pub task: crate::task::Task,
    pub steps: Vec<Job>,
    pub snapshots: Vec<crate::task::Snapshot>,
    /// Snapshot images included under `env/`: snapshot id -> file name. `None` id = base image.
    pub images: Vec<ArchiveImage>,
    /// Relative path -> sha256 of every file except manifest.json.
    pub checksums: std::collections::BTreeMap<String, String>,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ArchiveImage {
    pub snapshot_id: Option<SnapshotId>,
    pub image: String,
    pub file: String,
    pub bytes: u64,
    pub is_head: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ArchiveInfo {
    /// Path relative to the archive root (what `import` accepts).
    pub name: String,
    pub path: String,
    pub bytes: u64,
    pub modified_at: DateTime<Utc>,
    pub user: String,
    pub task_id: Option<TaskId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExportTaskRequest {
    /// Which snapshot images to include: "none", "head" (default from config), "pinned", "all".
    pub images: Option<String>,
    /// Also remove the task's SSD workspace and snapshot images after a verified export
    /// (the task must be closed).
    #[serde(default)]
    pub offload: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExportTaskResponse {
    pub path: String,
    pub bytes: u64,
    pub manifest: ArchiveManifest,
    pub offloaded: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImportArchiveRequest {
    /// Archive name as listed by `GET /archives` (relative to the archive root).
    pub name: String,
    /// Name for the new task (default: the archived name).
    pub task_name: Option<String>,
    /// Node to restore onto (default: the primary node). Needs a container runtime for the image.
    pub node: Option<String>,
}

/// Training curves parsed from a JSON-lines file in `/out` (default `metrics.jsonl`): one object
/// per line, e.g. `{"step": 100, "loss": 0.42, "lr": 1e-4}`. Every numeric field except the x key
/// becomes a series.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TrainMetrics {
    pub file: String,
    /// Field used as x: "step", "iter", "iteration", "global_step", "epoch", or "line" (the line number).
    pub x_key: String,
    /// series name -> [x, y] points, downsampled to at most `max_points` each.
    pub series: BTreeMap<String, Vec<(f64, f64)>>,
    /// Lines read (including ones that were not JSON objects).
    pub lines: usize,
    /// Only the last part of a very large file was read.
    pub truncated: bool,
}

impl TrainMetrics {
    /// Parse JSON lines. Non-object lines and non-finite numbers are skipped.
    pub fn parse(file: &str, text: &str, max_points: usize) -> TrainMetrics {
        const X_KEYS: [&str; 5] = ["step", "global_step", "iteration", "iter", "epoch"];
        let rows: Vec<serde_json::Map<String, serde_json::Value>> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
            .filter_map(|v| match v {
                serde_json::Value::Object(m) => Some(m),
                _ => None,
            })
            .collect();
        let x_key = X_KEYS
            .iter()
            .find(|k| {
                rows.iter()
                    .any(|r| r.get(**k).and_then(|v| v.as_f64()).is_some())
            })
            .map(|k| k.to_string())
            .unwrap_or_else(|| "line".into());
        let mut series: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
        for (i, r) in rows.iter().enumerate() {
            let x = if x_key == "line" {
                i as f64
            } else {
                match r.get(&x_key).and_then(|v| v.as_f64()) {
                    Some(x) => x,
                    None => continue,
                }
            };
            for (k, v) in r {
                if *k == x_key {
                    continue;
                }
                if let Some(y) = v.as_f64().filter(|y| y.is_finite()) {
                    series.entry(k.clone()).or_default().push((x, y));
                }
            }
        }
        if max_points > 0 {
            for pts in series.values_mut() {
                if pts.len() > max_points {
                    // Keep every n-th point and always the last one.
                    let stride = pts.len().div_ceil(max_points);
                    let last = *pts.last().unwrap();
                    let mut kept: Vec<(f64, f64)> = pts.iter().step_by(stride).copied().collect();
                    if kept.last() != Some(&last) {
                        kept.push(last);
                    }
                    *pts = kept;
                }
            }
        }
        TrainMetrics {
            file: file.to_string(),
            x_key,
            series,
            lines: text.lines().count(),
            truncated: false,
        }
    }
}

#[cfg(test)]
mod train_tests {
    use super::TrainMetrics;

    #[test]
    fn parses_jsonl_with_step_key() {
        let text = "{\"step\":1,\"loss\":2.0,\"tag\":\"a\"}\nnot json\n{\"step\":2,\"loss\":1.5,\"acc\":0.3}\n[1,2]\n";
        let m = TrainMetrics::parse("metrics.jsonl", text, 0);
        assert_eq!(m.x_key, "step");
        assert_eq!(m.series["loss"], vec![(1.0, 2.0), (2.0, 1.5)]);
        assert_eq!(m.series["acc"], vec![(2.0, 0.3)]);
        assert!(!m.series.contains_key("tag"));
        assert_eq!(m.lines, 4);
    }

    #[test]
    fn falls_back_to_line_number_and_downsamples() {
        let text: String = (0..1000).map(|i| format!("{{\"loss\":{i}}}\n")).collect();
        let m = TrainMetrics::parse("m", &text, 100);
        assert_eq!(m.x_key, "line");
        let pts = &m.series["loss"];
        assert!(pts.len() <= 101);
        assert_eq!(pts.last(), Some(&(999.0, 999.0)));
    }
}
