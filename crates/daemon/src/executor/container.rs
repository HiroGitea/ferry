//! Podman / Docker executor. Drives the CLI (`podman run -d ...`) so the daemon has no
//! dependency on a specific API socket, and swapping runtimes is a config change.

use std::process::Stdio;

use anyhow::{Context, bail};
use async_trait::async_trait;
use mlq_core::api::ImageInfo;
use tokio::process::Command;

use super::{Executor, Handle, RunSpec};
use crate::config::ContainerConfig;

pub struct ContainerExecutor {
    binary: String,
    cfg: ContainerConfig,
}

impl ContainerExecutor {
    pub fn new(binary: String, cfg: ContainerConfig) -> Self {
        Self { binary, cfg }
    }

    fn is_docker(&self) -> bool {
        self.binary.contains("docker")
    }

    fn gpu_style(&self) -> &str {
        match self.cfg.gpu_flag_style.as_str() {
            "auto" => {
                if self.is_docker() {
                    "docker"
                } else {
                    "cdi"
                }
            }
            s => s,
        }
    }

    async fn run_cli(&self, args: &[String]) -> anyhow::Result<String> {
        let out = Command::new(&self.binary)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .with_context(|| format!("running {} {}", self.binary, args.join(" ")))?;
        if !out.status.success() {
            bail!(
                "{} {} failed: {}",
                self.binary,
                args.first().cloned().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub fn container_name(job_id: i64) -> String {
        format!("mlq-{job_id}")
    }

    /// Build the `run` argument list. Public for unit tests.
    pub fn run_args(&self, spec: &RunSpec) -> Vec<String> {
        let job = &spec.job;
        let mut a: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            Self::container_name(job.id),
            "--label".into(),
            format!("mlq.job={}", job.id),
            "--label".into(),
            format!("mlq.user={}", job.user_name),
            "--cpus".into(),
            format!("{}", job.resources.cpus),
            "--memory".into(),
            format!("{}b", job.resources.mem_bytes),
            "--memory-swap".into(),
            format!("{}b", job.resources.mem_bytes),
            "--pids-limit".into(),
            self.cfg.pids_limit.to_string(),
            "--shm-size".into(),
            self.cfg.shm_size.clone(),
            "--network".into(),
            self.cfg.network.clone(),
            "-v".into(),
            format!("{}:/work", spec.workspace.work().display()),
            "-v".into(),
            format!("{}:/out", spec.workspace.out().display()),
            "-w".into(),
            "/work".into(),
        ];
        for m in &spec.mounts {
            let mut v = format!("{}:{}", m.host.display(), m.container);
            if m.read_only {
                v.push_str(":ro");
            }
            a.push("-v".into());
            a.push(v);
        }
        if let Some(u) = &self.cfg.user {
            a.push("--user".into());
            a.push(u.clone());
        }
        if let Some(ns) = &self.cfg.userns {
            a.push("--userns".into());
            a.push(ns.clone());
        }
        if !spec.gpu_ids.is_empty() {
            match self.gpu_style() {
                "docker" => {
                    a.push("--gpus".into());
                    a.push(format!(
                        "\"device={}\"",
                        spec.gpu_ids
                            .iter()
                            .map(|g| g.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    ));
                }
                _ => {
                    for g in &spec.gpu_ids {
                        a.push("--device".into());
                        a.push(format!("nvidia.com/gpu={g}"));
                    }
                }
            }
        }
        for (k, v) in &spec.env {
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        a.extend(self.cfg.extra_args.iter().cloned());
        a.push(job.image.clone());
        a.extend(job.command.iter().cloned());
        a
    }
}

#[async_trait]
impl Executor for ContainerExecutor {
    fn name(&self) -> &str {
        &self.binary
    }

    async fn image_present(&self, image: &str) -> anyhow::Result<bool> {
        let status = Command::new(&self.binary)
            .args(["image", "exists", image])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        match status {
            Ok(s) => Ok(s.success()),
            Err(e) => Err(e).context("checking image"),
        }
    }

    async fn pull_image(&self, image: &str) -> anyhow::Result<()> {
        self.run_cli(&["pull".into(), image.into()])
            .await
            .map(|_| ())
    }

    async fn list_images(&self) -> anyhow::Result<Vec<ImageInfo>> {
        let out = self
            .run_cli(&["images".into(), "--format".into(), "json".into()])
            .await?;
        let mut images = Vec::new();
        // podman: JSON array; docker: one JSON object per line.
        let values: Vec<serde_json::Value> = if out.trim_start().starts_with('[') {
            serde_json::from_str(&out).unwrap_or_default()
        } else {
            out.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        };
        for v in values {
            let id = v
                .get("Id")
                .or(v.get("ID"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let size = v.get("Size").and_then(|x| x.as_u64()).unwrap_or(0);
            let created = v
                .get("CreatedAt")
                .or(v.get("Created"))
                .map(|x| x.to_string().trim_matches('"').to_string());
            let names: Vec<String> = match v.get("Names") {
                Some(serde_json::Value::Array(arr)) => arr
                    .iter()
                    .filter_map(|n| n.as_str().map(String::from))
                    .collect(),
                _ => {
                    let repo = v
                        .get("Repository")
                        .and_then(|x| x.as_str())
                        .unwrap_or("<none>");
                    let tag = v.get("Tag").and_then(|x| x.as_str()).unwrap_or("<none>");
                    vec![format!("{repo}:{tag}")]
                }
            };
            for reference in names {
                images.push(ImageInfo {
                    reference,
                    id: id.clone(),
                    size,
                    created: created.clone(),
                });
            }
        }
        Ok(images)
    }

    async fn start(&self, spec: &RunSpec) -> anyhow::Result<Handle> {
        // Remove a stale container with the same name (e.g. after a crash).
        let _ = Command::new(&self.binary)
            .args(["rm", "-f", &Self::container_name(spec.job.id)])
            .stdin(Stdio::null())
            .output()
            .await;
        let args = self.run_args(spec);
        let id = self.run_cli(&args).await?;
        // Stream logs to the workspace log file.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spec.log_path)?;
        let log_err = log.try_clone()?;
        let mut child = Command::new(&self.binary)
            .args(["logs", "-f", &id])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()
            .context("starting log follower")?;
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        Ok(id)
    }

    async fn wait(&self, handle: &Handle) -> anyhow::Result<i32> {
        let out = self.run_cli(&["wait".into(), handle.clone()]).await?;
        out.lines()
            .last()
            .unwrap_or("")
            .trim()
            .parse::<i32>()
            .with_context(|| format!("parsing exit code '{out}'"))
    }

    async fn kill(&self, handle: &Handle) -> anyhow::Result<()> {
        let _ = self
            .run_cli(&["stop".into(), "-t".into(), "10".into(), handle.clone()])
            .await;
        Ok(())
    }

    async fn cleanup(&self, handle: &Handle) -> anyhow::Result<()> {
        let _ = self
            .run_cli(&["rm".into(), "-f".into(), handle.clone()])
            .await;
        Ok(())
    }

    async fn is_alive(&self, handle: &Handle) -> anyhow::Result<bool> {
        let out = self
            .run_cli(&[
                "inspect".into(),
                "--format".into(),
                "{{.State.Status}}".into(),
                handle.clone(),
            ])
            .await;
        Ok(matches!(out.as_deref(), Ok("running") | Ok("created")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mount;
    use crate::workspace::Workspace;
    use mlq_core::{Job, JobState, ResourceRequest};

    #[test]
    fn builds_podman_args() {
        let ex = ContainerExecutor::new("podman".into(), ContainerConfig::default());
        let job = Job {
            id: 7,
            user_id: 1,
            user_name: "alice".into(),
            node_id: None,
            name: "t".into(),
            image: "docker.io/library/alpine:3".into(),
            command: vec!["sh".into(), "-c".into(), "echo hi".into()],
            env: Default::default(),
            resources: ResourceRequest {
                cpus: 2.0,
                mem_bytes: 1 << 30,
                gpus: 2,
            },
            state: JobState::Queued,
            priority: 0,
            depends_on: vec![],
            array_index: None,
            time_limit_secs: None,
            submitted_at: chrono::Utc::now(),
            started_at: None,
            finished_at: None,
            exit_code: None,
            gpu_ids: vec![],
            container_id: None,
            workspace_path: None,
            reason: None,
            archive_path: None,
            workspace_deleted: false,
        };
        let spec = RunSpec {
            job,
            workspace: Workspace::open("/scratch/alice/jobs/7".into()),
            gpu_ids: vec![1, 3],
            mounts: vec![Mount {
                host: "/mnt/nas".into(),
                container: "/data".into(),
                read_only: true,
            }],
            env: [("MLQ_JOB_ID".to_string(), "7".to_string())]
                .into_iter()
                .collect(),
            log_path: "/scratch/alice/jobs/7/job.log".into(),
        };
        let args = ex.run_args(&spec);
        let s = args.join(" ");
        assert!(s.starts_with("run -d --name mlq-7 "));
        assert!(s.contains("--cpus 2 --memory 1073741824b"));
        assert!(s.contains(
            "-v /scratch/alice/jobs/7/work:/work -v /scratch/alice/jobs/7/out:/out -w /work"
        ));
        assert!(s.contains("-v /mnt/nas:/data:ro"));
        assert!(s.contains("--device nvidia.com/gpu=1 --device nvidia.com/gpu=3"));
        assert!(s.ends_with("docker.io/library/alpine:3 sh -c echo hi"));
        assert!(!s.contains("--gpus"));
    }
}
