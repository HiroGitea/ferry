use clap::Parser;

#[derive(Parser)]
#[command(name = "ferryd", version, about = "ferry scheduler daemon")]
struct Args {
    /// Path to ferry.toml (default: $FERRY_CONFIG, /etc/ferry/ferry.toml, or built-in defaults)
    #[arg(short, long, env = "FERRY_CONFIG")]
    config: Option<std::path::PathBuf>,
    /// Run as a compute agent for the control node in `[agent] control_url` instead of a daemon
    #[arg(long)]
    agent: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = ferry_daemon::config::Config::load(args.config.as_deref())?;
    if args.agent {
        ferry_daemon::init_tracing(&config.log_level);
        return ferry_daemon::agent::run(config).await;
    }
    ferry_daemon::run(config).await
}
