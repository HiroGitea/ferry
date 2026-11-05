//! MCP server exposing mlq to AI agents (Claude Code, Claude Desktop, Codex, ...).
//!
//! The same tool set is served two ways:
//! * `mlq mcp serve` — stdio transport; one fixed client (URL + token from the CLI config).
//! * `mlqd` `/mcp` — Streamable HTTP; the bearer token of each HTTP request selects the user.
//!   Tools call back into the daemon's own REST API over loopback, so there is one code path.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mlq_client::Client;
use mlq_core::api::{ListJobsQuery, SubmitJobRequest, UsageQuery};
use mlq_core::{JobState, ResourceRequest};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ProgressNotificationParam};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router,
};
use serde::Deserialize;

pub const INSTRUCTIONS: &str = "mlq schedules containerised ML jobs on a shared GPU server. \
Each job runs in its own container (default image: PyTorch + CUDA) with /work (inputs, cwd) and /out (results) mounted; \
shared datasets are mounted read-only (usually /data). Nothing a job does affects the host. \
Use cluster_status to see free GPUs/CPUs/memory, submit_job (dry_run=true by default) or run_job to execute, \
get_job_log / get_result / read_artifact to inspect results. Write results to /out so they can be read back. \
Memory is given like '16G'; time limits like '2h' or '1:30:00'. Jobs queue when resources or the user's quota are exhausted.";

#[derive(Clone)]
enum Mode {
    /// Fixed credentials (stdio).
    Fixed(Client),
    /// Per-request credentials from the HTTP Authorization header (daemon `/mcp`).
    PerRequest { base_url: String },
}

#[derive(Clone)]
pub struct MlqServer {
    mode: Mode,
    limiter: Arc<Mutex<Vec<Instant>>>,
}

const SUBMITS_PER_MINUTE: usize = 20;

fn text(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s)])
}

fn json<T: serde::Serialize>(v: &T) -> Result<CallToolResult, McpError> {
    let s = serde_json::to_string_pretty(v)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(text(s))
}

fn err_result(e: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!("error: {e}"))])
}

