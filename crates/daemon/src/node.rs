//! Nodes: where jobs run. A node bundles an executor (containers), a GPU provider, file access
//! to its scratch disk, and its managed resources. The local node wraps this machine; remote
//! nodes are agents (`mlqd --agent`) that registered over the network.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use mlq_core::Inventory;
use mlq_core::api::{ArtifactEntry, NodeInfo, NodeRegistration};
use mlq_core::scheduler::NodeInventory;

use crate::config::{Config, Mount};
use crate::executor::Executor;
use crate::gpu::GpuProvider;
use crate::workspace::{Workspace, read_log};

/// File operations on a node's scratch disk, addressed by node-local paths.
#[async_trait]
pub trait Files: Send + Sync {
    /// Create `root/{work,out,logs}`.
    async fn ensure_workspace(&self, root: &Path) -> anyhow::Result<()>;
    async fn write_files(
        &self,
        root: &Path,
        files: &BTreeMap<String, String>,
    ) -> anyhow::Result<()>;
    /// Extract a gzipped tar into `root/work`.
    async fn unpack_tar_gz(&self, root: &Path, data: Bytes) -> anyhow::Result<usize>;
    async fn read_log(
        &self,
        path: &Path,
        tail: Option<usize>,
        offset: Option<u64>,
    ) -> anyhow::Result<(String, u64)>;
    async fn list_out(&self, root: &Path) -> anyhow::Result<Vec<ArtifactEntry>>;
    /// Stream a file (size, body).
    async fn open_file(
        &self,
        path: &Path,
    ) -> anyhow::Result<(u64, BoxStream<'static, std::io::Result<Bytes>>)>;
    /// Copy `root/<subdir>` for each subdir into `dest/<subdir>` on the control daemon.
    async fn copy_to_local(&self, root: &Path, subdirs: &[&str], dest: &Path)
    -> anyhow::Result<()>;
    /// Copy a node file to a local path (e.g. a saved image).
    async fn fetch_file(&self, path: &Path, local_dest: &Path) -> anyhow::Result<()>;
    /// Copy a local file to a node path.
    async fn push_file(&self, local_src: &Path, path: &Path) -> anyhow::Result<()>;
    /// Unpack a plain tar that already lives on the node into `root` (restore from an archive).
    async fn unpack_tar_at(&self, tar_path: &Path, root: &Path) -> anyhow::Result<()>;
    async fn remove_dir(&self, path: &Path) -> anyhow::Result<()>;
    async fn remove_file(&self, path: &Path) -> anyhow::Result<()>;
    async fn dir_size(&self, path: &Path) -> anyhow::Result<u64>;
    async fn exists(&self, path: &Path) -> anyhow::Result<bool>;
}

pub struct LocalFiles;

pub fn copy_dir(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in walkdir::WalkDir::new(src).min_depth(1) {
        let e = e?;
        let rel = e.path().strip_prefix(src)?;
        let target = dst.join(rel);
        if e.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if e.file_type().is_file() {
            std::fs::copy(e.path(), &target)?;
        }
    }
    Ok(())
}

pub fn dir_size(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

#[async_trait]
impl Files for LocalFiles {
    async fn ensure_workspace(&self, root: &Path) -> anyhow::Result<()> {
        let ws = Workspace::create(root.to_path_buf())?;
        std::fs::create_dir_all(ws.root.join("logs"))?;
        Ok(())
    }
    async fn write_files(
        &self,
        root: &Path,
        files: &BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        Workspace::create(root.to_path_buf())?.write_files(files)
    }
    async fn unpack_tar_gz(&self, root: &Path, data: Bytes) -> anyhow::Result<usize> {
        let ws = Workspace::create(root.to_path_buf())?;
        tokio::task::spawn_blocking(move || ws.unpack_tar_gz(&data)).await?
    }
    async fn read_log(
        &self,
        path: &Path,
        tail: Option<usize>,
        offset: Option<u64>,
    ) -> anyhow::Result<(String, u64)> {
        read_log(path, tail, offset)
    }
    async fn list_out(&self, root: &Path) -> anyhow::Result<Vec<ArtifactEntry>> {
        Workspace::open(root.to_path_buf()).list_out()
    }
    async fn open_file(
        &self,
        path: &Path,
    ) -> anyhow::Result<(u64, BoxStream<'static, std::io::Result<Bytes>>)> {
        let md = tokio::fs::metadata(path).await?;
        if !md.is_file() {
            anyhow::bail!("not a file");
        }
        let f = tokio::fs::File::open(path).await?;
        Ok((md.len(), Box::pin(tokio_util::io::ReaderStream::new(f))))
    }
    async fn copy_to_local(
        &self,
        root: &Path,
        subdirs: &[&str],
        dest: &Path,
    ) -> anyhow::Result<()> {
        let pairs: Vec<(PathBuf, PathBuf)> = subdirs
            .iter()
            .map(|s| (root.join(s), dest.join(s)))
            .collect();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            for (s, d) in pairs {
                if s.exists() {
                    copy_dir(&s, &d)?;
                }
            }
            Ok(())
        })
        .await?
    }
    async fn fetch_file(&self, path: &Path, local_dest: &Path) -> anyhow::Result<()> {
        tokio::fs::copy(path, local_dest).await?;
        Ok(())
    }
    async fn push_file(&self, local_src: &Path, path: &Path) -> anyhow::Result<()> {
        if let Some(p) = path.parent() {
            tokio::fs::create_dir_all(p).await?;
        }
        tokio::fs::copy(local_src, path).await?;
        Ok(())
    }
    async fn unpack_tar_at(&self, tar_path: &Path, root: &Path) -> anyhow::Result<()> {
        let (t, r) = (tar_path.to_path_buf(), root.to_path_buf());
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            std::fs::create_dir_all(&r)?;
            let mut ar = tar::Archive::new(std::fs::File::open(&t)?);
            ar.unpack(&r)?;
            Ok(())
        })
        .await?
    }
    async fn remove_dir(&self, path: &Path) -> anyhow::Result<()> {
        if path.exists() {
            tokio::fs::remove_dir_all(path).await?;
        }
        Ok(())
    }
    async fn remove_file(&self, path: &Path) -> anyhow::Result<()> {
        let _ = tokio::fs::remove_file(path).await;
        Ok(())
    }
    async fn dir_size(&self, path: &Path) -> anyhow::Result<u64> {
        let p = path.to_path_buf();
        Ok(tokio::task::spawn_blocking(move || if p.exists() { dir_size(&p) } else { 0 }).await?)
    }
    async fn exists(&self, path: &Path) -> anyhow::Result<bool> {
        Ok(path.exists())
    }
}

