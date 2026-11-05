import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, fmtBytes } from "../api";
import MetricChart, { scopeColor, scopeLabel } from "../charts/MetricChart";
import { useMetricStream } from "../charts/sync";
import { RANGES, type Range } from "../charts/useMetrics";
import { Stat, cn } from "../components/ui";

const GPU_LABEL = (k: string) => `GPU ${k.split(".")[1]}`;
const GPU_COLOR = (k: string) => Number(k.split(".")[1]) || 0;
const suffix = (s: string) => (k: string) => k.endsWith(`.${s}`);

export function RangePicker({ value, onChange }: { value: string; onChange: (id: string) => void }) {
  return (
    <div className="inline-flex overflow-hidden rounded-md border border-border bg-card text-xs" role="group" aria-label="time range">
      {RANGES.map((r) => (
        <button
          key={r.id}
          onClick={() => onChange(r.id)}
          className={cn("px-2.5 py-1.5 font-medium", value === r.id ? "bg-fg text-bg" : "text-muted-fg hover:bg-muted")}
          aria-pressed={value === r.id}
        >
          {r.id}
        </button>
      ))}
    </div>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="space-y-2">
      <h2 className="text-xs font-semibold uppercase tracking-wide text-muted-fg">{title}</h2>
      <div className="grid gap-3 lg:grid-cols-2 2xl:grid-cols-3">{children}</div>
    </section>
  );
}

export default function Monitor() {
  const [rangeId, setRangeId] = useState("5m");
  const secs = RANGES.find((r) => r.id === rangeId)!.secs;
  const range: Range = { kind: "live", secs };
  const { connected } = useMetricStream();
  const status = useQuery({ queryKey: ["status"], queryFn: api.status, refetchInterval: 5000 });
  const s = status.data;

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-center gap-3">
        <h1 className="mr-auto text-base font-semibold">Monitor</h1>
        <span className="inline-flex items-center gap-1.5 text-xs text-muted-fg">
          <span className={cn("h-2 w-2 rounded-full", connected ? "bg-success" : "bg-warning")} />
          {secs <= 900 ? (connected ? "live, per second" : "reconnecting") : "auto-refresh"}
        </span>
        <RangePicker value={rangeId} onChange={setRangeId} />
      </div>

      {s && (
        <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
          <Stat label="CPUs free" value={`${s.free_cpus.toFixed(1)} / ${s.node.cpus_managed}`} hint={`${s.node.cpus_total} on host`} />
          <Stat label="Memory free" value={fmtBytes(s.free_mem)} hint={`of ${fmtBytes(s.node.mem_managed)} managed`} />
          <Stat label="GPUs free" value={`${s.free_gpus.length} / ${s.node.gpus_managed.length}`} hint={`${s.gpus.length} detected`} />
          <Stat label="Jobs" value={`${s.running_jobs} / ${s.queued_jobs}`} hint="running / queued" />
        </div>
      )}

      <Section title="System">
        <MetricChart
          title="CPU"
          keys="host.cpu.*"
          range={range}
          stacked
          yMax={100}
          order={["host.cpu.user", "host.cpu.system", "host.cpu.iowait"]}
          label={(k) => k.split(".")[2]}
        />
        <MetricChart
          title="Memory"
          keys="host.mem.*"
          range={range}
          stacked
          order={["host.mem.used", "host.mem.cache"]}
          label={(k) => k.split(".")[2]}
        />
        <MetricChart title="Load average (1 min)" keys="host.load.1" range={range} label={() => "load"} />
        <MetricChart title="Disk I/O" keys="host.disk.*" range={range} order={["host.disk.read", "host.disk.write"]} label={(k) => k.split(".")[2]} />
        <MetricChart title="Network" keys="host.net.*" range={range} order={["host.net.rx", "host.net.tx"]} label={(k) => k.split(".")[2]} />
        <MetricChart
          title="Storage used"
          keys="host.scratch.used,host.archive.used"
          range={range}
          yMax={100}
          order={["host.scratch.used", "host.archive.used"]}
          label={(k) => (k.startsWith("host.scratch") ? "SSD scratch" : "NAS archive")}
        />
      </Section>

      <Section title="GPUs">
        <MetricChart title="GPU utilisation" keys="gpu.*" filter={suffix("util")} range={range} yMax={100} label={GPU_LABEL} colorIndex={GPU_COLOR} empty="no GPUs" />
        <MetricChart title="GPU memory" keys="gpu.*" filter={suffix("mem")} range={range} label={GPU_LABEL} colorIndex={GPU_COLOR} empty="no GPUs" />
        <MetricChart title="GPU temperature" keys="gpu.*" filter={suffix("temp")} range={range} label={GPU_LABEL} colorIndex={GPU_COLOR} empty="no GPUs" />
        <MetricChart title="GPU power" keys="gpu.*" filter={suffix("power")} range={range} label={GPU_LABEL} colorIndex={GPU_COLOR} empty="no GPUs" />
      </Section>

      <Section title="Scheduler">
        <MetricChart
          title="Jobs"
          keys="sched.running,sched.queued"
          range={range}
          order={["sched.running", "sched.queued"]}
          label={(k) => k.split(".")[1]}
        />
        <MetricChart title="GPUs in use" keys="sched.gpus_busy" range={range} label={() => "busy"} />
      </Section>

      <Section title="Jobs & tasks">
        <MetricChart title="CPU per job (% of one core)" keys="job.*,task.*" filter={suffix("cpu")} range={range} label={scopeLabel} colorIndex={scopeColor} empty="nothing running" />
        <MetricChart title="Memory per job" keys="job.*,task.*" filter={suffix("mem")} range={range} label={scopeLabel} colorIndex={scopeColor} empty="nothing running" />
        <MetricChart title="GPU utilisation per job" keys="job.*,task.*" filter={suffix("gpu_util")} range={range} yMax={100} label={scopeLabel} colorIndex={scopeColor} empty="no GPU jobs" />
        <MetricChart title="Disk writes per job" keys="job.*,task.*" filter={suffix("io_write")} range={range} label={scopeLabel} colorIndex={scopeColor} empty="nothing running" />
      </Section>
    </div>
  );
}
