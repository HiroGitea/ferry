import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api";
import { useMetricStream } from "./sync";

export interface SeriesData {
  key: string;
  unit: string;
  points: Array<[number, number | null]>;
}

export type Range =
  | { kind: "live"; secs: number }
  | { kind: "fixed"; after: number; before: number | null };

export const RANGES: Array<{ id: string; label: string; secs: number }> = [
  { id: "5m", label: "5 min", secs: 300 },
  { id: "15m", label: "15 min", secs: 900 },
  { id: "1h", label: "1 hour", secs: 3600 },
  { id: "6h", label: "6 hours", secs: 6 * 3600 },
  { id: "24h", label: "24 hours", secs: 86400 },
  { id: "7d", label: "7 days", secs: 7 * 86400 },
  { id: "30d", label: "30 days", secs: 30 * 86400 },
];

/** Live per-second streaming is used for windows that fit in the daemon's memory ring. */
export const LIVE_MAX_SECS = 900;

function matches(pattern: string, key: string): boolean {
  return pattern.split(",").some((p) => {
    p = p.trim();
    return p.endsWith("*") ? key.startsWith(p.slice(0, -1)) : p === key;
  });
}

/**
 * Time series for `pattern` (comma list, `prefix*` allowed).
 * Live ranges <= 15 min: one query, then per-second ticks from the shared stream are appended.
 * Longer / fixed ranges: periodic re-query; the daemon picks the per-second / per-minute /
 * per-hour tier from the span.
 */
export function useMetrics(pattern: string, range: Range, points = 300) {
  const now = Math.floor(Date.now() / 1000);
  const live = range.kind === "live" && range.secs <= LIVE_MAX_SECS;
  const after = range.kind === "live" ? -range.secs : range.after;
  const before = range.kind === "live" ? undefined : range.before ?? undefined;
  const pts = live ? range.secs : points;
  const q = useQuery({
    queryKey: ["metrics", pattern, range.kind, after, before ?? null, pts],
    queryFn: () => api.metrics({ keys: pattern, after, before, points: pts }),
    refetchInterval: live ? false : range.kind === "live" ? (range.secs <= 6 * 3600 ? 10_000 : 60_000) : before ? false : 15_000,
    placeholderData: (prev) => prev,
    enabled: !!pattern,
  });

  // Live buffer, seeded from the query and extended by stream ticks.
  const buf = useRef(new Map<string, SeriesData>());
  const [version, setVersion] = useState(0);
  const { subscribe } = useMetricStream();

  useEffect(() => {
    if (!q.data) return;
    const m = new Map<string, SeriesData>();
    for (const s of q.data.series) {
      const pts = s.points.slice();
      // Drop trailing empty buckets (not collected yet when the query ran) and keep any
      // stream ticks that arrived while the query was in flight.
      while (pts.length && pts[pts.length - 1][1] === null) pts.pop();
      const lastT = pts.length ? pts[pts.length - 1][0] : -Infinity;
      const prev = buf.current.get(s.key);
      if (prev) for (const p of prev.points) if (p[0] > lastT && p[1] !== null) pts.push(p);
      m.set(s.key, { key: s.key, unit: s.unit, points: pts });
    }
    buf.current = m;
    setVersion((v) => v + 1);
  }, [q.data]);

  const liveSecs = range.kind === "live" ? range.secs : 0;
  useEffect(() => {
    if (!live) return;
    const secs = liveSecs;
    return subscribe((tick) => {
      let changed = false;
      for (const [k, v] of Object.entries(tick.values)) {
        if (!matches(pattern, k)) continue;
        let s = buf.current.get(k);
        if (!s) {
          s = { key: k, unit: "", points: [] };
          buf.current.set(k, s);
        }
        const last = s.points[s.points.length - 1];
        if (last && last[0] >= tick.t) continue;
        s.points.push([tick.t, v]);
        const cutoff = tick.t - secs;
        let drop = 0;
        while (drop < s.points.length && s.points[drop][0] < cutoff) drop++;
        if (drop) s.points.splice(0, drop);
        changed = true;
      }
      if (changed) setVersion((v) => v + 1);
    });
  }, [live, pattern, liveSecs, subscribe]);

  const series = useMemo(() => {
    const out = [...buf.current.values()];
    // units for stream-only series come from the query; fall back to sibling series
    for (const s of out) if (!s.unit) s.unit = out.find((o) => o.unit)?.unit ?? "";
    return out.sort((a, b) => a.key.localeCompare(b.key, undefined, { numeric: true }));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [version]);

  const from = range.kind === "live" ? now - range.secs : range.after;
  const to = range.kind === "live" ? now : range.before ?? now;
  return { series, from, to, loading: q.isLoading, fetching: q.isFetching, error: q.error, step: q.data?.step ?? 1, tier: q.data?.tier ?? 0 };
}
