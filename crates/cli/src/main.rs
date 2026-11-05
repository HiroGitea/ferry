mod config;
mod fmt;
mod submit;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use mlq_client::Client;
use mlq_core::api::{CreateTokenRequest, CreateUserRequest, ListJobsQuery, UsageQuery};
use mlq_core::{JobState, Quota, Role};

#[derive(Parser)]
#[command(name = "mlq", version, about = "Containerised GPU/CPU job queue for ML experiments", long_about = None)]
struct Cli {
    /// Daemon URL (overrides MLQ_URL and the config file)
    #[arg(long, global = true)]
    url: Option<String>,
    /// API token (overrides MLQ_TOKEN and the config file)
    #[arg(long, global = true, hide_env_values = true)]
    token: Option<String>,
    /// Print raw JSON instead of tables
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Save the daemon URL and token to ~/.config/mlq/config.toml
    Login {
        #[arg(long, default_value = "http://127.0.0.1:7070")]
        url: String,
        /// Token (prompted if omitted)
        #[arg(long)]
        token: Option<String>,
    },
    /// Submit a job (`mlq submit [opts] -- cmd args...` or `mlq submit script.sh`)
    Submit(submit::SubmitArgs),
    /// Submit, follow the log and wait for completion
    Run(submit::SubmitArgs),
    /// List jobs
    #[command(alias = "q", alias = "ls")]
    Queue {
        /// Include finished jobs
        #[arg(short, long)]
        all: bool,
        #[arg(short, long)]
        state: Option<String>,
        /// Filter by user name ("all" for everyone; admin only)
        #[arg(short, long)]
        user: Option<String>,
        #[arg(short = 'n', long, default_value = "100")]
        limit: u32,
    },
    /// Show one job
    Show { id: i64 },
    /// Cancel jobs
    Cancel { ids: Vec<i64> },
    /// Release a held job
    Release { id: i64 },
    /// Print (or follow) a job's log
    Logs {
        id: i64,
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'n', long, default_value = "200")]
        tail: usize,
    },
    /// Show a job's result and artifacts; optionally download /out
    Result {
        id: i64,
        /// Download all artifacts into this directory
        #[arg(long)]
        download: Option<PathBuf>,
    },
    /// Node, GPU and queue overview
    Info,
    /// Per-GPU live metrics
    Gpus,
    /// Container images
    Image {
        #[command(subcommand)]
        cmd: ImageCmd,
    },
    /// Manage users (admin)
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Show or set a user's quota
    Quota {
        user: String,
        #[arg(long)]
        gpus: Option<u32>,
        #[arg(long)]
        jobs: Option<u32>,
        #[arg(long)]
        queued: Option<u32>,
        /// Remove all limits
        #[arg(long)]
        clear: bool,
    },
    /// Usage statistics
    Usage {
        /// User name ("all" for everyone; admin only)
        #[arg(short, long)]
        user: Option<String>,
        #[arg(long, default_value = "30")]
        days: u32,
    },
    /// API tokens
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// Model Context Protocol server for Claude Code / Desktop
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },
    /// Run the scheduler daemon in the foreground (same as `mlqd`)
    Daemon {
        #[arg(short, long, env = "MLQ_CONFIG")]
        config: Option<PathBuf>,
    },
    /// Print an example mlq.toml
    ExampleConfig,
    /// Run the scratch retention sweep now (admin)
    Sweep,
    /// Show the authenticated user
    Whoami,
    /// Long-lived tasks: an environment that steps run in one after another, saved after each step
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Compute nodes (control node + registered agents)
    Node {
        #[command(subcommand)]
        cmd: NodeCmd,
    },
    /// Task archives (.7z) on the NAS: list, inspect, restore
    Archive {
        #[command(subcommand)]
        cmd: ArchiveCmd,
    },
    /// Print metric time series as sparklines (e.g. `mlq metrics 'gpu.*' --since 1h`)
    Metrics {
        /// Comma separated keys; `prefix*` patterns allowed. Use `--list` to see keys.
        #[arg(default_value = "host.*")]
        keys: String,
        #[arg(long, default_value = "10m")]
        since: String,
        #[arg(long, default_value = "60")]
        points: u32,
        /// List available keys
        #[arg(long)]
        list: bool,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Create a task; prints its id. Steps: `mlq run --task <id> -- cmd`
    Create {
        name: String,
        #[arg(short = 'i', long)]
        image: Option<String>,
        #[arg(short = 'c', long)]
        cpus: Option<f64>,
        #[arg(short = 'm', long)]
        mem: Option<String>,
        #[arg(short = 'g', long)]
        gpus: Option<u32>,
        /// Keep the environment running after a successful step, e.g. 10m (0 = off)
        #[arg(long, default_value = "10m")]
        keep_warm: String,
        /// Pin the task to a node (default: the node of its first step)
        #[arg(short = 'N', long)]
        node: Option<String>,
    },
    #[command(alias = "ls")]
    List {
        #[arg(short, long)]
        all: bool,
    },
    /// Steps and snapshots of a task
    Show {
        id: i64,
    },
    /// Continue from an earlier snapshot (or `--base` for the base image)
    Checkout {
        id: i64,
        snapshot: Option<i64>,
        #[arg(long, conflicts_with = "snapshot")]
        base: bool,
    },
    /// Pin a snapshot so it is never pruned
    Pin {
        id: i64,
        snapshot: i64,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        unpin: bool,
    },
    /// Change name, keep-warm or default resources
    Set {
        id: i64,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        keep_warm: Option<String>,
        #[arg(short = 'c', long)]
        cpus: Option<f64>,
        #[arg(short = 'm', long)]
        mem: Option<String>,
        #[arg(short = 'g', long)]
        gpus: Option<u32>,
    },
    Close {
        id: i64,
    },
    Reopen {
        id: i64,
    },
    /// Delete a closed task's snapshots and workspace (archive is kept)
    Rm {
        id: i64,
    },
    /// Write a self-contained .7z (environment image, /work, /out, logs, metrics) to the NAS
    Export {
        id: i64,
        /// Snapshot images to include: none, head, pinned, all
        #[arg(long)]
        images: Option<String>,
        /// After a verified export, free the SSD workspace and snapshot images (closed tasks only)
        #[arg(long)]
        offload: bool,
    },
}

