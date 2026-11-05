use std::io::Write;

use ferry_client::Client;
use ferry_core::api::{ClusterStatus, GpuInfo, ImageInfo, JobResult, UserUsage};
use ferry_core::resource::{format_bytes, format_duration};
use ferry_core::{Job, User};
use tabled::settings::Style;
use tabled::{Table, Tabled};

#[derive(Tabled)]
struct JobRow {
    #[tabled(rename = "ID")]
    id: i64,
    #[tabled(rename = "NAME")]
    name: String,
    #[tabled(rename = "NODE")]
    node: String,
    #[tabled(rename = "USER")]
    user: String,
    #[tabled(rename = "STATE")]
    state: String,
    #[tabled(rename = "RES")]
    res: String,
    #[tabled(rename = "GPUS")]
    gpus: String,
    #[tabled(rename = "TIME")]
    time: String,
    #[tabled(rename = "REASON")]
    reason: String,
}

pub fn print_jobs(jobs: &[Job]) {
    if jobs.is_empty() {
        println!("no jobs");
        return;
    }
    let now = chrono::Utc::now();
    let rows: Vec<JobRow> = jobs
        .iter()
        .map(|j| JobRow {
            id: j.id,
            name: truncate(&j.name, 28),
            node: j
                .node_id
                .clone()
                .or_else(|| j.requested_node.clone().map(|n| format!("→{n}")))
                .unwrap_or_default(),
            user: j.user_name.clone(),
            state: j.state.to_string(),
            res: format!(
                "{}c/{}/{}g",
                j.resources.cpus,
                format_bytes(j.resources.mem_bytes),
                j.resources.gpus
            ),
            gpus: j
                .gpu_ids
                .iter()
                .map(|g| g.to_string())
                .collect::<Vec<_>>()
                .join(","),
            time: j.runtime_secs(now).map(format_duration).unwrap_or_default(),
            reason: truncate(j.reason.as_deref().unwrap_or(""), 40),
        })
        .collect();
    println!("{}", Table::new(rows).with(Style::blank()));
}

