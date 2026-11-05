//! Development executor: runs the command directly on the host with no isolation.
//! Only intended for testing the daemon on machines without podman. Task "environments" are
//! virtual here: steps share the task's /work directory but nothing else, and snapshots are not
//! supported.

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::Mutex;

use async_trait::async_trait;
use mlq_core::Job;
use mlq_core::api::ImageInfo;
use tokio::process::{Child, Command};

use super::{EnvSpec, Executor, Handle, RunSpec, Usage};
use crate::workspace::Workspace;

#[derive(Default)]
pub struct ProcessExecutor {
    children: Mutex<HashMap<String, Child>>,
    /// env handle -> pid of the step currently running in it
    env_pids: Mutex<HashMap<String, i32>>,
}

impl ProcessExecutor {
    fn spawn(
        &self,
        command: &[String],
        workspace: &Workspace,
        vars: &BTreeMap<String, String>,
        gpu_ids: &[u32],
        log_path: &std::path::Path,
    ) -> anyhow::Result<Handle> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        let log_err = log.try_clone()?;
        let (prog, args) = command
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("empty command"))?;
        let mut cmd = Command::new(prog);
        cmd.args(args)
            .current_dir(workspace.work())
            .envs(vars)
            .env("MLQ_WORK", workspace.work())
            .env("MLQ_OUT", workspace.out())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .kill_on_drop(true);
        if !gpu_ids.is_empty() {
            cmd.env(
                "CUDA_VISIBLE_DEVICES",
                gpu_ids
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

    fn pid_of(&self, handle: &str) -> Option<i32> {
        if let Some(p) = handle.strip_prefix("pid:") {
            return p.parse().ok();
        }
        self.env_pids.lock().unwrap().get(handle).copied()
    }
}

#[async_trait]
impl Executor for ProcessExecutor {
    fn name(&self) -> &str {
        "process"
    }
    fn supports_snapshots(&self) -> bool {
        false
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
        self.spawn(
            &spec.job.command,
            &spec.workspace,
            &spec.env,
            &spec.gpu_ids,
            &spec.log_path,
        )
    }

    async fn wait(&self, handle: &Handle) -> anyhow::Result<i32> {
        let child = self.children.lock().unwrap().remove(handle);
        let Some(mut child) = child else {
            anyhow::bail!("unknown handle {handle}")
        };
        let status = child.wait().await?;
        self.env_pids
            .lock()
            .unwrap()
            .retain(|_, p| format!("pid:{p}") != *handle);
        Ok(status.code().unwrap_or(-1))
    }

    async fn kill(&self, handle: &Handle) -> anyhow::Result<()> {
        if let Some(pid) = self.pid_of(handle).filter(|p| *p > 0) {
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

    async fn start_env(&self, spec: &EnvSpec) -> anyhow::Result<Handle> {
        Ok(format!("env:{}", spec.name))
    }

    async fn exec(
        &self,
        env: &Handle,
        job: &Job,
        workspace: &Workspace,
        vars: &BTreeMap<String, String>,
        log_path: &std::path::Path,
    ) -> anyhow::Result<Handle> {
        let h = self.spawn(&job.command, workspace, vars, &job.gpu_ids, log_path)?;
        if let Some(pid) = h.strip_prefix("pid:").and_then(|p| p.parse().ok()) {
            self.env_pids.lock().unwrap().insert(env.clone(), pid);
        }
        Ok(h)
    }

    fn env_of(&self, handle: &Handle) -> Option<Handle> {
        let pid = handle.strip_prefix("pid:")?.parse::<i32>().ok()?;
        self.env_pids
            .lock()
            .unwrap()
            .iter()
            .find(|(_, p)| **p == pid)
            .map(|(e, _)| e.clone())
    }

    async fn commit(&self, _env: &Handle, _image: &str) -> anyhow::Result<Option<u64>> {
        anyhow::bail!("snapshots need a container runtime (runtime = \"podman\")")
    }

    async fn remove_image(&self, _image: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn save_image(&self, _image: &str, _path: &std::path::Path) -> anyhow::Result<()> {
        anyhow::bail!("image export needs a container runtime (runtime = \"podman\")")
    }

    async fn load_image(&self, _path: &std::path::Path) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("image import needs a container runtime (runtime = \"podman\")")
    }

    async fn tag_image(&self, _src: &str, _dst: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn env_idle(&self, _env: &Handle) -> anyhow::Result<bool> {
        Ok(true)
    }

    async fn remove_env(&self, env: &Handle) -> anyhow::Result<()> {
        self.env_pids.lock().unwrap().remove(env);
        Ok(())
    }

    async fn usage(&self, handle: &Handle) -> Option<Usage> {
        let pid = self.pid_of(handle)?;
        tokio::task::spawn_blocking(move || super::usage::process_group_usage(pid))
            .await
            .ok()
            .flatten()
    }
}
