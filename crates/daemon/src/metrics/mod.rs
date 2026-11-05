//! Metrics engine modelled on netdata's dbengine.
//!
//! * Collection every `interval_secs` (default 1 s) — see [`collector`].
//! * **Tier 0** (per-second): the last `memory_secs` live in an in-memory ring per series; every
//!   completed minute is written to SQLite as one compressed *page* of 60 samples.
//! * **Tier 1** (per-minute) and **tier 2** (per-hour): each point stores min / max / sum / count,
//!   so averages stay exact and peaks survive downsampling.
//! * Each tier has its own retention (`tier0_days`, `tier1_days`, `tier2_days`).
//! * Queries pick the finest tier that both covers the requested window and does not exceed the
//!   requested number of points, then aggregate into equal buckets — like netdata's `/api/v1/data`.

pub mod collector;

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use ferry_core::api::{MetricInfo, MetricSeries, MetricTick, MetricsResult};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use tokio::sync::broadcast;

use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub interval_secs: u64,
    /// Per-second samples kept in memory for live charts.
    pub memory_secs: i64,
    pub tier0_days: i64,
    pub tier1_days: i64,
    pub tier2_days: i64,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 1,
            memory_secs: 900,
            tier0_days: 7,
            tier1_days: 90,
            tier2_days: 730,
        }
    }
}

/// Aggregate of a set of samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Agg {
    pub min: f32,
    pub max: f32,
    pub sum: f64,
    pub count: u32,
}

impl Agg {
    pub const EMPTY: Agg = Agg {
        min: f32::INFINITY,
        max: f32::NEG_INFINITY,
        sum: 0.0,
        count: 0,
    };
    pub fn one(v: f32) -> Agg {
        Agg {
            min: v,
            max: v,
            sum: v as f64,
            count: 1,
        }
    }
    pub fn add(&mut self, o: &Agg) {
        if o.count == 0 {
            return;
        }
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
        self.sum += o.sum;
        self.count += o.count;
    }
    pub fn avg(&self) -> Option<f32> {
        (self.count > 0).then(|| (self.sum / self.count as f64) as f32)
    }
}

/// A finished minute page or hour row waiting to be written.
#[derive(Debug, Clone)]
pub enum PendingWrite {
    Page {
        id: i64,
        start: i64,
        data: Vec<u8>,
        agg: Agg,
    },
    Hour {
        id: i64,
        start: i64,
        agg: Agg,
    },
}

const PAGE: usize = 60;

#[derive(Debug)]
struct Series {
    id: i64,
    unit: String,
    ring: VecDeque<(i64, f32)>,
    page_start: i64,
    page: [f32; PAGE],
    page_has_data: bool,
    hour_start: i64,
    hour_acc: Agg,
    last_t: i64,
}

pub fn encode_page(page: &[f32; PAGE]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(PAGE * 4);
    for v in page {
        raw.extend_from_slice(&v.to_le_bytes());
    }
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    let _ = enc.write_all(&raw);
    enc.finish().unwrap_or(raw)
}

