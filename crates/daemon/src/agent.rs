//! Agent mode (`mlqd --agent`): this machine runs jobs for a control node elsewhere.
//!
//! The agent exposes the local executor, GPU provider and scratch-disk file operations over
//! HTTP (`/agent/v1`, bearer `agent.node_token`) and registers with the control node every
//! `heartbeat_secs`. Paths in requests must lie under `scratch_root`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use axum::body::{Body, Bytes};
use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use tracing::{info, warn};

use crate::config::Config;
use crate::executor::{EnvSpec, Executor, RunSpec};
use crate::gpu::GpuProvider;
use crate::metrics::collector::{HostPrev, host_samples};
use crate::node::{Files, LocalFiles};
use crate::remote::{
    Handles, ImageReq, PathReq, SamplesReq, SamplesResp, WaitResult, WireEnvSpec, WireExec,
    WireRunSpec, WriteFilesReq,
};
use crate::workspace::Workspace;

pub struct AgentState {
    pub config: Config,
    pub executor: Arc<dyn Executor>,
    pub gpu: Arc<dyn GpuProvider>,
    pub files: LocalFiles,
    pub host_prev: Mutex<HostPrev>,
}

type S = Arc<AgentState>;

struct AgentError(anyhow::Error);
impl From<anyhow::Error> for AgentError {
    fn from(e: anyhow::Error) -> Self {
        AgentError(e)
    }
}
impl From<std::io::Error> for AgentError {
    fn from(e: std::io::Error) -> Self {
        AgentError(e.into())
    }
}
impl IntoResponse for AgentError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", self.0)).into_response()
    }
}
type R<T> = Result<Json<T>, AgentError>;

impl AgentState {
    /// Paths from the control node must stay inside this agent's scratch root.
    fn check_path(&self, p: &str) -> anyhow::Result<PathBuf> {
        let path = PathBuf::from(p);
        let root = self.config.scratch_root();
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            || !path.starts_with(&root)
        {
            anyhow::bail!("path {p} is outside the scratch root {}", root.display());
        }
        Ok(path)
    }
    /// Where images are staged for transfer.
    pub fn transfer_dir(&self) -> PathBuf {
        self.config.scratch_root().join(".transfer")
    }
}

