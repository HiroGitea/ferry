import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams, useSearchParams } from "react-router-dom";
import { ArrowLeft, Pin, Play, RotateCcw } from "lucide-react";
import { ACTIVE, TERMINAL, api, fmtBytes, fmtDuration, fmtTime, parseBytes, runtimeSecs, type TaskDetail as TD } from "../api";
import MetricChart from "../charts/MetricChart";
import TrainCharts from "../charts/TrainCharts";
import type { Band } from "../charts/TimeChart";
import type { Range } from "../charts/useMetrics";
import { useTick } from "../hooks";
import { Button, Card, Empty, ErrorBox, Input, StateBadge, Table, Td, Th, cn } from "../components/ui";
import { JobDrawer } from "./Jobs";
import { ActivityBadge } from "./Tasks";

export default function TaskDetail() {
  const { id } = useParams();
  const tid = Number(id);
  const [params, setParams] = useSearchParams();
  const selected = params.get("job") ? Number(params.get("job")) : null;
  const q = useQuery({ queryKey: ["tasks", "one", tid], queryFn: () => api.task(tid), refetchInterval: 4000 });
  if (q.error) return <ErrorBox error={q.error} />;
  const d = q.data;
  if (!d) return <Empty>Loading…</Empty>;
  return (
    <div className="space-y-4">
      <Header d={d} />
      {d.summary.task.state === "open" && <RunStep d={d} />}
      <div className="grid gap-4 xl:grid-cols-5">
        <div className="xl:col-span-3">
          <Steps d={d} onSelect={(j) => setParams({ job: String(j) })} selected={selected} />
        </div>
        <div className="xl:col-span-2">
          <Snapshots d={d} />
        </div>
      </div>
      {(() => {
        // Steps share the task's /out: any started step shows the task's training curves.
        const last = [...d.steps].reverse().find((j) => j.started_at);
        return last ? <TrainCharts jobId={last.id} live={d.steps.some((j) => ACTIVE.includes(j.state))} /> : null;
      })()}
      <TaskCharts d={d} />
      {selected != null && <JobDrawer id={selected} onClose={() => setParams({})} />}
    </div>
  );
}

function Header({ d }: { d: TD }) {
  const t = d.summary.task;
  const qc = useQueryClient();
  const nav = useNavigate();
  const inv = () => {
    qc.invalidateQueries({ queryKey: ["tasks"] });
  };
  const close = useMutation({ mutationFn: () => api.closeTask(t.id), onSuccess: inv });
  const reopen = useMutation({ mutationFn: () => api.reopenTask(t.id), onSuccess: inv });
  const del = useMutation({
    mutationFn: () => api.deleteTask(t.id),
    onSuccess: () => {
      inv();
      nav("/tasks");
    },
  });
  const [warm, setWarm] = useState<string | null>(null);
  const setKeep = useMutation({ mutationFn: (secs: number) => api.updateTask(t.id, { keep_warm_secs: secs }), onSuccess: () => { setWarm(null); inv(); } });
  const err = close.error || reopen.error || del.error || setKeep.error;

  return (
    <Card>
      <div className="flex flex-wrap items-start gap-4">
        <div className="mr-auto min-w-0 space-y-1">
          <Link to="/tasks" className="inline-flex items-center gap-1 text-xs text-muted-fg hover:underline">
            <ArrowLeft size={12} /> Tasks
          </Link>
          <div className="flex items-center gap-2">
            <h1 className="truncate text-lg font-semibold">
              <span className="text-muted-fg">#{t.id}</span> {t.name}
            </h1>
            <ActivityBadge a={d.summary.activity} />
          </div>
          <div className="text-xs text-muted-fg">
            {t.user_name} · created {fmtTime(t.created_at)} · defaults {t.resources.cpus} CPU, {fmtBytes(t.resources.mem_bytes)}, {t.resources.gpus} GPU
            {t.node_id ? ` · node ${t.node_id}` : ""}
          </div>
          <div className="text-xs">
            <span className="text-muted-fg">next step starts from </span>
            <code className="break-all">{d.head_image}</code>
          </div>
          {t.archive_path && (
            <div className="text-xs">
              <span className="text-muted-fg">archive </span>
              <code className="break-all">{t.archive_path}</code>
            </div>
          )}
        </div>
        <div className="flex flex-col items-end gap-2">
          <div className="flex items-center gap-2 text-xs">
            <span className="text-muted-fg">keep warm</span>
            <Input
              className="w-16"
              value={warm ?? String(Math.round(t.keep_warm_secs / 60))}
              onChange={(e) => setWarm(e.target.value)}
              aria-label="keep warm minutes"
            />
            <span className="text-muted-fg">min</span>
            {warm !== null && (
              <Button onClick={() => setKeep.mutate(Math.round(Number(warm) * 60))} disabled={setKeep.isPending}>
                Save
              </Button>
            )}
          </div>
          {t.warm_until && (
            <div className="text-xs text-warning">environment warm until {new Date(t.warm_until).toLocaleTimeString()}</div>
          )}
          <div className="flex gap-2">
            {t.state === "open" ? (
              <Button onClick={() => close.mutate()} disabled={close.isPending}>
                Close task
              </Button>
            ) : (
              <>
                <Button onClick={() => reopen.mutate()}>Reopen</Button>
                <Button variant="danger" onClick={() => confirm(`Delete task ${t.id}? Snapshots and the SSD workspace are removed; the archive is kept.`) && del.mutate()}>
                  Delete
                </Button>
              </>
            )}
          </div>
        </div>
      </div>
      {err ? <div className="mt-3"><ErrorBox error={err} /></div> : null}
    </Card>
  );
}

