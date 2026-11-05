import { fmtBytes } from "../api";

/** Human formatting for a metric value given its unit. */
export function fmtValue(v: number | null | undefined, unit: string): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return "–";
  switch (unit) {
    case "B":
      return fmtBytes(Math.max(0, v));
    case "B/s":
      return `${fmtBytes(Math.max(0, v))}/s`;
    case "%":
      return `${v.toFixed(v >= 10 ? 0 : 1)}%`;
    case "W":
      return `${v.toFixed(0)} W`;
    case "°C":
      return `${v.toFixed(0)} °C`;
    case "load":
      return v.toFixed(2);
    case "num":
      return fmtNum(v);
    default:
      return Number.isInteger(v) ? String(v) : v.toFixed(1);
  }
}

/** Short axis label for a value. */
export function fmtTick(v: number, unit: string): string {
  if (unit === "B" || unit === "B/s") return fmtBytes(Math.max(0, v));
  if (unit === "%") return `${Math.round(v)}%`;
  if (unit === "num") return fmtNum(v);
  if (Math.abs(v) >= 1000) return `${(v / 1000).toFixed(1)}k`;
  return Number.isInteger(v) ? String(v) : v.toFixed(1);
}

/** Plain number with sensible precision (training metrics: loss 0.0123, lr 3e-4, step 12k). */
export function fmtNum(v: number): string {
  const a = Math.abs(v);
  if (a === 0) return "0";
  if (a >= 1e6) return `${(v / 1e6).toPrecision(3)}M`;
  if (a >= 1e4) return `${(v / 1e3).toPrecision(3)}k`;
  if (Number.isInteger(v)) return String(v);
  if (a < 1e-3) return v.toExponential(1);
  return String(Number(v.toPrecision(4)));
}

export function fmtClock(t: number, span: number): string {
  const d = new Date(t * 1000);
  if (span <= 3600) return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  if (span <= 2 * 86400) return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric" }) + " " + d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
}

/** A "nice" axis maximum >= v. */
export function niceMax(v: number): number {
  if (!(v > 0)) return 1;
  const exp = Math.pow(10, Math.floor(Math.log10(v)));
  const f = v / exp;
  const nf = f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10;
  return nf * exp;
}

/** Byte-scaled nice max (powers of 1024 * 1/2/4/8). */
export function niceBytesMax(v: number): number {
  if (!(v > 0)) return 1024;
  let unit = 1;
  while (v / unit >= 1024) unit *= 1024;
  const f = v / unit;
  const steps = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024];
  return (steps.find((s) => s >= f) ?? 1024) * unit;
}
