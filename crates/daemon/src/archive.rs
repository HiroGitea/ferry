//! Self-contained task archives: one `.7z` per task on the NAS (HDD), restorable anywhere.
//!
//! Layout of `task-<id>-<name>-<YYYYmmdd-HHMMSS>.7z`:
//!
//! ```text
//! manifest.json      task, steps, snapshot graph, included images, sha256 of every file
//! README.txt         human-readable summary and restore instructions
//! steps.jsonl        one job record per line (same data as manifest, grep-friendly)
//! env/<name>.oci.tar snapshot images (podman save): HEAD by default, pinned/all optional
//! work/              the task's /work (code, inputs)
//! out/               the task's /out (results)
//! logs/<step>.log    stdout+stderr of every step
//! metrics/task.csv   per-step resource usage (t,key,avg,max), from the metrics engine
//! ```
//!
//! Import creates a new task whose HEAD is the archived HEAD image and whose `/work`, `/out`
//! and logs are restored; the step history is kept in the manifest (steps are not re-created
//! as jobs, their ids belong to the old daemon).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, bail};
use chrono::Utc;
use mlq_core::api::{ArchiveImage, ArchiveInfo, ArchiveManifest, ExportTaskResponse};
use mlq_core::{Snapshot, Task, TaskState};
use sha2::Digest;
use tokio::process::Command;
use tracing::{info, warn};

use crate::state::AppState;
use crate::workspace::{Workspace, sanitize_name};

pub const FORMAT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePolicy {
    None,
    Head,
    Pinned,
    All,
}

impl ImagePolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "head" => Some(Self::Head),
            "pinned" => Some(Self::Pinned),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

fn archive_root(state: &AppState) -> anyhow::Result<PathBuf> {
    state
        .config
        .retention
        .archive_dir
        .clone()
        .context("retention.archive_dir is not configured (where on the NAS archives go)")
}

pub fn archive_name(task: &Task) -> String {
    let safe: String = sanitize_name(&task.name).chars().take(40).collect();
    format!(
        "task-{}-{}-{}.7z",
        task.id,
        safe.trim_matches('_'),
        Utc::now().format("%Y%m%d-%H%M%S")
    )
}