function RunStep({ d }: { d: TD }) {
  const qc = useQueryClient();
  const [cmd, setCmd] = useState("");
  const [cpus, setCpus] = useState("");
  const [mem, setMem] = useState("");
  const [gpus, setGpus] = useState("");
  const [time, setTime] = useState("");
  const m = useMutation({
    mutationFn: () =>
      api.submit({
        task_id: d.summary.task.id,
        command: ["sh", "-c", cmd],
        resources: {
          cpus: cpus ? Number(cpus) : null,
          mem_bytes: mem ? parseBytes(mem) : null,
          gpus: gpus ? Number(gpus) : null,
        },
        time_limit_secs: time ? Math.round(Number(time) * 60) : null,
      }),
    onSuccess: () => {
      setCmd("");
      qc.invalidateQueries({ queryKey: ["tasks"] });
    },
  });
  return (
    <Card title="Run a step">
      <form
        className="space-y-2"
        onSubmit={(e) => {
          e.preventDefault();
          m.mutate();
        }}
      >
        <textarea
          className="h-20 w-full rounded-md border border-border bg-bg p-2 font-mono text-sm outline-none focus:ring-2 focus:ring-info/40"
          placeholder="pip install -r requirements.txt   (runs with sh -c in /work)"
          value={cmd}
          onChange={(e) => setCmd(e.target.value)}
        />
        <div className="flex flex-wrap items-center gap-2 text-xs text-muted-fg">
          <Input className="w-20" placeholder={`${d.summary.task.resources.cpus} cpu`} value={cpus} onChange={(e) => setCpus(e.target.value)} aria-label="cpus" />
          <Input className="w-24" placeholder={fmtBytes(d.summary.task.resources.mem_bytes)} value={mem} onChange={(e) => setMem(e.target.value)} aria-label="memory" />
          <Input className="w-20" placeholder={`${d.summary.task.resources.gpus} gpu`} value={gpus} onChange={(e) => setGpus(e.target.value)} aria-label="gpus" />
          <Input className="w-28" placeholder="time limit (min)" value={time} onChange={(e) => setTime(e.target.value)} aria-label="time limit minutes" />
          <Button type="submit" variant="primary" disabled={!cmd.trim() || m.isPending}>
            <Play size={12} /> Run
          </Button>
          <span>empty fields use the task defaults</span>
        </div>
        {m.error ? <ErrorBox error={m.error} /> : null}
      </form>
    </Card>
  );
}

function Steps({ d, onSelect, selected }: { d: TD; onSelect: (id: number) => void; selected: number | null }) {
  const now = useTick(1000);
  const steps = [...d.steps].reverse();
  return (
    <Card title={`Steps (${d.steps.length})`}>
      {steps.length === 0 ? (
        <Empty>No steps yet.</Empty>
      ) : (
        <Table>
          <thead>
            <tr className="border-b border-border">
              <Th>Step</Th>
              <Th>State</Th>
              <Th>From → saved</Th>
              <Th>Runtime</Th>
              <Th>Command</Th>
            </tr>
          </thead>
          <tbody>
            {steps.map((j) => {
              const rt = runtimeSecs(j, now);
              return (
                <tr
                  key={j.id}
                  onClick={() => onSelect(j.id)}
                  className={cn("cursor-pointer border-b border-border/60 hover:bg-muted/60", selected === j.id && "bg-muted")}
                >
                  <Td className="tabular-nums text-muted-fg">#{j.id}</Td>
                  <Td>
                    <StateBadge state={j.state} />
                  </Td>
                  <Td className="whitespace-nowrap text-xs text-muted-fg tabular-nums">
                    {j.started_at ? (j.base_snapshot_id ? `snap ${j.base_snapshot_id}` : "base") : "–"}
                    {" → "}
                    {j.snapshot_id ? <span className="font-medium text-fg">snap {j.snapshot_id}</span> : TERMINAL.includes(j.state) ? (j.state === "succeeded" ? "–" : "rolled back") : "…"}
                  </Td>
                  <Td className="tabular-nums">{rt != null ? fmtDuration(rt) : ""}</Td>
                  <Td className="max-w-[22rem]">
                    <code className="block truncate text-xs" title={j.command.join(" ")}>
                      {j.command[0] === "sh" && j.command[1] === "-c" ? j.command.slice(2).join(" ") : j.command.join(" ")}
                    </code>
                    {j.reason && <div className="truncate text-xs text-muted-fg" title={j.reason}>{j.reason}</div>}
                  </Td>
                </tr>
              );
            })}
          </tbody>
        </Table>
      )}
    </Card>
  );
}

