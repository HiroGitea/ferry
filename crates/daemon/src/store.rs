//! SQLite persistence (sqlx).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::Context;
use chrono::{DateTime, Utc};
use mlq_core::api::TokenInfo;
use mlq_core::{
    Job, JobId, JobState, Quota, ResourceRequest, Role, Snapshot, SnapshotId, Task, TaskId,
    TaskState, User, UserId,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{FromRow, Row, SqlitePool};

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

#[derive(FromRow)]
struct JobRow {
    id: i64,
    user_id: i64,
    user_name: String,
    node_id: Option<String>,
    name: String,
    image: String,
    command: String,
    env: String,
    cpus: f64,
    mem_bytes: i64,
    gpus: i64,
    state: String,
    priority: i64,
    depends_on: String,
    array_index: Option<i64>,
    time_limit_secs: Option<i64>,
    submitted_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    exit_code: Option<i64>,
    gpu_ids: String,
    container_id: Option<String>,
    workspace_path: Option<String>,
    reason: Option<String>,
    archive_path: Option<String>,
    workspace_deleted: i64,
    task_id: Option<i64>,
    base_snapshot_id: Option<i64>,
    snapshot_id: Option<i64>,
    requested_node: Option<String>,
}

impl From<JobRow> for Job {
    fn from(r: JobRow) -> Self {
        Job {
            id: r.id,
            user_id: r.user_id,
            user_name: r.user_name,
            node_id: r.node_id,
            name: r.name,
            image: r.image,
            command: serde_json::from_str(&r.command).unwrap_or_default(),
            env: serde_json::from_str(&r.env).unwrap_or_default(),
            resources: ResourceRequest {
                cpus: r.cpus,
                mem_bytes: r.mem_bytes as u64,
                gpus: r.gpus as u32,
            },
            state: JobState::parse(&r.state).unwrap_or(JobState::Failed),
            priority: r.priority as i32,
            depends_on: serde_json::from_str(&r.depends_on).unwrap_or_default(),
            array_index: r.array_index.map(|v| v as u32),
            time_limit_secs: r.time_limit_secs.map(|v| v as u64),
            submitted_at: r.submitted_at,
            started_at: r.started_at,
            finished_at: r.finished_at,
            exit_code: r.exit_code.map(|v| v as i32),
            gpu_ids: serde_json::from_str(&r.gpu_ids).unwrap_or_default(),
            container_id: r.container_id,
            workspace_path: r.workspace_path,
            reason: r.reason,
            archive_path: r.archive_path,
            workspace_deleted: r.workspace_deleted != 0,
            task_id: r.task_id,
            base_snapshot_id: r.base_snapshot_id,
            snapshot_id: r.snapshot_id,
            requested_node: r.requested_node,
        }
    }
}

#[derive(FromRow)]
struct UserRow {
    id: i64,
    name: String,
    role: String,
    created_at: DateTime<Utc>,
}

impl From<UserRow> for User {
    fn from(r: UserRow) -> Self {
        User {
            id: r.id,
            name: r.name,
            role: Role::parse(&r.role).unwrap_or(Role::User),
            created_at: r.created_at,
        }
    }
}

const JOB_SELECT: &str =
    "SELECT j.*, u.name AS user_name FROM jobs j JOIN users u ON u.id = j.user_id";

/// Parameters for creating a job.
pub struct NewJob {
    pub user_id: UserId,
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
    pub task_id: Option<i64>,
    pub requested_node: Option<String>,
}

pub fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn generate_token() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    format!(
        "mlq_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

impl Store {
    pub async fn open(path: &Path) -> anyhow::Result<Store> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("running migrations")?;
        Ok(Store { pool })
    }

    pub async fn open_memory() -> anyhow::Result<Store> {
        let opts = SqliteConnectOptions::new()
            .in_memory(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Store { pool })
    }

    /// Create the initial admin user and token if the user table is empty.
    /// The token is written to `<data_dir>/admin.token` (mode 0600) and logged.
    pub async fn bootstrap_admin(&self, data_dir: &Path) -> anyhow::Result<()> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        if n > 0 {
            return Ok(());
        }
        let user = self.create_user("admin", Role::Admin).await?;
        let (token, _) = self.create_token(user.id, Some("bootstrap")).await?;
        let path = data_dir.join("admin.token");
        std::fs::write(&path, format!("{token}\n"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        tracing::warn!(
            "created initial admin user; token written to {}",
            path.display()
        );
        Ok(())
    }

    // ---- users & tokens ----

    pub async fn create_user(&self, name: &str, role: Role) -> anyhow::Result<User> {
        let now = Utc::now();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO users (name, role, created_at) VALUES (?, ?, ?) RETURNING id",
        )
        .bind(name)
        .bind(role.as_str())
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .with_context(|| format!("creating user {name}"))?;
        Ok(User {
            id,
            name: name.to_string(),
            role,
            created_at: now,
        })
    }

    pub async fn list_users(&self) -> anyhow::Result<Vec<User>> {
        let rows: Vec<UserRow> = sqlx::query_as("SELECT * FROM users ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn user_by_name(&self, name: &str) -> anyhow::Result<Option<User>> {
        let row: Option<UserRow> = sqlx::query_as("SELECT * FROM users WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    pub async fn user_by_id(&self, id: UserId) -> anyhow::Result<Option<User>> {
        let row: Option<UserRow> = sqlx::query_as("SELECT * FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    pub async fn delete_user(&self, id: UserId) -> anyhow::Result<bool> {
        let r = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Returns (plaintext token, token id).
    pub async fn create_token(
        &self,
        user_id: UserId,
        name: Option<&str>,
    ) -> anyhow::Result<(String, i64)> {
        let token = generate_token();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO tokens (user_id, hash, name, created_at) VALUES (?, ?, ?, ?) RETURNING id",
        )
        .bind(user_id)
        .bind(hash_token(&token))
        .bind(name)
        .bind(Utc::now())
        .fetch_one(&self.pool)
        .await?;
        Ok((token, id))
    }

    pub async fn list_tokens(&self, user_id: Option<UserId>) -> anyhow::Result<Vec<TokenInfo>> {
        let rows = match user_id {
            Some(u) => sqlx::query("SELECT id, user_id, name, created_at, last_used_at FROM tokens WHERE user_id = ? ORDER BY id").bind(u).fetch_all(&self.pool).await?,
            None => sqlx::query("SELECT id, user_id, name, created_at, last_used_at FROM tokens ORDER BY id").fetch_all(&self.pool).await?,
        };
        Ok(rows
            .into_iter()
            .map(|r| TokenInfo {
                id: r.get("id"),
                user_id: r.get("user_id"),
                name: r.get("name"),
                created_at: r.get("created_at"),
                last_used_at: r.get("last_used_at"),
            })
            .collect())
    }

    pub async fn delete_token(&self, id: i64, user_id: Option<UserId>) -> anyhow::Result<bool> {
        let r = match user_id {
            Some(u) => {
                sqlx::query("DELETE FROM tokens WHERE id = ? AND user_id = ?")
                    .bind(id)
                    .bind(u)
                    .execute(&self.pool)
                    .await?
            }
            None => {
                sqlx::query("DELETE FROM tokens WHERE id = ?")
                    .bind(id)
                    .execute(&self.pool)
                    .await?
            }
        };
        Ok(r.rows_affected() > 0)
    }

    pub async fn user_for_token(&self, token: &str) -> anyhow::Result<Option<User>> {
        let hash = hash_token(token);
        let row: Option<(i64, i64)> =
            sqlx::query_as("SELECT id, user_id FROM tokens WHERE hash = ?")
                .bind(&hash)
                .fetch_optional(&self.pool)
                .await?;
        let Some((token_id, user_id)) = row else {
            return Ok(None);
        };
        let _ = sqlx::query("UPDATE tokens SET last_used_at = ? WHERE id = ?")
            .bind(Utc::now())
            .bind(token_id)
            .execute(&self.pool)
            .await;
        self.user_by_id(user_id).await
    }

    // ---- quotas ----

    pub async fn quota(&self, user_id: UserId) -> anyhow::Result<Quota> {
        let row = sqlx::query("SELECT max_running_jobs, max_running_gpus, max_queued_jobs FROM quotas WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            Some(r) => Quota {
                max_running_jobs: r
                    .get::<Option<i64>, _>("max_running_jobs")
                    .map(|v| v as u32),
                max_running_gpus: r
                    .get::<Option<i64>, _>("max_running_gpus")
                    .map(|v| v as u32),
                max_queued_jobs: r.get::<Option<i64>, _>("max_queued_jobs").map(|v| v as u32),
            },
            None => Quota::default(),
        })
    }

    pub async fn all_quotas(&self) -> anyhow::Result<HashMap<UserId, Quota>> {
        let rows = sqlx::query(
            "SELECT user_id, max_running_jobs, max_running_gpus, max_queued_jobs FROM quotas",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<i64, _>("user_id"),
                    Quota {
                        max_running_jobs: r
                            .get::<Option<i64>, _>("max_running_jobs")
                            .map(|v| v as u32),
                        max_running_gpus: r
                            .get::<Option<i64>, _>("max_running_gpus")
                            .map(|v| v as u32),
                        max_queued_jobs: r
                            .get::<Option<i64>, _>("max_queued_jobs")
                            .map(|v| v as u32),
                    },
                )
            })
            .collect())
    }

    pub async fn set_quota(&self, user_id: UserId, q: &Quota) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO quotas (user_id, max_running_jobs, max_running_gpus, max_queued_jobs) VALUES (?, ?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET max_running_jobs = excluded.max_running_jobs,
             max_running_gpus = excluded.max_running_gpus, max_queued_jobs = excluded.max_queued_jobs",
        )
        .bind(user_id)
        .bind(q.max_running_jobs.map(|v| v as i64))
        .bind(q.max_running_gpus.map(|v| v as i64))
        .bind(q.max_queued_jobs.map(|v| v as i64))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- jobs ----

    pub async fn create_job(&self, j: &NewJob) -> anyhow::Result<Job> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO jobs (user_id, name, image, command, env, cpus, mem_bytes, gpus, state, priority, depends_on,
             array_index, time_limit_secs, submitted_at, gpu_ids, task_id, requested_node)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '[]', ?, ?) RETURNING id",
        )
        .bind(j.user_id)
        .bind(&j.name)
        .bind(&j.image)
        .bind(serde_json::to_string(&j.command)?)
        .bind(serde_json::to_string(&j.env)?)
        .bind(j.resources.cpus)
        .bind(j.resources.mem_bytes as i64)
        .bind(j.resources.gpus as i64)
        .bind(j.state.as_str())
        .bind(j.priority as i64)
        .bind(serde_json::to_string(&j.depends_on)?)
        .bind(j.array_index.map(|v| v as i64))
        .bind(j.time_limit_secs.map(|v| v as i64))
        .bind(Utc::now())
        .bind(j.task_id)
        .bind(&j.requested_node)
        .fetch_one(&self.pool)
        .await?;
        self.job(id).await?.context("job vanished after insert")
    }

    pub async fn job(&self, id: JobId) -> anyhow::Result<Option<Job>> {
        let row: Option<JobRow> = sqlx::query_as(&format!("{JOB_SELECT} WHERE j.id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    pub async fn jobs_by_ids(&self, ids: &[JobId]) -> anyhow::Result<Vec<Job>> {
        let mut out = Vec::new();
        for id in ids {
            if let Some(j) = self.job(*id).await? {
                out.push(j);
            }
        }
        Ok(out)
    }

    /// Non-terminal jobs.
    pub async fn active_jobs(&self) -> anyhow::Result<Vec<Job>> {
        let rows: Vec<JobRow> = sqlx::query_as(&format!(
            "{JOB_SELECT} WHERE j.state IN ('queued','held','starting','pulling','running') ORDER BY j.id"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn list_jobs(
        &self,
        state: Option<JobState>,
        user_id: Option<UserId>,
        all: bool,
        limit: u32,
    ) -> anyhow::Result<Vec<Job>> {
        let mut sql = format!("{JOB_SELECT} WHERE 1=1");
        if let Some(s) = state {
            sql.push_str(&format!(" AND j.state = '{}'", s.as_str()));
        } else if !all {
            sql.push_str(" AND j.state IN ('queued','held','starting','pulling','running')");
        }
        if user_id.is_some() {
            sql.push_str(" AND j.user_id = ?");
        }
        sql.push_str(" ORDER BY j.id DESC LIMIT ?");
        let mut q = sqlx::query_as::<_, JobRow>(&sql);
        if let Some(u) = user_id {
            q = q.bind(u);
        }
        q = q.bind(limit as i64);
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Jobs that started within the window (for usage accounting).
    pub async fn jobs_started_between(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        user_id: Option<UserId>,
    ) -> anyhow::Result<Vec<Job>> {
        let mut sql = format!(
            "{JOB_SELECT} WHERE j.started_at IS NOT NULL AND j.started_at >= ? AND j.started_at <= ?"
        );
        if user_id.is_some() {
            sql.push_str(" AND j.user_id = ?");
        }
        sql.push_str(" ORDER BY j.id");
        let mut q = sqlx::query_as::<_, JobRow>(&sql).bind(from).bind(to);
        if let Some(u) = user_id {
            q = q.bind(u);
        }
        Ok(q.fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub async fn finished_jobs_since(&self, since: DateTime<Utc>) -> anyhow::Result<Vec<Job>> {
        let rows: Vec<JobRow> = sqlx::query_as(&format!(
            "{JOB_SELECT} WHERE j.finished_at IS NOT NULL AND j.finished_at >= ?"
        ))
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Finished jobs whose workspace has not been deleted yet, oldest first.
    pub async fn finished_jobs_with_workspace(&self) -> anyhow::Result<Vec<Job>> {
        let rows: Vec<JobRow> = sqlx::query_as(&format!(
            "{JOB_SELECT} WHERE j.finished_at IS NOT NULL AND j.workspace_deleted = 0 AND j.workspace_path IS NOT NULL AND j.task_id IS NULL ORDER BY j.finished_at"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn mark_archived(&self, id: JobId, path: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE jobs SET archived = 1, archive_path = ? WHERE id = ?")
            .bind(path)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Finished jobs whose results have not been archived yet (only meaningful when an archive dir is configured).
    pub async fn unarchived_finished_jobs(&self) -> anyhow::Result<Vec<Job>> {
        let rows: Vec<JobRow> = sqlx::query_as(&format!(
            "{JOB_SELECT} WHERE j.finished_at IS NOT NULL AND j.archived = 0 AND j.workspace_deleted = 0 AND j.workspace_path IS NOT NULL AND j.task_id IS NULL ORDER BY j.finished_at"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn mark_workspace_deleted(&self, id: JobId) -> anyhow::Result<()> {
        sqlx::query("UPDATE jobs SET workspace_deleted = 1 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn count_user_jobs(
        &self,
        user_id: UserId,
        states: &[JobState],
    ) -> anyhow::Result<u32> {
        let list = states
            .iter()
            .map(|s| format!("'{}'", s.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        let n: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM jobs WHERE user_id = ? AND state IN ({list})"
        ))
        .bind(user_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(n as u32)
    }

    pub async fn set_state(
        &self,
        id: JobId,
        state: JobState,
        reason: Option<&str>,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE jobs SET state = ?, reason = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(reason)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_requested_node(&self, id: JobId, node: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE jobs SET requested_node = ? WHERE id = ?")
            .bind(node)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_reason(&self, id: JobId, reason: Option<&str>) -> anyhow::Result<()> {
        sqlx::query("UPDATE jobs SET reason = ? WHERE id = ?")
            .bind(reason)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_started(
        &self,
        id: JobId,
        gpu_ids: &[u32],
        node_id: &str,
        workspace: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE jobs SET state = 'running', started_at = ?, gpu_ids = ?, node_id = ?, workspace_path = ?, reason = NULL WHERE id = ?")
            .bind(Utc::now())
            .bind(serde_json::to_string(gpu_ids)?)
            .bind(node_id)
            .bind(workspace)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Claim resources for a job that is about to start (state `starting`).
    pub async fn set_assigned(
        &self,
        id: JobId,
        gpu_ids: &[u32],
        node_id: &str,
        workspace: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE jobs SET state = 'starting', gpu_ids = ?, node_id = ?, workspace_path = ?, reason = NULL WHERE id = ?")
            .bind(serde_json::to_string(gpu_ids)?)
            .bind(node_id)
            .bind(workspace)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_container(&self, id: JobId, container_id: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE jobs SET container_id = ? WHERE id = ?")
            .bind(container_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_finished(
        &self,
        id: JobId,
        state: JobState,
        exit_code: Option<i32>,
        reason: Option<&str>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE jobs SET state = ?, finished_at = ?, exit_code = ?, reason = ? WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(Utc::now())
        .bind(exit_code)
        .bind(reason)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn scratch_jobs_bytes(&self) -> anyhow::Result<Vec<(JobId, Option<String>)>> {
        let rows = sqlx::query("SELECT id, workspace_path FROM jobs WHERE workspace_deleted = 0 AND workspace_path IS NOT NULL").fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get("id"), r.get("workspace_path")))
            .collect())
    }
}

// ---------------------------------------------------------------- tasks & snapshots

#[derive(FromRow)]
struct TaskRow {
    id: i64,
    user_id: i64,
    user_name: String,
    name: String,
    base_image: String,
    cpus: f64,
    mem_bytes: i64,
    gpus: i64,
    keep_warm_secs: i64,
    state: String,
    head_snapshot_id: Option<i64>,
    node_id: Option<String>,
    workspace_path: Option<String>,
    archive_path: Option<String>,
    warm_container: Option<String>,
    warm_until: Option<DateTime<Utc>>,
    warm_gpus: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    closed_at: Option<DateTime<Utc>>,
}

impl From<TaskRow> for Task {
    fn from(r: TaskRow) -> Self {
        Task {
            id: r.id,
            user_id: r.user_id,
            user_name: r.user_name,
            name: r.name,
            base_image: r.base_image,
            resources: ResourceRequest {
                cpus: r.cpus,
                mem_bytes: r.mem_bytes as u64,
                gpus: r.gpus as u32,
            },
            keep_warm_secs: r.keep_warm_secs.max(0) as u64,
            state: TaskState::parse(&r.state).unwrap_or(TaskState::Closed),
            head_snapshot_id: r.head_snapshot_id,
            node_id: r.node_id,
            workspace_path: r.workspace_path,
            archive_path: r.archive_path,
            warm_container: r.warm_container,
            warm_until: r.warm_until,
            warm_gpus: serde_json::from_str(&r.warm_gpus).unwrap_or_default(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            closed_at: r.closed_at,
        }
    }
}

#[derive(FromRow)]
struct SnapshotRow {
    id: i64,
    task_id: i64,
    parent_id: Option<i64>,
    job_id: Option<i64>,
    image: String,
    size_bytes: Option<i64>,
    note: Option<String>,
    pinned: i64,
    deleted: i64,
    created_at: DateTime<Utc>,
}

impl From<SnapshotRow> for Snapshot {
    fn from(r: SnapshotRow) -> Self {
        Snapshot {
            id: r.id,
            task_id: r.task_id,
            parent_id: r.parent_id,
            job_id: r.job_id,
            image: r.image,
            size_bytes: r.size_bytes.map(|v| v.max(0) as u64),
            note: r.note,
            pinned: r.pinned != 0,
            deleted: r.deleted != 0,
            created_at: r.created_at,
        }
    }
}

const TASK_SELECT: &str =
    "SELECT t.*, u.name AS user_name FROM tasks t JOIN users u ON u.id = t.user_id";

/// What a warm container was started with; it is reused only for an identical step setup.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WarmSpec {
    pub image: String,
    pub cpus: f64,
    pub mem_bytes: u64,
    pub gpus: Vec<u32>,
}

impl Store {
    pub async fn create_task(
        &self,
        user_id: UserId,
        name: &str,
        base_image: &str,
        res: &ResourceRequest,
        keep_warm_secs: u64,
        node_id: Option<&str>,
    ) -> anyhow::Result<Task> {
        let now = Utc::now();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO tasks (user_id, name, base_image, cpus, mem_bytes, gpus, keep_warm_secs, state, created_at, updated_at, node_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, 'open', ?, ?, ?) RETURNING id",
        )
        .bind(user_id)
        .bind(name)
        .bind(base_image)
        .bind(res.cpus)
        .bind(res.mem_bytes as i64)
        .bind(res.gpus as i64)
        .bind(keep_warm_secs as i64)
        .bind(now)
        .bind(now)
        .bind(node_id)
        .fetch_one(&self.pool)
        .await?;
        self.task(id).await?.context("task vanished after insert")
    }

    pub async fn task(&self, id: TaskId) -> anyhow::Result<Option<Task>> {
        let row: Option<TaskRow> = sqlx::query_as(&format!("{TASK_SELECT} WHERE t.id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    pub async fn list_tasks(
        &self,
        user_id: Option<UserId>,
        include_closed: bool,
    ) -> anyhow::Result<Vec<Task>> {
        let mut sql = format!("{TASK_SELECT} WHERE t.state != 'deleted'");
        if !include_closed {
            sql.push_str(" AND t.state = 'open'");
        }
        if user_id.is_some() {
            sql.push_str(" AND t.user_id = ?");
        }
        sql.push_str(" ORDER BY t.updated_at DESC");
        let mut q = sqlx::query_as::<_, TaskRow>(&sql);
        if let Some(u) = user_id {
            q = q.bind(u);
        }
        Ok(q.fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    /// Tasks that currently have a warm container recorded.
    pub async fn warm_tasks(&self) -> anyhow::Result<Vec<Task>> {
        let rows: Vec<TaskRow> =
            sqlx::query_as(&format!("{TASK_SELECT} WHERE t.warm_container IS NOT NULL"))
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn touch_task(&self, id: TaskId) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET updated_at = ? WHERE id = ?")
            .bind(Utc::now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn update_task(
        &self,
        id: TaskId,
        name: Option<&str>,
        keep_warm_secs: Option<u64>,
        res: Option<&ResourceRequest>,
    ) -> anyhow::Result<()> {
        if let Some(n) = name {
            sqlx::query("UPDATE tasks SET name = ? WHERE id = ?")
                .bind(n)
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        if let Some(k) = keep_warm_secs {
            sqlx::query("UPDATE tasks SET keep_warm_secs = ? WHERE id = ?")
                .bind(k as i64)
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        if let Some(r) = res {
            sqlx::query("UPDATE tasks SET cpus = ?, mem_bytes = ?, gpus = ? WHERE id = ?")
                .bind(r.cpus)
                .bind(r.mem_bytes as i64)
                .bind(r.gpus as i64)
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        self.touch_task(id).await
    }

    pub async fn set_task_state(&self, id: TaskId, st: TaskState) -> anyhow::Result<()> {
        let closed = if st == TaskState::Open {
            None
        } else {
            Some(Utc::now())
        };
        sqlx::query("UPDATE tasks SET state = ?, closed_at = ?, updated_at = ? WHERE id = ?")
            .bind(st.as_str())
            .bind(closed)
            .bind(Utc::now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_task_node(&self, id: TaskId, node: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET node_id = ? WHERE id = ?")
            .bind(node)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_task_workspace(&self, id: TaskId, path: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET workspace_path = ? WHERE id = ?")
            .bind(path)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_task_archive(&self, id: TaskId, path: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET archive_path = ? WHERE id = ?")
            .bind(path)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_task_head(
        &self,
        id: TaskId,
        snapshot: Option<SnapshotId>,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET head_snapshot_id = ?, updated_at = ? WHERE id = ?")
            .bind(snapshot)
            .bind(Utc::now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_task_warm(
        &self,
        id: TaskId,
        container: &str,
        until: DateTime<Utc>,
        spec: &WarmSpec,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET warm_container = ?, warm_until = ?, warm_gpus = ?, warm_spec = ? WHERE id = ?")
            .bind(container)
            .bind(until)
            .bind(serde_json::to_string(&spec.gpus)?)
            .bind(serde_json::to_string(spec)?)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn task_warm_spec(&self, id: TaskId) -> anyhow::Result<Option<WarmSpec>> {
        let s: Option<Option<String>> =
            sqlx::query_scalar("SELECT warm_spec FROM tasks WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(s.flatten().and_then(|s| serde_json::from_str(&s).ok()))
    }

    pub async fn clear_task_warm(&self, id: TaskId) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET warm_container = NULL, warm_until = NULL, warm_gpus = '[]', warm_spec = NULL WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn task_steps(&self, task_id: TaskId) -> anyhow::Result<Vec<Job>> {
        let rows: Vec<JobRow> =
            sqlx::query_as(&format!("{JOB_SELECT} WHERE j.task_id = ? ORDER BY j.id"))
                .bind(task_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn set_job_snapshots(
        &self,
        job_id: JobId,
        base: Option<SnapshotId>,
        produced: Option<SnapshotId>,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE jobs SET base_snapshot_id = ?, snapshot_id = COALESCE(?, snapshot_id) WHERE id = ?")
            .bind(base)
            .bind(produced)
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_job_image(&self, job_id: JobId, image: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE jobs SET image = ? WHERE id = ?")
            .bind(image)
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn create_snapshot(
        &self,
        task_id: TaskId,
        parent: Option<SnapshotId>,
        job_id: Option<JobId>,
        image: &str,
        size: Option<u64>,
    ) -> anyhow::Result<Snapshot> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO snapshots (task_id, parent_id, job_id, image, size_bytes, created_at) VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(task_id)
        .bind(parent)
        .bind(job_id)
        .bind(image)
        .bind(size.map(|v| v as i64))
        .bind(Utc::now())
        .fetch_one(&self.pool)
        .await?;
        self.snapshot(id).await?.context("snapshot vanished")
    }

    pub async fn snapshot(&self, id: SnapshotId) -> anyhow::Result<Option<Snapshot>> {
        let row: Option<SnapshotRow> = sqlx::query_as("SELECT * FROM snapshots WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    pub async fn snapshots(&self, task_id: TaskId) -> anyhow::Result<Vec<Snapshot>> {
        let rows: Vec<SnapshotRow> =
            sqlx::query_as("SELECT * FROM snapshots WHERE task_id = ? ORDER BY id")
                .bind(task_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn next_snapshot_seq(&self, task_id: TaskId) -> anyhow::Result<i64> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM snapshots WHERE task_id = ?")
            .bind(task_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(n + 1)
    }

    pub async fn pin_snapshot(
        &self,
        id: SnapshotId,
        pinned: bool,
        note: Option<&str>,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE snapshots SET pinned = ?, note = COALESCE(?, note) WHERE id = ?")
            .bind(pinned as i64)
            .bind(note)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_snapshot_deleted(&self, id: SnapshotId) -> anyhow::Result<()> {
        sqlx::query("UPDATE snapshots SET deleted = 1 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------- nodes

impl Store {
    pub async fn upsert_node(&self, reg: &mlq_core::api::NodeRegistration) -> anyhow::Result<()> {
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO nodes (id, url, hostname, info, first_seen, last_seen) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET url = excluded.url, hostname = excluded.hostname, info = excluded.info, last_seen = excluded.last_seen",
        )
        .bind(&reg.id)
        .bind(&reg.url)
        .bind(&reg.hostname)
        .bind(serde_json::to_string(reg)?)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn known_nodes(&self) -> anyhow::Result<Vec<mlq_core::api::NodeRegistration>> {
        let rows = sqlx::query("SELECT info FROM nodes ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| serde_json::from_str(&r.get::<String, _>("info")).ok())
            .collect())
    }

    pub async fn delete_node(&self, id: &str) -> anyhow::Result<bool> {
        let r = sqlx::query("DELETE FROM nodes WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }
}
