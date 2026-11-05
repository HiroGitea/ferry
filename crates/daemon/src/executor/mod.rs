//! Job executors. The scheduler only talks to the `Executor` trait so that a remote-agent
//! executor can be added for multi-node operation later.
//!
//! Two ways of running work:
//! * one-shot jobs: `start` a fresh container per job, `wait`, `cleanup`;
//! * task steps: `start_env` a long-lived environment container (idle `sleep infinity`), run each
//!   step in it with `exec`, save it with `commit` after a successful step, `remove_env` when done.

pub mod container;
pub mod process;
pub mod usage;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use mlq_core::api::ImageInfo;
use mlq_core::{Job, ResourceRequest};

use crate::config::{Config, Mount};
use crate::workspace::Workspace;

pub use usage::Usage;

/// Everything an executor needs to start one job.
pub struct RunSpec {
    pub job: Job,
    pub workspace: Workspace,
    pub gpu_ids: Vec<u32>,
    pub mounts: Vec<Mount>,
    pub env: BTreeMap<String, String>,
    pub log_path: PathBuf,
}

/// A long-lived task environment.
pub struct EnvSpec {
    /// Container name (stable per task, e.g. `mlq-task-3`).
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub image: String,
    pub user_name: String,
    pub resources: ResourceRequest,
    pub gpu_ids: Vec<u32>,
    pub workspace: Workspace,
    pub mounts: Vec<Mount>,
    pub env: BTreeMap<String, String>,
}

/// Opaque handle to a started job, environment or exec session.
pub type Handle = String;

#[async_trait]
pub trait Executor: Send + Sync {
    fn name(&self) -> &str;
    /// Whether `commit` can save environments (container runtimes only).
    fn supports_snapshots(&self) -> bool;
    async fn image_present(&self, image: &str) -> anyhow::Result<bool>;
    async fn pull_image(&self, image: &str) -> anyhow::Result<()>;
    async fn list_images(&self) -> anyhow::Result<Vec<ImageInfo>>;
    /// Start the job; must return quickly with a handle. Output is appended to `spec.log_path`.
    async fn start(&self, spec: &RunSpec) -> anyhow::Result<Handle>;
    /// Wait for completion of a job or exec session; returns the exit code.
    async fn wait(&self, handle: &Handle) -> anyhow::Result<i32>;
    /// Stop a job. For an exec session this stops its whole environment (rolling it back).
    async fn kill(&self, handle: &Handle) -> anyhow::Result<()>;
    /// Release resources associated with a one-shot job handle (container removal).
    async fn cleanup(&self, handle: &Handle) -> anyhow::Result<()>;
    /// Whether a handle from before a daemon restart still refers to a live job/environment.
    async fn is_alive(&self, handle: &Handle) -> anyhow::Result<bool>;

    /// Start an idle environment container for a task.
    async fn start_env(&self, spec: &EnvSpec) -> anyhow::Result<Handle>;
    /// Run one step inside an environment; output is appended to `log_path`.
    async fn exec(
        &self,
        env: &Handle,
        job: &Job,
        workspace: &Workspace,
        vars: &BTreeMap<String, String>,
        log_path: &std::path::Path,
    ) -> anyhow::Result<Handle>;
    /// Save the environment's filesystem as `image`; returns the image size if known.
    async fn commit(&self, env: &Handle, image: &str) -> anyhow::Result<Option<u64>>;
    async fn remove_image(&self, image: &str) -> anyhow::Result<()>;
    /// True when nothing but the environment's idle init process is left running.
    async fn env_idle(&self, env: &Handle) -> anyhow::Result<bool>;
    /// Stop and remove an environment.
    async fn remove_env(&self, env: &Handle) -> anyhow::Result<()>;
    /// Cumulative resource counters of a job/environment, if available.
    async fn usage(&self, handle: &Handle) -> Option<Usage>;
}

pub fn from_config(cfg: &Config) -> anyhow::Result<Arc<dyn Executor>> {
    match cfg.runtime.as_str() {
        "process" => Ok(Arc::new(process::ProcessExecutor::default())),
        other => Ok(Arc::new(container::ContainerExecutor::new(
            other.to_string(),
            cfg.container.clone(),
        ))),
    }
}
