//! Per-second collection, in the spirit of netdata's proc.plugin (host), cgroups.plugin
//! (containers) and nvidia-smi collector (GPUs).
//!
//! Keys:
//! * `host.cpu.{user,system,iowait}` (% of all cores), `host.load.1`,
//!   `host.mem.{used,cache}` (bytes), `host.disk.{read,write}` and `host.net.{rx,tx}` (B/s),
//!   `host.scratch.used` / `host.archive.used` (% of the filesystem)
//! * `gpu.<i>.{util,mem,temp,power}`
//! * `sched.{running,queued,gpus_busy}`
//! * `job.<id>.*` for one-shot jobs and `task.<id>.*` for task environments:
//!   `cpu` (% of one core), `mem` (bytes), `io_read`/`io_write` (B/s), `pids`,
//!   `gpu_util` (% averaged over its GPUs), `gpu_mem` (bytes)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mlq_core::JobState;
use tracing::warn;

use super::now_secs;
use crate::executor::Usage;
use crate::state::AppState;

#[derive(Default)]
struct Prev {
    cpu: Option<[u64; 8]>,
    disk: Option<(u64, u64)>,
    net: Option<(u64, u64)>,
    usage: HashMap<String, (Usage, Instant)>,
    at: Option<Instant>,
}

type Samples = Vec<(String, &'static str, f32)>;

pub fn spawn(state: Arc<AppState>) {
    if !state.metrics.cfg.enabled {
        return;
    }
    let st = state.clone();
    tokio::spawn(async move {
        let mut prev = Prev::default();
        let every = Duration::from_secs(st.metrics.cfg.interval_secs.max(1));
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let t = now_secs();
            let samples = collect(&st, &mut prev).await;
            if let Err(e) = st.metrics.ingest(t, samples).await {
                warn!("metrics ingest failed: {e:#}");
            }
        }
    });
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            if let Err(e) = state.metrics.cleanup().await {
                warn!("metrics cleanup failed: {e:#}");
            }
        }
    });
}

fn read(p: &str) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

/// user nice system idle iowait irq softirq steal
fn cpu_times() -> Option<[u64; 8]> {
    let s = read("/proc/stat")?;
    let line = s.lines().next()?;
    let mut v = [0u64; 8];
    for (i, f) in line.split_whitespace().skip(1).take(8).enumerate() {
        v[i] = f.parse().ok()?;
    }
    Some(v)
}

fn meminfo() -> HashMap<String, u64> {
    read("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, rest) = l.split_once(':')?;
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            Some((k.to_string(), kb * 1024))
        })
        .collect()
}

/// Sum of sectors read/written over whole physical disks, in bytes.
fn disk_bytes() -> Option<(u64, u64)> {
    let s = read("/proc/diskstats")?;
    let (mut r, mut w) = (0u64, 0u64);
    for line in s.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 {
            continue;
        }
        let name = f[2];
        if ["loop", "ram", "zram", "dm-", "sr", "md"]
            .iter()
            .any(|p| name.starts_with(p))
        {
            continue;
        }
        if !std::path::Path::new("/sys/block").join(name).exists() {
            continue; // partition
        }
        r += f[5].parse::<u64>().unwrap_or(0) * 512;
        w += f[9].parse::<u64>().unwrap_or(0) * 512;
    }
    Some((r, w))
}

fn net_bytes() -> Option<(u64, u64)> {
    let s = read("/proc/net/dev")?;
    let (mut rx, mut tx) = (0u64, 0u64);
    for line in s.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name == "lo"
            || [
                "veth", "cni", "podman", "docker", "br-", "virbr", "tap", "tun",
            ]
            .iter()
            .any(|p| name.starts_with(p))
        {
            continue;
        }
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() >= 9 {
            rx += f[0].parse::<u64>().unwrap_or(0);
            tx += f[8].parse::<u64>().unwrap_or(0);
        }
    }
    Some((rx, tx))
}

fn fs_used_pct(path: &std::path::Path) -> Option<f32> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a properly sized out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 || st.f_blocks == 0 {
        return None;
    }
    let total = st.f_blocks as f64;
    let avail = st.f_bavail as f64;
    let free = st.f_bfree as f64;
    // Same as df: used / (used + available)
    let used = total - free;
    Some((used / (used + avail) * 100.0) as f32)
}

fn rate(cur: u64, prev: u64, secs: f64) -> f32 {
    if secs <= 0.0 {
        0.0
    } else {
        (cur.saturating_sub(prev) as f64 / secs) as f32
    }
}

