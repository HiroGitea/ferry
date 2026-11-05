//! SQLite persistence (sqlx).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::Context;
use chrono::{DateTime, Utc};
use mlq_core::api::TokenInfo;
use mlq_core::{Job, JobId, JobState, Quota, ResourceRequest, Role, User, UserId};
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
             array_index, time_limit_secs, submitted_at, gpu_ids)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '[]') RETURNING id",
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
            "{JOB_SELECT} WHERE j.finished_at IS NOT NULL AND j.workspace_deleted = 0 AND j.workspace_path IS NOT NULL ORDER BY j.finished_at"
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
            "{JOB_SELECT} WHERE j.finished_at IS NOT NULL AND j.archived = 0 AND j.workspace_deleted = 0 AND j.workspace_path IS NOT NULL ORDER BY j.finished_at"
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