fn truncate(s: &str, n: usize) -> String {
    let s = &s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

pub fn print_status(s: &ClusterStatus) {
    if s.nodes.len() > 1 {
        println!(
            "ferryd {}  {} nodes ({} online)",
            s.version,
            s.nodes.len(),
            s.nodes.iter().filter(|n| n.online).count()
        );
        println!(
            "free      {:.1} cpus, {} memory, {} of {} gpus    jobs {} running, {} queued",
            s.free_cpus,
            format_bytes(s.free_mem),
            s.free_gpu_count,
            s.managed_gpu_count,
            s.running_jobs,
            s.queued_jobs
        );
        println!();
        print_nodes(&s.nodes);
        if !s.gpus.is_empty() {
            println!();
            print_gpus(&s.gpus);
        }
        return;
    }
    println!(
        "node      {} ({})  ferryd {}  runtime {}",
        s.node.hostname, s.node.id, s.version, s.node.runtime
    );
    println!(
        "cpus      {:.1} managed of {:.0} total, {:.1} free",
        s.node.cpus_managed, s.node.cpus_total, s.free_cpus
    );
    println!(
        "memory    {} managed of {} total, {} free",
        format_bytes(s.node.mem_managed),
        format_bytes(s.node.mem_total),
        format_bytes(s.free_mem)
    );
    println!(
        "gpus      {} managed {:?}, free {:?}",
        s.node.gpus_managed.len(),
        s.node.gpus_managed,
        s.free_gpus
    );
    println!(
        "jobs      {} running, {} queued",
        s.running_jobs, s.queued_jobs
    );
    println!("image     {}", s.node.default_image);
    if !s.gpus.is_empty() {
        println!();
        print_gpus(&s.gpus);
    }
}

#[derive(Tabled)]
struct NodeRow {
    #[tabled(rename = "NODE")]
    id: String,
    #[tabled(rename = "STATE")]
    state: String,
    #[tabled(rename = "HOST")]
    host: String,
    #[tabled(rename = "RUNTIME")]
    runtime: String,
    #[tabled(rename = "CPUS FREE")]
    cpus: String,
    #[tabled(rename = "MEM FREE")]
    mem: String,
    #[tabled(rename = "GPUS FREE")]
    gpus: String,
    #[tabled(rename = "JOBS")]
    jobs: u32,
    #[tabled(rename = "MOUNTS")]
    mounts: String,
}

pub fn print_nodes(nodes: &[ferry_core::api::NodeStatus]) {
    let rows: Vec<NodeRow> = nodes
        .iter()
        .map(|n| NodeRow {
            id: format!("{}{}", n.info.id, if n.local { " (this)" } else { "" }),
            state: if n.online {
                "online".into()
            } else {
                format!("offline since {}", n.last_seen.format("%H:%M"))
            },
            host: n.url.clone().unwrap_or_else(|| n.info.hostname.clone()),
            runtime: n.info.runtime.clone(),
            cpus: format!("{:.1}/{}", n.free_cpus, n.info.cpus_managed),
            mem: format!(
                "{}/{}",
                format_bytes(n.free_mem),
                format_bytes(n.info.mem_managed)
            ),
            gpus: format!(
                "{}/{} {:?}",
                n.free_gpus.len(),
                n.info.gpus_managed.len(),
                n.free_gpus
            ),
            jobs: n.running_jobs,
            mounts: n.mounts.join(","),
        })
        .collect();
    println!("{}", Table::new(rows).with(Style::blank()));
}

#[derive(Tabled)]
struct GpuRow {
    #[tabled(rename = "NODE")]
    node: String,
    #[tabled(rename = "GPU")]
    index: u32,
    #[tabled(rename = "NAME")]
    name: String,
    #[tabled(rename = "MEM")]
    mem: String,
    #[tabled(rename = "UTIL")]
    util: String,
    #[tabled(rename = "TEMP")]
    temp: String,
    #[tabled(rename = "MANAGED")]
    managed: String,
    #[tabled(rename = "JOB")]
    job: String,
}

pub fn print_gpus(gpus: &[GpuInfo]) {
    if gpus.is_empty() {
        println!("no GPUs detected");
        return;
    }
    let rows: Vec<GpuRow> = gpus
        .iter()
        .map(|g| GpuRow {
            node: g.node_id.clone(),
            index: g.index,
            name: g.name.clone(),
            mem: format!("{}/{}", format_bytes(g.mem_used), format_bytes(g.mem_total)),
            util: format!("{}%", g.util_percent),
            temp: g.temp_c.map(|t| format!("{t}C")).unwrap_or_default(),
            managed: if g.managed {
                "yes".into()
            } else {
                "reserved".into()
            },
            job: g.job_id.map(|j| j.to_string()).unwrap_or_default(),
        })
        .collect();
    println!("{}", Table::new(rows).with(Style::blank()));
}

#[derive(Tabled)]
struct ImageRow {
    #[tabled(rename = "IMAGE")]
    reference: String,
    #[tabled(rename = "ID")]
    id: String,
    #[tabled(rename = "SIZE")]
    size: String,
}

pub fn print_images(imgs: &[ImageInfo]) {
    let rows: Vec<ImageRow> = imgs
        .iter()
        .map(|i| ImageRow {
            reference: i.reference.clone(),
            id: i.id.chars().take(12).collect(),
            size: format_bytes(i.size),
        })
        .collect();
    println!("{}", Table::new(rows).with(Style::blank()));
}

#[derive(Tabled)]
struct UserRow {
    #[tabled(rename = "ID")]
    id: i64,
    #[tabled(rename = "NAME")]
    name: String,
    #[tabled(rename = "ROLE")]
    role: String,
    #[tabled(rename = "CREATED")]
    created: String,
}

pub fn print_users(users: &[User]) {
    let rows: Vec<UserRow> = users
        .iter()
        .map(|u| UserRow {
            id: u.id,
            name: u.name.clone(),
            role: u.role.as_str().into(),
            created: u.created_at.format("%Y-%m-%d").to_string(),
        })
        .collect();
    println!("{}", Table::new(rows).with(Style::blank()));
}

#[derive(Tabled)]
struct UsageRow {
    #[tabled(rename = "USER")]
    user: String,
    #[tabled(rename = "GPU-H")]
    gpu_h: String,
    #[tabled(rename = "CPU-H")]
    cpu_h: String,
    #[tabled(rename = "JOBS")]
    jobs: String,
    #[tabled(rename = "RUNNING")]
    running: String,
    #[tabled(rename = "QUEUED")]
    queued: u32,
    #[tabled(rename = "QUOTA(gpus/jobs/queued)")]
    quota: String,
    #[tabled(rename = "FAIRSHARE")]
    fair: String,
}

pub fn print_usage(u: &[UserUsage]) {
    let rows: Vec<UsageRow> = u
        .iter()
        .map(|x| UsageRow {
            user: x.user.name.clone(),
            gpu_h: format!("{:.1}", x.gpu_seconds / 3600.0),
            cpu_h: format!("{:.1}", x.cpu_seconds / 3600.0),
            jobs: format!(
                "{} ({} ok, {} failed)",
                x.jobs_total, x.jobs_succeeded, x.jobs_failed
            ),
            running: format!("{} jobs / {} gpus", x.running_jobs, x.running_gpus),
            queued: x.queued_jobs,
            quota: format!(
                "{}/{}/{}",
                x.quota
                    .max_running_gpus
                    .map(|v| v.to_string())
                    .unwrap_or("-".into()),
                x.quota
                    .max_running_jobs
                    .map(|v| v.to_string())
                    .unwrap_or("-".into()),
                x.quota
                    .max_queued_jobs
                    .map(|v| v.to_string())
                    .unwrap_or("-".into())
            ),
            fair: format!("{:.1}h", x.fair_share_usage / 3600.0),
        })
        .collect();
    println!("{}", Table::new(rows).with(Style::blank()));
}

pub fn print_result(r: &JobResult) {
    let j = &r.job;
    println!(
        "job {} ({})  state={}  exit={}  runtime={}  {}",
        j.id,
        j.name,
        j.state,
        j.exit_code.map(|c| c.to_string()).unwrap_or("-".into()),
        j.runtime_secs(chrono::Utc::now())
            .map(format_duration)
            .unwrap_or_default(),
        j.reason.clone().unwrap_or_default()
    );
    if let Some(a) = &j.archive_path {
        println!(
            "archived to {a}{}",
            if j.workspace_deleted {
                " (SSD workspace removed)"
            } else {
                ""
            }
        );
    } else if j.workspace_deleted {
        println!("warning: SSD workspace was removed and no archive exists");
    }
    if r.artifacts.is_empty() {
        println!("artifacts: none (write files to /out)");
    } else {
        println!("artifacts (/out):");
        for a in &r.artifacts {
            if !a.is_dir {
                println!("  {:>10}  {}", format_bytes(a.size), a.path);
            }
        }
    }
}

/// Stream the log until the job finishes; returns the final job.
pub async fn follow_log(c: &Client, id: i64) -> anyhow::Result<Job> {
    let mut offset = 0u64;
    let mut stdout = std::io::stdout();
    let mut last_state = None;
    loop {
        let chunk = c.log(id, None, Some(offset)).await?;
        if !chunk.content.is_empty() {
            stdout.write_all(chunk.content.as_bytes())?;
            stdout.flush()?;
            offset = chunk.next_offset;
        }
        if last_state != Some(chunk.state) {
            eprintln!("[job {id}: {}]", chunk.state);
            last_state = Some(chunk.state);
        }
        if chunk.state.is_terminal() {
            // Drain anything written after the last read.
            let chunk = c.log(id, None, Some(offset)).await?;
            stdout.write_all(chunk.content.as_bytes())?;
            stdout.flush()?;
            return Ok(c.job(id).await?);
        }
        tokio::time::sleep(std::time::Duration::from_millis(
            if chunk.state.is_active() { 500 } else { 1500 },
        ))
        .await;
    }
}

#[derive(Tabled)]
struct TaskRow {
    #[tabled(rename = "ID")]
    id: i64,
    #[tabled(rename = "NAME")]
    name: String,
    #[tabled(rename = "USER")]
    user: String,
    #[tabled(rename = "ACTIVITY")]
    activity: String,
    #[tabled(rename = "STEPS")]
    steps: u32,
    #[tabled(rename = "SNAPSHOTS")]
    snapshots: u32,
    #[tabled(rename = "DEFAULTS")]
    defaults: String,
    #[tabled(rename = "LAST STEP")]
    last: String,
}

pub fn print_tasks(tasks: &[ferry_core::TaskSummary]) {
    if tasks.is_empty() {
        println!("no tasks (create one with `ferry task create <name>`)");
        return;
    }
    let rows: Vec<TaskRow> = tasks
        .iter()
        .map(|s| TaskRow {
            id: s.task.id,
            name: truncate(&s.task.name, 28),
            user: s.task.user_name.clone(),
            activity: s.activity.clone(),
            steps: s.steps,
            snapshots: s.snapshots,
            defaults: format!(
                "{}c/{}/{}g",
                s.task.resources.cpus,
                format_bytes(s.task.resources.mem_bytes),
                s.task.resources.gpus
            ),
            last: s
                .last_step
                .as_ref()
                .map(|j| {
                    format!(
                        "#{} {} {}",
                        j.id,
                        j.state,
                        truncate(&j.command.join(" "), 30)
                    )
                })
                .unwrap_or_default(),
        })
        .collect();
    println!("{}", Table::new(rows).with(Style::blank()));
}

#[derive(Tabled)]
struct StepRow {
    #[tabled(rename = "STEP")]
    id: i64,
    #[tabled(rename = "STATE")]
    state: String,
    #[tabled(rename = "FROM")]
    from: String,
    #[tabled(rename = "SAVED")]
    saved: String,
    #[tabled(rename = "TIME")]
    time: String,
    #[tabled(rename = "COMMAND")]
    command: String,
    #[tabled(rename = "REASON")]
    reason: String,
}

#[derive(Tabled)]
struct SnapRow {
    #[tabled(rename = "SNAPSHOT")]
    id: String,
    #[tabled(rename = "PARENT")]
    parent: String,
    #[tabled(rename = "STEP")]
    step: String,
    #[tabled(rename = "SIZE")]
    size: String,
    #[tabled(rename = "IMAGE")]
    image: String,
    #[tabled(rename = "NOTE")]
    note: String,
}

pub fn print_task(d: &ferry_core::TaskDetail) {
    let t = &d.summary.task;
    println!(
        "task {} \"{}\"  user={}  state={}  activity={}",
        t.id,
        t.name,
        t.user_name,
        t.state.as_str(),
        d.summary.activity
    );
    println!("next step starts from: {}", d.head_image);
    println!(
        "defaults: {} cpus, {}, {} gpus   keep-warm: {}s{}",
        t.resources.cpus,
        format_bytes(t.resources.mem_bytes),
        t.resources.gpus,
        t.keep_warm_secs,
        match (&t.warm_container, t.warm_until) {
            (Some(_), Some(u)) => format!("  (warm until {})", u.format("%H:%M:%S")),
            _ => String::new(),
        }
    );
    if let Some(a) = &t.archive_path {
        println!("archive: {a}");
    }
    let now = chrono::Utc::now();
    if !d.steps.is_empty() {
        println!();
        let rows: Vec<StepRow> = d
            .steps
            .iter()
            .map(|j| StepRow {
                id: j.id,
                state: j.state.to_string(),
                from: j
                    .base_snapshot_id
                    .map(|s| format!("snap {s}"))
                    .unwrap_or_else(|| {
                        if j.started_at.is_some() {
                            "base".into()
                        } else {
                            String::new()
                        }
                    }),
                saved: j
                    .snapshot_id
                    .map(|s| format!("snap {s}"))
                    .unwrap_or_default(),
                time: j.runtime_secs(now).map(format_duration).unwrap_or_default(),
                command: truncate(&j.command.join(" "), 40),
                reason: truncate(j.reason.as_deref().unwrap_or(""), 50),
            })
            .collect();
        println!("{}", Table::new(rows).with(Style::blank()));
    }
    if !d.snapshots.is_empty() {
        println!();
        let rows: Vec<SnapRow> = d
            .snapshots
            .iter()
            .map(|s| SnapRow {
                id: format!(
                    "{}{}{}{}",
                    s.id,
                    if Some(s.id) == t.head_snapshot_id {
                        " (HEAD)"
                    } else {
                        ""
                    },
                    if s.pinned { " [pinned]" } else { "" },
                    if s.deleted { " [pruned]" } else { "" }
                ),
                parent: s
                    .parent_id
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "base".into()),
                step: s.job_id.map(|j| j.to_string()).unwrap_or_default(),
                size: s.size_bytes.map(format_bytes).unwrap_or_default(),
                image: s.image.clone(),
                note: s.note.clone().unwrap_or_default(),
            })
            .collect();
        println!("{}", Table::new(rows).with(Style::blank()));
    }
}

