//! HTTP client for the ferryd REST API, shared by the CLI and the MCP server.

use std::time::Duration;

use anyhow::Context;
use ferry_core::api::{
    ArchiveInfo, ArchiveManifest, ArtifactEntry, CheckoutRequest, ClusterStatus,
    CreateInviteRequest, CreateTaskRequest, CreateTokenRequest, CreateUserRequest, ErrorBody,
    Event, ExportTaskRequest, ExportTaskResponse, GpuInfo, ImageInfo, ImportArchiveRequest,
    InviteCreated, InviteInfo, InvitePreview, JobResult, ListJobsQuery, ListTasksQuery, LogChunk,
    Me, MetricInfo, MetricsQuery, MetricsResult, PinSnapshotRequest, PullImageRequest,
    RegisterRequest, SubmitJobRequest, SubmitJobResponse, TokenCreated, TokenInfo,
    UpdateTaskRequest, UsageQuery, UserUsage,
};
use ferry_core::{Job, JobId, Quota, TaskDetail, TaskId, TaskSummary, User};
use futures::StreamExt;
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{status}: {message}")]
    Api { status: StatusCode, message: String },
    #[error("cannot reach ferryd at {url}: {source}")]
    Connect { url: String, source: reqwest::Error },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Percent-encode a relative artifact path, segment by segment, so that `..` and special
