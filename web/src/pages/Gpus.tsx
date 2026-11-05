import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { api, fmtBytes } from "../api";
import { Card, Empty, ErrorBox, Progress, cn } from "../components/ui";

export default function Gpus() {
  const q = useQuery({ queryKey: ["status"], queryFn: api.status, refetchInterval: 5000 });
  if (q.error) return <ErrorBox error={q.error} />;
  const s = q.data;
  if (!s) return <Empty>Loading…</Empty>;
  return (
    <div className="space-y-4">
      <Card title={`${s.node.hostname} · ${s.node.runtime}`}>
        <dl className="grid grid-cols-2 gap-x-6 gap-y-2 text-sm sm:grid-cols-4">
          <div>
            <dt className="text-xs uppercase text-muted-fg">CPUs</dt>
            <dd className="tabular-nums">
              {s.free_cpus.toFixed(1)} free / {s.node.cpus_managed} managed / {s.node.cpus_total} total
            </dd>
          </div>
          <div>
            <dt className="text-xs uppercase text-muted-fg">Memory</dt>
            <dd className="tabular-nums">
              {fmtBytes(s.free_mem)} free / {fmtBytes(s.node.mem_managed)} managed
            </dd>
          </div>
          <div>
            <dt className="text-xs uppercase text-muted-fg">GPUs</dt>
            <dd className="tabular-nums">
              {s.free_gpus.length} free / {s.node.gpus_managed.length} managed / {s.gpus.length} detected
            </dd>
          </div>
          <div>
            <dt className="text-xs uppercase text-muted-fg">Default image</dt>
            <dd className="truncate font-mono text-xs">{s.node.default_image}</dd>
          </div>
        </dl>
      </Card>
      {s.gpus.length === 0 ? (
        <Card>
          <Empty>No GPUs detected (NVML unavailable or gpu_provider = "none").</Empty>
        </Card>
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
          {s.gpus.map((g) => (
            <Card
              key={g.index}
              title={
                <span className="flex items-center gap-2">
                  <span className={cn("h-2 w-2 rounded-full", g.job_id ? "bg-info" : g.mem_used > 500 * 1024 * 1024 ? "bg-warning" : "bg-success")} />
                  GPU {g.index} · {g.name}
                </span>
              }
              action={
                <span className={cn("rounded-full px-2 py-0.5 text-xs", g.managed ? "bg-muted text-muted-fg" : "bg-warning/15 text-warning")}>
                  {g.managed ? "managed" : "reserved for host"}
                </span>
              }
            >
              <div className="space-y-3 text-sm">
                <div>
                  <div className="mb-1 flex justify-between text-xs text-muted-fg">
                    <span>Memory</span>
                    <span className="tabular-nums">
                      {fmtBytes(g.mem_used)} / {fmtBytes(g.mem_total)}
                    </span>
                  </div>
                  <Progress value={Math.round(g.mem_used / 1073741824)} max={Math.max(1, Math.round(g.mem_total / 1073741824))} />
                </div>
                <div className="flex justify-between">
                  <span className="text-muted-fg">Utilisation</span>
                  <span className="tabular-nums">{g.util_percent}%</span>
                </div>
                <div className="flex justify-between">
                  <span className="text-muted-fg">Temperature</span>
                  <span className="tabular-nums">{g.temp_c != null ? `${g.temp_c} °C` : "–"}</span>
                </div>
                <div className="flex justify-between">
                  <span className="text-muted-fg">Job</span>
                  <span>
                    {g.job_id ? (
                      <Link className="underline" to={`/jobs?job=${g.job_id}`}>
                        #{g.job_id}
                      </Link>
                    ) : g.mem_used > 500 * 1024 * 1024 ? (
                      <span className="text-warning">used outside mlq</span>
                    ) : (
                      "idle"
                    )}
                  </span>
                </div>
                <div className="truncate font-mono text-[11px] text-muted-fg">{g.uuid}</div>
              </div>
            </Card>
          ))}
        </div>
      )}
    </div>
  );
}
