use serde::{Deserialize, Serialize};

/// Resources a job asks for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ResourceRequest {
    /// CPU cores (fractional allowed, e.g. 0.5).
    pub cpus: f64,
    /// Memory limit in bytes.
    pub mem_bytes: u64,
    /// Number of whole GPUs.
    pub gpus: u32,
}

impl Default for ResourceRequest {
    fn default() -> Self {
        Self {
            cpus: 2.0,
            mem_bytes: 8 * 1024 * 1024 * 1024,
            gpus: 0,
        }
    }
}

/// Per-field resource overrides; unset fields inherit from a base (the task's defaults or the
/// global default).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ResourceOverrides {
    pub cpus: Option<f64>,
    pub mem_bytes: Option<u64>,
    pub gpus: Option<u32>,
}

impl ResourceOverrides {
    pub fn apply(&self, base: &ResourceRequest) -> ResourceRequest {
        ResourceRequest {
            cpus: self.cpus.unwrap_or(base.cpus),
            mem_bytes: self.mem_bytes.unwrap_or(base.mem_bytes),
            gpus: self.gpus.unwrap_or(base.gpus),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.cpus.is_none() && self.mem_bytes.is_none() && self.gpus.is_none()
    }
}

/// The pool of resources the scheduler is allowed to hand out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    pub cpus: f64,
    pub mem_bytes: u64,
    /// GPU indices managed by the scheduler (others are left for direct host use).
    pub gpus: Vec<u32>,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid size '{0}' (expected e.g. 512M, 16G, 1.5T or a plain number of bytes)")]
pub struct ParseBytesError(String);

/// Parse "16G", "512M", "1.5T", "1024" (bytes), "2GiB" into bytes (binary units).
pub fn parse_bytes(s: &str) -> Result<u64, ParseBytesError> {
    let s = s.trim();
    let err = || ParseBytesError(s.to_string());
    let idx = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(idx);
    let num: f64 = num.parse().map_err(|_| err())?;
    let unit = unit.trim().to_ascii_lowercase();
    let mult: f64 = match unit.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024f64.powi(2),
        "g" | "gb" | "gib" => 1024f64.powi(3),
        "t" | "tb" | "tib" => 1024f64.powi(4),
        _ => return Err(err()),
    };
    Ok((num * mult).round() as u64)
}

pub fn format_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b}B")
    } else {
        format!("{v:.1}{}", UNITS[i])
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid duration '{0}' (expected HH:MM:SS, MM:SS, minutes, or e.g. 2h30m)")]
pub struct ParseDurationError(String);

/// Slurm-style time limit: "2:00:00" (h:m:s), "30:00" (m:s), "90" (minutes), or "2h30m".
pub fn parse_duration_secs(s: &str) -> Result<u64, ParseDurationError> {
    let s = s.trim();
    let err = || ParseDurationError(s.to_string());
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        let nums: Result<Vec<u64>, _> = parts.iter().map(|p| p.parse::<u64>()).collect();
        let nums = nums.map_err(|_| err())?;
        return match nums.as_slice() {
            [h, m, sec] => Ok(h * 3600 + m * 60 + sec),
            [m, sec] => Ok(m * 60 + sec),
            _ => Err(err()),
        };
    }
    if let Ok(mins) = s.parse::<u64>() {
        return Ok(mins * 60);
    }
    // 2h30m / 45m / 90s
    let mut total = 0u64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: u64 = num.parse().map_err(|_| err())?;
            num.clear();
            total += match c {
                'd' => n * 86400,
                'h' => n * 3600,
                'm' => n * 60,
                's' => n,
                _ => return Err(err()),
            };
        }
    }
    if !num.is_empty() {
        return Err(err());
    }
    if total == 0 { Err(err()) } else { Ok(total) }
}

pub fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        assert_eq!(parse_bytes("16G").unwrap(), 16 << 30);
        assert_eq!(parse_bytes("512M").unwrap(), 512 << 20);
        assert_eq!(parse_bytes("1024").unwrap(), 1024);
        assert_eq!(parse_bytes("1.5G").unwrap(), 1_610_612_736);
        assert!(parse_bytes("abc").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_secs("2:00:00").unwrap(), 7200);
        assert_eq!(parse_duration_secs("30:00").unwrap(), 1800);
        assert_eq!(parse_duration_secs("90").unwrap(), 5400);
        assert_eq!(parse_duration_secs("2h30m").unwrap(), 9000);
        assert!(parse_duration_secs("x").is_err());
    }
}