async fn auth(State(st): State<S>, req: Request, next: Next) -> Result<Response, StatusCode> {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| !st.config.agent.node_token.is_empty() && t.trim() == st.config.agent.node_token)
        .unwrap_or(false);
    if ok {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

// ---- executor

async fn image_present(State(st): State<S>, Json(r): Json<ImageReq>) -> R<bool> {
    Ok(Json(st.executor.image_present(&r.image).await?))
}
async fn image_pull(State(st): State<S>, Json(r): Json<ImageReq>) -> R<()> {
    Ok(Json(st.executor.pull_image(&r.image).await?))
}
async fn images(State(st): State<S>) -> R<Vec<mlq_core::api::ImageInfo>> {
    Ok(Json(st.executor.list_images().await?))
}
async fn image_remove(State(st): State<S>, Json(r): Json<ImageReq>) -> R<()> {
    Ok(Json(st.executor.remove_image(&r.image).await?))
}
async fn image_save(State(st): State<S>, Json(r): Json<ImageReq>) -> R<()> {
    let p = st.check_path(&r.path)?;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    Ok(Json(st.executor.save_image(&r.image, &p).await?))
}
async fn image_load(State(st): State<S>, Json(r): Json<ImageReq>) -> R<Vec<String>> {
    let p = st.check_path(&r.path)?;
    Ok(Json(st.executor.load_image(&p).await?))
}
async fn image_tag(State(st): State<S>, Json(r): Json<ImageReq>) -> R<()> {
    Ok(Json(st.executor.tag_image(&r.image, &r.dst).await?))
}
async fn start(State(st): State<S>, Json(w): Json<WireRunSpec>) -> R<Handles> {
    let root = st.check_path(&w.workspace_root)?;
    let spec = RunSpec {
        job: w.job,
        workspace: Workspace::open(root),
        gpu_ids: w.gpu_ids,
        mounts: st.config.mounts.clone(),
        env: w.env,
        log_path: st.check_path(&w.log_path)?,
    };
    Ok(Json(Handles {
        handle: st.executor.start(&spec).await?,
    }))
}
async fn wait(State(st): State<S>, Json(h): Json<Handles>) -> R<WaitResult> {
    // Return within ~50 s so proxies and the client's retry loop stay happy.
    match tokio::time::timeout(Duration::from_secs(50), st.executor.wait(&h.handle)).await {
        Ok(Ok(code)) => Ok(Json(WaitResult {
            done: true,
            code: Some(code),
            error: None,
        })),
        Ok(Err(e)) => Ok(Json(WaitResult {
            done: true,
            code: None,
            error: Some(format!("{e:#}")),
        })),
        Err(_) => Ok(Json(WaitResult {
            done: false,
            code: None,
            error: None,
        })),
    }
}
async fn kill(State(st): State<S>, Json(h): Json<Handles>) -> R<()> {
    Ok(Json(st.executor.kill(&h.handle).await?))
}
async fn cleanup(State(st): State<S>, Json(h): Json<Handles>) -> R<()> {
    Ok(Json(st.executor.cleanup(&h.handle).await?))
}
async fn alive(State(st): State<S>, Json(h): Json<Handles>) -> R<bool> {
    Ok(Json(st.executor.is_alive(&h.handle).await?))
}
async fn start_env(State(st): State<S>, Json(w): Json<WireEnvSpec>) -> R<Handles> {
    let spec = EnvSpec {
        name: w.name,
        labels: w.labels,
        image: w.image,
        user_name: w.user_name,
        resources: w.resources,
        gpu_ids: w.gpu_ids,
        workspace: Workspace::open(st.check_path(&w.workspace_root)?),
        mounts: st.config.mounts.clone(),
        env: w.env,
    };
    Ok(Json(Handles {
        handle: st.executor.start_env(&spec).await?,
    }))
}
async fn exec(State(st): State<S>, Json(w): Json<WireExec>) -> R<Handles> {
    let ws = Workspace::open(st.check_path(&w.workspace_root)?);
    let log = st.check_path(&w.log_path)?;
    Ok(Json(Handles {
        handle: st.executor.exec(&w.env, &w.job, &ws, &w.vars, &log).await?,
    }))
}
async fn commit(State(st): State<S>, Json(r): Json<ImageReq>) -> R<Option<u64>> {
    Ok(Json(st.executor.commit(&r.path, &r.image).await?))
}
async fn env_idle(State(st): State<S>, Json(h): Json<Handles>) -> R<bool> {
    Ok(Json(st.executor.env_idle(&h.handle).await?))
}
async fn remove_env(State(st): State<S>, Json(h): Json<Handles>) -> R<()> {
    Ok(Json(st.executor.remove_env(&h.handle).await?))
}
async fn usage(State(st): State<S>, Json(h): Json<Handles>) -> R<Option<crate::remote::WireUsage>> {
    Ok(Json(st.executor.usage(&h.handle).await.map(Into::into)))
}

/// Host + GPU samples for one collection tick, plus usage counters of the given handles.
async fn samples(State(st): State<S>, Json(req): Json<SamplesReq>) -> R<SamplesResp> {
    let samples = {
        let mut prev = st.host_prev.lock().unwrap();
        host_samples(&mut prev, &st.config)
    };
    st.gpu.set_busy(&req.busy_gpus);
    let gpus = st.gpu.list();
    let mut usage = BTreeMap::new();
    for h in &req.handles {
        if let Some(u) = st.executor.usage(h).await {
            usage.insert(h.clone(), u.into());
        }
    }
    Ok(Json(SamplesResp {
        samples,
        gpus,
        usage,
    }))
}

// ---- files

async fn fs_ensure(State(st): State<S>, Json(r): Json<PathReq>) -> R<()> {
    Ok(Json(
        st.files.ensure_workspace(&st.check_path(&r.path)?).await?,
    ))
}
async fn fs_write_files(State(st): State<S>, Json(r): Json<WriteFilesReq>) -> R<()> {
    Ok(Json(
        st.files
            .write_files(&st.check_path(&r.root)?, &r.files)
            .await?,
    ))
}
#[derive(Deserialize)]
struct RootQ {
    root: Option<String>,
    path: Option<String>,
}
async fn fs_unpack(State(st): State<S>, Query(q): Query<RootQ>, body: Bytes) -> R<usize> {
    let root = st.check_path(q.root.as_deref().unwrap_or_default())?;
    Ok(Json(st.files.unpack_tar_gz(&root, body).await?))
}
async fn fs_log(State(st): State<S>, Json(r): Json<PathReq>) -> R<(String, u64)> {
    Ok(Json(
        st.files
            .read_log(&st.check_path(&r.path)?, r.tail, r.offset)
            .await?,
    ))
}
async fn fs_list_out(
    State(st): State<S>,
    Json(r): Json<PathReq>,
) -> R<Vec<mlq_core::api::ArtifactEntry>> {
    Ok(Json(st.files.list_out(&st.check_path(&r.path)?).await?))
}
async fn fs_file_get(State(st): State<S>, Query(q): Query<RootQ>) -> Result<Response, AgentError> {
    let p = st.check_path(q.path.as_deref().unwrap_or_default())?;
    let (len, stream) = st.files.open_file(&p).await?;
    Ok((
        [(header::CONTENT_LENGTH, len.to_string())],
        Body::from_stream(stream),
    )
        .into_response())
}
async fn fs_file_put(State(st): State<S>, Query(q): Query<RootQ>, body: Body) -> R<()> {
    let p = st.check_path(q.path.as_deref().unwrap_or_default())?;
    if let Some(d) = p.parent() {
        tokio::fs::create_dir_all(d).await?;
    }
    let mut f = tokio::fs::File::create(&p).await?;
    let mut stream = body.into_data_stream();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow::anyhow!("upload: {e}"))?;
        tokio::io::AsyncWriteExt::write_all(&mut f, &chunk).await?;
    }
    tokio::io::AsyncWriteExt::flush(&mut f).await?;
    Ok(Json(()))
}
/// Stream `root/<subdir>...` as an uncompressed tar (the network is the bottleneck, not CPU).
async fn fs_pack(State(st): State<S>, Json(r): Json<PathReq>) -> Result<Response, AgentError> {
    let root = st.check_path(&r.path)?;
    let subdirs = r.subdirs.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(8);
    tokio::task::spawn_blocking(move || {
        struct ChanWriter(tokio::sync::mpsc::Sender<std::io::Result<Bytes>>);
        impl std::io::Write for ChanWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .blocking_send(Ok(Bytes::copy_from_slice(buf)))
                    .map_err(|_| std::io::Error::other("receiver gone"))?;
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut ar = tar::Builder::new(ChanWriter(tx.clone()));
        for sub in subdirs {
            let dir = root.join(&sub);
            if dir.is_dir() {
                if let Err(e) = ar.append_dir_all(&sub, &dir) {
                    let _ = tx.blocking_send(Err(std::io::Error::other(e)));
                    return;
                }
            }
        }
        let _ = ar.finish();
    });
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Ok(Body::from_stream(stream).into_response())
}
async fn fs_unpack_at(State(st): State<S>, Json(r): Json<PathReq>) -> R<()> {
    let tar = st.check_path(&r.path)?;
    let root = st.check_path(r.subdirs.first().map(String::as_str).unwrap_or_default())?;
    Ok(Json(st.files.unpack_tar_at(&tar, &root).await?))
}
async fn fs_remove_dir(State(st): State<S>, Json(r): Json<PathReq>) -> R<()> {
    Ok(Json(st.files.remove_dir(&st.check_path(&r.path)?).await?))
}
async fn fs_remove_file(State(st): State<S>, Json(r): Json<PathReq>) -> R<()> {
    Ok(Json(st.files.remove_file(&st.check_path(&r.path)?).await?))
}
async fn fs_size(State(st): State<S>, Json(r): Json<PathReq>) -> R<u64> {
    Ok(Json(st.files.dir_size(&st.check_path(&r.path)?).await?))
}
async fn fs_exists(State(st): State<S>, Json(r): Json<PathReq>) -> R<bool> {
    Ok(Json(st.files.exists(&st.check_path(&r.path)?).await?))
}

