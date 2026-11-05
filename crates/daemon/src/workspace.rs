//! Per-job scratch workspace on local (SSD) storage.
//!
//! Layout: `<scratch_root>/<user>/jobs/<job_id>/{work,out,job.log}`.
//! Only `work` and `out` are mounted into the container; users cannot request other host paths.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use mlq_core::JobId;

#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    pub fn path_for(scratch_root: &Path, user: &str, job_id: JobId) -> PathBuf {
        scratch_root
            .join(sanitize_name(user))
            .join("jobs")
            .join(job_id.to_string())
    }

    /// Workspace of a long-lived task: `<scratch_root>/<user>/tasks/<task_id>`.
    pub fn task_path_for(scratch_root: &Path, user: &str, task_id: i64) -> PathBuf {
        scratch_root
            .join(sanitize_name(user))
            .join("tasks")
            .join(task_id.to_string())
    }

    /// Log file of a job: `job.log` for one-shot jobs, `logs/<job_id>.log` for task steps
    /// (several steps share one task workspace).
    pub fn log_for(&self, job: &mlq_core::Job) -> PathBuf {
        match job.task_id {
            Some(_) => self.root.join("logs").join(format!("{}.log", job.id)),
            None => self.log(),
        }
    }

    pub fn open(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn create(root: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(root.join("work"))?;
        std::fs::create_dir_all(root.join("out"))?;
        Ok(Self { root })
    }

    pub fn work(&self) -> PathBuf {
        self.root.join("work")
    }
    pub fn out(&self) -> PathBuf {
        self.root.join("out")
    }
    pub fn log(&self) -> PathBuf {
        self.root.join("job.log")
    }

    /// Write small text files into `work/`.
    pub fn write_files(&self, files: &BTreeMap<String, String>) -> anyhow::Result<()> {
        for (rel, content) in files {
            let target = self.resolve_in(&self.work(), rel)?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, content)
                .with_context(|| format!("writing {}", target.display()))?;
        }
        Ok(())
    }

    /// Resolve a user-supplied relative path inside `base`, rejecting traversal and absolute paths.
    pub fn resolve_in(&self, base: &Path, rel: &str) -> anyhow::Result<PathBuf> {
        let rel_path = Path::new(rel);
        let mut out = base.to_path_buf();
        for c in rel_path.components() {
            match c {
                Component::Normal(p) => out.push(p),
                Component::CurDir => {}
                _ => bail!("invalid path '{rel}'"),
            }
        }
        Ok(out)
    }

    /// Extract a gzipped tar archive into `work/`.
    pub fn unpack_tar_gz(&self, data: &[u8]) -> anyhow::Result<usize> {
        let dec = flate2::read::GzDecoder::new(data);
        let mut ar = tar::Archive::new(dec);
        ar.set_preserve_permissions(true);
        let mut n = 0;
        for entry in ar.entries()? {
            let mut e = entry?;
            // `unpack_in` refuses paths escaping the destination.
            if e.unpack_in(self.work())? {
                n += 1;
            }
        }
        Ok(n)
    }

    pub fn list_out(&self) -> anyhow::Result<Vec<mlq_core::api::ArtifactEntry>> {
        let base = self.out();
        let mut entries = Vec::new();
        if !base.exists() {
            return Ok(entries);
        }
        for e in walkdir::WalkDir::new(&base)
            .min_depth(1)
            .sort_by_file_name()
        {
            let e = e?;
            let rel = e.path().strip_prefix(&base)?.to_string_lossy().to_string();
            let md = e.metadata()?;
            entries.push(mlq_core::api::ArtifactEntry {
                path: rel,
                size: md.len(),
                is_dir: md.is_dir(),
            });
        }
        Ok(entries)
    }

    pub fn total_size(&self) -> u64 {
        walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
    }
}

pub fn sanitize_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Read the last `tail` lines (or from `offset`) of a log file.
pub fn read_log(
    path: &Path,
    tail: Option<usize>,
    offset: Option<u64>,
) -> anyhow::Result<(String, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return Ok((String::new(), 0));
    };
    let len = f.metadata()?.len();
    if let Some(off) = offset {
        let off = off.min(len);
        f.seek(SeekFrom::Start(off))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        return Ok((String::from_utf8_lossy(&buf).into_owned(), len));
    }
    // Tail: read at most 1 MiB from the end, then keep last N lines.
    const MAX: u64 = 1 << 20;
    let start = len.saturating_sub(MAX);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let n = tail.unwrap_or(200);
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(n);
    let mut out = lines[skip..].join("\n");
    if text.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    Ok((out, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal() {
        let ws = Workspace {
            root: PathBuf::from("/tmp/x"),
        };
        assert!(
            ws.resolve_in(Path::new("/tmp/x/work"), "../etc/passwd")
                .is_err()
        );
        assert!(
            ws.resolve_in(Path::new("/tmp/x/work"), "/etc/passwd")
                .is_err()
        );
        assert_eq!(
            ws.resolve_in(Path::new("/tmp/x/work"), "a/./b.txt")
                .unwrap(),
            PathBuf::from("/tmp/x/work/a/b.txt")
        );
    }

    #[test]
    fn tail_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        std::fs::write(&p, "a\nb\nc\nd\n").unwrap();
        let (t, len) = read_log(&p, Some(2), None).unwrap();
        assert_eq!(t, "c\nd\n");
        assert_eq!(len, 8);
        let (t2, _) = read_log(&p, None, Some(4)).unwrap();
        assert_eq!(t2, "c\nd\n");
    }
}
