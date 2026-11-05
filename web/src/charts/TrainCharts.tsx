import { useQuery } from "@tanstack/react-query";
import { api, ApiError } from "../api";
import TimeChart, { SERIES_COLORS } from "./TimeChart";
import { ChartSyncProvider } from "./sync";
import { fmtNum } from "./format";

/** Training curves from the job's /out/metrics.jsonl; one small chart per metric, shared crosshair. */
export default function TrainCharts({ jobId, live, title = "Training metrics" }: { jobId: number; live: boolean; title?: string }) {
  const q = useQuery({
    queryKey: ["train", jobId],
    queryFn: () => api.trainMetrics(jobId),
    refetchInterval: live ? 10000 : false,
    retry: false,
  });
  if (q.error) {
    if (q.error instanceof ApiError && q.error.status === 404) {
      return (
        <section className="space-y-1">
          <h3 className="text-xs font-medium uppercase text-muted-fg">{title}</h3>
          <p className="text-xs text-muted-fg">
            Append one JSON object per line to <code>/out/metrics.jsonl</code> to see curves here, e.g.{" "}
            <code>{'{"step": 100, "loss": 0.42}'}</code>.
          </p>
        </section>
      );
    }
    return null;
  }
  const m = q.data;
  if (!m) return null;
  const names = Object.keys(m.series);
  const xs = names.flatMap((n) => m.series[n].map((p) => p[0]));
  const from = xs.length ? Math.min(...xs) : 0;
  const to = xs.length ? Math.max(...xs) : 1;
  const xFormat = (x: number) => `${m.x_key} ${fmtNum(Math.round(x * 100) / 100)}`;
  return (
    <ChartSyncProvider>
      <section className="space-y-2">
        <h3 className="text-xs font-medium uppercase text-muted-fg">
          {title} <span className="normal-case">· /out/{m.file}{m.truncated ? " (last 32 MiB)" : ""}</span>
        </h3>
        {names.length === 0 ? (
          <p className="text-xs text-muted-fg">No numeric fields yet ({m.lines} lines).</p>
        ) : (
          <div className="grid gap-2 sm:grid-cols-2">
            {names.map((n, i) => (
              <TimeChart
                key={n}
                title={n}
                unit="num"
                height={120}
                from={from}
                to={Math.max(to, from + 1)}
                xFormat={xFormat}
                series={[{ key: n, label: n, color: SERIES_COLORS[i % SERIES_COLORS.length], points: m.series[n] }]}
              />
            ))}
          </div>
        )}
      </section>
    </ChartSyncProvider>
  );
}
