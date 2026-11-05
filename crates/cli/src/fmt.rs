use std::io::Write;

use mlq_client::Client;
use mlq_core::api::{ClusterStatus, GpuInfo, ImageInfo, JobResult, UserUsage};
use mlq_core::resource::{format_bytes, format_duration};
use mlq_core::{Job, User};
use tabled::settings::Style;
use tabled::{Table, Tabled};

#[derive(Tabled)]
struct JobRow {
    #[tabled(rename = "ID")]
    id: i64,
    #[tabled(rename = "NAME")]
    name: String,
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
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

pub fn print_status(s: &ClusterStatus) {
    println!(
        "node      {} ({})  mlqd {}  runtime {}",
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
struct GpuRow {
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