// ---------- parameter types ----------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SubmitParams {
    /// Shell command to run inside the container (executed with `sh -c`, cwd = /work).
    pub command: String,
    /// Container image reference (default: the server's default_image).
    pub image: Option<String>,
    /// CPU cores (default 2).
    pub cpus: Option<f64>,
    /// Memory limit, e.g. "16G" (default "8G").
    pub mem: Option<String>,
    /// Number of GPUs (default 0).
    pub gpus: Option<u32>,
    /// Human readable job name.
    pub name: Option<String>,
    /// Environment variables.
    pub env: Option<BTreeMap<String, String>>,
    /// Small text files to create under /work before start (path -> content), e.g. {"train.py": "..."}.
    pub files: Option<BTreeMap<String, String>>,
    /// Time limit like "2h", "90" (minutes) or "1:30:00".
    pub time_limit: Option<String>,
    /// Higher runs first (default 0).
    pub priority: Option<i32>,
    /// Job ids that must succeed before this job starts.
    pub depends_on: Option<Vec<i64>>,
    /// When true (the default) nothing is submitted; the response says whether/when the job would run.
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunParams {
    /// Shell command to run inside the container (executed with `sh -c`, cwd = /work).
    pub command: String,
    pub image: Option<String>,
    pub cpus: Option<f64>,
    /// Memory limit, e.g. "16G".
    pub mem: Option<String>,
    pub gpus: Option<u32>,
    pub name: Option<String>,
    pub env: Option<BTreeMap<String, String>>,
    /// Small text files to create under /work before start (path -> content).
    pub files: Option<BTreeMap<String, String>>,
    /// Time limit like "2h" or "1:30:00".
    pub time_limit: Option<String>,
    /// Seconds to wait for completion before returning (default 600). The job keeps running after that;
    /// use get_result with the returned job id.
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JobIdParams {
    pub id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListParams {
    /// Filter: queued, held, starting, pulling, running, succeeded, failed, cancelled, timeout.
    pub state: Option<String>,
    /// Include finished jobs (default false).
    pub all: Option<bool>,
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LogParams {
    pub id: i64,
    /// Number of trailing lines (default 200).
    pub tail: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArtifactParams {
    pub id: i64,
    /// Path relative to /out, e.g. "metrics.json".
    pub path: String,
    /// Maximum bytes to return (default 65536).
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UsageParams {
    /// Window in days (default 30).
    pub since_days: Option<u32>,
}

fn build_request(
    command: &str,
    image: Option<String>,
    cpus: Option<f64>,
    mem: Option<String>,
    gpus: Option<u32>,
    name: Option<String>,
    env: Option<BTreeMap<String, String>>,
    files: Option<BTreeMap<String, String>>,
    time_limit: Option<String>,
    priority: Option<i32>,
    depends_on: Option<Vec<i64>>,
    dry_run: bool,
) -> Result<SubmitJobRequest, String> {
    let mem_bytes = match mem {
        Some(m) => mlq_core::parse_bytes(&m).map_err(|e| e.to_string())?,
        None => 8 << 30,
    };
    let time_limit_secs = match time_limit {
        Some(t) => Some(mlq_core::parse_duration_secs(&t).map_err(|e| e.to_string())?),
        None => None,
    };
    Ok(SubmitJobRequest {
        name,
        image,
        command: vec!["sh".into(), "-c".into(), command.to_string()],
        env: env.unwrap_or_default(),
        resources: ResourceRequest {
            cpus: cpus.unwrap_or(2.0),
            mem_bytes,
            gpus: gpus.unwrap_or(0),
        },
        priority: priority.unwrap_or(0),
        depends_on: depends_on.unwrap_or_default(),
        array: None,
        time_limit_secs,
        hold: false,
        files: files.unwrap_or_default(),
        dry_run,
    })
}

#[tool_router]
impl MlqServer {
    fn client_for(&self, ctx: &RequestContext<RoleServer>) -> Result<Client, McpError> {
        match &self.mode {
            Mode::Fixed(c) => Ok(c.clone()),
            Mode::PerRequest { base_url } => {
                let parts = ctx
                    .extensions
                    .get::<http::request::Parts>()
                    .ok_or_else(|| McpError::internal_error("missing HTTP request parts", None))?;
                let token = parts
                    .headers
                    .get(http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .map(|t| t.trim().to_string())
                    .or_else(|| {
                        parts.uri.query().and_then(|q| {
                            q.split('&')
                                .find_map(|kv| kv.strip_prefix("token=").map(String::from))
                        })
                    })
                    .ok_or_else(|| McpError::invalid_request("missing bearer token", None))?;
                Client::new(base_url, &token)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))
            }
        }
    }

    fn check_rate(&self) -> Result<(), McpError> {
        let mut v = self.limiter.lock().unwrap();
        let now = Instant::now();
        v.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        if v.len() >= SUBMITS_PER_MINUTE {
            return Err(McpError::invalid_request(
                format!("rate limit: at most {SUBMITS_PER_MINUTE} submissions per minute"),
                None,
            ));
        }
        v.push(now);
        Ok(())
    }

    #[tool(
        description = "Free GPUs/CPUs/memory, queue length, managed inventory and live per-GPU metrics."
    )]
    async fn cluster_status(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let c = self.client_for(&ctx)?;
        match c.status().await {
            Ok(s) => json(&s),
            Err(e) => Ok(err_result(e)),
        }
    }

    #[tool(
        description = "Submit a job to the queue. dry_run defaults to TRUE: it validates the request and reports when it would run. Pass dry_run=false to actually submit. Returns the job(s)."
    )]
    async fn submit_job(
        &self,
        Parameters(p): Parameters<SubmitParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let c = self.client_for(&ctx)?;
        let dry = p.dry_run.unwrap_or(true);
        if !dry {
            self.check_rate()?;
        }
        let req = match build_request(
            &p.command,
            p.image,
            p.cpus,
            p.mem,
            p.gpus,
            p.name,
            p.env,
            p.files,
            p.time_limit,
            p.priority,
            p.depends_on,
            dry,
        ) {
            Ok(r) => r,
            Err(e) => return Ok(err_result(e)),
        };
        match c.submit(&req).await {
            Ok(r) => json(&r),
            Err(e) => Ok(err_result(e)),
        }
    }

    #[tool(
        description = "Submit a job and wait for it to finish (up to timeout_secs, default 600). Returns exit code, log tail and the list of files in /out. If it times out, the job keeps running: poll with get_job / get_result."
    )]
    async fn run_job(
        &self,
        Parameters(p): Parameters<RunParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let c = self.client_for(&ctx)?;
        self.check_rate()?;
        let req = match build_request(
            &p.command,
            p.image,
            p.cpus,
            p.mem,
            p.gpus,
            p.name,
            p.env,
            p.files,
            p.time_limit,
            None,
            None,
            false,
        ) {
            Ok(r) => r,
            Err(e) => return Ok(err_result(e)),
        };
        let submitted = match c.submit(&req).await {
            Ok(r) => r,
            Err(e) => return Ok(err_result(e)),
        };
        let Some(job) = submitted.jobs.first() else {
            return Ok(err_result("no job created"));
        };
        let id = job.id;
        let deadline =
            Instant::now() + Duration::from_secs(p.timeout_secs.unwrap_or(600).clamp(1, 24 * 3600));
        let progress_token = ctx.meta.get_progress_token();
        let mut step = 0f64;
        let mut final_job = None;
        while Instant::now() < deadline {
            if ctx.ct.is_cancelled() {
                let _ = c.cancel(id).await;
                return Ok(err_result(format!("cancelled; job {id} was cancelled")));
            }
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_secs()
                .clamp(1, 20);
            match c.wait(id, remaining).await {
                Ok(j) if j.state.is_terminal() => {
                    final_job = Some(j);
                    break;
                }
                Ok(j) => {
                    step += 1.0;
                    if let Some(tok) = &progress_token {
                        let tail = c
                            .log(id, Some(5), None)
                            .await
                            .map(|l| l.content)
                            .unwrap_or_default();
                        let msg = format!(
                            "job {id} is {}{}",
                            j.state,
                            if tail.is_empty() {
                                String::new()
                            } else {
                                format!(": {}", tail.trim_end())
                            }
                        );
                        let mut param = ProgressNotificationParam::new(tok.clone(), step);
                        param.message = Some(msg);
                        let _ = ctx.peer.notify_progress(param).await;
                    }
                }
                Err(e) => return Ok(err_result(e)),
            }
        }
        match final_job {
            Some(_) => match c.result(id).await {
                Ok(r) => json(&r),
                Err(e) => Ok(err_result(e)),
            },
            None => {
                let j = c
                    .job(id)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                Ok(text(format!(
                    "timeout after {}s: job {id} is still {} (reason: {}). It keeps running; call get_result(id={id}) later or cancel_job.",
                    p.timeout_secs.unwrap_or(600),
                    j.state,
                    j.reason.unwrap_or_default()
                )))
            }
        }
    }

    #[tool(
        description = "List jobs (yours; admins see everyone's). By default only non-finished jobs."
    )]
    async fn list_jobs(
        &self,
        Parameters(p): Parameters<ListParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let c = self.client_for(&ctx)?;
        let state = match p.state.as_deref() {
            Some(s) => match JobState::parse(s) {
                Some(st) => Some(st),
                None => return Ok(err_result(format!("unknown state '{s}'"))),
            },
            None => None,
        };
        match c
            .list_jobs(&ListJobsQuery {
                state,
                user: None,
                all: p.all.unwrap_or(false),
                limit: p.limit,
            })
            .await
        {
            Ok(jobs) => json(&jobs),
            Err(e) => Ok(err_result(e)),
        }
    }

    #[tool(
        description = "Get one job's full record (state, resources, assigned GPUs, timestamps, exit code, reason)."
    )]
    async fn get_job(
        &self,
        Parameters(p): Parameters<JobIdParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let c = self.client_for(&ctx)?;
        match c.job(p.id).await {
            Ok(j) => json(&j),
            Err(e) => Ok(err_result(e)),
        }
    }

    #[tool(description = "Cancel a queued or running job.")]
    async fn cancel_job(
        &self,
        Parameters(p): Parameters<JobIdParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let c = self.client_for(&ctx)?;
        match c.cancel(p.id).await {
            Ok(j) => json(&j),
            Err(e) => Ok(err_result(e)),
        }
    }

    #[tool(description = "Return the last N lines of a job's combined stdout/stderr log.")]
    async fn get_job_log(
        &self,
        Parameters(p): Parameters<LogParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let c = self.client_for(&ctx)?;
        match c.log(p.id, Some(p.tail.unwrap_or(200)), None).await {
            Ok(l) => Ok(text(format!(
                "[job {} state={}]\n{}",
                l.job_id, l.state, l.content
            ))),
            Err(e) => Ok(err_result(e)),
        }
    }

    #[tool(
        description = "Job outcome: state, exit code, log tail and the files written to /out (artifacts)."
    )]
    async fn get_result(
        &self,
        Parameters(p): Parameters<JobIdParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let c = self.client_for(&ctx)?;
        match c.result(p.id).await {
            Ok(r) => json(&r),
            Err(e) => Ok(err_result(e)),
        }
    }

    #[tool(
        description = "Read a file the job wrote to /out (text returned verbatim, binary summarised). Path is relative to /out."
    )]
    async fn read_artifact(
        &self,
        Parameters(p): Parameters<ArtifactParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let c = self.client_for(&ctx)?;
        let max = p.max_bytes.unwrap_or(65536).min(4 << 20);
        match c.artifact_bytes(p.id, &p.path, Some(max)).await {
            Ok(bytes) => {
                let truncated = bytes.len() as u64 >= max;
                match std::str::from_utf8(&bytes) {
                    Ok(s) => Ok(text(if truncated {
                        format!("{s}\n...[truncated at {max} bytes]")
                    } else {
                        s.to_string()
                    })),
                    Err(_) => Ok(text(format!(
                        "binary file, {} bytes read{}",
                        bytes.len(),
                        if truncated { " (truncated)" } else { "" }
                    ))),
                }
            }
            Err(e) => Ok(err_result(e)),
        }
    }

    #[tool(
        description = "GPU-hours, job counts, quota and fair-share standing for the calling user (admins: all users)."
    )]
    async fn get_usage(
        &self,
        Parameters(p): Parameters<UsageParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let c = self.client_for(&ctx)?;
        let from = chrono_days_ago(p.since_days.unwrap_or(30));
        match c
            .usage(&UsageQuery {
                user: None,
                from: Some(from),
                to: None,
            })
            .await
        {
            Ok(u) => json(&u),
            Err(e) => Ok(err_result(e)),
        }
    }
}

fn chrono_days_ago(days: u32) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() - chrono::Duration::days(days as i64)
}

#[tool_handler]
impl ServerHandler for MlqServer {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        rmcp::model::ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            "mlq",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(INSTRUCTIONS)
    }
}

impl MlqServer {
    pub fn with_client(client: Client) -> Self {
        Self {
            mode: Mode::Fixed(client),
            limiter: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn per_request(base_url: String) -> Self {
        Self {
            mode: Mode::PerRequest { base_url },
            limiter: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

/// Serve over stdio until the client disconnects.
pub async fn serve_stdio(client: Client) -> anyhow::Result<()> {
    let service = MlqServer::with_client(client)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

/// Tower service for mounting at `/mcp` in an axum router.
pub fn http_service(base_url: String) -> StreamableHttpService<MlqServer, LocalSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .disable_allowed_hosts()
        .disable_allowed_origins()
        .with_sse_keep_alive(Some(Duration::from_secs(15)));
    StreamableHttpService::new(
        move || Ok(MlqServer::per_request(base_url.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}