fn fmt_value(v: f32, unit: &str) -> String {
    match unit {
        "B" => format_bytes(v.max(0.0) as u64),
        "B/s" => format!("{}/s", format_bytes(v.max(0.0) as u64)),
        "%" => format!("{v:.1}%"),
        _ => format!("{v:.2} {unit}"),
    }
}

pub fn print_metrics(r: &ferry_core::api::MetricsResult) {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    println!(
        "{} points, {}s each (tier {})",
        r.series.first().map(|s| s.points.len()).unwrap_or(0),
        r.step,
        r.tier
    );
    let w = r.series.iter().map(|s| s.key.len()).max().unwrap_or(10);
    for s in &r.series {
        let vals: Vec<Option<f32>> = s.points.iter().map(|(_, v)| *v).collect();
        let present: Vec<f32> = vals.iter().flatten().copied().collect();
        if present.is_empty() {
            println!("{:<w$}  (no data)", s.key);
            continue;
        }
        let lo = present
            .iter()
            .copied()
            .fold(f32::INFINITY, f32::min)
            .min(0.0);
        let hi = present.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let spark: String = vals
            .iter()
            .map(|v| match v {
                None => ' ',
                Some(x) if hi > lo => {
                    BARS[(((x - lo) / (hi - lo)) * 7.0).round().clamp(0.0, 7.0) as usize]
                }
                Some(_) => BARS[0],
            })
            .collect();
        let avg = present.iter().sum::<f32>() / present.len() as f32;
        let max = s
            .max
            .iter()
            .flatten()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max);
        println!(
            "{:<w$}  {}  last {}  avg {}  max {}",
            s.key,
            spark,
            fmt_value(*present.last().unwrap(), &s.unit),
            fmt_value(avg, &s.unit),
            fmt_value(max, &s.unit)
        );
    }
}