function Snapshots({ d }: { d: TD }) {
  const qc = useQueryClient();
  const t = d.summary.task;
  const inv = () => qc.invalidateQueries({ queryKey: ["tasks"] });
  const checkout = useMutation({ mutationFn: (sid: number | null) => api.checkout(t.id, sid), onSuccess: inv });
  const pin = useMutation({ mutationFn: (p: { sid: number; pinned: boolean }) => api.pinSnapshot(t.id, p.sid, p.pinned), onSuccess: inv });
  const snaps = [...d.snapshots].reverse();
  const open = t.state === "open";
  return (
    <Card
      title={`Snapshots (${d.snapshots.filter((s) => !s.deleted).length})`}
      action={
        open && t.head_snapshot_id !== null ? (
          <Button variant="ghost" onClick={() => confirm("Start the next step from the base image?") && checkout.mutate(null)} title="reset to base image">
            <RotateCcw size={12} /> base
          </Button>
        ) : null
      }
    >
      {snaps.length === 0 ? (
        <Empty>Snapshots appear after each successful step.</Empty>
      ) : (
        <ul className="divide-y divide-border">
          {snaps.map((s) => {
            const head = s.id === t.head_snapshot_id;
            return (
              <li key={s.id} className={cn("flex items-center gap-2 py-2 text-sm", s.deleted && "opacity-50")}>
                <span className={cn("h-2.5 w-2.5 shrink-0 rounded-full border-2", head ? "border-info bg-info" : "border-border")} />
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="font-medium tabular-nums">snap {s.id}</span>
                    {head && <span className="rounded bg-info/15 px-1.5 text-xs font-medium text-info">HEAD</span>}
                    {s.pinned && <span className="rounded bg-muted px-1.5 text-xs">pinned</span>}
                    {s.deleted && <span className="rounded bg-muted px-1.5 text-xs">pruned</span>}
                  </div>
                  <div className="truncate text-xs text-muted-fg">
                    from {s.parent_id ? `snap ${s.parent_id}` : "base"} · step #{s.job_id} · {s.size_bytes ? fmtBytes(s.size_bytes) : "?"} · {fmtTime(s.created_at)}
                    {s.note ? ` · ${s.note}` : ""}
                  </div>
                </div>
                {!s.deleted && (
                  <>
                    <Button variant="ghost" title={s.pinned ? "unpin" : "pin (never pruned)"} onClick={() => pin.mutate({ sid: s.id, pinned: !s.pinned })}>
                      <Pin size={12} className={cn(s.pinned && "fill-current")} />
                    </Button>
                    {open && !head && (
                      <Button onClick={() => checkout.mutate(s.id)} disabled={checkout.isPending}>
                        Checkout
                      </Button>
                    )}
                  </>
                )}
              </li>
            );
          })}
        </ul>
      )}
      {checkout.error || pin.error ? <ErrorBox error={checkout.error || pin.error} /> : null}
    </Card>
  );
}

function TaskCharts({ d }: { d: TD }) {
  const t = d.summary.task;
  const created = Math.floor(Date.parse(t.created_at) / 1000);
  const end = t.closed_at ? Math.floor(Date.parse(t.closed_at) / 1000) + 30 : null;
  const range: Range = useMemo(() => ({ kind: "fixed", after: created - 30, before: end }), [created, end]);
  const now = Math.floor(Date.now() / 1000);
  const bands: Band[] = d.steps
    .filter((j) => j.started_at)
    .map((j) => ({
      from: Math.floor(Date.parse(j.started_at!) / 1000),
      to: j.finished_at ? Math.floor(Date.parse(j.finished_at) / 1000) : now,
      label: `#${j.id}`,
    }));
  const scope = `task.${t.id}`;
  const one = (k: string) => () => k;
  return (
    <section className="space-y-2">
      <h2 className="text-xs font-semibold uppercase tracking-wide text-muted-fg">Resource usage over the task's lifetime (shaded = steps)</h2>
      <div className="grid gap-3 lg:grid-cols-2">
        <MetricChart title="CPU (% of one core)" keys={`${scope}.cpu`} range={range} bands={bands} label={one("cpu")} empty="no samples yet" />
        <MetricChart title="Memory" keys={`${scope}.mem`} range={range} bands={bands} label={one("memory")} empty="no samples yet" />
        <MetricChart title="GPU utilisation" keys={`${scope}.gpu_util`} range={range} bands={bands} yMax={100} label={one("gpu")} empty="no GPU steps" />
        <MetricChart title="GPU memory" keys={`${scope}.gpu_mem`} range={range} bands={bands} label={one("gpu memory")} empty="no GPU steps" />
        <MetricChart
          title="Disk I/O"
          keys={`${scope}.io_read,${scope}.io_write`}
          range={range}
          bands={bands}
          order={[`${scope}.io_read`, `${scope}.io_write`]}
          label={(k) => (k.endsWith("read") ? "read" : "write")}
          empty="no samples yet"
        />
      </div>
    </section>
  );
}
