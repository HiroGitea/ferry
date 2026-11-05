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
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl AppState {
    pub fn new(
        config: Config,
        store: Store,
        executor: Arc<dyn Executor>,
        gpu: Arc<dyn GpuProvider>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            store,
            events: EventBus::new(),
            executor,
            gpu,
            wake: Notify::new(),
            cancels: Mutex::new(HashMap::new()),
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

    pub fn remove_cancel(&self, id: JobId) {
        self.cancels.lock().unwrap().remove(&id);
    }
}
