use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use mlq_core::JobState;
use mlq_core::api::{ClusterStatus, GpuInfo, ImageInfo, Me, NodeInfo, PullImageRequest};

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::sched::{NODE_ID, host_resources, inventory};
use crate::state::AppState;

pub async fn me(State(state): State<Arc<AppState>>, user: AuthUser) -> ApiResult<Me> {
    let quota = state.store.quota(user.0.id).await?;
    Ok(Json(Me {
        user: user.0,
        quota,
    }))
}

pub async fn gpu_infos(state: &AppState) -> anyhow::Result<Vec<GpuInfo>> {
    let inv = inventory(state);
    let active = state.store.active_jobs().await?;
    Ok(state
        .gpu
        .list()
        .into_iter()
        .map(|g| GpuInfo {
            job_id: active
                .iter()
                .find(|j| j.state.is_active() && j.gpu_ids.contains(&g.index))
                .map(|j| j.id),
            managed: inv.gpus.contains(&g.index),
            index: g.index,
            name: g.name,
            uuid: g.uuid,
            mem_total: g.mem_total,
            mem_used: g.mem_used,
            util_percent: g.util_percent,
            temp_c: g.temp_c,
        })
        .collect())
}

pub async fn gpus(State(state): State<Arc<AppState>>, _user: AuthUser) -> ApiResult<Vec<GpuInfo>> {
    Ok(Json(gpu_infos(&state).await?))
}

pub async fn cluster_status(state: &AppState) -> anyhow::Result<ClusterStatus> {
    let inv = inventory(state);
    let (host_cpus, host_mem) = host_resources();
    let active = state.store.active_jobs().await?;
    let mut free_cpus = inv.cpus;
    let mut free_mem = inv.mem_bytes;
    let mut free_gpus = inv.gpus.clone();
    for j in active.iter().filter(|j| j.state.is_active()) {
        free_cpus -= j.resources.cpus;
        free_mem = free_mem.saturating_sub(j.resources.mem_bytes);
        free_gpus.retain(|g| !j.gpu_ids.contains(g));
    }
    let hostname = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".into());
    Ok(ClusterStatus {
        node: NodeInfo {
            id: NODE_ID.into(),
            hostname,
            cpus_total: host_cpus,
            mem_total: host_mem,
            cpus_managed: inv.cpus,
            mem_managed: inv.mem_bytes,
            gpus_managed: inv.gpus.clone(),
            runtime: state.executor.name().to_string(),
            default_image: state.config.default_image.clone(),
        },
        gpus: gpu_infos(state).await?,
        queued_jobs: active
            .iter()
            .filter(|j| j.state == JobState::Queued)
            .count() as u32,
        running_jobs: active.iter().filter(|j| j.state.is_active()).count() as u32,
        free_cpus: free_cpus.max(0.0),
        free_mem,
        free_gpus,
        version: crate::VERSION.into(),
    })
}

pub async fn status(
    State(state): State<Arc<AppState>>,
    _user: AuthUser,
) -> ApiResult<ClusterStatus> {
    Ok(Json(cluster_status(&state).await?))
}

pub async fn images(
    State(state): State<Arc<AppState>>,
    _user: AuthUser,
) -> ApiResult<Vec<ImageInfo>> {
    Ok(Json(state.executor.list_images().await?))
}

pub async fn pull_image(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Json(req): Json<PullImageRequest>,
) -> ApiResult<serde_json::Value> {
    if !state.config.image_allowed(&req.reference) {
        return Err(ApiError::Forbidden(format!(
            "image '{}' is not in allowed_images",
            req.reference
        )));
    }
    if !user.is_admin() && !state.config.allowed_images.is_empty() {
        // Non-admins may only pull whitelisted images (already checked above).
    }
    state.executor.pull_image(&req.reference).await?;
    Ok(Json(serde_json::json!({ "pulled": req.reference })))
}
