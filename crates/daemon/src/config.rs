use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Address for the REST/MCP/dashboard server.
    pub listen: String,
    /// Database, admin token, and (by default) job workspaces live here. Put it on the SSD.
    pub data_dir: PathBuf,
    /// Root for per-user job workspaces (fast local disk). Defaults to `<data_dir>/users`.
    pub scratch_root: Option<PathBuf>,
    /// "podman" (default), "docker", or "process" (no isolation, development only).
    pub runtime: String,
    /// Image used when a job does not specify one.
    pub default_image: String,
    /// If non-empty, only these image references (exact or prefix ending with '/') may be used.
    pub allowed_images: Vec<String>,
    /// GPU indices the scheduler may allocate. `None` = all detected GPUs. `[]` = none.
    pub managed_gpus: Option<Vec<u32>>,
    /// CPUs kept free for interactive host use.
    pub reserve_cpus: f64,
    /// Memory kept free for interactive host use (e.g. "8G").
    pub reserve_mem: String,
    /// Hard cap on CPUs offered to jobs (default: host CPUs - reserve).
    pub max_cpus: Option<f64>,
    /// Hard cap on memory offered to jobs (default: host memory - reserve).
    pub max_mem: Option<String>,
    /// "auto" (NVML if available), "none", or "fake:<n>" for development.
    pub gpu_provider: String,
    /// Extra host directories mounted into every job (e.g. the NFS dataset share).
    pub mounts: Vec<Mount>,
    pub container: ContainerConfig,
    pub retention: RetentionConfig,
    pub archive: ArchiveConfig,
    pub cluster: ClusterConfig,
    pub agent: AgentConfig,
    pub fair_share: FairShareConfig,
    pub metrics: crate::metrics::MetricsConfig,
    pub notify: NotifyConfig,
    pub idle: IdleConfig,
    pub backup: BackupConfig,
    pub log_level: String,
}

/// Periodic copies of the control database (users, tokens, jobs, tasks) to the NAS.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackupConfig {
    pub enabled: bool,
    /// Default: `<retention.archive_dir>/_ferry-backup`; no archive dir and no `dir` = off.
    pub dir: Option<PathBuf>,
    /// How often, e.g. "24h".
    pub interval: String,
    /// Backups kept; older ones are deleted.
    pub keep: usize,
    /// Also copy metrics.db (can be large).
    pub include_metrics: bool,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: None,
            interval: "24h".into(),
            keep: 14,
            include_metrics: false,
        }
    }
}

impl Config {
    pub fn backup_dir(&self) -> Option<PathBuf> {
        if !self.backup.enabled {
            return None;
        }
        self.backup.dir.clone().or_else(|| {
            self.retention
                .archive_dir
                .as_ref()
                .map(|d| d.join("_ferry-backup"))
        })
    }
}

/// Jobs that hold GPUs without using them: warn the owner, then cancel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IdleConfig {
    pub enabled: bool,
    /// GPU utilisation (%) at or below which a job counts as idle.
    pub util_threshold: f32,
    /// Warn after the GPUs were idle this long, e.g. "30m".
    pub warn_after: String,
    /// Cancel this long after the warning if they stayed idle, e.g. "30m".
    pub cancel_after: String,
    /// Users whose jobs are never reclaimed.
    pub exempt_users: Vec<String>,
}

impl Default for IdleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            util_threshold: 2.0,
            warn_after: "30m".into(),
            cancel_after: "30m".into(),
            exempt_users: vec![],
        }
    }
}

/// Server side of notifications; users choose their own channels.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotifyConfig {
    /// Bot token from @BotFather. Users put their chat id in their settings.
    pub telegram_bot_token: Option<String>,
    /// Telegram API base, e.g. a reverse proxy where api.telegram.org is not reachable.
    /// `HTTPS_PROXY` in the service environment works too.
    pub telegram_api: Option<String>,
    /// Dashboard URL used for links in messages, e.g. "http://gpu1.tailnet:7070".
    pub public_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub host: PathBuf,
    pub container: String,
    #[serde(default = "default_true")]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContainerConfig {
    /// `--user` for the container process, e.g. "1000:1000". Leave empty to use the image default.
    pub user: Option<String>,
    /// `--userns` value (rootless podman: "keep-id" maps the daemon user to the container user).
    pub userns: Option<String>,
    /// podman/docker network mode: "slirp4netns", "pasta", "bridge", "none", "host".
    pub network: String,
    pub pids_limit: u32,
    /// Shared memory size for the container (PyTorch DataLoader needs more than the 64M default).
    pub shm_size: String,
    /// "cdi" => `--device nvidia.com/gpu=N` (podman); "docker" => `--gpus device=N`;
    /// "none" => no device flags (fake GPUs, testing).
    pub gpu_flag_style: String,
    /// Extra arguments appended to every `run` invocation.
    pub extra_args: Vec<String>,
}

/// Multi-node: this daemon as the control node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClusterConfig {
    /// Name of this machine as a node (jobs show it as `node_id`).
    pub node_name: String,
    /// Run jobs on this machine too. `false` makes a pure control node (API, scheduler, NAS).
    pub local_node: bool,
    /// Shared secret agents present when registering. Empty = agents are not accepted.
    pub node_token: String,
    /// An agent that has not reported for this long is marked offline.
    pub node_timeout_secs: u64,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            node_name: "local".into(),
            local_node: true,
            node_token: String::new(),
            node_timeout_secs: 30,
        }
    }
}

