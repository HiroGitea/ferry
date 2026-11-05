//! Podman / Docker executor. Drives the CLI (`podman run -d ...`) so the daemon has no
//! dependency on a specific API socket, and swapping runtimes is a config change.

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::Mutex;

use anyhow::{Context, bail};
use async_trait::async_trait;
use ferry_core::api::ImageInfo;
use ferry_core::{Job, ResourceRequest};
use tokio::process::Command;

use super::{EnvSpec, Executor, Handle, RunSpec, Usage};
use crate::config::{ContainerConfig, Mount};
use crate::workspace::Workspace;

pub struct ContainerExecutor {
    binary: String,
    cfg: ContainerConfig,
    /// container -> cgroup path (None = could not be determined)
    cgroups: Mutex<HashMap<String, Option<String>>>,
}

/// Arguments shared by one-shot job containers and task environments.
pub struct CommonArgs<'a> {
    pub name: &'a str,
    pub labels: &'a [(String, String)],
    pub resources: &'a ResourceRequest,
    pub gpu_ids: &'a [u32],
    pub workspace: &'a Workspace,
    pub mounts: &'a [Mount],
    pub env: &'a BTreeMap<String, String>,
}

impl ContainerExecutor {
    pub fn new(binary: String, cfg: ContainerConfig) -> Self {
        Self {
            binary,
            cfg,
            cgroups: Mutex::new(HashMap::new()),
        }
    }

    /// `exec|<env>|<exit file>` -> env
    fn exec_env(&self, handle: &str) -> Option<String> {
        parse_exec(handle).map(|(env, _)| env)
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
        format!("ferry-{job_id}")
    }

