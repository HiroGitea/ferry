//! Client side of a remote node: `Executor`, `GpuProvider` and `Files` implemented over the
//! agent's HTTP API (see `agent.rs`). All calls carry the shared node token.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use bytes::Bytes;
use ferry_core::Job;
use ferry_core::api::{ArtifactEntry, ImageInfo};
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

use crate::executor::{EnvSpec, Executor, Handle, RunSpec, Usage};
use crate::gpu::{GpuProvider, RawGpu};
use crate::node::Files;
use crate::workspace::Workspace;

#[derive(Clone)]
pub struct Agent {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Agent {
    /// WebSocket URL of the agent's shell endpoint, and the token to send with it.
    pub fn shell_url(&self, query: &str) -> (String, String) {
        let ws = if let Some(rest) = self.base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else {
            format!("ws://{}", self.base.trim_start_matches("http://"))
        };
        (format!("{ws}/shell?{query}"), self.token.clone())
    }
}

// ---- wire types shared with agent.rs

#[derive(Serialize, Deserialize)]
pub struct WireRunSpec {
    pub job: Job,
    pub workspace_root: String,
    pub gpu_ids: Vec<u32>,
    pub env: BTreeMap<String, String>,
    pub log_path: String,
}

#[derive(Serialize, Deserialize)]
pub struct WireEnvSpec {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub image: String,
    pub user_name: String,
    pub resources: ferry_core::ResourceRequest,
    pub gpu_ids: Vec<u32>,
    pub workspace_root: String,
    pub env: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
pub struct WireExec {
    pub env: String,
    pub job: Job,
    pub workspace_root: String,
    pub vars: BTreeMap<String, String>,
    pub log_path: String,
}

#[derive(Serialize, Deserialize)]
pub struct Handles {
    pub handle: String,
}

#[derive(Serialize, Deserialize)]
pub struct ImageReq {
    pub image: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub dst: String,
}

#[derive(Serialize, Deserialize)]
pub struct WaitResult {
    pub done: bool,
    pub code: Option<i32>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct SamplesReq {
    pub handles: Vec<String>,
    pub busy_gpus: Vec<u32>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct SamplesResp {
    pub samples: Vec<(String, String, f32)>,
    pub gpus: Vec<RawGpu>,
    pub usage: BTreeMap<String, WireUsage>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct WireUsage {
    pub cpu_usec: u64,
    pub mem_bytes: u64,
    pub io_read: u64,
    pub io_write: u64,
    pub pids: u64,
}

impl From<Usage> for WireUsage {
    fn from(u: Usage) -> Self {
        WireUsage {
            cpu_usec: u.cpu_usec,
            mem_bytes: u.mem_bytes,
            io_read: u.io_read,
            io_write: u.io_write,
            pids: u.pids,
        }
    }
}
impl From<WireUsage> for Usage {
    fn from(u: WireUsage) -> Self {
        Usage {
            cpu_usec: u.cpu_usec,
            mem_bytes: u.mem_bytes,
            io_read: u.io_read,
            io_write: u.io_write,
            pids: u.pids,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct PathReq {
    pub path: String,
    #[serde(default)]
    pub tail: Option<usize>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub subdirs: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct WriteFilesReq {
    pub root: String,
    pub files: BTreeMap<String, String>,
}

impl Agent {
    pub fn new(url: &str, token: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(24 * 3600))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base: format!("{}/agent/v1", url.trim_end_matches('/')),
            token: token.into(),
        })
    }

    async fn check(resp: reqwest::Response) -> anyhow::Result<reqwest::Response> {
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("agent {status}: {text}");
        }
        Ok(resp)
    }

    pub async fn post<T: serde::de::DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .with_context(|| format!("agent unreachable ({})", self.base))?;
        Ok(Self::check(resp).await?.json().await?)
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> anyhow::Result<T> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .query(query)
            .send()
            .await
            .with_context(|| format!("agent unreachable ({})", self.base))?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn samples(&self, req: &SamplesReq) -> anyhow::Result<SamplesResp> {
        self.post("/samples", req).await
    }
}

// ---------------------------------------------------------------- executor

pub struct RemoteExecutor {
    pub agent: Agent,
    pub runtime: String,
    pub snapshots: bool,
}

#[async_trait]
impl Executor for RemoteExecutor {
    fn name(&self) -> &str {
        &self.runtime
    }
    fn supports_snapshots(&self) -> bool {
        self.snapshots
    }
    async fn image_present(&self, image: &str) -> anyhow::Result<bool> {
        self.agent
            .post(
                "/image/present",
                &ImageReq {
                    image: image.into(),
                    path: String::new(),
                    dst: String::new(),
                },
            )
            .await
    }
    async fn pull_image(&self, image: &str) -> anyhow::Result<()> {
        self.agent
            .post::<(), _>(
                "/image/pull",
                &ImageReq {
                    image: image.into(),
                    path: String::new(),
                    dst: String::new(),
                },
            )
            .await
    }
    async fn list_images(&self) -> anyhow::Result<Vec<ImageInfo>> {
        self.agent.get("/images", &[]).await
    }
    async fn start(&self, spec: &RunSpec) -> anyhow::Result<Handle> {
        let w = WireRunSpec {
            job: spec.job.clone(),
            workspace_root: spec.workspace.root.to_string_lossy().into(),
            gpu_ids: spec.gpu_ids.clone(),
            env: spec.env.clone(),
            log_path: spec.log_path.to_string_lossy().into(),
        };
        let h: Handles = self.agent.post("/start", &w).await?;
        Ok(h.handle)
    }
    async fn wait(&self, handle: &Handle) -> anyhow::Result<i32> {
        // Long-poll in slices so a dropped connection is noticed and retried.
        loop {
            let r: WaitResult = self
                .agent
                .post(
                    "/wait",
                    &Handles {
                        handle: handle.clone(),
                    },
                )
                .await?;
            if r.done {
                return match (r.code, r.error) {
                    (Some(c), _) => Ok(c),
                    (None, Some(e)) => Err(anyhow::anyhow!(e)),
                    _ => Err(anyhow::anyhow!("wait returned no exit code")),
                };
            }
        }
    }
    async fn kill(&self, handle: &Handle) -> anyhow::Result<()> {
        self.agent
            .post::<(), _>(
                "/kill",
                &Handles {
                    handle: handle.clone(),
                },
            )
            .await
    }
    async fn cleanup(&self, handle: &Handle) -> anyhow::Result<()> {
        self.agent
            .post::<(), _>(
                "/cleanup",
                &Handles {
                    handle: handle.clone(),
                },
            )
            .await
    }
    async fn is_alive(&self, handle: &Handle) -> anyhow::Result<bool> {
        self.agent
            .post(
                "/alive",
                &Handles {
                    handle: handle.clone(),
                },
            )
            .await
    }
    async fn start_env(&self, spec: &EnvSpec) -> anyhow::Result<Handle> {
        let w = WireEnvSpec {
            name: spec.name.clone(),
            labels: spec.labels.clone(),
            image: spec.image.clone(),
            user_name: spec.user_name.clone(),
            resources: spec.resources.clone(),
            gpu_ids: spec.gpu_ids.clone(),
            workspace_root: spec.workspace.root.to_string_lossy().into(),
            env: spec.env.clone(),
        };
        let h: Handles = self.agent.post("/start_env", &w).await?;
        Ok(h.handle)
    }
    async fn exec(
        &self,
        env: &Handle,
        job: &Job,
        workspace: &Workspace,
        vars: &BTreeMap<String, String>,
        log_path: &Path,
    ) -> anyhow::Result<Handle> {
        let w = WireExec {
            env: env.clone(),
            job: job.clone(),
            workspace_root: workspace.root.to_string_lossy().into(),
            vars: vars.clone(),
            log_path: log_path.to_string_lossy().into(),
        };
        let h: Handles = self.agent.post("/exec", &w).await?;
        Ok(h.handle)
    }
    fn env_of(&self, handle: &Handle) -> Option<Handle> {
        // Same handle format as the container executor (`exec|<env>|<exit file>`).
        handle
            .strip_prefix("exec|")
            .and_then(|r| r.split_once('|'))
            .map(|(e, _)| e.to_string())
    }
    async fn commit(&self, env: &Handle, image: &str) -> anyhow::Result<Option<u64>> {
        self.agent
            .post(
                "/commit",
                &ImageReq {
                    image: image.into(),
                    path: env.clone(),
                    dst: String::new(),
                },
            )
            .await
    }
    async fn remove_image(&self, image: &str) -> anyhow::Result<()> {
        self.agent
            .post::<(), _>(
                "/image/remove",
                &ImageReq {
                    image: image.into(),
                    path: String::new(),
                    dst: String::new(),
                },
            )
            .await
    }
    async fn save_image(&self, image: &str, path: &Path) -> anyhow::Result<()> {
        self.agent
            .post::<(), _>(
                "/image/save",
                &ImageReq {
                    image: image.into(),
                    path: path.to_string_lossy().into(),
                    dst: String::new(),
                },
            )
            .await
    }
    async fn load_image(&self, path: &Path) -> anyhow::Result<Vec<String>> {
        self.agent
            .post(
                "/image/load",
                &ImageReq {
                    image: String::new(),
                    path: path.to_string_lossy().into(),
                    dst: String::new(),
                },
            )
            .await
    }
    async fn tag_image(&self, src: &str, dst: &str) -> anyhow::Result<()> {
        self.agent
            .post::<(), _>(
                "/image/tag",
                &ImageReq {
                    image: src.into(),
                    path: String::new(),
                    dst: dst.into(),
                },
            )
            .await
    }
    async fn env_idle(&self, env: &Handle) -> anyhow::Result<bool> {
        self.agent
            .post(
                "/env_idle",
                &Handles {
                    handle: env.clone(),
                },
            )
            .await
    }
    async fn remove_env(&self, env: &Handle) -> anyhow::Result<()> {
        self.agent
            .post::<(), _>(
                "/remove_env",
                &Handles {
                    handle: env.clone(),
                },
            )
            .await
    }
    async fn usage(&self, handle: &Handle) -> Option<Usage> {
        let u: Option<WireUsage> = self
            .agent
            .post(
                "/usage",
                &Handles {
                    handle: handle.clone(),
                },
            )
            .await
            .ok()?;
        u.map(Into::into)
    }
}

// ---------------------------------------------------------------- gpu

/// GPU list as last reported by the agent (refreshed by the metrics collector every second).
pub struct RemoteGpu {
    cache: Mutex<Vec<RawGpu>>,
}

impl RemoteGpu {
    pub fn new(initial: Vec<RawGpu>) -> Self {
        Self {
            cache: Mutex::new(initial),
        }
    }
    pub fn update(&self, gpus: Vec<RawGpu>) {
        *self.cache.lock().unwrap() = gpus;
    }
}

impl GpuProvider for RemoteGpu {
    fn name(&self) -> &'static str {
        "remote"
    }
    fn list(&self) -> Vec<RawGpu> {
        self.cache.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------- files

pub struct RemoteFiles {
    pub agent: Agent,
}

#[async_trait]
impl Files for RemoteFiles {
    async fn ensure_workspace(&self, root: &Path) -> anyhow::Result<()> {
        self.agent
            .post::<(), _>(
                "/fs/ensure",
                &PathReq {
                    path: root.to_string_lossy().into(),
                    tail: None,
                    offset: None,
                    subdirs: vec![],
                },
            )
            .await
    }
    async fn write_files(
        &self,
        root: &Path,
        files: &BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        self.agent
            .post::<(), _>(
                "/fs/write_files",
                &WriteFilesReq {
                    root: root.to_string_lossy().into(),
                    files: files.clone(),
                },
            )
            .await
    }
    async fn unpack_tar_gz(&self, root: &Path, data: Bytes) -> anyhow::Result<usize> {
        let resp = self
            .agent
            .http
            .put(format!("{}/fs/unpack", self.agent.base))
            .bearer_auth(&self.agent.token)
            .query(&[("root", root.to_string_lossy().to_string())])
            .body(data)
            .send()
            .await?;
        Ok(Agent::check(resp).await?.json().await?)
    }
    async fn read_log(
        &self,
        path: &Path,
        tail: Option<usize>,
        offset: Option<u64>,
    ) -> anyhow::Result<(String, u64)> {
        self.agent
            .post(
                "/fs/log",
                &PathReq {
                    path: path.to_string_lossy().into(),
                    tail,
                    offset,
                    subdirs: vec![],
                },
            )
            .await
    }
    async fn list_out(&self, root: &Path) -> anyhow::Result<Vec<ArtifactEntry>> {
        self.agent
            .post(
                "/fs/list_out",
                &PathReq {
                    path: root.to_string_lossy().into(),
                    tail: None,
                    offset: None,
                    subdirs: vec![],
                },
            )
            .await
    }
    async fn open_file(
        &self,
        path: &Path,
    ) -> anyhow::Result<(u64, BoxStream<'static, std::io::Result<Bytes>>)> {
        let resp = self
            .agent
            .http
            .get(format!("{}/fs/file", self.agent.base))
            .bearer_auth(&self.agent.token)
            .query(&[("path", path.to_string_lossy().to_string())])
            .send()
            .await?;
        let resp = Agent::check(resp).await?;
        let len = resp.content_length().unwrap_or(0);
        let stream = resp.bytes_stream().map_err(std::io::Error::other);
        Ok((len, Box::pin(stream)))
    }
    async fn copy_to_local(
        &self,
        root: &Path,
        subdirs: &[&str],
        dest: &Path,
    ) -> anyhow::Result<()> {
        // The agent streams a tar of the requested subdirectories; unpack it under `dest`.
        let resp = self
            .agent
            .http
            .post(format!("{}/fs/pack", self.agent.base))
            .bearer_auth(&self.agent.token)
            .json(&PathReq {
                path: root.to_string_lossy().into(),
                tail: None,
                offset: None,
                subdirs: subdirs.iter().map(|s| s.to_string()).collect(),
            })
            .send()
            .await?;
        let resp = Agent::check(resp).await?;
        let tmp = tempfile::NamedTempFile::new()?;
        let mut f = tokio::fs::File::create(tmp.path()).await?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            tokio::io::AsyncWriteExt::write_all(&mut f, &chunk?).await?;
        }
        tokio::io::AsyncWriteExt::flush(&mut f).await?;
        let dest = dest.to_path_buf();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            std::fs::create_dir_all(&dest)?;
            let mut ar = tar::Archive::new(std::fs::File::open(tmp.path())?);
            ar.unpack(&dest)?;
            Ok(())
        })
        .await?
    }
    async fn fetch_file(&self, path: &Path, local_dest: &Path) -> anyhow::Result<()> {
        let (_, mut stream) = self.open_file(path).await?;
        let mut f = tokio::fs::File::create(local_dest).await?;
        while let Some(chunk) = stream.next().await {
            tokio::io::AsyncWriteExt::write_all(&mut f, &chunk?).await?;
        }
        tokio::io::AsyncWriteExt::flush(&mut f).await?;
        Ok(())
    }
    async fn push_file(&self, local_src: &Path, path: &Path) -> anyhow::Result<()> {
        let f = tokio::fs::File::open(local_src).await?;
        let len = f.metadata().await?.len();
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(f));
        let resp = self
            .agent
            .http
            .put(format!("{}/fs/file", self.agent.base))
            .bearer_auth(&self.agent.token)
            .query(&[("path", path.to_string_lossy().to_string())])
            .header("content-length", len)
            .body(body)
            .send()
            .await?;
        Agent::check(resp).await?;
        Ok(())
    }
    async fn unpack_tar_at(&self, tar_path: &Path, root: &Path) -> anyhow::Result<()> {
        self.agent
            .post::<(), _>(
                "/fs/unpack_at",
                &PathReq {
                    path: tar_path.to_string_lossy().into(),
                    tail: None,
                    offset: None,
                    subdirs: vec![root.to_string_lossy().into()],
                },
            )
            .await
    }
    async fn remove_dir(&self, path: &Path) -> anyhow::Result<()> {
        self.agent
            .post::<(), _>(
                "/fs/remove_dir",
                &PathReq {
                    path: path.to_string_lossy().into(),
                    tail: None,
                    offset: None,
                    subdirs: vec![],
                },
            )
            .await
    }
    async fn remove_file(&self, path: &Path) -> anyhow::Result<()> {
        self.agent
            .post::<(), _>(
                "/fs/remove_file",
                &PathReq {
                    path: path.to_string_lossy().into(),
                    tail: None,
                    offset: None,
                    subdirs: vec![],
                },
            )
            .await
    }
    async fn dir_size(&self, path: &Path) -> anyhow::Result<u64> {
        self.agent
            .post(
                "/fs/size",
                &PathReq {
                    path: path.to_string_lossy().into(),
                    tail: None,
                    offset: None,
                    subdirs: vec![],
                },
            )
            .await
    }
    async fn exists(&self, path: &Path) -> anyhow::Result<bool> {
        self.agent
            .post(
                "/fs/exists",
                &PathReq {
                    path: path.to_string_lossy().into(),
                    tail: None,
                    offset: None,
                    subdirs: vec![],
                },
            )
            .await
    }
}
