import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { api, fmtBytes, fmtTime, type GpuInfo, type NodeStatus } from "../api";
import { Card, Empty, ErrorBox, Progress, cn } from "../components/ui";

export default function Gpus() {
  const q = useQuery({ queryKey: ["status"], queryFn: api.status, refetchInterval: 5000 });
  if (q.error) return <ErrorBox error={q.error} />;
  const s = q.data;
  if (!s) return <Empty>Loading…</Empty>;
  return (
    <div className="space-y-6">
      {s.nodes.map((n) => (
        <NodeSection key={n.info.id} n={n} many={s.nodes.length > 1} />
      ))}
    </div>
  );
}

function NodeSection({ n, many }: { n: NodeStatus; many: boolean }) {
  const title = (
    <span className="flex items-center gap-2">
      <span className={cn("h-2 w-2 rounded-full", n.online ? "bg-success" : "bg-danger")} />
      {n.info.id}
      {n.local && <span className="rounded bg-muted px-1.5 text-xs font-normal text-muted-fg">this machine</span>}
      <span className="font-normal text-muted-fg">
        · {n.info.hostname} · {n.info.runtime}
        {n.url ? ` · ${n.url}` : ""}
      </span>
    </span>
  );
  return (
    <section className="space-y-3">
      <Card
        title={title}
        action={!n.online ? <span className="text-xs text-danger">offline since {fmtTime(n.last_seen)}</span> : <span className="text-xs text-muted-fg">{n.running_jobs} running</span>}
      >
        <dl className="grid grid-cols-2 gap-x-6 gap-y-2 text-sm sm:grid-cols-5">
          <div>
            <dt className="text-xs uppercase text-muted-fg">CPUs</dt>
            <dd className="tabular-nums">
              {n.free_cpus.toFixed(1)} free / {n.info.cpus_managed} managed / {n.info.cpus_total} total
            </dd>
          </div>
          <div>
            <dt className="text-xs uppercase text-muted-fg">Memory</dt>
            <dd className="tabular-nums">
              {fmtBytes(n.free_mem)} free / {fmtBytes(n.info.mem_managed)} managed
            </dd>
          </div>
          <div>
            <dt className="text-xs uppercase text-muted-fg">GPUs</dt>
            <dd className="tabular-nums">
              {n.free_gpus.length} free / {n.info.gpus_managed.length} managed / {n.gpus.length} detected
            </dd>
          </div>
          <div>
            <dt className="text-xs uppercase text-muted-fg">Mounts</dt>
            <dd className="font-mono text-xs">{n.mounts.length ? n.mounts.join(", ") : "–"}</dd>
          </div>
          <div>
            <dt className="text-xs uppercase text-muted-fg">Default image</dt>
            <dd className="truncate font-mono text-xs">{n.info.default_image}</dd>
          </div>
        </dl>
      </Card>
      {n.gpus.length === 0 ? (
        <Card>
          <Empty>No GPUs detected on {n.info.id} (NVML unavailable or gpu_provider = "none").</Empty>
        </Card>
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
          {n.gpus.map((g) => (
            <GpuCard key={g.index} g={g} many={many} />
          ))}
        </div>
      )}
    </section>
  );
}

function GpuCard({ g, many }: { g: GpuInfo; many: boolean }) {
  return (
    <Card
      title={
        <span className="flex items-center gap-2">
          <span className={cn("h-2 w-2 rounded-full", g.job_id ? "bg-info" : g.mem_used > 500 * 1024 * 1024 ? "bg-warning" : "bg-success")} />
          {many ? `${g.node_id} · ` : ""}GPU {g.index} · {g.name}
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
              <span className="text-warning">used outside ferry</span>
            ) : (
              "idle"
            )}
          </span>
        </div>
        <div className="truncate font-mono text-[11px] text-muted-fg">{g.uuid}</div>
      </div>
    </Card>
  );
}
