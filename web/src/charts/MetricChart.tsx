import { useMemo } from "react";
import TimeChart, { SERIES_COLORS, type Band } from "./TimeChart";
import { useMetrics, type Range } from "./useMetrics";

interface Props {
  title: string;
  /** Comma separated keys / `prefix*` patterns. */
  keys: string;
  range: Range;
  /** Keep only matching keys (e.g. a suffix filter on `job.*`). */
  filter?: (key: string) => boolean;
  label?: (key: string) => string;
  /** Fixed categorical slot per key (entity-stable colours). Defaults to position in `order` or sorted order. */
  colorIndex?: (key: string, i: number) => number;
  /** Explicit display order of keys (e.g. user, system, iowait). */
  order?: string[];
  stacked?: boolean;
  yMax?: number;
  unit?: string;
  height?: number;
  bands?: Band[];
  empty?: string;
  points?: number;
}

const MAX_SERIES = 8;

export default function MetricChart({ title, keys, range, filter, label, colorIndex, order, stacked, yMax, unit, height, bands, empty, points }: Props) {
  const m = useMetrics(keys, range, points);
  const series = useMemo(() => {
    let s = m.series.filter((x) => (filter ? filter(x.key) : true));
    if (order) {
      s = s.slice().sort((a, b) => {
        const ia = order.indexOf(a.key);
        const ib = order.indexOf(b.key);
        return (ia < 0 ? 999 : ia) - (ib < 0 ? 999 : ib);
      });
    }
    return s.slice(0, MAX_SERIES).map((x, i) => ({
      key: x.key,
      label: label ? label(x.key) : x.key,
      color: SERIES_COLORS[(colorIndex ? colorIndex(x.key, i) : i) % SERIES_COLORS.length],
      points: x.points,
    }));
  }, [m.series, filter, label, colorIndex, order]);
  const u = unit ?? m.series.find((x) => (filter ? filter(x.key) : true))?.unit ?? "";
  return (
    <TimeChart
      title={title}
      unit={u}
      series={series}
      from={m.from}
      to={m.to}
      stacked={stacked}
      yMax={yMax}
      height={height}
      bands={bands}
      dimmed={m.fetching && !m.loading && range.kind === "fixed"}
      empty={m.error ? String((m.error as Error).message) : empty}
    />
  );
}

/** Stable colour slot for job/task scopes: derived from the numeric id, never from rank. */
export function scopeColor(key: string): number {
  const id = Number(key.split(".")[1]);
  const kind = key.startsWith("task.") ? 4 : 0;
  return Number.isFinite(id) ? (id + kind) % 8 : 0;
}

export function scopeLabel(key: string): string {
  const [kind, id] = key.split(".");
  return kind === "task" ? `task ${id}` : `job ${id}`;
}
