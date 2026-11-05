//! Development executor: runs the command directly on the host with no isolation.
//! Only intended for testing the daemon on machines without podman.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;

use async_trait::async_trait;
use mlq_core::api::ImageInfo;
use tokio::process::{Child, Command};

use super::{Executor, Handle, RunSpec};

#[derive(Default)]
pub struct ProcessExecutor {
    children: Mutex<HashMap<String, Child>>,
}

#[async_trait]
impl Executor for ProcessExecutor {
    fn name(&self) -> &str {
        "process"
    }
    async fn image_present(&self, _image: &str) -> anyhow::Result<bool> {
        Ok(true)
    }
    async fn pull_image(&self, _image: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn list_images(&self) -> anyhow::Result<Vec<ImageInfo>> {
        Ok(vec![])
    }

    async fn start(&self, spec: &RunSpec) -> anyhow::Result<Handle> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spec.log_path)?;
        let log_err = log.try_clone()?;
        let (prog, args) = spec
            .job
            .command
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("empty command"))?;
        let mut cmd = Command::new(prog);
        cmd.args(args)
            .current_dir(spec.workspace.work())
            .envs(&spec.env)
            .env("MLQ_WORK", spec.workspace.work())
            .env("MLQ_OUT", spec.workspace.out())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .kill_on_drop(true);
        if !spec.gpu_ids.is_empty() {
            cmd.env(
                "CUDA_VISIBLE_DEVICES",
                spec.gpu_ids
                    .iter()
                    .map(|g| g.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        #[cfg(unix)]
        cmd.process_group(0);
        let child = cmd.spawn()?;
        let handle = format!("pid:{}", child.id().unwrap_or(0));
        self.children.lock().unwrap().insert(handle.clone(), child);
        Ok(handle)
    }

    async fn wait(&self, handle: &Handle) -> anyhow::Result<i32> {
        let child = self.children.lock().unwrap().remove(handle);
        let Some(mut child) = child else {
            anyhow::bail!("unknown handle {handle}")
        };
        let status = child.wait().await?;
        Ok(status.code().unwrap_or(-1))
    }

    async fn kill(&self, handle: &Handle) -> anyhow::Result<()> {
        let pid: i32 = handle
            .strip_prefix("pid:")
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        if pid > 0 {
            // Kill the whole process group.
            let _ = Command::new("kill")
                .args(["-TERM", "--", &format!("-{pid}")])
                .status()
                .await;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .status()
                .await;
        }
        Ok(())
    }

    async fn cleanup(&self, _handle: &Handle) -> anyhow::Result<()> {
        Ok(())
    }

    async fn is_alive(&self, handle: &Handle) -> anyhow::Result<bool> {
        Ok(self.children.lock().unwrap().contains_key(handle))
    }
}
