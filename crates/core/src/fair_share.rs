//! Slurm-style exponentially decayed usage accounting.

use chrono::{DateTime, Utc};

/// Weight of usage that finished at `finished_at`, seen from `now`, with the given half-life.
pub fn decay_weight(finished_at: DateTime<Utc>, now: DateTime<Utc>, half_life_secs: f64) -> f64 {
    if half_life_secs <= 0.0 {
        return 1.0;
    }
    let age = (now - finished_at).num_seconds().max(0) as f64;
    0.5f64.powf(age / half_life_secs)
}

/// Sum decayed GPU-seconds for a sequence of (gpu_seconds, finished_at) records.
pub fn decayed_usage<I>(records: I, now: DateTime<Utc>, half_life_secs: f64) -> f64
where
    I: IntoIterator<Item = (f64, DateTime<Utc>)>,
{
    records
        .into_iter()
        .map(|(gpu_secs, finished)| gpu_secs * decay_weight(finished, now, half_life_secs))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn halves_after_half_life() {
        let now = Utc::now();
        let w = decay_weight(now - Duration::seconds(3600), now, 3600.0);
        assert!((w - 0.5).abs() < 1e-9);
        assert_eq!(decay_weight(now, now, 3600.0), 1.0);
    }
}
