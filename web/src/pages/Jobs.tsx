import { useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useSearchParams } from "react-router-dom";
import { Download, RefreshCw, Square, X } from "lucide-react";
import { ACTIVE, TERMINAL, api, fmtBytes, fmtDuration, fmtTime, runtimeSecs, type Job } from "../api";
import MetricChart from "../charts/MetricChart";
import type { Range } from "../charts/useMetrics";
import { useTick } from "../hooks";
import { Button, Card, Empty, ErrorBox, Select, StateBadge, Table, Td, Th, cn } from "../components/ui";

export default function Jobs({ isAdmin }: { isAdmin: boolean }) {
  const [params, setParams] = useSearchParams();
  const [state, setState] = useState<string>("");
  const [showAll, setShowAll] = useState(true);
  const selected = params.get("job") ? Number(params.get("job")) : null;
  const now = useTick(1000);

  const q = useQuery({
    queryKey: ["jobs", state, showAll],
    queryFn: () => api.jobs({ state: state || undefined, all: showAll, limit: 300, user: isAdmin ? "all" : undefined }),
    refetchInterval: 10000,
  });

  const jobs = q.data ?? [];
  const counts = useMemo(() => {
    const c = { running: 0, queued: 0 };
    for (const j of jobs) {
      if (ACTIVE.includes(j.state)) c.running++;
      else if (j.state === "queued" || j.state === "held") c.queued++;
    }
    return c;
  }, [jobs]);

  return (
    <div className="space-y-4">
      <Card
        title={`Jobs · ${counts.running} running · ${counts.queued} queued`}
        action={
          <div className="flex items-center gap-2">
            <Select value={state} onChange={(e) => setState(e.target.value)}>
              <option value="">all states</option>
              {["queued", "held", "running", "succeeded", "failed", "cancelled", "timeout"].map((s) => (
                <option key={s} value={s}>
                  {s}
                </option>
              ))}
            </Select>
            <label className="flex items-center gap-1 text-xs text-muted-fg">
              <input type="checkbox" checked={showAll} onChange={(e) => setShowAll(e.target.checked)} /> include finished
            </label>
            <Button variant="ghost" onClick={() => q.refetch()} title="refresh">
              <RefreshCw size={14} />
            </Button>
          </div>
        }
      >
        {q.error ? <ErrorBox error={q.error} /> : null}
        {jobs.length === 0 ? (
          <Empty>No jobs. Submit one with <code>ferry submit</code> or ask Claude.</Empty>
        ) : (
          <Table>
            <thead>
              <tr className="border-b border-border">
                <Th>ID</Th>
                <Th>Name</Th>
                <Th>Task</Th>
                <Th>Node</Th>
                <Th>User</Th>
                <Th>State</Th>
                <Th>Resources</Th>
                <Th>GPUs</Th>
                <Th>Runtime</Th>
                <Th>Submitted</Th>
                <Th>Reason</Th>
              </tr>
            </thead>
            <tbody>
              {jobs.map((j) => {
                const rt = runtimeSecs(j, now);
                return (
                  <tr
                    key={j.id}
                    onClick={() => setParams({ job: String(j.id) })}
                    className={cn("cursor-pointer border-b border-border/60 hover:bg-muted/60", selected === j.id && "bg-muted")}
                  >
                    <Td className="tabular-nums text-muted-fg">{j.id}</Td>
                    <Td className="max-w-[16rem] truncate font-medium">{j.name}</Td>
                    <Td>
                      {j.task_id ? (
                        <Link className="text-xs hover:underline" to={`/tasks/${j.task_id}`} onClick={(e) => e.stopPropagation()}>
                          task {j.task_id}
                        </Link>
                      ) : (
                        <span className="text-xs text-muted-fg">–</span>
                      )}
                    </Td>
                    <Td className="text-xs text-muted-fg">{j.node_id ?? (j.requested_node ? `→ ${j.requested_node}` : "")}</Td>
                    <Td>{j.user_name}</Td>
                    <Td>
                      <StateBadge state={j.state} />
                    </Td>
                    <Td className="tabular-nums text-muted-fg">
                      {j.resources.cpus}c · {fmtBytes(j.resources.mem_bytes)} · {j.resources.gpus}g
                    </Td>
                    <Td>
                      <span className="flex gap-1">
                        {j.gpu_ids.map((g) => (
                          <span key={g} className="rounded bg-success/15 px-1.5 text-xs font-medium text-success">
                            {g}
                          </span>
                        ))}
                      </span>
                    </Td>
                    <Td className="tabular-nums">{rt != null ? fmtDuration(rt) : ""}</Td>
                    <Td className="text-muted-fg">{fmtTime(j.submitted_at)}</Td>
                    <Td className="max-w-[16rem] truncate text-muted-fg" title={j.reason ?? ""}>
                      {j.reason}
                    </Td>
                  </tr>
                );
              })}
            </tbody>
          </Table>
        )}
      </Card>
      {selected != null && <JobDrawer id={selected} onClose={() => setParams({})} />}
    </div>
  );
}