pub fn decode_page(data: &[u8]) -> Vec<f32> {
    let mut raw = Vec::new();
    if flate2::read::DeflateDecoder::new(data)
        .read_to_end(&mut raw)
        .is_err()
    {
        return vec![];
    }
    let (chunks, _) = raw.as_chunks::<4>();
    chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

fn page_agg(page: &[f32; PAGE]) -> Agg {
    let mut a = Agg::EMPTY;
    for v in page.iter().filter(|v| !v.is_nan()) {
        a.add(&Agg::one(*v));
    }
    a
}

impl Series {
    fn new(id: i64, unit: String, t: i64) -> Self {
        let page_start = t - t.rem_euclid(60);
        Series {
            id,
            unit,
            ring: VecDeque::new(),
            page_start,
            page: [f32::NAN; PAGE],
            page_has_data: false,
            hour_start: page_start - page_start.rem_euclid(3600),
            hour_acc: Agg::EMPTY,
            last_t: 0,
        }
    }

    /// Close the current page (if it has data) and roll the hour accumulator.
    fn flush_page(&mut self, out: &mut Vec<PendingWrite>) {
        if !self.page_has_data {
            return;
        }
        let agg = page_agg(&self.page);
        out.push(PendingWrite::Page {
            id: self.id,
            start: self.page_start,
            data: encode_page(&self.page),
            agg,
        });
        let hour = self.page_start - self.page_start.rem_euclid(3600);
        if hour != self.hour_start {
            self.flush_hour(out);
            self.hour_start = hour;
        }
        self.hour_acc.add(&agg);
        self.page = [f32::NAN; PAGE];
        self.page_has_data = false;
    }

    fn flush_hour(&mut self, out: &mut Vec<PendingWrite>) {
        if self.hour_acc.count > 0 {
            out.push(PendingWrite::Hour {
                id: self.id,
                start: self.hour_start,
                agg: self.hour_acc,
            });
        }
        self.hour_acc = Agg::EMPTY;
    }

    fn push(&mut self, t: i64, v: f32, memory_secs: i64, out: &mut Vec<PendingWrite>) {
        let minute = t - t.rem_euclid(60);
        if minute != self.page_start {
            self.flush_page(out);
            self.page_start = minute;
            let hour = minute - minute.rem_euclid(3600);
            if hour != self.hour_start {
                self.flush_hour(out);
                self.hour_start = hour;
            }
        }
        self.page[(t - self.page_start) as usize] = v;
        self.page_has_data = true;
        if self.ring.back().map(|(lt, _)| *lt < t).unwrap_or(true) {
            self.ring.push_back((t, v));
        }
        while self
            .ring
            .front()
            .map(|(ft, _)| t - ft >= memory_secs)
            .unwrap_or(false)
        {
            self.ring.pop_front();
        }
        self.last_t = t;
    }

    /// Per-minute aggregates of the in-memory data, for minutes >= `from`.
    fn mem_minutes(&self, from: i64) -> Vec<(i64, Agg)> {
        let mut out: Vec<(i64, Agg)> = Vec::new();
        for (t, v) in &self.ring {
            let m = t - t.rem_euclid(60);
            if m < from {
                continue;
            }
            match out.last_mut() {
                Some((lm, a)) if *lm == m => a.add(&Agg::one(*v)),
                _ => out.push((m, Agg::one(*v))),
            }
        }
        out
    }

    /// First minute fully covered by the in-memory ring.
    fn mem_cutoff_minute(&self) -> i64 {
        match self.ring.front() {
            Some((t, _)) => {
                let m = t - t.rem_euclid(60);
                if *t == m { m } else { m + 60 }
            }
            None => i64::MAX,
        }
    }
}

pub struct MetricsHub {
    pub cfg: MetricsConfig,
    pool: Option<SqlitePool>,
    series: Mutex<HashMap<String, Series>>,
    live: broadcast::Sender<MetricTick>,
    /// "job.<id>" / "task.<id>" -> owning user, for access control.
    owners: Mutex<HashMap<String, i64>>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS metrics (id INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE, unit TEXT NOT NULL, last_t INTEGER);
CREATE TABLE IF NOT EXISTS tier0 (metric_id INTEGER NOT NULL, start INTEGER NOT NULL, data BLOB NOT NULL, PRIMARY KEY (metric_id, start)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS tier1 (metric_id INTEGER NOT NULL, t INTEGER NOT NULL, min REAL NOT NULL, max REAL NOT NULL, sum REAL NOT NULL, count INTEGER NOT NULL, PRIMARY KEY (metric_id, t)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS tier2 (metric_id INTEGER NOT NULL, t INTEGER NOT NULL, min REAL NOT NULL, max REAL NOT NULL, sum REAL NOT NULL, count INTEGER NOT NULL, PRIMARY KEY (metric_id, t)) WITHOUT ROWID;
";

pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Does `key` match a comma-list entry (`a.b.c` exact or `a.b.*` prefix)?
pub fn key_matches(pattern: &str, key: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => key.starts_with(prefix),
        None => pattern == key,
    }
}

/// "job.12.cpu" -> Some("job.12"); "task.3.mem" -> Some("task.3"); host keys -> None.
pub fn owner_scope(key: &str) -> Option<String> {
    let mut it = key.splitn(3, '.');
    let kind = it.next()?;
    let id = it.next()?;
    matches!(kind, "job" | "task").then(|| format!("{kind}.{id}"))
}

impl MetricsHub {
    pub async fn open(config: &Config) -> anyhow::Result<Arc<MetricsHub>> {
        let cfg = config.metrics.clone();
        let pool = if cfg.enabled {
            let opts = SqliteConnectOptions::new()
                .filename(config.data_dir.join("metrics.db"))
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .busy_timeout(std::time::Duration::from_secs(10));
            let pool = SqlitePoolOptions::new()
                .max_connections(4)
                .connect_with(opts)
                .await
                .context("opening metrics.db")?;
            sqlx::raw_sql(SCHEMA).execute(&pool).await?;
            Some(pool)
        } else {
            None
        };
        Ok(Self::with_pool(cfg, pool))
    }

    pub async fn open_memory(cfg: MetricsConfig) -> anyhow::Result<Arc<MetricsHub>> {
        let opts = SqliteConnectOptions::new().in_memory(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        Ok(Self::with_pool(cfg, Some(pool)))
    }

    fn with_pool(cfg: MetricsConfig, pool: Option<SqlitePool>) -> Arc<MetricsHub> {
        let (live, _) = broadcast::channel(64);
        Arc::new(MetricsHub {
            cfg,
            pool,
            series: Mutex::new(HashMap::new()),
            live,
            owners: Mutex::new(HashMap::new()),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<MetricTick> {
        self.live.subscribe()
    }

    pub fn set_owner(&self, scope: &str, user_id: i64) {
        self.owners
            .lock()
            .unwrap()
            .insert(scope.to_string(), user_id);
    }

    pub fn owner(&self, scope: &str) -> Option<i64> {
        self.owners.lock().unwrap().get(scope).copied()
    }

    async fn metric_id(&self, key: &str, unit: &str) -> anyhow::Result<i64> {
        let pool = self.pool.as_ref().context("metrics disabled")?;
        sqlx::query("INSERT OR IGNORE INTO metrics (key, unit) VALUES (?, ?)")
            .bind(key)
            .bind(unit)
            .execute(pool)
            .await?;
        Ok(sqlx::query_scalar("SELECT id FROM metrics WHERE key = ?")
            .bind(key)
            .fetch_one(pool)
            .await?)
    }

    /// Add one collection tick. `samples` = (key, unit, value).
    pub async fn ingest(&self, t: i64, samples: Vec<(String, String, f32)>) -> anyhow::Result<()> {
        if self.pool.is_none() {
            return Ok(());
        }
        // Resolve ids for new series outside the lock.
        let missing: Vec<(String, String)> = {
            let map = self.series.lock().unwrap();
            samples
                .iter()
                .filter(|(k, _, _)| !map.contains_key(k))
                .map(|(k, u, _)| (k.clone(), u.clone()))
                .collect()
        };
        let mut new_ids = Vec::new();
        for (k, u) in missing {
            let id = self.metric_id(&k, &u).await?;
            new_ids.push((k, u, id));
        }
        let mut writes = Vec::new();
        let mut tick = MetricTick {
            t,
            values: Default::default(),
        };
        {
            let mut map = self.series.lock().unwrap();
            for (k, u, id) in new_ids {
                map.entry(k).or_insert_with(|| Series::new(id, u, t));
            }
            for (k, _, v) in &samples {
                if !v.is_finite() {
                    continue;
                }
                if let Some(s) = map.get_mut(k) {
                    s.push(t, *v, self.cfg.memory_secs, &mut writes);
                    tick.values.insert(k.clone(), *v);
                }
            }
            // Series that stopped reporting: close their page once its minute is over, and
            // forget them when their in-memory window has passed.
            let mem = self.cfg.memory_secs;
            map.retain(|_, s| {
                if s.last_t < t && s.page_has_data && s.page_start + 60 <= t {
                    s.flush_page(&mut writes);
                }
                let keep = t - s.last_t < mem;
                if !keep {
                    s.flush_hour(&mut writes);
                }
                keep
            });
        }
        let _ = self.live.send(tick);
        self.write(writes).await
    }

    /// Flush everything in memory (used on shutdown and in tests).
    /// Copy metrics.db with `VACUUM INTO` (no-op when metrics are disabled).
    pub async fn backup_to(&self, dest: &std::path::Path) -> anyhow::Result<bool> {
        let Some(pool) = &self.pool else {
            return Ok(false);
        };
        sqlx::query("VACUUM INTO ?")
            .bind(dest.to_string_lossy().to_string())
            .execute(pool)
            .await?;
        Ok(true)
    }

    pub async fn flush_all(&self) -> anyhow::Result<()> {
        let mut writes = Vec::new();
        {
            let mut map = self.series.lock().unwrap();
            for s in map.values_mut() {
                s.flush_page(&mut writes);
                s.flush_hour(&mut writes);
            }
        }
        self.write(writes).await
    }

    async fn write(&self, writes: Vec<PendingWrite>) -> anyhow::Result<()> {
        if writes.is_empty() {
            return Ok(());
        }
        let pool = self.pool.as_ref().context("metrics disabled")?;
        let mut tx = pool.begin().await?;
        for w in writes {
            match w {
                PendingWrite::Page {
                    id,
                    start,
                    data,
                    agg,
                } => {
                    sqlx::query(
                        "INSERT OR REPLACE INTO tier0 (metric_id, start, data) VALUES (?, ?, ?)",
                    )
                    .bind(id)
                    .bind(start)
                    .bind(data)
                    .execute(&mut *tx)
                    .await?;
                    upsert_agg(&mut tx, "tier1", id, start, &agg).await?;
                    sqlx::query("UPDATE metrics SET last_t = ? WHERE id = ?")
                        .bind(start + 59)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
                PendingWrite::Hour { id, start, agg } => {
                    upsert_agg(&mut tx, "tier2", id, start, &agg).await?
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Delete data older than each tier's retention.
    pub async fn cleanup(&self) -> anyhow::Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };
        let now = now_secs();
        sqlx::query("DELETE FROM tier0 WHERE start < ?")
            .bind(now - self.cfg.tier0_days * 86400)
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM tier1 WHERE t < ?")
            .bind(now - self.cfg.tier1_days * 86400)
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM tier2 WHERE t < ?")
            .bind(now - self.cfg.tier2_days * 86400)
            .execute(pool)
            .await?;
        Ok(())
    }

    /// Known metric keys (persisted and in memory).
    pub async fn keys(&self) -> anyhow::Result<Vec<MetricInfo>> {
        let Some(pool) = &self.pool else {
            return Ok(vec![]);
        };
        let rows = sqlx::query("SELECT key, unit, last_t FROM metrics ORDER BY key")
            .fetch_all(pool)
            .await?;
        let mem = self.series.lock().unwrap();
        Ok(rows
            .into_iter()
            .map(|r| {
                let key: String = r.get("key");
                let last_t = mem
                    .get(&key)
                    .map(|s| s.last_t)
                    .or(r.get::<Option<i64>, _>("last_t"));
                MetricInfo {
                    unit: r.get("unit"),
                    key,
                    last_t,
                }
            })
            .collect())
    }

    /// Query `keys` (already expanded, exact) over [after, before] with about `points` points.
    pub async fn query(
        &self,
        keys: &[String],
        after: i64,
        before: i64,
        points: u32,
    ) -> anyhow::Result<MetricsResult> {
        let now = now_secs();
        let before = before.min(now).max(after + 1);
        let span = before - after;
        let points = points.clamp(1, 5000) as i64;
        let raw_step = ((span + points - 1) / points).max(1);
        let oldest_t0 = now - self.cfg.tier0_days * 86400;
        let oldest_t1 = now - self.cfg.tier1_days * 86400;
        let mut tier: u8 = if raw_step < 60 {
            0
        } else if raw_step < 3600 {
            1
        } else {
            2
        };
        if tier == 0 && after < oldest_t0 {
            tier = 1;
        }
        if tier == 1 && after < oldest_t1 {
            tier = 2;
        }
        let res = [1i64, 60, 3600][tier as usize];
        let step = ((raw_step + res - 1) / res) * res;
        let start = after - after.rem_euclid(step);
        let n = ((before - start) / step + 1) as usize;

        let mut series = Vec::new();
        for key in keys {
            let (unit, pts) = self.points(key, start, before, tier).await?;
            let Some(unit) = unit else { continue };
            let mut buckets = vec![Agg::EMPTY; n];
            for (t, a) in pts {
                if t < start || t > before {
                    continue;
                }
                let i = ((t - start) / step) as usize;
                if i < n {
                    buckets[i].add(&a);
                }
            }
            series.push(MetricSeries {
                key: key.clone(),
                unit,
                points: buckets
                    .iter()
                    .enumerate()
                    .map(|(i, a)| (start + i as i64 * step, a.avg()))
                    .collect(),
                max: buckets
                    .iter()
                    .map(|a| (a.count > 0).then_some(a.max))
                    .collect(),
            });
        }
        Ok(MetricsResult {
            after: start,
            before,
            step,
            tier,
            series,
        })
    }

    /// Raw points of one series at the given tier's resolution within [from, to].
    async fn points(
        &self,
        key: &str,
        from: i64,
        to: i64,
        tier: u8,
    ) -> anyhow::Result<(Option<String>, Vec<(i64, Agg)>)> {
        let Some(pool) = &self.pool else {
            return Ok((None, vec![]));
        };
        // Snapshot the in-memory part.
        let mem = {
            let map = self.series.lock().unwrap();
            map.get(key).map(|s| {
                (
                    s.id,
                    s.unit.clone(),
                    s.ring.iter().copied().collect::<Vec<_>>(),
                    s.mem_minutes(i64::MIN),
                    s.mem_cutoff_minute(),
                    s.hour_start,
                    {
                        let mut a = s.hour_acc;
                        a.add(&page_agg(&s.page));
                        a
                    },
                )
            })
        };
        let (id, unit) = match &mem {
            Some((id, unit, ..)) => (*id, unit.clone()),
            None => {
                let row = sqlx::query("SELECT id, unit FROM metrics WHERE key = ?")
                    .bind(key)
                    .fetch_optional(pool)
                    .await?;
                match row {
                    Some(r) => (r.get::<i64, _>("id"), r.get::<String, _>("unit")),
                    None => return Ok((None, vec![])),
                }
            }
        };
        let mut out: Vec<(i64, Agg)> = Vec::new();
        match tier {
            0 => {
                let ring_first = mem
                    .as_ref()
                    .and_then(|m| m.2.first().map(|(t, _)| *t))
                    .unwrap_or(i64::MAX);
                let db_to = to.min(ring_first - 1);
                if from <= db_to {
                    let rows = sqlx::query("SELECT start, data FROM tier0 WHERE metric_id = ? AND start >= ? AND start <= ? ORDER BY start")
                        .bind(id)
                        .bind(from - 59)
                        .bind(db_to)
                        .fetch_all(pool)
                        .await?;
                    for r in rows {
                        let start: i64 = r.get("start");
                        let data: Vec<u8> = r.get("data");
                        for (i, v) in decode_page(&data).into_iter().enumerate() {
                            let t = start + i as i64;
                            if !v.is_nan() && t >= from && t <= db_to {
                                out.push((t, Agg::one(v)));
                            }
                        }
                    }
                }
                if let Some(m) = &mem {
                    out.extend(
                        m.2.iter()
                            .filter(|(t, _)| *t >= from && *t <= to)
                            .map(|(t, v)| (*t, Agg::one(*v))),
                    );
                }
            }
            1 => {
                let cutoff = mem.as_ref().map(|m| m.4).unwrap_or(i64::MAX);
                out.extend(read_agg(pool, "tier1", id, from, to.min(cutoff - 1)).await?);
                if let Some(m) = &mem {
                    out.extend(
                        m.3.iter()
                            .filter(|(t, _)| *t >= cutoff.max(from) && *t <= to)
                            .copied(),
                    );
                }
            }
            _ => {
                let cur_hour = mem.as_ref().map(|m| m.5).unwrap_or(i64::MAX);
                out.extend(read_agg(pool, "tier2", id, from, to.min(cur_hour - 1)).await?);
                if let Some(m) = &mem {
                    // current hour: flushed minutes are in tier1 but not yet in tier2
                    let mut a = Agg::EMPTY;
                    for (_, x) in read_agg(pool, "tier1", id, cur_hour, cur_hour + 3599).await? {
                        a.add(&x);
                    }
                    // hour_acc already covers flushed pages of this hour, so prefer it when present
                    let acc = m.6;
                    let cur = if acc.count >= a.count { acc } else { a };
                    if cur.count > 0 && cur_hour >= from && cur_hour <= to {
                        out.push((cur_hour, cur));
                    }
                }
            }
        }
        out.sort_by_key(|(t, _)| *t);
        Ok((Some(unit), out))
    }
}

async fn upsert_agg(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    id: i64,
    t: i64,
    a: &Agg,
) -> anyhow::Result<()> {
    sqlx::query(&format!(
        "INSERT INTO {table} (metric_id, t, min, max, sum, count) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(metric_id, t) DO UPDATE SET min = MIN(min, excluded.min), max = MAX(max, excluded.max),
         sum = sum + excluded.sum, count = count + excluded.count"
    ))
    .bind(id)
    .bind(t)
    .bind(a.min as f64)
    .bind(a.max as f64)
    .bind(a.sum)
    .bind(a.count as i64)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn read_agg(
    pool: &SqlitePool,
    table: &str,
    id: i64,
    from: i64,
    to: i64,
) -> anyhow::Result<Vec<(i64, Agg)>> {
    if from > to {
        return Ok(vec![]);
    }
    let rows = sqlx::query(&format!("SELECT t, min, max, sum, count FROM {table} WHERE metric_id = ? AND t >= ? AND t <= ? ORDER BY t"))
        .bind(id)
        .bind(from)
        .bind(to)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<i64, _>("t"),
                Agg {
                    min: r.get::<f64, _>("min") as f32,
                    max: r.get::<f64, _>("max") as f32,
                    sum: r.get("sum"),
                    count: r.get::<i64, _>("count") as u32,
                },
            )
        })
        .collect())
}

/// Expand a comma list of exact keys / `prefix*` patterns against the known keys.
pub fn expand_keys(patterns: &str, known: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for p in patterns.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if p.ends_with('*') {
            for k in known.iter().filter(|k| key_matches(p, k)) {
                if !out.contains(k) {
                    out.push(k.clone());
                }
            }
        } else if !out.iter().any(|k| k == p) {
            out.push(p.to_string());
        }
    }
    out
}

#[allow(dead_code)]
fn _assert_send(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MetricsConfig {
        MetricsConfig {
            memory_secs: 120,
            ..Default::default()
        }
    }

    #[test]
    fn page_roundtrip() {
        let mut p = [f32::NAN; PAGE];
        p[0] = 1.5;
        p[59] = -3.0;
        let d = decode_page(&encode_page(&p));
        assert_eq!(d.len(), 60);
        assert_eq!(d[0], 1.5);
        assert!(d[1].is_nan());
        assert_eq!(d[59], -3.0);
    }

    #[test]
    fn keys() {
        let known = vec![
            "host.cpu.user".to_string(),
            "host.cpu.system".into(),
            "gpu.0.util".into(),
        ];
        assert_eq!(
            expand_keys("host.cpu.*", &known),
            vec!["host.cpu.user", "host.cpu.system"]
        );
        assert_eq!(
            expand_keys("gpu.0.util,host.cpu.user", &known),
            vec!["gpu.0.util", "host.cpu.user"]
        );
        assert_eq!(owner_scope("job.12.cpu").as_deref(), Some("job.12"));
        assert_eq!(owner_scope("host.cpu.user"), None);
    }

    #[tokio::test]
    async fn tiers_and_query() {
        let hub = MetricsHub::open_memory(cfg()).await.unwrap();
        // Three hours of per-second data ending 10 s ago: value = minute index within the hour.
        let end = now_secs() - 10;
        let begin = end - 3 * 3600;
        for t in begin..=end {
            let v = ((t / 60) % 60) as f32;
            hub.ingest(t, vec![("test.v".into(), "u".into(), v)])
                .await
                .unwrap();
        }
        // Per-second query of the last 60 s comes from memory.
        let r = hub
            .query(&["test.v".into()], end - 59, end, 60)
            .await
            .unwrap();
        assert_eq!(r.tier, 0);
        assert_eq!(r.step, 1);
        let s = &r.series[0];
        assert!(s.points.iter().filter(|(_, v)| v.is_some()).count() >= 59);

        // Per-second query of an older window must come from tier0 pages in SQLite.
        let old_from = begin + 600;
        let r = hub
            .query(&["test.v".into()], old_from, old_from + 119, 120)
            .await
            .unwrap();
        assert_eq!(r.tier, 0);
        let got: Vec<f32> = r.series[0].points.iter().filter_map(|(_, v)| *v).collect();
        assert_eq!(got.len(), 120);
        assert_eq!(got[0], ((old_from / 60) % 60) as f32);

        // Two-hour window at ~120 points -> per-minute tier, averages equal the minute value.
        let r = hub
            .query(&["test.v".into()], end - 7200, end, 120)
            .await
            .unwrap();
        assert_eq!(r.tier, 1);
        assert_eq!(r.step, 60);
        for (t, v) in &r.series[0].points {
            if let Some(v) = v {
                assert_eq!(*v, ((t / 60) % 60) as f32, "minute {t}");
            }
        }

        // Whole range at 3 points -> per-hour tier; average of 0..59 = 29.5, max 59.
        hub.flush_all().await.unwrap();
        let r = hub.query(&["test.v".into()], begin, end, 3).await.unwrap();
        assert_eq!(r.tier, 2);
        let full: Vec<_> = r.series[0]
            .points
            .iter()
            .zip(&r.series[0].max)
            .filter(|((t, _), _)| *t >= begin && *t + 3600 <= end)
            .collect();
        assert!(!full.is_empty());
        for ((_, avg), max) in full {
            assert!((avg.unwrap() - 29.5).abs() < 1e-3, "avg {avg:?}");
            assert_eq!(max.unwrap(), 59.0);
        }
    }
}
