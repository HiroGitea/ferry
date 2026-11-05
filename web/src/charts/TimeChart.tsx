import { useMemo, useRef, useState } from "react";
import { cn } from "../components/ui";
import { fmtClock, fmtTick, fmtValue, niceBytesMax, niceMax } from "./format";
import { useChartSync } from "./sync";

export interface ChartSeries {
  key: string;
  label: string;
  /** CSS color (use the categorical slot variables, in fixed order). */
  color: string;
  points: Array<[number, number | null]>;
}

export interface Band {
  from: number;
  to: number;
  label: string;
}

interface Props {
  title: string;
  unit: string;
  series: ChartSeries[];
  from: number;
  to: number;
  /** Stack series as areas (e.g. CPU user/system/iowait). */
  stacked?: boolean;
  /** Fixed y maximum (e.g. 100 for %). */
  yMax?: number;
  height?: number;
  /** Shaded spans (e.g. task steps). */
  bands?: Band[];
  dimmed?: boolean;
  empty?: string;
  /** Non-time x axis (e.g. training step): label for an x value. */
  xFormat?: (x: number) => string;
}

const PAD = { l: 52, r: 10, t: 8, b: 20 };

/** Value of a series at time t: the last point at or before t (within 3 steps). */
function valueAt(points: Array<[number, number | null]>, t: number, step: number): number | null {
  let lo = 0;
  let hi = points.length - 1;
  let best = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (points[mid][0] <= t) {
      best = mid;
      lo = mid + 1;
    } else hi = mid - 1;
  }
  if (best < 0) return null;
  const [pt, v] = points[best];
  return t - pt <= step * 3 ? v : null;
}

function lastValue(points: Array<[number, number | null]>): number | null {
  for (let i = points.length - 1; i >= 0; i--) if (points[i][1] !== null) return points[i][1];
  return null;
}

function medianStep(points: Array<[number, number | null]>): number {
  if (points.length < 2) return 1;
  const d: number[] = [];
  for (let i = 1; i < Math.min(points.length, 50); i++) d.push(points[i][0] - points[i - 1][0]);
  d.sort((a, b) => a - b);
  return Math.max(1, d[d.length >> 1]);
}

