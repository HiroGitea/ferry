use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ferry_core::{Job, JobId, Task};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::events::EventBus;
use crate::node::{Node, Nodes};
use crate::store::Store;

pub struct AppState {
    pub config: Config,
    pub store: Store,
    pub events: EventBus,
    /// All known nodes (local + registered agents).
    pub nodes: Nodes,
    /// Wake the scheduler loop.
    pub wake: Notify,
    /// Cancellation tokens of running jobs.
    pub cancels: Mutex<HashMap<JobId, CancellationToken>>,
    /// Jobs being cancelled by an admin preemption: they go back to the queue instead of ending.
    pub preempting: Mutex<std::collections::HashSet<JobId>>,
    /// One lock per task: serialises steps, checkout, close and warm-expiry of a task.
    pub task_locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    pub metrics: Arc<crate::metrics::MetricsHub>,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl AppState {
    pub fn new(
        config: Config,
        store: Store,
        nodes: Nodes,
        metrics: Arc<crate::metrics::MetricsHub>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            store,
            events: EventBus::new(),
            nodes,
            wake: Notify::new(),
            cancels: Mutex::new(HashMap::new()),
            preempting: Mutex::new(Default::default()),
            task_locks: Mutex::new(HashMap::new()),
            metrics,
            started_at: chrono::Utc::now(),
        })
    }

    /// Node by id; `None` or an unknown id falls back to the primary node (for records written
    /// before nodes existed).
    pub fn node(&self, id: Option<&str>) -> anyhow::Result<Arc<Node>> {
        match id {
            Some(i) => self
                .nodes
                .get(i)
                .or_else(|| self.nodes.primary())
                .ok_or_else(|| anyhow::anyhow!("node {i} not found")),
            None => self
                .nodes
                .primary()
                .ok_or_else(|| anyhow::anyhow!("no nodes registered")),
        }
    }

    pub fn node_for_job(&self, job: &Job) -> anyhow::Result<Arc<Node>> {
        self.node(job.node_id.as_deref())
    }

    pub fn node_for_task(&self, task: &Task) -> anyhow::Result<Arc<Node>> {
        self.node(task.node_id.as_deref())
    }

    pub fn cancel_token(&self, id: JobId) -> CancellationToken {
        let tok = CancellationToken::new();
        self.cancels.lock().unwrap().insert(id, tok.clone());
        tok
    }

    pub fn request_cancel(&self, id: JobId) -> bool {
        match self.cancels.lock().unwrap().get(&id) {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    pub fn task_lock(&self, task_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        self.task_locks
            .lock()
            .unwrap()
            .entry(task_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Kill a running job and put it back in the queue. Returns false if it has no watcher.
    pub fn request_preempt(&self, id: JobId) -> bool {
        self.preempting.lock().unwrap().insert(id);
        if self.request_cancel(id) {
            true
        } else {
            self.preempting.lock().unwrap().remove(&id);
            false
        }
    }
    pub fn take_preempt(&self, id: JobId) -> bool {
        self.preempting.lock().unwrap().remove(&id)
    }

    pub fn remove_cancel(&self, id: JobId) {
        self.cancels.lock().unwrap().remove(&id);
    }
}