/// characters reach the server verbatim (which then rejects traversal) instead of being
/// normalised away by the URL parser.
pub fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            let mut out = String::new();
            for b in seg.bytes() {
                match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'~' => {
                        out.push(b as char)
                    }
                    b'.' if seg != "." && seg != ".." => out.push('.'),
                    _ => out.push_str(&format!("%{b:02X}")),
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    pub fn new(url: &str, token: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(620))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base: format!("{}/api/v1", url.trim_end_matches('/')),
            token: token.to_string(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// The server root (what the dashboard is served from), without `/api/v1`.
    pub fn server_url(&self) -> &str {
        self.base.trim_end_matches("/api/v1")
    }

    fn req(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
    }

    async fn send<T: DeserializeOwned>(&self, rb: RequestBuilder) -> Result<T, ClientError> {
        let resp = rb.send().await.map_err(|e| ClientError::Connect {
            url: self.base.clone(),
            source: e,
        })?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<ErrorBody>(&text)
                .map(|b| b.error)
                .unwrap_or(text);
            return Err(ClientError::Api { status, message });
        }
        Ok(resp.json::<T>().await.context("decoding response")?)
    }

    pub async fn me(&self) -> Result<Me, ClientError> {
        self.send(self.req(Method::GET, "/me")).await
    }
    pub async fn status(&self) -> Result<ClusterStatus, ClientError> {
        self.send(self.req(Method::GET, "/status")).await
    }
    pub async fn nodes(&self) -> Result<Vec<ferry_core::api::NodeStatus>, ClientError> {
        self.send(self.req(Method::GET, "/nodes")).await
    }
    pub async fn delete_node(&self, id: &str) -> Result<serde_json::Value, ClientError> {
        self.send(self.req(Method::DELETE, &format!("/nodes/{id}")))
            .await
    }
    pub async fn gpus(&self) -> Result<Vec<GpuInfo>, ClientError> {
        self.send(self.req(Method::GET, "/gpus")).await
    }
    pub async fn images(&self) -> Result<Vec<ImageInfo>, ClientError> {
        self.send(self.req(Method::GET, "/images")).await
    }
    pub async fn pull_image(&self, reference: &str) -> Result<serde_json::Value, ClientError> {
        self.send(
            self.req(Method::POST, "/images/pull")
                .json(&PullImageRequest {
                    reference: reference.into(),
                }),
        )
        .await
    }

    pub async fn submit(&self, req: &SubmitJobRequest) -> Result<SubmitJobResponse, ClientError> {
        self.send(self.req(Method::POST, "/jobs").json(req)).await
    }
    pub async fn list_jobs(&self, q: &ListJobsQuery) -> Result<Vec<Job>, ClientError> {
        self.send(self.req(Method::GET, "/jobs").query(q)).await
    }
    pub async fn job(&self, id: JobId) -> Result<Job, ClientError> {
        self.send(self.req(Method::GET, &format!("/jobs/{id}")))
            .await
    }
    pub async fn cancel(&self, id: JobId) -> Result<Job, ClientError> {
        self.send(self.req(Method::DELETE, &format!("/jobs/{id}")))
            .await
    }
    pub async fn release(&self, id: JobId) -> Result<Job, ClientError> {
        self.send(self.req(Method::POST, &format!("/jobs/{id}/release")))
            .await
    }
    pub async fn log(
        &self,
        id: JobId,
        tail: Option<usize>,
        offset: Option<u64>,
    ) -> Result<LogChunk, ClientError> {
        let mut rb = self.req(Method::GET, &format!("/jobs/{id}/log"));
        if let Some(t) = tail {
            rb = rb.query(&[("tail", t.to_string())]);
        }
        if let Some(o) = offset {
            rb = rb.query(&[("offset", o.to_string())]);
        }
        self.send(rb).await
    }
    /// Long-poll until terminal or `timeout_secs` (server caps at 600).
    pub async fn wait(&self, id: JobId, timeout_secs: u64) -> Result<Job, ClientError> {
        self.send(
            self.req(Method::GET, &format!("/jobs/{id}/wait"))
                .query(&[("timeout", timeout_secs.to_string())]),
        )
        .await
    }
    pub async fn result(&self, id: JobId) -> Result<JobResult, ClientError> {
        self.send(self.req(Method::GET, &format!("/jobs/{id}/result")))
            .await
    }
    pub async fn artifacts(&self, id: JobId) -> Result<Vec<ArtifactEntry>, ClientError> {
        self.send(self.req(Method::GET, &format!("/jobs/{id}/artifacts")))
            .await
    }
    pub async fn artifact_bytes(
        &self,
        id: JobId,
        path: &str,
        max_bytes: Option<u64>,
    ) -> Result<bytes::Bytes, ClientError> {
        if path.is_empty()
            || path.starts_with('/')
            || path.split('/').any(|s| s == ".." || s == ".")
        {
            return Err(ClientError::Other(anyhow::anyhow!(
                "invalid artifact path '{path}': must be relative to /out without '..'"
            )));
        }
        let resp = self
            .req(
                Method::GET,
                &format!("/jobs/{id}/artifacts/{}", encode_path(path)),
            )
            .send()
            .await
            .map_err(|e| ClientError::Connect {
                url: self.base.clone(),
                source: e,
            })?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<ErrorBody>(&text)
                .map(|b| b.error)
                .unwrap_or(text);
            return Err(ClientError::Api { status, message });
        }
        match max_bytes {
            None => Ok(resp.bytes().await.context("reading artifact")?),
            Some(max) => {
                let mut out = Vec::new();
                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.context("reading artifact")?;
                    let room = (max as usize).saturating_sub(out.len());
                    out.extend_from_slice(&chunk[..chunk.len().min(room)]);
                    if out.len() >= max as usize {
                        break;
                    }
                }
                Ok(out.into())
            }
        }
    }
    pub async fn upload_tar_gz(
        &self,
        id: JobId,
        data: Vec<u8>,
    ) -> Result<serde_json::Value, ClientError> {
        self.send(
            self.req(Method::PUT, &format!("/jobs/{id}/files"))
                .header("content-type", "application/gzip")
                .body(data),
        )
        .await
    }

    pub async fn users(&self) -> Result<Vec<User>, ClientError> {
        self.send(self.req(Method::GET, "/users")).await
    }
    pub async fn create_user(&self, req: &CreateUserRequest) -> Result<TokenCreated, ClientError> {
        self.send(self.req(Method::POST, "/users").json(req)).await
    }
    pub async fn delete_user(&self, name: &str) -> Result<serde_json::Value, ClientError> {
        self.send(self.req(Method::DELETE, &format!("/users/{name}")))
            .await
    }
    pub async fn quota(&self, name: &str) -> Result<Quota, ClientError> {
        self.send(self.req(Method::GET, &format!("/users/{name}/quota")))
            .await
    }
    pub async fn set_quota(&self, name: &str, q: &Quota) -> Result<Quota, ClientError> {
        self.send(
            self.req(Method::PUT, &format!("/users/{name}/quota"))
                .json(q),
        )
        .await
    }
    pub async fn tokens(&self) -> Result<Vec<TokenInfo>, ClientError> {
        self.send(self.req(Method::GET, "/tokens")).await
    }
    pub async fn create_token(
        &self,
        req: &CreateTokenRequest,
    ) -> Result<TokenCreated, ClientError> {
        self.send(self.req(Method::POST, "/tokens").json(req)).await
    }
    pub async fn delete_token(&self, id: i64) -> Result<serde_json::Value, ClientError> {
        self.send(self.req(Method::DELETE, &format!("/tokens/{id}")))
            .await
    }
    pub async fn invites(&self) -> Result<Vec<InviteInfo>, ClientError> {
        self.send(self.req(Method::GET, "/invites")).await
    }
    pub async fn create_invite(
        &self,
        req: &CreateInviteRequest,
    ) -> Result<InviteCreated, ClientError> {
        self.send(self.req(Method::POST, "/invites").json(req))
            .await
    }
    pub async fn delete_invite(&self, id: i64) -> Result<serde_json::Value, ClientError> {
        self.send(self.req(Method::DELETE, &format!("/invites/{id}")))
            .await
    }
    /// Public: no token needed.
    pub async fn preview_invite(&self, code: &str) -> Result<InvitePreview, ClientError> {
        self.send(self.req(Method::GET, "/invite").query(&[("code", code)]))
            .await
    }
    /// Public: no token needed.
    pub async fn register(&self, req: &RegisterRequest) -> Result<TokenCreated, ClientError> {
        self.send(self.req(Method::POST, "/register").json(req))
            .await
    }
    pub async fn usage(&self, q: &UsageQuery) -> Result<Vec<UserUsage>, ClientError> {
        self.send(self.req(Method::GET, "/usage").query(q)).await
    }
    pub async fn sweep(&self) -> Result<serde_json::Value, ClientError> {
        self.send(self.req(Method::POST, "/retention/sweep")).await
    }

    // ---- tasks
    pub async fn create_task(&self, req: &CreateTaskRequest) -> Result<TaskDetail, ClientError> {
        self.send(self.req(Method::POST, "/tasks").json(req)).await
    }
    pub async fn tasks(&self, q: &ListTasksQuery) -> Result<Vec<TaskSummary>, ClientError> {
        self.send(self.req(Method::GET, "/tasks").query(q)).await
    }
    pub async fn task(&self, id: TaskId) -> Result<TaskDetail, ClientError> {
        self.send(self.req(Method::GET, &format!("/tasks/{id}")))
            .await
    }
    pub async fn update_task(
        &self,
        id: TaskId,
        req: &UpdateTaskRequest,
    ) -> Result<TaskDetail, ClientError> {
        self.send(self.req(Method::PATCH, &format!("/tasks/{id}")).json(req))
            .await
    }
    pub async fn checkout(
        &self,
        id: TaskId,
        snapshot_id: Option<i64>,
    ) -> Result<TaskDetail, ClientError> {
        self.send(
            self.req(Method::POST, &format!("/tasks/{id}/checkout"))
                .json(&CheckoutRequest { snapshot_id }),
        )
        .await
    }
    pub async fn pin_snapshot(
        &self,
        id: TaskId,
        sid: i64,
        pinned: bool,
        note: Option<String>,
    ) -> Result<TaskDetail, ClientError> {
        self.send(
            self.req(Method::POST, &format!("/tasks/{id}/snapshots/{sid}/pin"))
                .json(&PinSnapshotRequest { pinned, note }),
        )
        .await
    }
    pub async fn close_task(&self, id: TaskId) -> Result<TaskDetail, ClientError> {
        self.send(self.req(Method::POST, &format!("/tasks/{id}/close")))
            .await
    }
    pub async fn reopen_task(&self, id: TaskId) -> Result<TaskDetail, ClientError> {
        self.send(self.req(Method::POST, &format!("/tasks/{id}/reopen")))
            .await
    }
    pub async fn delete_task(&self, id: TaskId) -> Result<serde_json::Value, ClientError> {
        self.send(self.req(Method::DELETE, &format!("/tasks/{id}")))
            .await
    }
    pub async fn export_task(
        &self,
        id: TaskId,
        req: &ExportTaskRequest,
    ) -> Result<ExportTaskResponse, ClientError> {
        self.send(
            self.req(Method::POST, &format!("/tasks/{id}/export"))
                .json(req),
        )
        .await
    }
    pub async fn archives(&self) -> Result<Vec<ArchiveInfo>, ClientError> {
        self.send(self.req(Method::GET, "/archives")).await
    }
    pub async fn archive_manifest(&self, name: &str) -> Result<ArchiveManifest, ClientError> {
        self.send(
            self.req(Method::GET, "/archives/manifest")
                .query(&[("name", name)]),
        )
        .await
    }
    pub async fn import_archive(
        &self,
        req: &ImportArchiveRequest,
    ) -> Result<TaskDetail, ClientError> {
        self.send(self.req(Method::POST, "/archives/import").json(req))
            .await
    }

    // ---- metrics
    pub async fn metric_keys(&self, prefix: Option<&str>) -> Result<Vec<MetricInfo>, ClientError> {
        let mut rb = self.req(Method::GET, "/metrics/keys");
        if let Some(p) = prefix {
            rb = rb.query(&[("prefix", p)]);
        }
        self.send(rb).await
    }
    pub async fn metrics(&self, q: &MetricsQuery) -> Result<MetricsResult, ClientError> {
        self.send(self.req(Method::GET, "/metrics/query").query(q))
            .await
    }

    /// Subscribe to server-sent events. Calls `f` for every decoded event until the connection drops.
    pub async fn events<F: FnMut(Event)>(&self, mut f: F) -> Result<(), ClientError> {
        let resp = self
            .req(Method::GET, "/events")
            .timeout(Duration::from_secs(24 * 3600))
            .send()
            .await
            .map_err(|e| ClientError::Connect {
                url: self.base.clone(),
                source: e,
            })?;
        if !resp.status().is_success() {
            return Err(ClientError::Api {
                status: resp.status(),
                message: "events stream rejected".into(),
            });
        }
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading event stream")?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find("\n\n") {
                let frame = buf[..pos].to_string();
                buf.drain(..pos + 2);
                for line in frame.lines() {
                    if let Some(data) = line.strip_prefix("data:")
                        && let Ok(ev) = serde_json::from_str::<Event>(data.trim())
                    {
                        f(ev);
                    }
                }
            }
        }
        Ok(())
    }
}