pub fn print_export(r: &ferry_core::api::ExportTaskResponse) {
    let m = &r.manifest;
    println!(
        "archived task {} \"{}\" -> {}",
        m.task.id, m.task.name, r.path
    );
    println!(
        "  {} compressed from {} ({} steps, {} snapshots, {} image(s){})",
        format_bytes(r.bytes),
        format_bytes(m.total_bytes),
        m.steps.len(),
        m.snapshots.len(),
        m.images.len(),
        if r.offloaded {
            "; SSD workspace and images removed"
        } else {
            ""
        }
    );
    for i in &m.images {
        println!(
            "  image {:<28} {:>8}  {}{}",
            i.file,
            format_bytes(i.bytes),
            i.image,
            if i.is_head { " (HEAD)" } else { "" }
        );
    }
}

#[derive(Tabled)]
struct ArchiveRow {
    #[tabled(rename = "NAME")]
    name: String,
    #[tabled(rename = "USER")]
    user: String,
    #[tabled(rename = "TASK")]
    task: String,
    #[tabled(rename = "SIZE")]
    size: String,
    #[tabled(rename = "MODIFIED")]
    modified: String,
}

pub fn print_archives(a: &[ferry_core::api::ArchiveInfo]) {
    if a.is_empty() {
        println!("no archives (tasks are archived on close, or with `ferry task export <id>`)");
        return;
    }
    let rows: Vec<ArchiveRow> = a
        .iter()
        .map(|x| ArchiveRow {
            name: x.name.clone(),
            user: x.user.clone(),
            task: x.task_id.map(|t| t.to_string()).unwrap_or_default(),
            size: format_bytes(x.bytes),
            modified: x.modified_at.format("%Y-%m-%d %H:%M").to_string(),
        })
        .collect();
    println!("{}", Table::new(rows).with(Style::blank()));
}