#[derive(Subcommand)]
enum NodeCmd {
    #[command(alias = "ls")]
    List,
    /// Forget an offline agent (admin)
    Rm { id: String },
}

#[derive(Subcommand)]
enum ArchiveCmd {
    /// Archives on the NAS
    #[command(alias = "ls")]
    List,
    /// Show an archive's manifest (steps, snapshots, included images)
    Show { name: String },
    /// Restore an archive as a new task
    Import {
        name: String,
        #[arg(long)]
        name_as: Option<String>,
        /// Node to restore onto (needs a container runtime)
        #[arg(short = 'N', long)]
        node: Option<String>,
    },
}

#[derive(Subcommand)]
enum ImageCmd {
    List,
    Pull { reference: String },
}

#[derive(Subcommand)]
enum UserCmd {
    List,
    /// Create a user and print their first token
    Add {
        name: String,
        #[arg(long)]
        admin: bool,
    },
    Rm {
        name: String,
    },
}

#[derive(Subcommand)]
enum TokenCmd {
    List,
    Create {
        #[arg(long)]
        name: Option<String>,
        /// Create for another user (admin)
        #[arg(long)]
        user: Option<String>,
    },
    Rm {
        id: i64,
    },
}

#[derive(Subcommand)]
enum McpCmd {
    /// Serve MCP over stdio (add with: claude mcp add mlq -- mlq mcp serve)
    Serve,
}

#[derive(Args)]
struct Noop {}

fn client(cli: &Cli) -> anyhow::Result<Client> {
    let (url, token) = config::resolve(cli.url.clone(), cli.token.clone())?;
    Client::new(&url, &token)
}