/// `ferryd --agent`: this machine runs jobs for a control node elsewhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// URL of the control daemon, e.g. "http://control.tailnet:7070".
    pub control_url: String,
    /// Must equal `cluster.node_token` on the control node.
    pub node_token: String,
    /// Address the agent API listens on (reachable from the control node, e.g. the tailscale IP).
    pub listen: String,
    /// URL the control node should use to reach this agent (default: http://<first non-loopback IP>:<port>).
    pub advertise_url: Option<String>,
    pub heartbeat_secs: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            control_url: String::new(),
            node_token: String::new(),
            listen: "0.0.0.0:7071".into(),
            advertise_url: None,
            heartbeat_secs: 10,
        }
    }
}

/// Self-contained `.7z` task archives written to `retention.archive_dir` (the NAS).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveConfig {
    /// Write a .7z archive when a task is closed (and remove the per-step mirror directory).
    pub on_close: bool,
    /// Snapshot images to include by default: "none", "head", "pinned" (head + pinned), "all".
    pub images: String,
    /// 7z compression level 0-9 (5 = default; 1 is much faster for already-compressed layers).
    pub level: u8,
    /// Path of the 7z binary (`7z`, `7zz` or `7za`).
    pub seven_zip: String,
    /// Keep per-step CSV metrics in the archive.
    pub metrics: bool,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            on_close: true,
            images: "head".into(),
            level: 5,
            seven_zip: "7z".into(),
            metrics: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// Delete job workspaces (SSD scratch) this many days after the job finished.
    pub keep_days: u32,
    /// If total scratch usage exceeds this, oldest finished workspaces are deleted first.
    pub max_scratch: Option<String>,
    /// Before deleting, copy `/out` (and the log) to `<archive_dir>/<user>/<job_id>/` (e.g. on the NAS).
    pub archive_dir: Option<PathBuf>,
    /// How often the retention sweep runs.
    pub interval_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FairShareConfig {
    pub enabled: bool,
    pub half_life_secs: f64,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        let root = nix_is_root();
        let data_dir = if root {
            PathBuf::from("/var/lib/ferry")
        } else {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("ferry")
        };
        Self {
            listen: "127.0.0.1:7070".into(),
            data_dir,
            scratch_root: None,
            runtime: "podman".into(),
            default_image: "docker.io/pytorch/pytorch:latest".into(),
            allowed_images: vec![],
            managed_gpus: None,
            reserve_cpus: 2.0,
            reserve_mem: "8G".into(),
            max_cpus: None,
            max_mem: None,
            gpu_provider: "auto".into(),
            mounts: vec![],
            container: ContainerConfig::default(),
            retention: RetentionConfig::default(),
            archive: ArchiveConfig::default(),
            cluster: ClusterConfig::default(),
            agent: AgentConfig::default(),
            fair_share: FairShareConfig::default(),
            metrics: crate::metrics::MetricsConfig::default(),
            notify: NotifyConfig::default(),
            idle: IdleConfig::default(),
            backup: BackupConfig::default(),
            log_level: "info".into(),
        }
    }
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            user: None,
            userns: None,
            network: "slirp4netns".into(),
            pids_limit: 4096,
            shm_size: "8g".into(),
            gpu_flag_style: "auto".into(),
            extra_args: vec![],
        }
    }
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            keep_days: 14,
            max_scratch: None,
            archive_dir: None,
            interval_secs: 3600,
        }
    }
}

impl Default for FairShareConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            half_life_secs: 7.0 * 86400.0,
        }
    }
}

fn nix_is_root() -> bool {
    std::fs::metadata("/proc/self")
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.uid() == 0
        })
        .unwrap_or(false)
}

impl Config {
    pub fn load(path: Option<&Path>) -> anyhow::Result<Config> {
        let candidates: Vec<PathBuf> = match path {
            Some(p) => vec![p.to_path_buf()],
            None => vec![PathBuf::from("/etc/ferry/ferry.toml")],
        };
        for p in candidates {
            if p.exists() {
                let text = std::fs::read_to_string(&p)
                    .with_context(|| format!("reading {}", p.display()))?;
                let cfg: Config =
                    toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?;
                return Ok(cfg);
            } else if path.is_some() {
                anyhow::bail!("config file {} not found", p.display());
            }
        }
        Ok(Config::default())
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("ferry.db")
    }

    pub fn scratch_root(&self) -> PathBuf {
        self.scratch_root
            .clone()
            .unwrap_or_else(|| self.data_dir.join("users"))
    }

    pub fn reserve_mem_bytes(&self) -> u64 {
        ferry_core::parse_bytes(&self.reserve_mem).unwrap_or(0)
    }

    pub fn image_allowed(&self, image: &str) -> bool {
        if self.allowed_images.is_empty() {
            return true;
        }
        self.allowed_images.iter().any(|a| {
            if a.ends_with('/') {
                image.starts_with(a)
            } else {
                a == image
            }
        })
    }

    pub fn example_toml() -> String {
        let mut c = Config {
            mounts: vec![Mount {
                host: "/mnt/nas/datasets".into(),
                container: "/data".into(),
                read_only: true,
            }],
            ..Config::default()
        };
        c.retention.archive_dir = Some("/mnt/nas/ferry-results".into());
        c.retention.max_scratch = Some("500G".into());
        toml::to_string_pretty(&c).unwrap_or_default()
    }
}