/// Static facts about a node (what an agent reports, or the local config).
#[derive(Debug, Clone)]
pub struct NodeFacts {
    pub hostname: String,
    pub runtime: String,
    pub version: String,
    pub cpus_total: f64,
    pub mem_total: u64,
    /// Managed pool (after reservations).
    pub inventory: Inventory,
    pub scratch_root: PathBuf,
    pub default_image: String,
    pub mounts: Vec<Mount>,
    pub supports_snapshots: bool,
}

/// Client handles of a remote node (agent).
pub struct RemoteParts {
    pub agent: crate::remote::Agent,
    pub gpu: Arc<crate::remote::RemoteGpu>,
}

pub struct Node {
    pub id: String,
    pub local: bool,
    pub url: Option<String>,
    pub remote: Option<RemoteParts>,
    pub executor: Arc<dyn Executor>,
    pub gpu: Arc<dyn GpuProvider>,
    pub files: Arc<dyn Files>,
    pub facts: RwLock<NodeFacts>,
    pub last_seen: Mutex<DateTime<Utc>>,
    pub online: Mutex<bool>,
}

impl Node {
    pub fn facts(&self) -> NodeFacts {
        self.facts.read().unwrap().clone()
    }
    pub fn scratch_root(&self) -> PathBuf {
        self.facts.read().unwrap().scratch_root.clone()
    }
    pub fn mounts(&self) -> Vec<Mount> {
        self.facts.read().unwrap().mounts.clone()
    }
    pub fn is_online(&self) -> bool {
        *self.online.lock().unwrap()
    }
    pub fn info(&self) -> NodeInfo {
        let f = self.facts();
        NodeInfo {
            id: self.id.clone(),
            hostname: f.hostname,
            cpus_total: f.cpus_total,
            mem_total: f.mem_total,
            cpus_managed: f.inventory.cpus,
            mem_managed: f.inventory.mem_bytes,
            gpus_managed: f.inventory.gpus,
            runtime: f.runtime,
            default_image: f.default_image,
        }
    }
    pub fn inventory(&self) -> NodeInventory {
        NodeInventory {
            id: self.id.clone(),
            inventory: self.facts().inventory,
            online: self.is_online(),
        }
    }
    pub fn touch(&self) {
        *self.last_seen.lock().unwrap() = Utc::now();
        *self.online.lock().unwrap() = true;
    }
}