#[tokio::main]
async fn main() {
    // Behave like a normal Unix tool when piped into `head` etc.: exit quietly on EPIPE.
    // SAFETY: restoring the default SIGPIPE disposition before any I/O happens.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    if let Err(e) = real_main().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Login { url, token } => {
            let token = match token {
                Some(t) => t.clone(),
                None => dialoguer::Password::new()
                    .with_prompt("API token")
                    .interact()?,
            };
            let c = Client::new(url, &token)?;
            let me = c.me().await.context("token rejected")?;
            config::save(&config::CliConfig {
                url: Some(url.clone()),
                token: Some(token),
            })?;
            println!(
                "logged in as {} ({:?}); saved to {}",
                me.user.name,
                me.user.role,
                config::path().display()
            );
        }
        Cmd::Submit(args) => {
            let c = client(&cli)?;
            let jobs = submit::submit(&c, args, false).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&jobs)?);
            }
        }
        Cmd::Run(args) => {
            let c = client(&cli)?;
            let jobs = submit::submit(&c, args, true).await?;
            let job = jobs.first().context("no job")?;
            let final_job = fmt::follow_log(&c, job.id).await?;
            let r = c.result(final_job.id).await?;
            fmt::print_result(&r);
            if !matches!(final_job.state, JobState::Succeeded) {
                std::process::exit(final_job.exit_code.unwrap_or(1));
            }
        }
        Cmd::Queue {
            all,
            state,
            user,
            limit,
        } => {
            let c = client(&cli)?;
            let state = match state {
                Some(s) => {
                    Some(JobState::parse(s).with_context(|| format!("unknown state '{s}'"))?)
                }
                None => None,
            };
            let jobs = c
                .list_jobs(&ListJobsQuery {
                    state,
                    user: user.clone(),
                    all: *all,
                    limit: Some(*limit),
                })
                .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&jobs)?);
            } else {
                fmt::print_jobs(&jobs);
            }
        }
        Cmd::Show { id } => {
            let c = client(&cli)?;
            let j = c.job(*id).await?;
            println!("{}", serde_json::to_string_pretty(&j)?);
        }
        Cmd::Cancel { ids } => {
            let c = client(&cli)?;
            for id in ids {
                match c.cancel(*id).await {
                    Ok(j) => println!("job {} -> {}", j.id, j.state),
                    Err(e) => eprintln!("job {id}: {e}"),
                }
            }
        }
        Cmd::Release { id } => {
            let c = client(&cli)?;
            let j = c.release(*id).await?;
            println!("job {} -> {}", j.id, j.state);
        }
        Cmd::Logs { id, follow, tail } => {
            let c = client(&cli)?;
            if *follow {
                fmt::follow_log(&c, *id).await?;
            } else {
                let l = c.log(*id, Some(*tail), None).await?;
                print!("{}", l.content);
            }
        }
        Cmd::Result { id, download } => {
            let c = client(&cli)?;
            let r = c.result(*id).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                fmt::print_result(&r);
            }
            if let Some(dir) = download {
                for a in r.artifacts.iter().filter(|a| !a.is_dir) {
                    let target = dir.join(&a.path);
                    if let Some(p) = target.parent() {
                        std::fs::create_dir_all(p)?;
                    }
                    let bytes = c.artifact_bytes(*id, &a.path, None).await?;
                    std::fs::write(&target, &bytes)?;
                    println!("downloaded {} ({} bytes)", target.display(), bytes.len());
                }
            }
        }
        Cmd::Info => {
            let c = client(&cli)?;
            let s = c.status().await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                fmt::print_status(&s);
            }
        }
        Cmd::Gpus => {
            let c = client(&cli)?;
            let g = c.gpus().await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&g)?);
            } else {
                fmt::print_gpus(&g);
            }
        }
        Cmd::Image { cmd } => {
            let c = client(&cli)?;
            match cmd {
                ImageCmd::List => {
                    let imgs = c.images().await?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&imgs)?);
                    } else {
                        fmt::print_images(&imgs);
                    }
                }
                ImageCmd::Pull { reference } => {
                    println!("pulling {reference} ...");
                    c.pull_image(reference).await?;
                    println!("done");
                }
            }
        }
        Cmd::User { cmd } => {
            let c = client(&cli)?;
            match cmd {
                UserCmd::List => {
                    let users = c.users().await?;
                    fmt::print_users(&users);
                }
                UserCmd::Add { name, admin } => {
                    let t = c
                        .create_user(&CreateUserRequest {
                            name: name.clone(),
                            role: if *admin { Role::Admin } else { Role::User },
                        })
                        .await?;
                    println!(
                        "user {} created (id {})\ntoken (shown once): {}",
                        t.user.name, t.user.id, t.token
                    );
                    println!(
                        "\nthey can log in with:\n  mlq login --url <daemon-url> --token {}",
                        t.token
                    );
                }
                UserCmd::Rm { name } => {
                    c.delete_user(name).await?;
                    println!("deleted {name}");
                }
            }
        }
        Cmd::Quota {
            user,
            gpus,
            jobs,
            queued,
            clear,
        } => {
            let c = client(&cli)?;
            if *clear || gpus.is_some() || jobs.is_some() || queued.is_some() {
                let mut q = if *clear {
                    Quota::default()
                } else {
                    c.quota(user).await?
                };
                if gpus.is_some() {
                    q.max_running_gpus = *gpus;
                }
                if jobs.is_some() {
                    q.max_running_jobs = *jobs;
                }
                if queued.is_some() {
                    q.max_queued_jobs = *queued;
                }
                let q = c.set_quota(user, &q).await?;
                println!("{}", serde_json::to_string_pretty(&q)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&c.quota(user).await?)?);
            }
        }
        Cmd::Usage { user, days } => {
            let c = client(&cli)?;
            let from = chrono::Utc::now() - chrono::Duration::days(*days as i64);
            let u = c
                .usage(&UsageQuery {
                    user: user.clone(),
                    from: Some(from),
                    to: None,
                })
                .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&u)?);
            } else {
                fmt::print_usage(&u);
            }
        }
        Cmd::Token { cmd } => {
            let c = client(&cli)?;
            match cmd {
                TokenCmd::List => {
                    let t = c.tokens().await?;
                    println!("{}", serde_json::to_string_pretty(&t)?);
                }
                TokenCmd::Create { name, user } => {
                    let t = c
                        .create_token(&CreateTokenRequest {
                            name: name.clone(),
                            user: user.clone(),
                        })
                        .await?;
                    println!(
                        "token for {} (id {}), shown once:\n{}",
                        t.user.name, t.id, t.token
                    );
                }
                TokenCmd::Rm { id } => {
                    c.delete_token(*id).await?;
                    println!("deleted token {id}");
                }
            }
        }
        Cmd::Mcp { cmd: McpCmd::Serve } => {
            // stdout is the protocol channel: log to stderr only.
            mlq_daemon::init_tracing("warn");
            let c = client(&cli)?;
            mlq_mcp::serve_stdio(c).await?;
        }
        Cmd::Daemon { config } => {
            let cfg = mlq_daemon::config::Config::load(config.as_deref())?;
            mlq_daemon::run(cfg).await?;
        }
        Cmd::ExampleConfig => {
            print!("{}", mlq_daemon::config::Config::example_toml());
        }
        Cmd::Sweep => {
            let c = client(&cli)?;
            println!("{}", serde_json::to_string_pretty(&c.sweep().await?)?);
        }
        Cmd::Task { cmd } => {
            let c = client(&cli)?;
            let secs = |s: &str| -> anyhow::Result<u64> {
                if s == "0" {
                    Ok(0)
                } else {
                    Ok(mlq_core::parse_duration_secs(s)?)
                }
            };
            let detail = match cmd {
                TaskCmd::Create {
                    name,
                    image,
                    cpus,
                    mem,
                    gpus,
                    keep_warm,
                    node,
                } => {
                    let req = mlq_core::api::CreateTaskRequest {
                        name: name.clone(),
                        image: image.clone(),
                        resources: mlq_core::ResourceOverrides {
                            cpus: *cpus,
                            mem_bytes: mem.as_deref().map(mlq_core::parse_bytes).transpose()?,
                            gpus: *gpus,
                        },
                        keep_warm_secs: Some(secs(keep_warm)?),
                        node: node.clone(),
                    };
                    let d = c.create_task(&req).await?;
                    if !cli.json {
                        println!(
                            "task {} created; run steps with: mlq run --task {} -- <command>",
                            d.summary.task.id, d.summary.task.id
                        );
                    }
                    Some(d)
                }
                TaskCmd::List { all } => {
                    let t = c
                        .tasks(&mlq_core::api::ListTasksQuery {
                            all: *all,
                            user: None,
                        })
                        .await?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&t)?);
                    } else {
                        fmt::print_tasks(&t);
                    }
                    None
                }
                TaskCmd::Show { id } => Some(c.task(*id).await?),
                TaskCmd::Checkout { id, snapshot, base } => {
                    let sid = if *base {
                        None
                    } else {
                        Some(snapshot.context("give a snapshot id or --base")?)
                    };
                    Some(c.checkout(*id, sid).await?)
                }
                TaskCmd::Pin {
                    id,
                    snapshot,
                    note,
                    unpin,
                } => Some(c.pin_snapshot(*id, *snapshot, !unpin, note.clone()).await?),
                TaskCmd::Set {
                    id,
                    name,
                    keep_warm,
                    cpus,
                    mem,
                    gpus,
                } => {
                    let req = mlq_core::api::UpdateTaskRequest {
                        name: name.clone(),
                        keep_warm_secs: keep_warm.as_deref().map(secs).transpose()?,
                        resources: mlq_core::ResourceOverrides {
                            cpus: *cpus,
                            mem_bytes: mem.as_deref().map(mlq_core::parse_bytes).transpose()?,
                            gpus: *gpus,
                        },
                    };
                    Some(c.update_task(*id, &req).await?)
                }
                TaskCmd::Close { id } => Some(c.close_task(*id).await?),
                TaskCmd::Reopen { id } => Some(c.reopen_task(*id).await?),
                TaskCmd::Rm { id } => {
                    c.delete_task(*id).await?;
                    println!("task {id} deleted");
                    None
                }
                TaskCmd::Export {
                    id,
                    images,
                    offload,
                } => {
                    let r = c
                        .export_task(
                            *id,
                            &mlq_core::api::ExportTaskRequest {
                                images: images.clone(),
                                offload: *offload,
                            },
                        )
                        .await?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&r)?);
                    } else {
                        fmt::print_export(&r);
                    }
                    None
                }
            };
            if let Some(d) = detail {
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&d)?);
                } else {
                    fmt::print_task(&d);
                }
            }
        }
        Cmd::Node { cmd } => {
            let c = client(&cli)?;
            match cmd {
                NodeCmd::List => {
                    let n = c.nodes().await?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&n)?);
                    } else {
                        fmt::print_nodes(&n);
                    }
                }
                NodeCmd::Rm { id } => {
                    c.delete_node(id).await?;
                    println!("node {id} removed");
                }
            }
        }
        Cmd::Archive { cmd } => {
            let c = client(&cli)?;
            match cmd {
                ArchiveCmd::List => {
                    let a = c.archives().await?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&a)?);
                    } else {
                        fmt::print_archives(&a);
                    }
                }
                ArchiveCmd::Show { name } => {
                    let m = c.archive_manifest(name).await?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&m)?);
                    } else {
                        fmt::print_manifest(&m);
                    }
                }
                ArchiveCmd::Import {
                    name,
                    name_as,
                    node,
                } => {
                    let d = c
                        .import_archive(&mlq_core::api::ImportArchiveRequest {
                            name: name.clone(),
                            task_name: name_as.clone(),
                            node: node.clone(),
                        })
                        .await?;
                    println!(
                        "imported as task {}; continue with: mlq run --task {} -- <command>",
                        d.summary.task.id, d.summary.task.id
                    );
                    fmt::print_task(&d);
                }
            }
        }
        Cmd::Metrics {
            keys,
            since,
            points,
            list,
        } => {
            let c = client(&cli)?;
            if *list {
                for k in c.metric_keys(None).await? {
                    println!("{:<32} {}", k.key, k.unit);
                }
            } else {
                let secs = mlq_core::parse_duration_secs(since)? as i64;
                let r = c
                    .metrics(&mlq_core::api::MetricsQuery {
                        keys: keys.clone(),
                        after: Some(-secs),
                        before: None,
                        points: Some(*points),
                    })
                    .await?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&r)?);
                } else {
                    fmt::print_metrics(&r);
                }
            }
        }
        Cmd::Whoami => {
            let c = client(&cli)?;
            let me = c.me().await?;
            println!(
                "{} ({:?})  quota: {}",
                me.user.name,
                me.user.role,
                serde_json::to_string(&me.quota)?
            );
        }
    }
    Ok(())
}
