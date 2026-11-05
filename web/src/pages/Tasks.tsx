import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "react-router-dom";
import { Plus } from "lucide-react";
import { api, fmtBytes, fmtTime, parseBytes, type TaskSummary } from "../api";
import { Button, Card, Empty, ErrorBox, Input, StateBadge, Table, Td, Th, cn } from "../components/ui";

export const ACTIVITY_STYLE: Record<TaskSummary["activity"], string> = {
  running: "bg-info/15 text-info",
  queued: "bg-muted text-muted-fg",
  warm: "bg-warning/15 text-warning",
  idle: "bg-muted text-muted-fg",
  closed: "bg-muted text-muted-fg",
};

export function ActivityBadge({ a }: { a: TaskSummary["activity"] }) {
  return (
    <span className={cn("inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-xs font-medium", ACTIVITY_STYLE[a])}>
      {a === "running" && <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-current" />}
      {a}
    </span>
  );
}

export default function Tasks({ isAdmin }: { isAdmin: boolean }) {
  const [all, setAll] = useState(false);
  const [creating, setCreating] = useState(false);
  const q = useQuery({
    queryKey: ["tasks", all],
    queryFn: () => api.tasks({ all, user: isAdmin ? "all" : undefined }),
    refetchInterval: 10000,
  });
  const tasks = q.data ?? [];

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-3">
        <h1 className="mr-auto text-base font-semibold">Tasks</h1>
        <label className="flex items-center gap-1 text-xs text-muted-fg">
          <input type="checkbox" checked={all} onChange={(e) => setAll(e.target.checked)} /> include closed
        </label>
        <Button variant="primary" onClick={() => setCreating((c) => !c)}>
          <Plus size={14} /> New task
        </Button>
      </div>
      <p className="max-w-3xl text-sm text-muted-fg">
        A task is a long-lived environment. Each step runs in it; a successful step is saved as a snapshot so the next step continues from there
        (installed packages, files outside <code>/work</code>). A failed step rolls back to the last snapshot. <code>/work</code> and <code>/out</code> are
        shared by all steps.
      </p>
      {creating && <CreateTask onDone={() => setCreating(false)} />}
      <Card>
        {q.error ? <ErrorBox error={q.error} /> : null}
        {tasks.length === 0 ? (
          <Empty>
            No tasks yet. Create one here, with <code>mlq task create</code>, or ask Claude to.
          </Empty>
        ) : (
          <Table>
            <thead>
              <tr className="border-b border-border">
                <Th>ID</Th>
                <Th>Name</Th>
                <Th>User</Th>
                <Th>Activity</Th>
                <Th className="text-right">Steps</Th>
                <Th className="text-right">Snapshots</Th>
                <Th>Defaults</Th>
                <Th>Last step</Th>
                <Th>Updated</Th>
              </tr>
            </thead>
            <tbody>
              {tasks.map((s) => (
                <tr key={s.task.id} className="border-b border-border/60 hover:bg-muted/60">
                  <Td className="tabular-nums text-muted-fg">{s.task.id}</Td>
                  <Td className="font-medium">
                    <Link className="hover:underline" to={`/tasks/${s.task.id}`}>
                      {s.task.name}
                    </Link>
                  </Td>
                  <Td>{s.task.user_name}</Td>
                  <Td>
                    <ActivityBadge a={s.activity} />
                  </Td>
                  <Td className="text-right tabular-nums">{s.steps}</Td>
                  <Td className="text-right tabular-nums">{s.snapshots}</Td>
                  <Td className="tabular-nums text-muted-fg">
                    {s.task.resources.cpus}c · {fmtBytes(s.task.resources.mem_bytes)} · {s.task.resources.gpus}g
                  </Td>
                  <Td className="max-w-[18rem]">
                    {s.last_step ? (
                      <span className="flex items-center gap-2">
                        <StateBadge state={s.last_step.state} />
                        <code className="truncate text-xs text-muted-fg">{s.last_step.command.join(" ")}</code>
                      </span>
                    ) : (
                      <span className="text-muted-fg">–</span>
                    )}
                  </Td>
                  <Td className="text-muted-fg">{fmtTime(s.task.updated_at)}</Td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}
      </Card>
    </div>
  );
}

function CreateTask({ onDone }: { onDone: () => void }) {
  const qc = useQueryClient();
  const nav = useNavigate();
  const [name, setName] = useState("");
  const [image, setImage] = useState("");
  const [cpus, setCpus] = useState("2");
  const [mem, setMem] = useState("8G");
  const [gpus, setGpus] = useState("0");
  const [warm, setWarm] = useState("10");
  const m = useMutation({
    mutationFn: () =>
      api.createTask({
        name: name.trim(),
        image: image.trim() || undefined,
        resources: { cpus: Number(cpus), mem_bytes: parseBytes(mem), gpus: Number(gpus) },
        keep_warm_secs: Math.round(Number(warm) * 60),
      }),
    onSuccess: (d) => {
      qc.invalidateQueries({ queryKey: ["tasks"] });
      onDone();
      nav(`/tasks/${d.summary.task.id}`);
    },
  });
  return (
    <Card title="New task">
      <form
        className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4"
        onSubmit={(e) => {
          e.preventDefault();
          m.mutate();
        }}
      >
        <label className="space-y-1 text-xs text-muted-fg sm:col-span-2">
          Name
          <Input className="w-full" value={name} onChange={(e) => setName(e.target.value)} placeholder="resnet50 sweep" autoFocus />
        </label>
        <label className="space-y-1 text-xs text-muted-fg sm:col-span-2">
          Base image (empty = server default)
          <Input className="w-full font-mono" value={image} onChange={(e) => setImage(e.target.value)} placeholder="docker.io/pytorch/pytorch:…" />
        </label>
        <label className="space-y-1 text-xs text-muted-fg">
          CPUs per step
          <Input className="w-full" value={cpus} onChange={(e) => setCpus(e.target.value)} />
        </label>
        <label className="space-y-1 text-xs text-muted-fg">
          Memory per step
          <Input className="w-full" value={mem} onChange={(e) => setMem(e.target.value)} />
        </label>
        <label className="space-y-1 text-xs text-muted-fg">
          GPUs per step
          <Input className="w-full" value={gpus} onChange={(e) => setGpus(e.target.value)} />
        </label>
        <label className="space-y-1 text-xs text-muted-fg">
          Keep warm (minutes, 0 = off)
          <Input className="w-full" value={warm} onChange={(e) => setWarm(e.target.value)} />
        </label>
        <div className="flex items-center gap-2 sm:col-span-2 lg:col-span-4">
          <Button type="submit" variant="primary" disabled={!name.trim() || m.isPending}>
            Create
          </Button>
          <Button type="button" variant="ghost" onClick={onDone}>
            Cancel
          </Button>
          {m.error ? <ErrorBox error={m.error} /> : null}
        </div>
      </form>
    </Card>
  );
}