async fn collect(state: &AppState, prev: &mut Prev) -> Samples {
    let mut s: Samples = Vec::with_capacity(64);
    let now = Instant::now();
    let dt = prev
        .at
        .map(|p| now.duration_since(p).as_secs_f64())
        .unwrap_or(0.0);
    prev.at = Some(now);

    // ---- host
    if let Some(c) = cpu_times() {
        if let Some(p) = prev.cpu {
            let d: Vec<f64> = c
                .iter()
                .zip(p.iter())
                .map(|(a, b)| a.saturating_sub(*b) as f64)
                .collect();
            let total: f64 = d.iter().sum();
            if total > 0.0 {
                s.push((
                    "host.cpu.user".into(),
                    "%",
                    ((d[0] + d[1]) / total * 100.0) as f32,
                ));
                s.push((
                    "host.cpu.system".into(),
                    "%",
                    ((d[2] + d[5] + d[6]) / total * 100.0) as f32,
                ));
                s.push(("host.cpu.iowait".into(), "%", (d[4] / total * 100.0) as f32));
            }
        }
        prev.cpu = Some(c);
    }
    if let Some(l) = read("/proc/loadavg").and_then(|s| {
        s.split_whitespace()
            .next()
            .and_then(|v| v.parse::<f32>().ok())
    }) {
        s.push(("host.load.1".into(), "load", l));
    }
    let mi = meminfo();
    if let (Some(total), Some(avail)) = (mi.get("MemTotal"), mi.get("MemAvailable")) {
        s.push((
            "host.mem.used".into(),
            "B",
            total.saturating_sub(*avail) as f32,
        ));
        let cache =
            mi.get("Cached").copied().unwrap_or(0) + mi.get("Buffers").copied().unwrap_or(0);
        s.push(("host.mem.cache".into(), "B", cache as f32));
    }
    if let Some(d) = disk_bytes() {
        if let Some(p) = prev.disk {
            s.push(("host.disk.read".into(), "B/s", rate(d.0, p.0, dt)));
            s.push(("host.disk.write".into(), "B/s", rate(d.1, p.1, dt)));
        }
        prev.disk = Some(d);
    }
    if let Some(n) = net_bytes() {
        if let Some(p) = prev.net {
            s.push(("host.net.rx".into(), "B/s", rate(n.0, p.0, dt)));
            s.push(("host.net.tx".into(), "B/s", rate(n.1, p.1, dt)));
        }
        prev.net = Some(n);
    }
    if let Some(p) = fs_used_pct(&state.config.scratch_root()) {
        s.push(("host.scratch.used".into(), "%", p));
    }
    if let Some(dir) = &state.config.retention.archive_dir {
        if let Some(p) = fs_used_pct(dir) {
            s.push(("host.archive.used".into(), "%", p));
        }
    }

    // ---- scheduler + jobs
    let active = state.store.active_jobs().await.unwrap_or_default();
    let busy: Vec<u32> = active
        .iter()
        .filter(|j| j.state.is_active())
        .flat_map(|j| j.gpu_ids.iter().copied())
        .collect();
    state.gpu.set_busy(&busy);
    let gpus = state.gpu.list();
    for g in &gpus {
        let i = g.index;
        s.push((format!("gpu.{i}.util"), "%", g.util_percent as f32));
        s.push((format!("gpu.{i}.mem"), "B", g.mem_used as f32));
        if let Some(t) = g.temp_c {
            s.push((format!("gpu.{i}.temp"), "°C", t as f32));
        }
        if let Some(p) = g.power_w {
            s.push((format!("gpu.{i}.power"), "W", p));
        }
    }
    s.push((
        "sched.running".into(),
        "jobs",
        active.iter().filter(|j| j.state.is_active()).count() as f32,
    ));
    s.push((
        "sched.queued".into(),
        "jobs",
        active
            .iter()
            .filter(|j| j.state == JobState::Queued)
            .count() as f32,
    ));
    s.push(("sched.gpus_busy".into(), "gpus", busy.len() as f32));

    // Containers to sample: running jobs/steps, plus warm task environments.
    let mut targets: Vec<(String, String, Vec<u32>, i64)> = Vec::new(); // (scope, handle, gpus, user)
    for j in active.iter().filter(|j| j.state == JobState::Running) {
        let Some(h) = &j.container_id else { continue };
        let scope = match j.task_id {
            Some(t) => format!("task.{t}"),
            None => format!("job.{}", j.id),
        };
        targets.push((scope, h.clone(), j.gpu_ids.clone(), j.user_id));
    }
    for t in state.store.warm_tasks().await.unwrap_or_default() {
        let scope = format!("task.{}", t.id);
        if let Some(c) = t.warm_container {
            if !targets.iter().any(|x| x.0 == scope) {
                targets.push((scope, c, vec![], t.user_id));
            }
        }
    }
    let mut seen = Vec::new();
    for (scope, handle, gpu_ids, user) in targets {
        state.metrics.set_owner(&scope, user);
        seen.push(handle.clone());
        if let Some(u) = state.executor.usage(&handle).await {
            if let Some((p, at)) = prev.usage.get(&handle) {
                let secs = now.duration_since(*at).as_secs_f64();
                if secs > 0.0 {
                    s.push((
                        format!("{scope}.cpu"),
                        "%",
                        (u.cpu_usec.saturating_sub(p.cpu_usec) as f64 / (secs * 1e6) * 100.0)
                            as f32,
                    ));
                    s.push((
                        format!("{scope}.io_read"),
                        "B/s",
                        rate(u.io_read, p.io_read, secs),
                    ));
                    s.push((
                        format!("{scope}.io_write"),
                        "B/s",
                        rate(u.io_write, p.io_write, secs),
                    ));
                }
            }
            s.push((format!("{scope}.mem"), "B", u.mem_bytes as f32));
            s.push((format!("{scope}.pids"), "procs", u.pids as f32));
            prev.usage.insert(handle.clone(), (u, now));
        }
        if !gpu_ids.is_empty() {
            let mine: Vec<_> = gpus.iter().filter(|g| gpu_ids.contains(&g.index)).collect();
            if !mine.is_empty() {
                let util =
                    mine.iter().map(|g| g.util_percent as f32).sum::<f32>() / mine.len() as f32;
                s.push((format!("{scope}.gpu_util"), "%", util));
                s.push((
                    format!("{scope}.gpu_mem"),
                    "B",
                    mine.iter().map(|g| g.mem_used as f32).sum(),
                ));
            }
        }
    }
    prev.usage.retain(|h, _| seen.contains(h));
    s
}
