mod config;
mod fmt;
mod submit;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use ferry_client::Client;
use ferry_core::api::{
    CreateInviteRequest, CreateTokenRequest, CreateUserRequest, ListJobsQuery, RegisterRequest,
    UsageQuery,
};
use ferry_core::{JobState, Quota, Role};

#[derive(Parser)]
#[command(name = "ferry", version, about = "Containerised GPU/CPU job queue for ML experiments", long_about = None)]
struct Cli {
    /// Daemon URL (overrides FERRY_URL and the config file)
    #[arg(long, global = true)]
    url: Option<String>,
    /// API token (overrides FERRY_TOKEN and the config file)
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
    /// Save the daemon URL and token to ~/.config/ferry/config.toml
    Login {
        #[arg(long, default_value = "http://127.0.0.1:7070")]
        url: String,
        /// Token (prompted if omitted)
        #[arg(long)]
        token: Option<String>,
    },
    /// Create an account from an invite link and log in with it
    Register {
        /// Invite link (http://server:7070/register?code=inv_...) or bare code (then pass --url)
        invite: String,
        /// User name (prompted if omitted)
        #[arg(long)]
        name: Option<String>,
    },
    /// Your notification channels (webhook, Telegram)
    Notify {
        #[command(subcommand)]
        cmd: Option<NotifyCmd>,
    },
    /// Invite links for new users (admin)
    Invite {
        #[command(subcommand)]
        cmd: InviteCmd,
    },
    /// Submit a job (`ferry submit [opts] -- cmd args...` or `ferry submit script.sh`)
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
    /// Run the scheduler daemon in the foreground (same as `ferryd`)
    Daemon {
        #[arg(short, long, env = "FERRY_CONFIG")]
        config: Option<PathBuf>,
    },
    /// Print an example ferry.toml
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
    /// Print metric time series as sparklines (e.g. `ferry metrics 'gpu.*' --since 1h`)
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
    /// Create a task; prints its id. Steps: `ferry run --task <id> -- cmd`
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
        /// Minimum memory per GPU, e.g. 40G
        #[arg(long = "gpu-mem")]
        gpu_mem: Option<String>,
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
        /// Minimum memory per GPU, e.g. 40G
        #[arg(long = "gpu-mem")]
        gpu_mem: Option<String>,
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
enum NotifyCmd {
    /// Show settings (default)
    Show,
    /// Change settings; only the flags you pass change
    Set {
        /// Webhook URL ("" to remove)
        #[arg(long)]
        webhook: Option<String>,
        /// Body format: json, slack, feishu, dingtalk, wecom
        #[arg(long)]
        format: Option<String>,
        /// Telegram chat id ("" to remove); message the server's bot, then get your id from @userinfobot
        #[arg(long)]
        telegram: Option<String>,
        /// Notify when jobs succeed
        #[arg(long)]
        on_success: Option<bool>,
        /// Notify when jobs fail or time out
        #[arg(long)]
        on_failure: Option<bool>,
        /// Also notify for every task step
        #[arg(long)]
        task_steps: Option<bool>,
    },
    /// Send a test message
    Test,
}

#[derive(Subcommand)]
enum InviteCmd {
    List,
    /// Create an invite link; prints it once
    Create {
        /// New users become admins
        #[arg(long)]
        admin: bool,
        /// Only allow this user name
        #[arg(long)]
        name: Option<String>,
        /// How many accounts the link can create
        #[arg(long, default_value_t = 1)]
        uses: u32,
        /// Lifetime, e.g. 7d, 12h
        #[arg(long, default_value = "7d")]
        expires: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Revoke an invite
    Rm {
        id: i64,
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
    /// Serve MCP over stdio (add with: claude mcp add ferry -- ferry mcp serve)
    Serve,
}

#[derive(Args)]
struct Noop {}

/// Split an invite link into (server URL, code); a bare code uses --url / FERRY_URL / config.
fn parse_invite(invite: &str, url: Option<&str>) -> anyhow::Result<(String, String)> {
    let invite = invite.trim();
    if let Some((base, query)) = invite.split_once("/register?") {
        let code = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("code="))
            .context("invite link has no code= parameter")?;
        return Ok((base.trim_end_matches('/').to_string(), code.to_string()));
    }
    let url = url
        .map(str::to_string)
        .or_else(|| std::env::var("FERRY_URL").ok())
        .or(config::load().url)
        .context("pass the full invite link, or --url with a bare code")?;
    Ok((url.trim_end_matches('/').to_string(), invite.to_string()))
}

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
        Cmd::Register { invite, name } => {
            let (url, code) = parse_invite(invite, cli.url.as_deref())?;
            let anon = Client::new(&url, "")?;
            let preview = anon
                .preview_invite(&code)
                .await
                .context("checking the invite")?;
            let name = match (name, &preview.name) {
                (Some(n), _) => n.clone(),
                (None, Some(fixed)) => fixed.clone(),
                (None, None) => {
                    let default = std::env::var("USER").unwrap_or_default();
                    dialoguer::Input::<String>::new()
                        .with_prompt("user name")
                        .default(default)
                        .interact_text()?
                }
            };
            let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
            let t = anon
                .register(&RegisterRequest {
                    code,
                    name,
                    token_name: Some(format!("cli@{}", host.trim())),
                })
                .await?;
            config::save(&config::CliConfig {
                url: Some(url.clone()),
                token: Some(t.token.clone()),
            })?;
            println!(
                "registered as {} ({}); logged in, saved to {}",
                t.user.name,
                t.user.role.as_str(),
                config::path().display()
            );
            println!(
                "token (shown once; also use it for the dashboard at {url} and for Claude):\n{}",
                t.token
            );
        }
        Cmd::Notify { cmd } => {
            let c = client(&cli)?;
            let info = match cmd.as_ref().unwrap_or(&NotifyCmd::Show) {
                NotifyCmd::Show => c.notify_settings().await?,
                NotifyCmd::Set {
                    webhook,
                    format,
                    telegram,
                    on_success,
                    on_failure,
                    task_steps,
                } => {
                    let mut s = c.notify_settings().await?.settings;
                    if let Some(w) = webhook {
                        s.webhook_url = Some(w.clone()).filter(|w| !w.is_empty());
                    }
                    if let Some(f) = format {
                        s.webhook_format = ferry_core::api::WebhookFormat::parse(f).context(
                            "format must be one of json, slack, feishu, dingtalk, wecom",
                        )?;
                    }
                    if let Some(t) = telegram {
                        s.telegram_chat_id = Some(t.clone()).filter(|t| !t.is_empty());
                    }
                    if let Some(v) = on_success {
                        s.on_success = *v;
                    }
                    if let Some(v) = on_failure {
                        s.on_failure = *v;
                    }
                    if let Some(v) = task_steps {
                        s.task_steps = *v;
                    }
                    c.set_notify_settings(&s).await?
                }
                NotifyCmd::Test => {
                    c.test_notify().await?;
                    println!("test notification sent");
                    return Ok(());
                }
            };
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                let s = &info.settings;
                println!(
                    "webhook:    {}",
                    s.webhook_url
                        .as_deref()
                        .map(|u| format!("{u} ({})", s.webhook_format.as_str()))
                        .unwrap_or_else(|| "-".into())
                );
                println!(
                    "telegram:   {}{}",
                    s.telegram_chat_id.as_deref().unwrap_or("-"),
                    if info.telegram_available {
                        ""
                    } else {
                        "  (no bot on this server)"
                    }
                );
                println!(
                    "notify on:  success={} failure={} task_steps={}",
                    s.on_success, s.on_failure, s.task_steps
                );
            }
        }
        Cmd::Invite { cmd } => {
            let c = client(&cli)?;
            match cmd {
                InviteCmd::List => {
                    let list = c.invites().await?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&list)?);
                    } else {
                        fmt::print_invites(&list);
                    }
                }
                InviteCmd::Create {
                    admin,
                    name,
                    uses,
                    expires,
                    note,
                } => {
                    let created = c
                        .create_invite(&CreateInviteRequest {
                            role: Some(if *admin { Role::Admin } else { Role::User }),
                            name: name.clone(),
                            note: note.clone(),
                            max_uses: Some(*uses),
                            expires_in_secs: Some(ferry_core::parse_duration_secs(expires)?),
                        })
                        .await?;
                    let link = format!("{}/register?code={}", c.server_url(), created.code);
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "invite": created.invite, "code": created.code, "link": link
                            }))?
                        );
                    } else {
                        let i = &created.invite;
                        println!(
                            "invite {} ({}, {} use{}, expires {}), shown once:\n\n  {link}\n",
                            i.id,
                            i.role.as_str(),
                            i.max_uses,
                            if i.max_uses == 1 { "" } else { "s" },
                            i.expires_at
                                .with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M"),
                        );
                        println!("open it in a browser, or run:  ferry register '{link}'");
                    }
                }
                InviteCmd::Rm { id } => {
                    c.delete_invite(*id).await?;
                    println!("revoked invite {id}");
                }
            }
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
                        "\nthey can log in with:\n  ferry login --url <daemon-url> --token {}",
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
            ferry_daemon::init_tracing("warn");
            let c = client(&cli)?;
            ferry_mcp::serve_stdio(c).await?;
        }
        Cmd::Daemon { config } => {
            let cfg = ferry_daemon::config::Config::load(config.as_deref())?;
            ferry_daemon::run(cfg).await?;
        }
        Cmd::ExampleConfig => {
            print!("{}", ferry_daemon::config::Config::example_toml());
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
                    Ok(ferry_core::parse_duration_secs(s)?)
                }
            };
            let detail = match cmd {
                TaskCmd::Create {
                    name,
                    image,
                    cpus,
                    mem,
                    gpus,
                    gpu_mem,
                    keep_warm,
                    node,
                } => {
                    let req = ferry_core::api::CreateTaskRequest {
                        name: name.clone(),
                        image: image.clone(),
                        resources: ferry_core::ResourceOverrides {
                            cpus: *cpus,
                            mem_bytes: mem.as_deref().map(ferry_core::parse_bytes).transpose()?,
                            gpus: *gpus,
                            gpu_mem_bytes: gpu_mem
                                .as_deref()
                                .map(ferry_core::parse_bytes)
                                .transpose()?,
                        },
                        keep_warm_secs: Some(secs(keep_warm)?),
                        node: node.clone(),
                    };
                    let d = c.create_task(&req).await?;
                    if !cli.json {
                        println!(
                            "task {} created; run steps with: ferry run --task {} -- <command>",
                            d.summary.task.id, d.summary.task.id
                        );
                    }
                    Some(d)
                }
                TaskCmd::List { all } => {
                    let t = c
                        .tasks(&ferry_core::api::ListTasksQuery {
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
                    gpu_mem,
                } => {
                    let req = ferry_core::api::UpdateTaskRequest {
                        name: name.clone(),
                        keep_warm_secs: keep_warm.as_deref().map(secs).transpose()?,
                        resources: ferry_core::ResourceOverrides {
                            cpus: *cpus,
                            mem_bytes: mem.as_deref().map(ferry_core::parse_bytes).transpose()?,
                            gpus: *gpus,
                            gpu_mem_bytes: gpu_mem
                                .as_deref()
                                .map(ferry_core::parse_bytes)
                                .transpose()?,
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
                            &ferry_core::api::ExportTaskRequest {
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
                        .import_archive(&ferry_core::api::ImportArchiveRequest {
                            name: name.clone(),
                            task_name: name_as.clone(),
                            node: node.clone(),
                        })
                        .await?;
                    println!(
                        "imported as task {}; continue with: ferry run --task {} -- <command>",
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
                let secs = ferry_core::parse_duration_secs(since)? as i64;
                let r = c
                    .metrics(&ferry_core::api::MetricsQuery {
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
