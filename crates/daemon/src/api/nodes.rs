//! `/api/v1/nodes`: node listing, agent registration and liveness.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use chrono::Utc;
use ferry_core::api::{NodeRegistration, NodeStatus};
use tracing::{info, warn};

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::config::Config;
use crate::node::{Node, RemoteParts, facts_from_registration};
use crate::remote::{Agent, RemoteExecutor, RemoteFiles, RemoteGpu};
use crate::state::AppState;

pub fn node_from_registration(
    cfg: &Config,
    reg: &NodeRegistration,
    online: bool,
) -> anyhow::Result<Arc<Node>> {
    let agent = Agent::new(&reg.url, &cfg.cluster.node_token)?;
    let gpu = Arc::new(RemoteGpu::new(vec![]));
    Ok(Arc::new(Node {
        id: reg.id.clone(),
        local: false,
        url: Some(reg.url.clone()),
        remote: Some(RemoteParts {
            agent: agent.clone(),
            gpu: gpu.clone(),
        }),
        executor: Arc::new(RemoteExecutor {
            agent: agent.clone(),
            runtime: reg.runtime.clone(),
            snapshots: reg.supports_snapshots,
        }),
        gpu,
        files: Arc::new(RemoteFiles { agent }),
        facts: RwLock::new(facts_from_registration(reg)),
        last_seen: Mutex::new(Utc::now()),
        online: Mutex::new(online),
    }))
}

/// Agents authenticate with the cluster node token, not a user token.
pub async fn require_node_token(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    let token = &state.config.cluster.node_token;
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    if token.is_empty() {
        return Err(ApiError::Forbidden(
            "this daemon does not accept agents (cluster.node_token is empty)".into(),
        ));
    }
    if given != Some(token.as_str()) {
        return Err(ApiError::Unauthorized);
    }
    Ok(next.run(req).await)
}

pub async fn register(
    State(state): State<Arc<AppState>>,
    Json(reg): Json<NodeRegistration>,
) -> ApiResult<serde_json::Value> {
    let id = reg.id.trim();
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::BadRequest(
            "node id must be 1-64 chars of [A-Za-z0-9_-]".into(),
        ));
    }
    if let Some(n) = state.nodes.get(id) {
        if n.local {
            return Err(ApiError::Conflict(format!(
                "'{id}' is the control node's own name; pick another cluster.node_name"
            )));
        }
        // Known node: refresh facts (and the client if its URL changed).
        if n.url.as_deref() == Some(reg.url.as_str()) {
            *n.facts.write().unwrap() = facts_from_registration(&reg);
            let was_online = n.is_online();
            n.touch();
            if !was_online {
                info!(node = id, "node back online");
                state.wake.notify_one();
            }
            state.store.upsert_node(&reg).await?;
            return Ok(Json(serde_json::json!({ "ok": true })));
        }
    }
    let node = node_from_registration(&state.config, &reg, true)?;
    state.nodes.insert(node);
    state.store.upsert_node(&reg).await?;
    info!(node = id, url = %reg.url, gpus = ?reg.gpus_managed, "node registered");
    state.wake.notify_one();
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn node_status(state: &AppState, n: &Node) -> anyhow::Result<NodeStatus> {
    let active = state.store.active_jobs().await?;
    let f = n.facts();
    let mine: Vec<_> = active
        .iter()
        .filter(|j| j.state.is_active() && j.node_id.as_deref() == Some(&n.id))
        .collect();
    let mut free_cpus = f.inventory.cpus;
    let mut free_mem = f.inventory.mem_bytes;
    let mut free_gpus = f.inventory.gpus.clone();
    for j in &mine {
        free_cpus -= j.resources.cpus;
        free_mem = free_mem.saturating_sub(j.resources.mem_bytes);
        free_gpus.retain(|g| !j.gpu_ids.contains(g));
    }
    Ok(NodeStatus {
        info: n.info(),
        online: n.is_online(),
        local: n.local,
        url: n.url.clone(),
        last_seen: *n.last_seen.lock().unwrap(),
        gpus: super::info::gpu_infos_of(state, n, &active),
        running_jobs: mine.len() as u32,
        free_cpus: free_cpus.max(0.0),
        free_mem,
        free_gpus,
        mounts: f.mounts.iter().map(|m| m.container.clone()).collect(),
        supports_snapshots: f.supports_snapshots,
    })
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    _user: AuthUser,
) -> ApiResult<Vec<NodeStatus>> {
    let mut out = Vec::new();
    for n in state.nodes.all() {
        out.push(node_status(&state, &n).await?);
    }
    Ok(Json(out))
}

/// Forget an offline agent (admin). Jobs that ran on it keep their records.
pub async fn delete(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    user.require_admin()?;
    let Some(n) = state.nodes.get(&id) else {
        return Err(ApiError::NotFound(format!("node {id}")));
    };
    if n.local {
        return Err(ApiError::BadRequest("cannot remove the local node".into()));
    }
    if n.is_online() {
        return Err(ApiError::Conflict(
            "node is online; stop the agent first".into(),
        ));
    }
    state.nodes.remove(&id);
    state.store.delete_node(&id).await?;
    Ok(Json(serde_json::json!({ "removed": id })))
}

/// Mark agents offline when their heartbeat stops.
pub fn spawn_liveness(state: Arc<AppState>) {
    tokio::spawn(async move {
        let timeout =
            chrono::Duration::seconds(state.config.cluster.node_timeout_secs.max(5) as i64);
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            for n in state.nodes.all() {
                if n.local || !n.is_online() {
                    continue;
                }
                let last = *n.last_seen.lock().unwrap();
                if Utc::now() - last > timeout {
                    *n.online.lock().unwrap() = false;
                    warn!(node = %n.id, "node offline (no heartbeat for {}s)", timeout.num_seconds());
                }
            }
        }
    });
}

#[allow(dead_code)]
fn _unused(_: StatusCode) {}
