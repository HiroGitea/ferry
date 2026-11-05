//! mlqd: single-node container job scheduler daemon.

pub mod api;
pub mod archive;
pub mod auth;
pub mod config;
pub mod events;
pub mod executor;
pub mod gpu;
pub mod metrics;
pub mod retention;
pub mod sched;
pub mod state;
pub mod store;
pub mod tasks;
pub mod workspace;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

use std::sync::Arc;

use anyhow::Context;
use tracing::info;

use crate::config::Config;
use crate::state::AppState;

pub fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Build application state (DB, executor, GPU provider) without starting servers. Used by tests.
pub async fn build_state(config: Config) -> anyhow::Result<Arc<AppState>> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("creating {}", config.data_dir.display()))?;
    std::fs::create_dir_all(config.scratch_root())?;
    let store = store::Store::open(&config.db_path()).await?;
    store.bootstrap_admin(&config.data_dir).await?;
    let executor = executor::from_config(&config)?;
    let gpu = gpu::from_config(&config);
    let metrics = metrics::MetricsHub::open(&config).await?;
    Ok(AppState::new(config, store, executor, gpu, metrics))
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    init_tracing(&config.log_level);
    info!(version = VERSION, "starting mlqd");
    let state = build_state(config).await?;
    let listen = state.config.listen.clone();

    sched::spawn(state.clone());
    retention::spawn(state.clone());
    tasks::spawn_warm_reaper(state.clone());
    metrics::collector::spawn(state.clone());

    let app = api::router(state.clone());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    info!(%listen, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    info!("shutdown");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut s) = signal(SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