async fn seven_zip(
    state: &AppState,
    args: &[String],
    cwd: Option<&Path>,
) -> anyhow::Result<String> {
    let bin = &state.config.archive.seven_zip;
    let mut cmd = Command::new(bin);
    cmd.args(args).stdin(Stdio::null());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let out = cmd
        .output()
        .await
        .with_context(|| format!("running {bin} (is 7zip installed?)"))?;
    if !out.status.success() {
        bail!(
            "{bin} {} failed: {}",
            args.first().cloned().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn sha256_file(p: &Path) -> anyhow::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

fn copy_dir(src: &Path, dst: &Path) -> anyhow::Result<()> {
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

fn checksums(root: &Path) -> anyhow::Result<(BTreeMap<String, String>, u64)> {
    let mut map = BTreeMap::new();
    let mut total = 0;
    for e in walkdir::WalkDir::new(root).min_depth(1).sort_by_file_name() {
        let e = e?;
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e
            .path()
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        if rel == "manifest.json" {
            continue;
        }
        total += e.metadata()?.len();
        map.insert(rel, sha256_file(e.path())?);
    }
    Ok((map, total))
}

fn readme(m: &ArchiveManifest) -> String {
    let mut s = String::new();
    let t = &m.task;
    s.push_str(&format!(
        "mlq task archive — task #{} \"{}\" by {}\n",
        t.id, t.name, t.user_name
    ));
    s.push_str(&format!(
        "exported {} from {} (mlq {})\n\n",
        m.created_at.format("%Y-%m-%d %H:%M:%S UTC"),
        m.hostname,
        m.mlq_version
    ));
    s.push_str(&format!("base image : {}\n", t.base_image));
    s.push_str(&format!(
        "defaults   : {} cpus, {} bytes, {} gpus\n",
        t.resources.cpus, t.resources.mem_bytes, t.resources.gpus
    ));
    s.push_str(&format!(
        "steps      : {}   snapshots: {}   HEAD: {}\n\n",
        m.steps.len(),
        m.snapshots.len(),
        t.head_snapshot_id
            .map(|h| format!("snap {h}"))
            .unwrap_or("base".into())
    ));
    s.push_str("steps:\n");
    for j in &m.steps {
        s.push_str(&format!(
            "  #{:<5} {:<10} from {:<8} -> {:<8} {}\n",
            j.id,
            j.state,
            j.base_snapshot_id
                .map(|v| format!("snap {v}"))
                .unwrap_or("base".into()),
            j.snapshot_id
                .map(|v| format!("snap {v}"))
                .unwrap_or("-".into()),
            j.command.join(" ")
        ));
    }
    s.push_str("\nimages (env/):\n");
    if m.images.is_empty() {
        s.push_str("  none (restore needs the base image and re-running the steps)\n");
    }
    for i in &m.images {
        s.push_str(&format!(
            "  {:<28} {:>12} bytes  {}{}\n",
            i.file,
            i.bytes,
            i.image,
            if i.is_head { "  (HEAD)" } else { "" }
        ));
    }
    s.push_str("\nrestore with mlq:   mlq archive import <this file>\n");
    s.push_str("restore by hand:    podman load -i env/<file>.oci.tar ; podman run -it -v $PWD/work:/work -v $PWD/out:/out <image> bash\n");
    s.push_str(
        "layout: manifest.json steps.jsonl env/ work/ out/ logs/<step>.log metrics/task.csv\n",
    );
    s
}

async fn metrics_csv(state: &AppState, task: &Task, path: &Path) -> anyhow::Result<()> {
    let after = task.created_at.timestamp() - 60;
    let before = task
        .closed_at
        .map(|c| c.timestamp() + 60)
        .unwrap_or_else(|| Utc::now().timestamp());
    let known: Vec<String> = state
        .metrics
        .keys()
        .await?
        .into_iter()
        .map(|k| k.key)
        .collect();
    let keys = crate::metrics::expand_keys(&format!("task.{}.*", task.id), &known);
    if keys.is_empty() {
        return Ok(());
    }
    let r = state.metrics.query(&keys, after, before, 20_000).await?;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(f, "t,key,unit,avg,max")?;
    for s in &r.series {
        for ((t, v), m) in s.points.iter().zip(&s.max) {
            if let (Some(v), Some(m)) = (v, m) {
                writeln!(f, "{t},{},{},{v},{m}", s.key, s.unit)?;
            }
        }
    }
    Ok(())
}

/// Export a task to `<archive_dir>/<user>/<archive_name>.7z`. Returns the archive path and manifest.
pub async fn export_task(
    state: &AppState,
    task: &Task,
    policy: ImagePolicy,
) -> anyhow::Result<ExportTaskResponse> {
    let root = archive_root(state)?;
    let dest_dir = root.join(sanitize_name(&task.user_name));
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(archive_name(task));
    let staging = state.config.data_dir.join("export").join(format!(
        "task-{}-{}",
        task.id,
        std::process::id()
    ));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    let result = build(state, task, policy, &staging, &dest).await;
    let _ = std::fs::remove_dir_all(&staging);
    let (manifest, bytes) = result?;
    info!(task = task.id, path = %dest.display(), bytes, "task archived");
    Ok(ExportTaskResponse {
        path: dest.to_string_lossy().to_string(),
        bytes,
        manifest,
        offloaded: false,
    })
}

async fn build(
    state: &AppState,
    task: &Task,
    policy: ImagePolicy,
    staging: &Path,
    dest: &Path,
) -> anyhow::Result<(ArchiveManifest, u64)> {
    let ws = crate::tasks::task_workspace(state, task);
    let steps = state.store.task_steps(task.id).await?;
    let snapshots: Vec<Snapshot> = state.store.snapshots(task.id).await?;

    // Files from the node's SSD workspace.
    let node = state.node_for_task(task)?;
    node.files
        .copy_to_local(&ws.root, &["work", "out", "logs"], staging)
        .await?;
    for sub in ["work", "out", "logs"] {
        std::fs::create_dir_all(staging.join(sub))?;
    }
    // Images are saved on the node and fetched (the local node saves straight into staging).
    let transfer = node.scratch_root().join(".transfer");
    let save = |image: String, dest: PathBuf| {
        let node = node.clone();
        let transfer = transfer.clone();
        async move {
            if node.local {
                node.executor.save_image(&image, &dest).await
            } else {
                let remote = transfer.join(dest.file_name().unwrap_or_default());
                node.executor.save_image(&image, &remote).await?;
                let r = node.files.fetch_file(&remote, &dest).await;
                let _ = node.files.remove_file(&remote).await;
                r
            }
        }
    };

    // Snapshot images.
    let mut images = Vec::new();
    if policy != ImagePolicy::None && node.executor.supports_snapshots() {
        std::fs::create_dir_all(staging.join("env"))?;
        let head = task.head_snapshot_id;
        for s in snapshots.iter().filter(|s| !s.deleted) {
            let is_head = Some(s.id) == head;
            let include = match policy {
                ImagePolicy::None => false,
                ImagePolicy::Head => is_head,
                ImagePolicy::Pinned => is_head || s.pinned,
                ImagePolicy::All => true,
            };
            if !include {
                continue;
            }
            let file = format!("snap-{}.oci.tar", s.id);
            let path = staging.join("env").join(&file);
            save(s.image.clone(), path.clone())
                .await
                .with_context(|| format!("saving {}", s.image))?;
            let bytes = std::fs::metadata(&path)?.len();
            images.push(ArchiveImage {
                snapshot_id: Some(s.id),
                image: s.image.clone(),
                file: format!("env/{file}"),
                bytes,
                is_head,
            });
        }
        if head.is_none() && policy != ImagePolicy::None {
            // HEAD is the base image: include it so the archive restores without registry access.
            let file = "base.oci.tar".to_string();
            let path = staging.join("env").join(&file);
            if save(task.base_image.clone(), path.clone()).await.is_ok() {
                let bytes = std::fs::metadata(&path)?.len();
                images.push(ArchiveImage {
                    snapshot_id: None,
                    image: task.base_image.clone(),
                    file: format!("env/{file}"),
                    bytes,
                    is_head: true,
                });
            }
        }
    }

    // Metrics.
    if state.config.archive.metrics {
        std::fs::create_dir_all(staging.join("metrics"))?;
        if let Err(e) = metrics_csv(state, task, &staging.join("metrics").join("task.csv")).await {
            warn!(task = task.id, "metrics export skipped: {e:#}");
        }
    }

    // steps.jsonl
    {
        let mut f = std::io::BufWriter::new(std::fs::File::create(staging.join("steps.jsonl"))?);
        for j in &steps {
            writeln!(f, "{}", serde_json::to_string(j)?)?;
        }
    }

    let (sums, total) = {
        let st = staging.to_path_buf();
        tokio::task::spawn_blocking(move || checksums(&st)).await??
    };
    let hostname = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".into());
    let manifest = ArchiveManifest {
        format: FORMAT,
        created_at: Utc::now(),
        mlq_version: crate::VERSION.into(),
        hostname,
        task: task.clone(),
        steps,
        snapshots,
        images,
        checksums: sums,
        total_bytes: total,
    };
    std::fs::write(
        staging.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    std::fs::write(staging.join("README.txt"), readme(&manifest))?;

    // Pack: solid 7z, LZMA2, all cores. Image tars already contain compressed layers for
    // registry images, but committed layers are plain tar and compress well.
    let tmp = dest.with_extension("7z.part");
    let _ = std::fs::remove_file(&tmp);
    let level = state.config.archive.level.min(9);
    seven_zip(
        state,
        &[
            "a".into(),
            "-t7z".into(),
            format!("-mx={level}"),
            "-mmt=on".into(),
            "-bso0".into(),
            "-bsp0".into(),
            tmp.to_string_lossy().to_string(),
            ".".into(),
        ],
        Some(staging),
    )
    .await?;
    seven_zip(
        state,
        &[
            "t".into(),
            "-bso0".into(),
            "-bsp0".into(),
            tmp.to_string_lossy().to_string(),
        ],
        None,
    )
    .await
    .context("archive verification")?;
    std::fs::rename(&tmp, dest)?;
    let bytes = std::fs::metadata(dest)?.len();
    Ok((manifest, bytes))
}

/// Archives visible to `user` (admins see everyone's), newest first.
pub async fn list(state: &AppState, user: Option<&str>) -> anyhow::Result<Vec<ArchiveInfo>> {
    let root = archive_root(state)?;
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for e in walkdir::WalkDir::new(&root).min_depth(2).max_depth(2) {
        let e = e?;
        if !e.file_type().is_file() || e.path().extension().map(|x| x != "7z").unwrap_or(true) {
            continue;
        }
        let rel = e.path().strip_prefix(&root)?;
        let owner = rel
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .unwrap_or_default();
        if let Some(u) = user {
            if sanitize_name(u) != owner {
                continue;
            }
        }
        let md = e.metadata()?;
        let fname = e.file_name().to_string_lossy().to_string();
        let task_id = fname
            .strip_prefix("task-")
            .and_then(|r| r.split('-').next())
            .and_then(|i| i.parse().ok());
        out.push(ArchiveInfo {
            name: rel.to_string_lossy().replace('\\', "/"),
            path: e.path().to_string_lossy().to_string(),
            bytes: md.len(),
            modified_at: md
                .modified()
                .map(chrono::DateTime::<Utc>::from)
                .unwrap_or_else(|_| Utc::now()),
            user: owner,
            task_id,
        });
    }
    out.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    Ok(out)
}

/// Resolve an archive name (relative to the archive root) to a path, refusing traversal.
pub fn resolve(state: &AppState, name: &str) -> anyhow::Result<PathBuf> {
    let root = archive_root(state)?;
    let p = Path::new(name);
    if p.is_absolute()
        || p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("archive name must be relative to the archive root");
    }
    let full = root.join(p);
    if !full.is_file() {
        bail!("archive {name} not found");
    }
    Ok(full)
}

/// Read the manifest of an archive without extracting the rest.
pub async fn read_manifest(state: &AppState, path: &Path) -> anyhow::Result<ArchiveManifest> {
    let out = seven_zip(
        state,
        &[
            "x".into(),
            "-so".into(),
            path.to_string_lossy().to_string(),
            "manifest.json".into(),
        ],
        None,
    )
    .await?;
    let m: ArchiveManifest = serde_json::from_str(&out).context("parsing manifest.json")?;
    if m.format > FORMAT {
        bail!(
            "archive format {} is newer than this mlq ({FORMAT})",
            m.format
        );
    }
    Ok(m)
}

/// Create a new task for `user_id` from an archive: restores work/out/logs and the HEAD image.
pub async fn import_task(
    state: &AppState,
    user_id: i64,
    path: &Path,
    task_name: Option<&str>,
    node: Option<String>,
) -> anyhow::Result<Task> {
    let m = read_manifest(state, path).await?;
    let name = task_name
        .map(str::to_string)
        .unwrap_or_else(|| m.task.name.clone());
    let task = state
        .store
        .create_task(
            user_id,
            &name,
            &m.task.base_image,
            &m.task.resources,
            m.task.keep_warm_secs,
            node.as_deref(),
        )
        .await?;
    let ws = crate::tasks::ensure_workspace(state, &task).await?;
    let task = state.store.task(task.id).await?.context("task vanished")?;
    let staging = state.config.data_dir.join("export").join(format!(
        "import-{}-{}",
        task.id,
        std::process::id()
    ));
    std::fs::create_dir_all(&staging)?;
    let res = restore(state, &m, &task, &ws, path, &staging).await;
    let _ = std::fs::remove_dir_all(&staging);
    res?;
    info!(task = task.id, from = %path.display(), "task imported");
    state.store.task(task.id).await?.context("task vanished")
}

async fn restore(
    state: &AppState,
    m: &ArchiveManifest,
    task: &Task,
    ws: &Workspace,
    path: &Path,
    staging: &Path,
) -> anyhow::Result<()> {
    seven_zip(
        state,
        &[
            "x".into(),
            "-bso0".into(),
            "-bsp0".into(),
            format!("-o{}", staging.display()),
            path.to_string_lossy().to_string(),
        ],
        None,
    )
    .await?;
    // Verify checksums before touching the workspace.
    {
        let st = staging.to_path_buf();
        let expect = m.checksums.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            for (rel, sum) in &expect {
                let got = sha256_file(&st.join(rel)).with_context(|| format!("missing {rel}"))?;
                if &got != sum {
                    bail!("checksum mismatch for {rel}");
                }
            }
            Ok(())
        })
        .await??;
    }
    let node = state.node_for_task(task)?;
    // Restore work/out/logs onto the task's node.
    if node.local {
        for sub in ["work", "out", "logs"] {
            let src = staging.join(sub);
            if src.exists() {
                let (s, d) = (src, ws.root.join(sub));
                tokio::task::spawn_blocking(move || copy_dir(&s, &d)).await??;
            }
        }
    } else {
        // One tar with work/, out/ and logs/ at its root, pushed and unpacked on the node.
        let tar_path = staging.join("restore.tar");
        let (st, tp) = (staging.to_path_buf(), tar_path.clone());
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut ar = tar::Builder::new(std::fs::File::create(&tp)?);
            for sub in ["work", "out", "logs"] {
                if st.join(sub).is_dir() {
                    ar.append_dir_all(sub, st.join(sub))?;
                }
            }
            ar.finish()?;
            Ok(())
        })
        .await??;
        let remote = node
            .scratch_root()
            .join(".transfer")
            .join(format!("restore-{}.tar", task.id));
        node.files.push_file(&tar_path, &remote).await?;
        node.files.unpack_tar_at(&remote, &ws.root).await?;
        let _ = node.files.remove_file(&remote).await;
    }
    std::fs::copy(
        staging.join("manifest.json"),
        ws.root.join("imported-manifest.json"),
    )
    .ok();
    // HEAD image → snapshot 1 of the new task.
    if let Some(img) = m.images.iter().find(|i| i.is_head) {
        if node.executor.supports_snapshots() {
            let local_tar = staging.join(&img.file);
            let node_tar = if node.local {
                local_tar.clone()
            } else {
                let p = node
                    .scratch_root()
                    .join(".transfer")
                    .join(format!("import-{}.oci.tar", task.id));
                node.files.push_file(&local_tar, &p).await?;
                p
            };
            let loaded = node.executor.load_image(&node_tar).await?;
            if !node.local {
                let _ = node.files.remove_file(&node_tar).await;
            }
            let src = loaded.first().cloned().unwrap_or_else(|| img.image.clone());
            let tag = crate::tasks::snapshot_image(task.id, 1);
            node.executor.tag_image(&src, &tag).await?;
            if src != tag && src.starts_with("localhost/mlq-task-") {
                let _ = node.executor.remove_image(&src).await;
            }
            let snap = state
                .store
                .create_snapshot(task.id, None, None, &tag, Some(img.bytes))
                .await?;
            state
                .store
                .pin_snapshot(
                    snap.id,
                    true,
                    Some(&format!(
                        "imported from {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    )),
                )
                .await?;
            state.store.set_task_head(task.id, Some(snap.id)).await?;
        }
    }
    Ok(())
}

/// After a verified export of a closed task: remove the SSD workspace, snapshot images and the
/// per-step mirror directory. The task row stays (state closed) and records the archive path.
pub async fn offload(state: &AppState, task: &Task, archive_path: &str) -> anyhow::Result<()> {
    if task.state != TaskState::Closed {
        bail!("only closed tasks can be offloaded");
    }
    let node = state.node_for_task(task)?;
    for s in state.store.snapshots(task.id).await? {
        if !s.deleted {
            let _ = node.executor.remove_image(&s.image).await;
            state.store.mark_snapshot_deleted(s.id).await?;
        }
    }
    if let Some(p) = &task.workspace_path {
        let _ = node.files.remove_dir(Path::new(p)).await;
    }
    if let Some(root) = &state.config.retention.archive_dir {
        let mirror = crate::tasks::archive_dir_for(root, task);
        if mirror.is_dir() {
            let _ = std::fs::remove_dir_all(&mirror);
        }
    }
    state.store.set_task_archive(task.id, archive_path).await?;
    Ok(())
}