pub fn print_manifest(m: &ferry_core::api::ArchiveManifest) {
    let t = &m.task;
    println!(
        "task {} \"{}\"  user={}  exported {} from {} (ferry {})",
        t.id,
        t.name,
        t.user_name,
        m.created_at.format("%Y-%m-%d %H:%M"),
        m.hostname,
        m.ferry_version
    );
    println!(
        "base image {}   HEAD {}   {} files, {}",
        t.base_image,
        t.head_snapshot_id
            .map(|h| format!("snap {h}"))
            .unwrap_or("base".into()),
        m.checksums.len(),
        format_bytes(m.total_bytes)
    );
    for i in &m.images {
        println!(
            "  image {:<28} {:>8}  {}{}",
            i.file,
            format_bytes(i.bytes),
            i.image,
            if i.is_head { " (HEAD)" } else { "" }
        );
    }
    println!();
    let now = chrono::Utc::now();
    let rows: Vec<StepRow> = m
        .steps
        .iter()
        .map(|j| StepRow {
            id: j.id,
            state: j.state.to_string(),
            from: j
                .base_snapshot_id
                .map(|s| format!("snap {s}"))
                .unwrap_or_else(|| "base".into()),
            saved: j
                .snapshot_id
                .map(|s| format!("snap {s}"))
                .unwrap_or_default(),
            time: j.runtime_secs(now).map(format_duration).unwrap_or_default(),
            command: truncate(&j.command.join(" "), 40),
            reason: truncate(j.reason.as_deref().unwrap_or(""), 40),
        })
        .collect();
    if !rows.is_empty() {
        println!("{}", Table::new(rows).with(Style::blank()));
    }
}

pub fn print_invites(list: &[ferry_core::api::InviteInfo]) {
    #[derive(Tabled)]
    struct Row {
        id: i64,
        role: &'static str,
        name: String,
        uses: String,
        expires: String,
        by: String,
        used_by: String,
        note: String,
    }
    let now = chrono::Utc::now();
    let rows: Vec<Row> = list
        .iter()
        .map(|i| Row {
            id: i.id,
            role: i.role.as_str(),
            name: i.name.clone().unwrap_or_else(|| "any".into()),
            uses: format!("{}/{}", i.uses, i.max_uses),
            expires: if i.expires_at <= now {
                "expired".into()
            } else {
                i.expires_at
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            },
            by: i.created_by.clone().unwrap_or_default(),
            used_by: i.used_by.join(","),
            note: i.note.clone().unwrap_or_default(),
        })
        .collect();
    if rows.is_empty() {
        println!("no invites");
    } else {
        println!("{}", Table::new(rows).with(Style::blank()));
    }
}