    /// `run -d ...` up to (not including) the image. Public for unit tests.
    pub fn common_args(&self, c: &CommonArgs<'_>) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            c.name.to_string(),
        ];
        for (k, v) in c.labels {
            a.push("--label".into());
            a.push(format!("{k}={v}"));
        }
        a.extend([
            "--cpus".into(),
            format!("{}", c.resources.cpus),
            "--memory".into(),
            format!("{}b", c.resources.mem_bytes),
            "--memory-swap".into(),
            format!("{}b", c.resources.mem_bytes),
            "--pids-limit".into(),
            self.cfg.pids_limit.to_string(),
            "--shm-size".into(),
            self.cfg.shm_size.clone(),
            "--network".into(),
            self.cfg.network.clone(),
            "-v".into(),
            format!("{}:/work", c.workspace.work().display()),
            "-v".into(),
            format!("{}:/out", c.workspace.out().display()),
            "-w".into(),
            "/work".into(),
        ]);
        for m in c.mounts {
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
        if !c.gpu_ids.is_empty() {
            match self.gpu_style() {
                // "none": no device flags (fake GPUs / testing without the NVIDIA toolkit)
                "none" => {}
                "docker" => {
                    a.push("--gpus".into());
                    a.push(format!(
                        "\"device={}\"",
                        c.gpu_ids
                            .iter()
                            .map(|g| g.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    ));
                }
                _ => {
                    for g in c.gpu_ids {
                        a.push("--device".into());
                        a.push(format!("nvidia.com/gpu={g}"));
                    }
                }
            }
        }
        for (k, v) in c.env {
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        a.extend(self.cfg.extra_args.iter().cloned());
        a
    }

    /// Build the `run` argument list for a one-shot job. Public for unit tests.
    pub fn run_args(&self, spec: &RunSpec) -> Vec<String> {
        let job = &spec.job;
        let labels = vec![
            ("ferry.job".to_string(), job.id.to_string()),
            ("ferry.user".to_string(), job.user_name.clone()),
        ];
        let name = Self::container_name(job.id);
        let mut a = self.common_args(&CommonArgs {
            name: &name,
            labels: &labels,
            resources: &job.resources,
            gpu_ids: &spec.gpu_ids,
            workspace: &spec.workspace,
            mounts: &spec.mounts,
            env: &spec.env,
        });
        a.push(job.image.clone());
        a.extend(job.command.iter().cloned());
        a
    }

    /// Build the `run` argument list for an idle task environment. Public for unit tests.
    pub fn env_args(&self, spec: &EnvSpec) -> Vec<String> {
        let mut a = self.common_args(&CommonArgs {
            name: &spec.name,
            labels: &spec.labels,
            resources: &spec.resources,
            gpu_ids: &spec.gpu_ids,
            workspace: &spec.workspace,
            mounts: &spec.mounts,
            env: &spec.env,
        });
        // Step logs and exit codes are written by the step itself into /.ferry (the task's logs/
        // directory), so a step keeps running and stays observable if the daemon restarts.
        a.push("-v".into());
        a.push(format!(
            "{}:/.ferry",
            spec.workspace.root.join("logs").display()
        ));
        // A tiny init reaps orphans; the environment idles until steps are exec'd into it.
        a.insert(2, "--init".into());
        a.push(spec.image.clone());
        a.extend(["sleep".to_string(), "infinity".to_string()]);
        a
    }

    /// `exec` arguments for one step. Public for unit tests.
    pub fn exec_args(&self, env: &str, job: &Job, vars: &BTreeMap<String, String>) -> Vec<String> {
        let mut a: Vec<String> = vec!["exec".into(), "-d".into(), "-w".into(), "/work".into()];
        for (k, v) in vars {
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        a.push(env.to_string());
        // $0 = job id; the command and its arguments are passed as positional parameters so no
        // shell quoting is needed. Output -> /.ferry/<id>.log, exit code -> /.ferry/<id>.exit.
        a.extend([
            "sh".to_string(),
            "-c".to_string(),
            "exec >>\"/.ferry/$0.log\" 2>&1; \"$@\"; echo $? > \"/.ferry/$0.exit\"".to_string(),
            job.id.to_string(),
        ]);
        a.extend(job.command.iter().cloned());
        a
    }

    async fn cgroup_path(&self, container: &str) -> Option<String> {
        if let Some(p) = self.cgroups.lock().unwrap().get(container) {
            return p.clone();
        }
        let path = if self.is_docker() {
            let id = self
                .run_cli(&[
                    "inspect".into(),
                    "--format".into(),
                    "{{.Id}}".into(),
                    container.to_string(),
                ])
                .await
                .ok()?;
            [
                format!("/system.slice/docker-{id}.scope"),
                format!("/docker/{id}"),
            ]
            .into_iter()
            .find(|p| super::usage::cgroup_usage(p).is_some())
        } else {
            self.run_cli(&[
                "inspect".into(),
                "--format".into(),
                "{{.State.CgroupPath}}".into(),
                container.to_string(),
            ])
            .await
            .ok()
            .filter(|p| !p.is_empty())
        };
        self.cgroups
            .lock()
            .unwrap()
            .insert(container.to_string(), path.clone());
        path
    }
}

/// `<log>.log` -> `<log>.exit`
pub fn exit_file(log_path: &std::path::Path) -> std::path::PathBuf {
    log_path.with_extension("exit")
}

fn parse_exec(handle: &str) -> Option<(String, std::path::PathBuf)> {
    let rest = handle.strip_prefix("exec|")?;
    let (env, exit) = rest.split_once('|')?;
    Some((env.to_string(), std::path::PathBuf::from(exit)))
}

#[async_trait]
impl Executor for ContainerExecutor {
    fn name(&self) -> &str {
        &self.binary
    }

    fn supports_snapshots(&self) -> bool {
        true
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
        if let Some((env, exit_path)) = parse_exec(handle) {
            // Poll the exit file; if the environment itself is gone the step cannot finish.
            let mut n = 0u32;
            loop {
                if let Ok(text) = std::fs::read_to_string(&exit_path) {
                    return text
                        .trim()
                        .parse::<i32>()
                        .with_context(|| format!("parsing exit file '{text}'"));
                }
                if n.is_multiple_of(10) && !self.is_alive(&env).await.unwrap_or(false) {
                    bail!("environment container stopped before the step finished");
                }
                n += 1;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        let out = self.run_cli(&["wait".into(), handle.clone()]).await?;
        out.lines()
            .last()
            .unwrap_or("")
            .trim()
            .parse::<i32>()
            .with_context(|| format!("parsing exit code '{out}'"))
    }

    async fn kill(&self, handle: &Handle) -> anyhow::Result<()> {
        // Killing a step stops its environment: uncommitted changes are discarded, i.e. the
        // task rolls back to its last snapshot.
        let target = self.exec_env(handle).unwrap_or_else(|| handle.clone());
        let _ = self
            .run_cli(&["stop".into(), "-t".into(), "10".into(), target])
            .await;
        Ok(())
    }

    async fn cleanup(&self, handle: &Handle) -> anyhow::Result<()> {
        if handle.starts_with("exec|") {
            return Ok(());
        }
        let _ = self
            .run_cli(&["rm".into(), "-f".into(), handle.clone()])
            .await;
        Ok(())
    }

    async fn is_alive(&self, handle: &Handle) -> anyhow::Result<bool> {
        if let Some((env, _)) = parse_exec(handle) {
            // A step is "alive" while its environment is: the exit file tells the rest.
            return self.is_alive(&env).await;
        }
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

    async fn start_env(&self, spec: &EnvSpec) -> anyhow::Result<Handle> {
        let _ = Command::new(&self.binary)
            .args(["rm", "-f", &spec.name])
            .stdin(Stdio::null())
            .output()
            .await;
        let args = self.env_args(spec);
        self.run_cli(&args).await
    }

    async fn exec(
        &self,
        env: &Handle,
        job: &Job,
        _workspace: &Workspace,
        vars: &BTreeMap<String, String>,
        log_path: &std::path::Path,
    ) -> anyhow::Result<Handle> {
        let exit_path = exit_file(log_path);
        let _ = std::fs::remove_file(&exit_path);
        self.run_cli(&self.exec_args(env, job, vars))
            .await
            .context("starting exec")?;
        Ok(format!("exec|{env}|{}", exit_path.display()))
    }

    fn env_of(&self, handle: &Handle) -> Option<Handle> {
        self.exec_env(handle)
    }

    async fn commit(&self, env: &Handle, image: &str) -> anyhow::Result<Option<u64>> {
        self.run_cli(&["commit".into(), "-q".into(), env.clone(), image.to_string()])
            .await?;
        let size = self
            .run_cli(&[
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Size}}".into(),
                image.to_string(),
            ])
            .await
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        Ok(size)
    }

    async fn remove_image(&self, image: &str) -> anyhow::Result<()> {
        self.run_cli(&["rmi".into(), "-f".into(), image.to_string()])
            .await
            .map(|_| ())
    }

    async fn save_image(&self, image: &str, path: &std::path::Path) -> anyhow::Result<()> {
        let mut args = vec!["save".to_string()];
        if !self.is_docker() {
            // oci-archive keeps the reference name so `load` restores it.
            args.extend(["--format".to_string(), "oci-archive".to_string()]);
        }
        args.extend([
            "-o".to_string(),
            path.to_string_lossy().to_string(),
            image.to_string(),
        ]);
        self.run_cli(&args).await.map(|_| ())
    }

    async fn load_image(&self, path: &std::path::Path) -> anyhow::Result<Vec<String>> {
        let out = self
            .run_cli(&[
                "load".into(),
                "-i".into(),
                path.to_string_lossy().to_string(),
            ])
            .await?;
        // podman/docker: "Loaded image: localhost/ferry-task-3:3" (one per line)
        let names: Vec<String> = out
            .lines()
            .filter_map(|l| {
                l.trim()
                    .strip_prefix("Loaded image")
                    .map(|r| r.trim_start_matches([':', 's', ' ']).trim().to_string())
            })
            .filter(|s| !s.is_empty())
            .collect();
        Ok(names)
    }

    async fn tag_image(&self, src: &str, dst: &str) -> anyhow::Result<()> {
        self.run_cli(&["tag".into(), src.to_string(), dst.to_string()])
            .await
            .map(|_| ())
    }

    async fn env_idle(&self, env: &Handle) -> anyhow::Result<bool> {
        // init + `sleep infinity` are the only processes of an idle environment.
        let fmt: Vec<String> = if self.is_docker() {
            vec!["-eo".into(), "pid".into()]
        } else {
            vec!["pid".into()]
        };
        let mut args = vec!["top".to_string(), env.clone()];
        args.extend(fmt);
        let out = self.run_cli(&args).await?;
        let procs = out.lines().skip(1).filter(|l| !l.trim().is_empty()).count();
        Ok(procs <= 2)
    }

    async fn remove_env(&self, env: &Handle) -> anyhow::Result<()> {
        let _ = self
            .run_cli(&["stop".into(), "-t".into(), "5".into(), env.clone()])
            .await;
        let _ = self.run_cli(&["rm".into(), "-f".into(), env.clone()]).await;
        self.cgroups.lock().unwrap().remove(env);
        Ok(())
    }

    async fn usage(&self, handle: &Handle) -> Option<Usage> {
        let container = self.exec_env(handle).unwrap_or_else(|| handle.clone());
        let path = self.cgroup_path(&container).await?;
        tokio::task::spawn_blocking(move || super::usage::cgroup_usage(&path))
            .await
            .ok()
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mount;
    use crate::workspace::Workspace;
    use ferry_core::{Job, JobState, ResourceRequest};

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
                gpu_mem_bytes: 0,
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
            task_id: None,
            base_snapshot_id: None,
            snapshot_id: None,
            requested_node: None,
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
            env: [("FERRY_JOB_ID".to_string(), "7".to_string())]
                .into_iter()
                .collect(),
            log_path: "/scratch/alice/jobs/7/job.log".into(),
        };
        let args = ex.run_args(&spec);
        let s = args.join(" ");
        assert!(s.starts_with("run -d --name ferry-7 "));
        assert!(s.contains("--cpus 2 --memory 1073741824b"));
        assert!(s.contains(
            "-v /scratch/alice/jobs/7/work:/work -v /scratch/alice/jobs/7/out:/out -w /work"
        ));
        assert!(s.contains("-v /mnt/nas:/data:ro"));
        assert!(s.contains("--device nvidia.com/gpu=1 --device nvidia.com/gpu=3"));
        assert!(s.ends_with("docker.io/library/alpine:3 sh -c echo hi"));
        assert!(!s.contains("--gpus"));

        let env = EnvSpec {
            name: "ferry-task-3".into(),
            labels: vec![("ferry.task".into(), "3".into())],
            image: "localhost/ferry-task-3:2".into(),
            user_name: "alice".into(),
            resources: ResourceRequest {
                cpus: 1.5,
                mem_bytes: 512 << 20,
                gpus: 1,
                gpu_mem_bytes: 0,
            },
            gpu_ids: vec![0],
            workspace: Workspace::open("/scratch/alice/tasks/3".into()),
            mounts: vec![],
            env: Default::default(),
        };
        let e = ex.env_args(&env).join(" ");
        assert!(e.starts_with("run -d --init --name ferry-task-3 --label ferry.task=3 --cpus 1.5"));
        assert!(e.ends_with("localhost/ferry-task-3:2 sleep infinity"));
        assert!(e.contains("--device nvidia.com/gpu=0"));
        let mut job2 = spec.job.clone();
        job2.command = vec!["python".into(), "train.py".into()];
        let vars: BTreeMap<String, String> = [("FERRY_JOB_ID".to_string(), "9".to_string())]
            .into_iter()
            .collect();
        assert_eq!(
            ex.exec_args("abc123", &job2, &vars).join(" "),
            "exec -d -w /work -e FERRY_JOB_ID=9 abc123 sh -c exec >>\"/.ferry/$0.log\" 2>&1; \"$@\"; echo $? > \"/.ferry/$0.exit\" 7 python train.py"
        );
    }
}
