use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mlq_core::JobId;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::events::EventBus;
use crate::executor::Executor;
use crate::gpu::GpuProvider;
use crate::store::Store;

pub struct AppState {
    pub config: Config,
    pub store: Store,
    pub events: EventBus,
    pub executor: Arc<dyn Executor>,
    pub gpu: Arc<dyn GpuProvider>,
    /// Wake the scheduler loop.
    pub wake: Notify,
    /// Cancellation tokens of running jobs.
    pub cancels: Mutex<HashMap<JobId, CancellationToken>>,
    /// One lock per task: serialises steps, checkout, close and warm-expiry of a task.
    pub task_locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    pub metrics: Arc<crate::metrics::MetricsHub>,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl AppState {
    pub fn new(
        config: Config,
        store: Store,
        executor: Arc<dyn Executor>,
        gpu: Arc<dyn GpuProvider>,
        metrics: Arc<crate::metrics::MetricsHub>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            store,
            events: EventBus::new(),
            executor,
            gpu,
            wake: Notify::new(),
            cancels: Mutex::new(HashMap::new()),
            task_locks: Mutex::new(HashMap::new()),
            metrics,
            started_at: chrono::Utc::now(),
        })
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

    pub fn remove_cancel(&self, id: JobId) {
        self.cancels.lock().unwrap().remove(&id);
    }
}
