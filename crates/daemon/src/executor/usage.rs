//! Reading cumulative resource counters, the way netdata's cgroups.plugin / apps.plugin do:
//! cgroup files for containers (v2 unified or v1 per-controller), /proc for process groups.

use std::path::Path;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    /// Cumulative CPU time in microseconds.
    pub cpu_usec: u64,
    /// Current memory usage in bytes (excluding reclaimable page cache where the kernel reports it).
    pub mem_bytes: u64,
    /// Cumulative bytes read from / written to block devices.
    pub io_read: u64,
    pub io_write: u64,
    pub pids: u64,
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

fn kv(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        (it.next()? == key).then(|| it.next()?.parse().ok())?
    })
}

/// Read counters for a cgroup given its path relative to the cgroup root
/// (e.g. `/libpod_parent/libpod-<id>` or `/user.slice/.../libpod-<id>.scope`).
pub fn cgroup_usage(rel: &str) -> Option<Usage> {
    let rel = rel.trim_start_matches('/');
    let root = Path::new("/sys/fs/cgroup");
    // cgroup v2 (unified hierarchy)
    let v2 = root.join(rel);
    if v2.join("cgroup.controllers").exists() || v2.join("cpu.stat").exists() {
        let cpu = read(&v2.join("cpu.stat"))
            .and_then(|s| kv(&s, "usage_usec"))
            .unwrap_or(0);
        let mem_cur = read(&v2.join("memory.current"))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0u64);
        let inactive = read(&v2.join("memory.stat"))
            .and_then(|s| kv(&s, "inactive_file"))
            .unwrap_or(0);
        let (mut r, mut w) = (0u64, 0u64);
        if let Some(io) = read(&v2.join("io.stat")) {
            for line in io.lines() {
                for f in line.split_whitespace().skip(1) {
                    if let Some(v) = f.strip_prefix("rbytes=") {
                        r += v.parse::<u64>().unwrap_or(0);
                    } else if let Some(v) = f.strip_prefix("wbytes=") {
                        w += v.parse::<u64>().unwrap_or(0);
                    }
                }
            }
        }
        let pids = read(&v2.join("pids.current"))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        return Some(Usage {
            cpu_usec: cpu,
            mem_bytes: mem_cur.saturating_sub(inactive),
            io_read: r,
            io_write: w,
            pids,
        });
    }
    // cgroup v1: one hierarchy per controller
    let cpuacct = ["cpuacct", "cpu,cpuacct", "cpuacct,cpu"]
        .iter()
        .map(|c| root.join(c).join(rel).join("cpuacct.usage"))
        .find(|p| p.exists())?;
    let cpu_ns: u64 = read(&cpuacct)?.trim().parse().ok()?;
    let mem_dir = root.join("memory").join(rel);
    let mem_usage: u64 = read(&mem_dir.join("memory.usage_in_bytes"))
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let inactive = read(&mem_dir.join("memory.stat"))
        .and_then(|s| kv(&s, "total_inactive_file"))
        .unwrap_or(0);
    let (mut r, mut w) = (0u64, 0u64);
    if let Some(blk) = read(
        &root
            .join("blkio")
            .join(rel)
            .join("blkio.throttle.io_service_bytes"),
    ) {
        for line in blk.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() == 3 {
                let v: u64 = f[2].parse().unwrap_or(0);
                match f[1] {
                    "Read" => r += v,
                    "Write" => w += v,
                    _ => {}
                }
            }
        }
    }
    let pids = read(&root.join("pids").join(rel).join("pids.current"))
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    Some(Usage {
        cpu_usec: cpu_ns / 1000,
        mem_bytes: mem_usage.saturating_sub(inactive),
        io_read: r,
        io_write: w,
        pids,
    })
}

fn clk_tck() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as u64 } else { 100 }
}

fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 { v as u64 } else { 4096 }
}

/// Sum counters over every process in process group `pgid` (like netdata's apps.plugin).
pub fn process_group_usage(pgid: i32) -> Option<Usage> {
    let tck = clk_tck();
    let page = page_size();
    let mut u = Usage::default();
    let mut found = false;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Some(stat) = read(&entry.path().join("stat")) else {
            continue;
        };
        // Fields after the ")" of the comm field.
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
            continue;
        };
        let f: Vec<&str> = rest.split_whitespace().collect();
        // f[0]=state f[1]=ppid f[2]=pgrp ... f[11]=utime f[12]=stime ... f[21]=rss
        if f.len() < 22 || f[2].parse::<i32>().ok() != Some(pgid) {
            continue;
        }
        found = true;
        let ticks = f[11].parse::<u64>().unwrap_or(0) + f[12].parse::<u64>().unwrap_or(0);
        u.cpu_usec += ticks * 1_000_000 / tck;
        u.mem_bytes += f[21].parse::<u64>().unwrap_or(0) * page;
        u.pids += 1;
        if let Some(io) = read(&Path::new("/proc").join(pid.to_string()).join("io")) {
            u.io_read += kv(&io.replace(':', ""), "read_bytes").unwrap_or(0);
            u.io_write += kv(&io.replace(':', ""), "write_bytes").unwrap_or(0);
        }
    }
    found.then_some(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_group_is_visible() {
        // SAFETY: getpgrp has no preconditions.
        let pgid = unsafe { libc::getpgrp() };
        let u = process_group_usage(pgid).expect("own process group");
        assert!(u.pids >= 1);
        assert!(u.mem_bytes > 0);
    }

    #[test]
    fn kv_parses() {
        assert_eq!(kv("usage_usec 123\nuser_usec 5", "usage_usec"), Some(123));
        assert_eq!(kv("a 1", "b"), None);
    }
}
