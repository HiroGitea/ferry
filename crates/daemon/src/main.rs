use clap::Parser;

#[derive(Parser)]
#[command(name = "mlqd", version, about = "mlq scheduler daemon")]
struct Args {
    /// Path to mlq.toml (default: $MLQ_CONFIG, /etc/mlq/mlq.toml, or built-in defaults)
    #[arg(short, long, env = "MLQ_CONFIG")]
    config: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = mlq_daemon::config::Config::load(args.config.as_deref())?;
    mlq_daemon::run(config).await
}