export function JobDrawer({ id, onClose }: { id: number; onClose: () => void }) {
  const qc = useQueryClient();
  const job = useQuery({ queryKey: ["jobs", "one", id], queryFn: () => api.job(id), refetchInterval: 3000 });
  const result = useQuery({
    queryKey: ["jobs", "result", id, job.data?.state],
    queryFn: () => api.result(id),
    enabled: !!job.data && TERMINAL.includes(job.data.state),
  });
  const cancel = useMutation({
    mutationFn: () => api.cancel(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["jobs"] }),
  });
  const me = useQuery({ queryKey: ["me"], queryFn: api.me });
  const preempt = useMutation({
    mutationFn: () => api.preempt(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["jobs"] }),
  });
  const j = job.data;
  const terminal = j ? TERMINAL.includes(j.state) : false;

  return (
    <div className="fixed inset-y-0 right-0 z-20 flex w-full max-w-2xl flex-col border-l border-border bg-card shadow-xl">
      <header className="flex items-center justify-between border-b border-border px-4 py-3">
        <div className="min-w-0">
          <div className="truncate text-sm font-semibold">
            #{id} {j?.name}
          </div>
          <div className="text-xs text-muted-fg">
            {j?.user_name} · {j?.image}
          </div>
        </div>
        <div className="flex items-center gap-2">
          {j && ACTIVE.includes(j.state) && me.data?.user.role === "admin" && (
            <Button
              title="Kill it now and put it back in the queue (the owner is notified)"
              onClick={() => confirm(`Preempt job ${id}? It is killed and requeued.`) && preempt.mutate()}
              disabled={preempt.isPending}
            >
              Preempt
            </Button>
          )}
          {j && !terminal && (
            <Button variant="danger" onClick={() => cancel.mutate()} disabled={cancel.isPending}>
              <Square size={12} /> Cancel
            </Button>
          )}
          <Button variant="ghost" onClick={onClose}>
            <X size={16} />
          </Button>
        </div>
      </header>
      <div className="flex-1 space-y-4 overflow-y-auto p-4">
        {job.error ? <ErrorBox error={job.error} /> : null}
        {j && (
          <dl className="grid grid-cols-2 gap-x-4 gap-y-1 text-sm sm:grid-cols-3">
            <Field k="state" v={<StateBadge state={j.state} />} />
            <Field k="exit code" v={j.exit_code ?? "–"} />
            <Field k="GPUs" v={j.gpu_ids.join(", ") || "–"} />
            <Field k="node" v={j.node_id ?? j.requested_node ?? "any"} />
            <Field k="resources" v={`${j.resources.cpus} cpu · ${fmtBytes(j.resources.mem_bytes)} · ${j.resources.gpus} gpu`} />
            <Field k="submitted" v={fmtTime(j.submitted_at)} />
            <Field k="started" v={fmtTime(j.started_at) || "–"} />
            <Field k="finished" v={fmtTime(j.finished_at) || "–"} />
            <Field k="time limit" v={j.time_limit_secs ? fmtDuration(j.time_limit_secs) : "none"} />
            <Field k="priority" v={j.priority} />
            {j.reason && <Field k="reason" v={j.reason} wide />}
            <Field k="command" v={<code className="break-all text-xs">{j.command.join(" ")}</code>} wide />
            {j.archive_path && <Field k="archived to" v={<code className="break-all text-xs">{j.archive_path}</code>} wide />}
            {j.workspace_deleted && !j.archive_path && <Field k="workspace" v={<span className="text-warning">removed, no archive</span>} wide />}
          </dl>
        )}
        {result.data && (
          <section>
            <h3 className="mb-1 text-xs font-medium uppercase text-muted-fg">Artifacts (/out)</h3>
            {result.data.artifacts.filter((a) => !a.is_dir).length === 0 ? (
              <div className="text-sm text-muted-fg">none — write files to /out</div>
            ) : (
              <ul className="divide-y divide-border rounded-md border border-border text-sm">
                {result.data.artifacts
                  .filter((a) => !a.is_dir)
                  .map((a) => (
                    <li key={a.path} className="flex items-center justify-between px-3 py-1.5">
                      <span className="truncate font-mono text-xs">{a.path}</span>
                      <span className="flex items-center gap-3 text-xs text-muted-fg">
                        <span className="tabular-nums">{fmtBytes(a.size)}</span>
                        <a className="inline-flex items-center gap-1 underline" href={api.artifactUrl(id, a.path)} download>
                          <Download size={12} /> download
                        </a>
                      </span>
                    </li>
                  ))}
              </ul>
            )}
          </section>
        )}
        {j && j.started_at && <JobCharts job={j} />}
        <LogView id={id} live={!terminal} />
      </div>
    </div>
  );
}

