//! Job executors. The scheduler only talks to the `Executor` trait so that a remote-agent
//! executor can be added for multi-node operation later.

pub mod container;
pub mod process;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use mlq_core::Job;
use mlq_core::api::ImageInfo;

use crate::config::{Config, Mount};
use crate::workspace::Workspace;

/// Everything an executor needs to start one job.
pub struct RunSpec {
    pub job: Job,
    pub workspace: Workspace,
    pub gpu_ids: Vec<u32>,
    pub mounts: Vec<Mount>,
    pub env: BTreeMap<String, String>,
    pub log_path: PathBuf,
}

/// Opaque handle to a started job (container id or pid).
pub type Handle = String;

#[async_trait]
pub trait Executor: Send + Sync {
    fn name(&self) -> &str;
    /// Whether the image must be pulled before `start`.
    async fn image_present(&self, image: &str) -> anyhow::Result<bool>;
    async fn pull_image(&self, image: &str) -> anyhow::Result<()>;
    async fn list_images(&self) -> anyhow::Result<Vec<ImageInfo>>;
    /// Start the job; must return quickly with a handle. Output is appended to `spec.log_path`.
    async fn start(&self, spec: &RunSpec) -> anyhow::Result<Handle>;
    /// Wait for completion; returns the exit code.
    async fn wait(&self, handle: &Handle) -> anyhow::Result<i32>;
    /// Stop the job (SIGTERM then SIGKILL).
    async fn kill(&self, handle: &Handle) -> anyhow::Result<()>;
    /// Release resources associated with the handle (container removal).
    async fn cleanup(&self, handle: &Handle) -> anyhow::Result<()>;
    /// Whether a handle from before a daemon restart still refers to a live job.
    async fn is_alive(&self, handle: &Handle) -> anyhow::Result<bool>;
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