pub fn router(st: S) -> Router {
    Router::new()
        .route("/image/present", post(image_present))
        .route("/image/pull", post(image_pull))
        .route("/images", get(images))
        .route("/image/remove", post(image_remove))
        .route("/image/save", post(image_save))
        .route("/image/load", post(image_load))
        .route("/image/tag", post(image_tag))
        .route("/start", post(start))
        .route("/wait", post(wait))
        .route("/kill", post(kill))
        .route("/cleanup", post(cleanup))
        .route("/alive", post(alive))
        .route("/start_env", post(start_env))
        .route("/exec", post(exec))
        .route("/commit", post(commit))
        .route("/env_idle", post(env_idle))
        .route("/remove_env", post(remove_env))
        .route("/usage", post(usage))
        .route("/samples", post(samples))
        .route("/fs/ensure", post(fs_ensure))
        .route("/fs/write_files", post(fs_write_files))
        .route("/fs/unpack", put(fs_unpack))
        .route("/fs/log", post(fs_log))
        .route("/fs/list_out", post(fs_list_out))
        .route("/fs/file", get(fs_file_get).put(fs_file_put))
        .route("/fs/pack", post(fs_pack))
        .route("/fs/unpack_at", post(fs_unpack_at))
        .route("/fs/remove_dir", post(fs_remove_dir))
        .route("/fs/remove_file", post(fs_remove_file))
        .route("/fs/size", post(fs_size))
        .route("/fs/exists", post(fs_exists))
        .layer(axum::middleware::from_fn_with_state(st.clone(), auth))
        .layer(axum::extract::DefaultBodyLimit::max(
            64 * 1024 * 1024 * 1024,
        ))
        .with_state(st)
}