function Field({ k, v, wide }: { k: string; v: React.ReactNode; wide?: boolean }) {
  return (
    <div className={cn(wide && "col-span-2 sm:col-span-3")}>
      <dt className="text-xs uppercase text-muted-fg">{k}</dt>
      <dd>{v}</dd>
    </div>
  );
}

function LogView({ id, live }: { id: number; live: boolean }) {
  const [text, setText] = useState("");
  const offset = useRef<number>(0);
  const box = useRef<HTMLPreElement>(null);

  useEffect(() => {
    let stop = false;
    offset.current = 0;
    setText("");
    const tick = async () => {
      try {
        const first = offset.current === 0;
        const chunk = first ? await api.log(id, { tail: 500 }) : await api.log(id, { offset: offset.current });
        if (stop) return;
        if (chunk.content) {
          setText((t) => (first ? chunk.content : t + chunk.content));
        }
        offset.current = chunk.next_offset;
        if (!TERMINAL.includes(chunk.state) || first) {
          window.setTimeout(tick, TERMINAL.includes(chunk.state) ? 0 : 1000);
          if (TERMINAL.includes(chunk.state)) stop = true;
        }
      } catch {
        if (!stop) window.setTimeout(tick, 3000);
      }
    };
    tick();
    return () => {
      stop = true;
    };
  }, [id, live]);

  useEffect(() => {
    if (box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [text]);

  return (
    <section>
      <h3 className="mb-1 text-xs font-medium uppercase text-muted-fg">Log {live && <span className="text-info">· live</span>}</h3>
      <pre ref={box} className="max-h-[50vh] overflow-auto rounded-md border border-border bg-bg p-3 font-mono text-xs leading-relaxed">
        {text || <span className="text-muted-fg">(empty)</span>}
      </pre>
    </section>
  );
}

function JobCharts({ job }: { job: Job }) {
  const start = Math.floor(Date.parse(job.started_at!) / 1000);
  const end = job.finished_at ? Math.floor(Date.parse(job.finished_at) / 1000) : null;
  const range: Range = useMemo(() => ({ kind: "fixed", after: start - 5, before: end ? end + 5 : null }), [start, end]);
  const scope = job.task_id ? `task.${job.task_id}` : `job.${job.id}`;
  const one = (k: string) => () => k;
  return (
    <section className="space-y-2">
      <h3 className="text-xs font-medium uppercase text-muted-fg">Resources during this {job.task_id ? "step" : "job"}</h3>
      <div className="grid gap-2 sm:grid-cols-2">
        <MetricChart title="CPU" keys={`${scope}.cpu`} range={range} height={110} label={one("% of a core")} points={200} />
        <MetricChart title="Memory" keys={`${scope}.mem`} range={range} height={110} label={one("used")} points={200} />
        {job.gpu_ids.length > 0 && (
          <>
            <MetricChart title="GPU utilisation" keys={`${scope}.gpu_util`} range={range} height={110} yMax={100} label={one("util")} points={200} />
            <MetricChart title="GPU memory" keys={`${scope}.gpu_mem`} range={range} height={110} label={one("used")} points={200} />
          </>
        )}
      </div>
    </section>
  );
}

export type { Job };