export default function TimeChart({ title, unit, series, from, to, stacked, yMax, height = 150, bands = [], dimmed, empty, xFormat }: Props) {
  const { hoverT, setHoverT } = useChartSync();
  const ref = useRef<SVGSVGElement>(null);
  const [width, setWidth] = useState(600);
  const [localHover, setLocalHover] = useState(false);
  const span = Math.max(1, to - from);

  // Track container width.
  const measure = (el: SVGSVGElement | null) => {
    ref.current = el;
    if (el) {
      const w = el.parentElement?.clientWidth ?? 600;
      if (Math.abs(w - width) > 2) setWidth(w);
      if (!(el as unknown as { _ro?: ResizeObserver })._ro) {
        const ro = new ResizeObserver(() => {
          const w2 = el.parentElement?.clientWidth ?? 600;
          setWidth((old) => (Math.abs(w2 - old) > 2 ? w2 : old));
        });
        ro.observe(el.parentElement!);
        (el as unknown as { _ro?: ResizeObserver })._ro = ro;
      }
    }
  };

  const step = useMemo(() => Math.min(...series.map((s) => medianStep(s.points)), Infinity) || 1, [series]);
  const hasData = series.some((s) => s.points.some(([t, v]) => v !== null && t >= from && t <= to));

  // Stack: cumulative values per series index at each point's timestamp.
  const geometry = useMemo(() => {
    const iw = Math.max(10, width - PAD.l - PAD.r);
    const ih = height - PAD.t - PAD.b;
    const x = (t: number) => PAD.l + ((t - from) / span) * iw;
    let max = 0;
    const stackedPts: Array<Array<[number, number | null, number]>> = []; // [t, top, base]
    if (stacked) {
      const times = Array.from(new Set(series.flatMap((s) => s.points.map((p) => p[0])))).sort((a, b) => a - b);
      const lookup = series.map((s) => new Map(s.points.map((p) => [p[0], p[1]])));
      const base = new Map<number, number>();
      series.forEach((_, i) => {
        const arr: Array<[number, number | null, number]> = [];
        for (const t of times) {
          const v = lookup[i].get(t);
          const b = base.get(t) ?? 0;
          if (v === undefined || v === null) {
            arr.push([t, null, b]);
          } else {
            arr.push([t, b + v, b]);
            base.set(t, b + v);
            if (t >= from && t <= to) max = Math.max(max, b + v);
          }
        }
        stackedPts.push(arr);
      });
    } else {
      for (const s of series) for (const [t, v] of s.points) if (v !== null && t >= from && t <= to) max = Math.max(max, v);
    }
    const top = yMax ?? (unit === "B" || unit === "B/s" ? niceBytesMax(max * 1.05) : niceMax(max * 1.05));
    const y = (v: number) => PAD.t + ih - (Math.min(v, top) / top) * ih;
    // A missing collection or two must not break the line; longer holes do.
    const gap = Math.max(step * 3, 3);
    const paths = series.map((s, i) => {
      if (stacked) {
        const pts = stackedPts[i];
        let d = "";
        let seg: Array<[number, number, number]> = [];
        const flush = () => {
          if (seg.length) {
            const upper = seg.map(([t, v], j) => `${j ? "L" : "M"}${x(t).toFixed(1)},${y(v).toFixed(1)}`).join("");
            const lower = seg
              .slice()
              .reverse()
              .map(([t, , b]) => `L${x(t).toFixed(1)},${y(b).toFixed(1)}`)
              .join("");
            d += upper + lower + "Z";
          }
          seg = [];
        };
        let prevT = -Infinity;
        for (const [t, v, b] of pts) {
          if (v === null || t - prevT > gap) flush();
          if (v !== null) seg.push([t, v, b]);
          prevT = t;
        }
        flush();
        return { area: d, line: "" };
      }
      let d = "";
      let prevT = -Infinity;
      let pen = false;
      for (const [t, v] of s.points) {
        if (v === null || t < from - step || t > to + step) {
          pen = false;
          continue;
        }
        const cmd = pen && t - prevT <= gap ? "L" : "M";
        d += `${cmd}${x(t).toFixed(1)},${y(v).toFixed(1)}`;
        pen = true;
        prevT = t;
      }
      return { area: "", line: d };
    });
    const ticksY = [0, top / 2, top];
    const nx = Math.max(2, Math.min(6, Math.floor(iw / 110)));
    const ticksX = Array.from({ length: nx + 1 }, (_, i) => from + (span * i) / nx);
    return { x, y, iw, ih, paths, ticksY, ticksX, top };
  }, [series, from, to, span, width, height, stacked, yMax, unit, step]);

  const onMove = (e: React.PointerEvent<SVGSVGElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    const px = e.clientX - r.left;
    if (px < PAD.l || px > width - PAD.r) return setHoverT(null);
    const t = from + ((px - PAD.l) / geometry.iw) * span;
    // snap to the nearest data timestamp of the first series with data
    const ref0 = series.find((s) => s.points.length)?.points;
    let snapped = t;
    if (ref0) {
      let best = Infinity;
      for (const [pt] of ref0) {
        const dd = Math.abs(pt - t);
        if (dd < best) {
          best = dd;
          snapped = pt;
        }
      }
    }
    setHoverT(snapped);
  };

  const showT = hoverT !== null && hoverT >= from && hoverT <= to ? hoverT : null;
  const readout = series.map((s) => ({
    ...s,
    value: showT !== null ? valueAt(s.points, showT, step) : lastValue(s.points),
  }));

  return (
    <div className={cn("rounded-xl border border-border bg-card px-3 pb-2 pt-2.5 transition-opacity", dimmed && "opacity-60")}>
      <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
        <h3 className="text-[13px] font-semibold">{title}</h3>
        {/* Legend doubles as the value readout: latest value, or the value under the crosshair. */}
        <div className="flex flex-wrap items-center gap-x-3 gap-y-0.5 text-xs">
          {readout.map((s) => (
            <span key={s.key} className="inline-flex items-center gap-1.5">
              <span className="inline-block h-[3px] w-3 rounded-full" style={{ background: s.color }} />
              <span className="text-muted-fg">{s.label}</span>
              <span className="font-medium tabular-nums text-fg">{fmtValue(s.value, unit)}</span>
            </span>
          ))}
        </div>
      </div>
      <div className="relative mt-1">
        <svg
          ref={measure}
          width={width}
          height={height}
          className="block touch-none select-none"
          onPointerMove={onMove}
          onPointerEnter={() => setLocalHover(true)}
          onPointerLeave={() => {
            setLocalHover(false);
            setHoverT(null);
          }}
          role="img"
          aria-label={`${title} chart`}
        >
          {(() => {
            // Step bands; labels are skipped where they would collide with the previous one.
            let lastLabelEnd = -Infinity;
            return bands.map((b, i) => {
              const x1 = geometry.x(Math.max(from, b.from));
              const x2 = geometry.x(Math.min(to, b.to));
              if (x2 <= PAD.l || x1 >= width - PAD.r) return null;
              const w = Math.max(1.5, x2 - x1);
              const labelW = b.label.length * 6 + 4;
              const showLabel = x1 >= lastLabelEnd;
              if (showLabel) lastLabelEnd = x1 + labelW;
              return (
                <g key={i}>
                  <rect x={x1} y={PAD.t} width={w} height={geometry.ih} fill="var(--fg)" opacity={0.06} />
                  {showLabel && (
                    <text x={x1 + 2} y={PAD.t + 10} fontSize={10} fill="var(--tick)">
                      {b.label}
                    </text>
                  )}
                </g>
              );
            });
          })()}
          {geometry.ticksY.map((v, i) => (
            <g key={i}>
              <line x1={PAD.l} x2={width - PAD.r} y1={geometry.y(v)} y2={geometry.y(v)} stroke={i === 0 ? "var(--axis)" : "var(--grid)"} strokeWidth={1} />
              <text x={PAD.l - 6} y={geometry.y(v) + 3.5} textAnchor="end" fontSize={10} fill="var(--tick)" className="tabular-nums">
                {fmtTick(v, unit)}
              </text>
            </g>
          ))}
          {geometry.ticksX.map((t, i) => (
            <text
              key={i}
              x={geometry.x(t)}
              y={height - 5}
              textAnchor={i === 0 ? "start" : i === geometry.ticksX.length - 1 ? "end" : "middle"}
              fontSize={10}
              fill="var(--tick)"
              className="tabular-nums"
            >
              {xFormat ? xFormat(t) : fmtClock(t, span)}
            </text>
          ))}
          {geometry.paths.map((p, i) =>
            stacked ? (
              <path key={series[i].key} d={p.area} fill={series[i].color} fillOpacity={0.85} stroke="var(--card)" strokeWidth={1} />
            ) : (
              <path key={series[i].key} d={p.line} fill="none" stroke={series[i].color} strokeWidth={2} strokeLinejoin="round" strokeLinecap="round" />
            ),
          )}
          {showT !== null && (
            <line x1={geometry.x(showT)} x2={geometry.x(showT)} y1={PAD.t} y2={PAD.t + geometry.ih} stroke="var(--muted-fg)" strokeWidth={1} />
          )}
          {!hasData && (
            <text x={PAD.l + geometry.iw / 2} y={PAD.t + geometry.ih / 2} textAnchor="middle" fontSize={12} fill="var(--tick)">
              {empty ?? "no data in this window"}
            </text>
          )}
        </svg>
        {localHover && showT !== null && hasData && (
          <div
            className="pointer-events-none absolute top-1 z-10 rounded-md border border-border bg-card px-2 py-1.5 text-xs shadow-md"
            style={geometry.x(showT) > width / 2 ? { right: width - geometry.x(showT) + 8 } : { left: geometry.x(showT) + 8 }}
          >
            <div className="mb-0.5 text-[11px] text-muted-fg tabular-nums">{xFormat ? xFormat(showT) : new Date(showT * 1000).toLocaleString()}</div>
            {readout.map((s) => (
              <div key={s.key} className="flex items-center gap-2">
                <span className="inline-block h-[2px] w-3" style={{ background: s.color }} />
                <span className="font-semibold tabular-nums">{fmtValue(s.value, unit)}</span>
                <span className="text-muted-fg">{s.label}</span>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

export const SERIES_COLORS = Array.from({ length: 8 }, (_, i) => `var(--series-${i + 1})`);
