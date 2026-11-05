use clap::Parser;

#[derive(Parser)]
#[command(name = "mlqd", version, about = "mlq scheduler daemon")]
struct Args {
    /// Path to mlq.toml (default: $MLQ_CONFIG, /etc/mlq/mlq.toml, or built-in defaults)
    #[arg(short, long, env = "MLQ_CONFIG")]
    config: Option<std::path::PathBuf>,
    /// Run as a compute agent for the control node in `[agent] control_url` instead of a daemon
    #[arg(long)]
    agent: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = mlq_daemon::config::Config::load(args.config.as_deref())?;
    if args.agent {
        mlq_daemon::init_tracing(&config.log_level);
        return mlq_daemon::agent::run(config).await;
    }
    mlq_daemon::run(config).await
}