fn first_non_loopback_ip() -> Option<String> {
    // Prefer a tailscale address (100.64.0.0/10) when present.
    let text = std::fs::read_to_string("/proc/net/fib_trie").ok()?;
    let mut best: Option<String> = None;
    for line in text.lines() {
        let l = line.trim();
        if let Some(ip) = l.strip_prefix("|-- ").or_else(|| l.strip_prefix("+-- ")) {
            let ip = ip.split_whitespace().next().unwrap_or("");
            if ip.starts_with("127.") || ip.ends_with(".0") || ip.ends_with(".255") || ip.is_empty()
            {
                continue;
            }
            if ip.starts_with("100.") {
                return Some(ip.to_string());
            }
            if best.is_none() && !ip.starts_with("169.254") {
                best = Some(ip.to_string());
            }
        }
    }
    best
}

/// Run as an agent until shut down.
pub async fn run(config: Config) -> anyhow::Result<()> {
    anyhow::ensure!(
        !config.agent.control_url.is_empty(),
        "[agent] control_url is not set"
    );
    anyhow::ensure!(
        !config.agent.node_token.is_empty(),
        "[agent] node_token is not set"
    );
    std::fs::create_dir_all(config.scratch_root())?;
    let executor = crate::executor::from_config(&config)?;
    let gpu = crate::gpu::from_config(&config);
    let st = Arc::new(AgentState {
        config: config.clone(),
        executor,
        gpu,
        files: LocalFiles,
        host_prev: Mutex::new(HostPrev::default()),
    });
    std::fs::create_dir_all(st.transfer_dir())?;

    let listener = tokio::net::TcpListener::bind(&config.agent.listen)
        .await
        .with_context(|| format!("binding {}", config.agent.listen))?;
    let port = listener.local_addr()?.port();
    let url = config.agent.advertise_url.clone().unwrap_or_else(|| {
        format!(
            "http://{}:{port}",
            first_non_loopback_ip().unwrap_or_else(|| "127.0.0.1".into())
        )
    });
    info!(node = %config.cluster.node_name, control = %config.agent.control_url, %url, "agent starting");

    // Heartbeat / registration loop.
    {
        let st = st.clone();
        let url = url.clone();
        tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap();
            let mut every =
                tokio::time::interval(Duration::from_secs(st.config.agent.heartbeat_secs.max(2)));
            let mut was_ok = false;
            loop {
                every.tick().await;
                let reg = crate::node::registration(
                    &st.config,
                    st.executor.as_ref(),
                    st.gpu.as_ref(),
                    url.clone(),
                );
                let r = http
                    .post(format!(
                        "{}/api/v1/nodes/register",
                        st.config.agent.control_url.trim_end_matches('/')
                    ))
                    .bearer_auth(&st.config.agent.node_token)
                    .json(&reg)
                    .send()
                    .await;
                match r {
                    Ok(resp) if resp.status().is_success() => {
                        if !was_ok {
                            info!("registered with control node");
                        }
                        was_ok = true;
                    }
                    Ok(resp) => {
                        was_ok = false;
                        let status = resp.status();
                        warn!(%status, "control node rejected registration: {}", resp.text().await.unwrap_or_default());
                    }
                    Err(e) => {
                        was_ok = false;
                        warn!("control node unreachable: {e}");
                    }
                }
            }
        });
    }

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest("/agent/v1", router(st.clone()))
        .layer(tower_http::trace::TraceLayer::new_for_http());
    axum::serve(listener, app)
        .with_graceful_shutdown(crate::shutdown_signal())
        .await?;
    Ok(())
}

#[allow(dead_code)]
fn _p(_: &Path) {}
