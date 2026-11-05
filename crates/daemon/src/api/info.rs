use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use mlq_core::api::{ClusterStatus, GpuInfo, ImageInfo, Me, PullImageRequest};
use mlq_core::{Job, JobState};
use serde::Deserialize;

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::node::Node;
use crate::state::AppState;

pub async fn me(State(state): State<Arc<AppState>>, user: AuthUser) -> ApiResult<Me> {
    let quota = state.store.quota(user.0.id).await?;
    Ok(Json(Me {
        user: user.0,
        quota,
    }))
}

pub fn gpu_infos_of(_state: &AppState, node: &Node, active: &[Job]) -> Vec<GpuInfo> {
    let managed = node.facts().inventory.gpus;
    node.gpu
        .list()
        .into_iter()
        .map(|g| GpuInfo {
            node_id: node.id.clone(),
            job_id: active
                .iter()
                .find(|j| {
                    j.state.is_active()
                        && j.node_id.as_deref() == Some(&node.id)
                        && j.gpu_ids.contains(&g.index)
                })
                .map(|j| j.id),
            managed: managed.contains(&g.index),
            index: g.index,
            name: g.name,
            uuid: g.uuid,
            mem_total: g.mem_total,
            mem_used: g.mem_used,
            util_percent: g.util_percent,
            temp_c: g.temp_c,
        })
        .collect()
}

pub async fn gpu_infos(state: &AppState) -> anyhow::Result<Vec<GpuInfo>> {
    let active = state.store.active_jobs().await?;
    Ok(state
        .nodes
        .all()
        .iter()
        .flat_map(|n| gpu_infos_of(state, n, &active))
        .collect())
}

pub async fn gpus(State(state): State<Arc<AppState>>, _user: AuthUser) -> ApiResult<Vec<GpuInfo>> {
    Ok(Json(gpu_infos(&state).await?))
}

pub async fn cluster_status(state: &AppState) -> anyhow::Result<ClusterStatus> {
    let active = state.store.active_jobs().await?;
    let mut nodes = Vec::new();
    for n in state.nodes.all() {
        nodes.push(super::nodes::node_status(state, &n).await?);
    }
    let primary =
        state
            .nodes
            .primary()
            .map(|n| n.info())
            .unwrap_or_else(|| mlq_core::api::NodeInfo {
                id: "none".into(),
                hostname: "control".into(),
                cpus_total: 0.0,
                mem_total: 0,
                cpus_managed: 0.0,
                mem_managed: 0,
                gpus_managed: vec![],
                runtime: "none".into(),
                default_image: state.config.default_image.clone(),
            });
    let single = nodes.len() == 1;
    let online: Vec<_> = nodes.iter().filter(|n| n.online).collect();
    Ok(ClusterStatus {
        node: primary,
        gpus: nodes.iter().flat_map(|n| n.gpus.clone()).collect(),
        queued_jobs: active
            .iter()
            .filter(|j| j.state == JobState::Queued)
            .count() as u32,
        running_jobs: active.iter().filter(|j| j.state.is_active()).count() as u32,
        free_cpus: online.iter().map(|n| n.free_cpus).sum(),
        free_mem: online.iter().map(|n| n.free_mem).sum(),
        free_gpus: if single {
            nodes[0].free_gpus.clone()
        } else {
            vec![]
        },
        free_gpu_count: online.iter().map(|n| n.free_gpus.len() as u32).sum(),
        managed_gpu_count: online
            .iter()
            .map(|n| n.info.gpus_managed.len() as u32)
            .sum(),
        nodes,
        version: crate::VERSION.into(),
    })
}

pub async fn status(
    State(state): State<Arc<AppState>>,
    _user: AuthUser,
) -> ApiResult<ClusterStatus> {
    Ok(Json(cluster_status(&state).await?))
}

#[derive(Deserialize)]
pub struct NodeQuery {
    pub node: Option<String>,
}

pub async fn images(
    State(state): State<Arc<AppState>>,
    _user: AuthUser,
    Query(q): Query<NodeQuery>,
) -> ApiResult<Vec<ImageInfo>> {
    let node = state.node(q.node.as_deref())?;
    Ok(Json(node.executor.list_images().await?))
}

pub async fn pull_image(
    State(state): State<Arc<AppState>>,
    _user: AuthUser,
    Query(q): Query<NodeQuery>,
    Json(req): Json<PullImageRequest>,
) -> ApiResult<serde_json::Value> {
    if !state.config.image_allowed(&req.reference) {
        return Err(ApiError::Forbidden(format!(
            "image '{}' is not in allowed_images",
            req.reference
        )));
    }
    // Pull on one node, or on every online node when none is given.
    let targets: Vec<Arc<Node>> = match &q.node {
        Some(n) => vec![state.node(Some(n))?],
        None => state
            .nodes
            .all()
            .into_iter()
            .filter(|n| n.is_online())
            .collect(),
    };
    let mut pulled = Vec::new();
    for n in targets {
        n.executor
            .pull_image(&req.reference)
            .await
            .map_err(|e| ApiError::Internal(anyhow::anyhow!("node {}: {e:#}", n.id)))?;
        pulled.push(n.id.clone());
    }
    Ok(Json(
        serde_json::json!({ "pulled": req.reference, "nodes": pulled }),
    ))
}