/// Managed inventory of this machine from the config: host resources minus reservations,
/// GPUs filtered by `managed_gpus`.
pub fn local_inventory(cfg: &Config, gpu: &dyn GpuProvider) -> Inventory {
    let (host_cpus, host_mem) = crate::sched::host_resources();
    let cpus = cfg
        .max_cpus
        .unwrap_or((host_cpus - cfg.reserve_cpus).max(0.0));
    let mem = cfg
        .max_mem
        .as_deref()
        .and_then(|m| mlq_core::parse_bytes(m).ok())
        .unwrap_or(host_mem.saturating_sub(cfg.reserve_mem_bytes()));
    let detected: Vec<u32> = gpu.list().into_iter().map(|g| g.index).collect();
    let gpus = match &cfg.managed_gpus {
        Some(list) => list
            .iter()
            .copied()
            .filter(|g| detected.contains(g))
            .collect(),
        None => detected,
    };
    Inventory {
        cpus,
        mem_bytes: mem,
        gpus,
    }
}

pub fn local_facts(cfg: &Config, executor: &dyn Executor, gpu: &dyn GpuProvider) -> NodeFacts {
    let (host_cpus, host_mem) = crate::sched::host_resources();
    NodeFacts {
        hostname: std::fs::read_to_string("/etc/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "localhost".into()),
        runtime: executor.name().to_string(),
        version: crate::VERSION.into(),
        cpus_total: host_cpus,
        mem_total: host_mem,
        inventory: local_inventory(cfg, gpu),
        scratch_root: cfg.scratch_root(),
        default_image: cfg.default_image.clone(),
        mounts: cfg.mounts.clone(),
        supports_snapshots: executor.supports_snapshots(),
    }
}

pub fn local_node(
    cfg: &Config,
    executor: Arc<dyn Executor>,
    gpu: Arc<dyn GpuProvider>,
) -> Arc<Node> {
    let facts = local_facts(cfg, executor.as_ref(), gpu.as_ref());
    Arc::new(Node {
        id: cfg.cluster.node_name.clone(),
        local: true,
        url: None,
        remote: None,
        executor,
        gpu,
        files: Arc::new(LocalFiles),
        facts: RwLock::new(facts),
        last_seen: Mutex::new(Utc::now()),
        online: Mutex::new(true),
    })
}

/// Facts as reported by an agent.
pub fn facts_from_registration(r: &NodeRegistration) -> NodeFacts {
    NodeFacts {
        hostname: r.hostname.clone(),
        runtime: r.runtime.clone(),
        version: r.version.clone(),
        cpus_total: r.cpus_total,
        mem_total: r.mem_total,
        inventory: Inventory {
            cpus: r.cpus_managed,
            mem_bytes: r.mem_managed,
            gpus: r.gpus_managed.clone(),
        },
        scratch_root: PathBuf::from(&r.scratch_root),
        default_image: r.default_image.clone(),
        mounts: r
            .mounts
            .iter()
            .map(|c| Mount {
                host: PathBuf::new(),
                container: c.clone(),
                read_only: true,
            })
            .collect(),
        supports_snapshots: r.supports_snapshots,
    }
}

/// What the local node reports about itself (used by the agent when registering).
pub fn registration(
    cfg: &Config,
    executor: &dyn Executor,
    gpu: &dyn GpuProvider,
    url: String,
) -> NodeRegistration {
    let f = local_facts(cfg, executor, gpu);
    NodeRegistration {
        id: cfg.cluster.node_name.clone(),
        url,
        hostname: f.hostname,
        runtime: f.runtime,
        version: f.version,
        cpus_total: f.cpus_total,
        mem_total: f.mem_total,
        cpus_managed: f.inventory.cpus,
        mem_managed: f.inventory.mem_bytes,
        gpus_managed: f.inventory.gpus,
        scratch_root: f.scratch_root.to_string_lossy().to_string(),
        default_image: f.default_image,
        mounts: f.mounts.iter().map(|m| m.container.clone()).collect(),
        supports_snapshots: f.supports_snapshots,
    }
}

/// Registry of nodes known to the control daemon.
#[derive(Default)]
pub struct Nodes {
    inner: RwLock<BTreeMap<String, Arc<Node>>>,
}

impl Nodes {
    pub fn insert(&self, n: Arc<Node>) {
        self.inner.write().unwrap().insert(n.id.clone(), n);
    }
    pub fn get(&self, id: &str) -> Option<Arc<Node>> {
        self.inner.read().unwrap().get(id).cloned()
    }
    pub fn all(&self) -> Vec<Arc<Node>> {
        self.inner.read().unwrap().values().cloned().collect()
    }
    pub fn remove(&self, id: &str) -> Option<Arc<Node>> {
        self.inner.write().unwrap().remove(id)
    }
    /// The local node if there is one, else any node (for defaults such as the default image).
    pub fn primary(&self) -> Option<Arc<Node>> {
        let g = self.inner.read().unwrap();
        g.values()
            .find(|n| n.local)
            .cloned()
            .or_else(|| g.values().next().cloned())
    }
    pub fn inventories(&self) -> Vec<NodeInventory> {
        self.all().iter().map(|n| n.inventory()).collect()
    }
}
