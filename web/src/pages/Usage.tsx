import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Bar, BarChart, CartesianGrid, Legend, ResponsiveContainer, Tooltip, XAxis, YAxis } from "recharts";
import { api, hours, type UserUsage } from "../api";
import { Card, Empty, ErrorBox, Progress, Select, Stat, Table, Td, Th } from "../components/ui";

const SERIES = ["--series-1", "--series-2", "--series-3", "--series-4", "--series-5", "--series-6", "--series-7", "--series-8"];
const MAX_SERIES = 8;

function cssVar(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

export default function Usage({ isAdmin }: { isAdmin: boolean }) {
  const [days, setDays] = useState(30);
  const from = useMemo(() => new Date(Date.now() - days * 86400_000).toISOString(), [days]);
  const q = useQuery({
    queryKey: ["usage", days, isAdmin],
    queryFn: () => api.usage({ from, user: isAdmin ? "all" : undefined }),
    refetchInterval: 30000,
  });
  const users = q.data ?? [];

  // Stable series order: by user id, so a filter never repaints survivors.
  const ordered = useMemo(() => [...users].sort((a, b) => a.user.id - b.user.id), [users]);
  const shown = ordered.slice(0, MAX_SERIES);
  const other = ordered.slice(MAX_SERIES);

  const chart = useMemo(() => buildDaily(shown, other, days), [shown, other, days]);
  const totals = useMemo(
    () => ({
      gpu: users.reduce((s, u) => s + u.gpu_seconds, 0),
      cpu: users.reduce((s, u) => s + u.cpu_seconds, 0),
      jobs: users.reduce((s, u) => s + u.jobs_total, 0),
      failed: users.reduce((s, u) => s + u.jobs_failed, 0),
      running: users.reduce((s, u) => s + u.running_jobs, 0),
      queued: users.reduce((s, u) => s + u.queued_jobs, 0),
    }),
    [users],
  );
  const colors = useMemo(() => SERIES.map(cssVar), [q.dataUpdatedAt]);
  const surface = cssVar("--card") || "#fff";

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h1 className="text-base font-semibold">Usage &amp; quotas</h1>
        <Select value={days} onChange={(e) => setDays(Number(e.target.value))}>
          <option value={7}>last 7 days</option>
          <option value={30}>last 30 days</option>
          <option value={90}>last 90 days</option>
        </Select>
      </div>
      {q.error ? <ErrorBox error={q.error} /> : null}
      <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
        <Stat label="GPU hours" value={hours(totals.gpu)} hint={`last ${days} days`} />
        <Stat label="CPU hours" value={hours(totals.cpu)} hint={`last ${days} days`} />
        <Stat label="Jobs" value={totals.jobs} hint={`${totals.failed} failed`} />
        <Stat label="Now" value={`${totals.running} / ${totals.queued}`} hint="running / queued" />
      </div>

      <Card title="GPU hours per day">
        {chart.rows.length === 0 ? (
          <Empty>No GPU usage in this window.</Empty>
        ) : (
          <div className="h-64">
            <ResponsiveContainer width="100%" height="100%">
              <BarChart data={chart.rows} margin={{ top: 8, right: 8, left: 0, bottom: 0 }} barCategoryGap="25%">
                <CartesianGrid vertical={false} stroke={cssVar("--grid")} />
                <XAxis dataKey="day" tickLine={false} axisLine={{ stroke: cssVar("--axis") }} tick={{ fontSize: 11, fill: cssVar("--tick") }} tickFormatter={(d: string) => d.slice(5)} minTickGap={16} />
                <YAxis
                  tickLine={false}
                  axisLine={false}
                  tick={{ fontSize: 11, fill: cssVar("--tick") }}
                  width={48}
                  tickFormatter={(v: number) => (v >= 10 ? v.toFixed(0) : v >= 1 ? v.toFixed(1) : v === 0 ? "0" : v.toPrecision(2))}
                />
                <Tooltip
                  cursor={{ fill: cssVar("--muted"), opacity: 0.5 }}
                  contentStyle={{ background: surface, border: `1px solid ${cssVar("--border")}`, borderRadius: 8, fontSize: 12, color: cssVar("--fg") }}
                  formatter={(v) => [`${Number(v).toFixed(2)} h`]}
                />
                {chart.series.length > 1 && (
                  <Legend
                    wrapperStyle={{ fontSize: 12 }}
                    iconType="square"
                    iconSize={10}
                    formatter={(value) => <span style={{ color: cssVar("--muted-fg") }}>{value}</span>}
                  />
                )}
                {chart.series.map((name, i) => (
                  <Bar key={name} dataKey={name} stackId="gpu" fill={colors[i] ?? cssVar("--muted-fg")} stroke={surface} strokeWidth={1} maxBarSize={28} isAnimationActive={false} />
                ))}
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
      </Card>

      <Card title="Per user">
        {users.length === 0 ? (
          <Empty>No users.</Empty>
        ) : (
          <Table>
            <thead>
              <tr className="border-b border-border">
                <Th></Th>
                <Th>User</Th>
                <Th className="text-right">GPU h</Th>
                <Th className="text-right">CPU h</Th>
                <Th className="text-right">Jobs</Th>
                <Th>Running GPUs / quota</Th>
                <Th>Running jobs / quota</Th>
                <Th>Queued / quota</Th>
                <Th className="text-right" >Fair-share</Th>
              </tr>
            </thead>
            <tbody>
              {ordered.map((u, i) => (
                <tr key={u.user.id} className="border-b border-border/60">
                  <Td>
                    <span className="inline-block h-3 w-3 rounded-sm" style={{ background: i < MAX_SERIES ? colors[i] : cssVar("--muted-fg") }} />
                  </Td>
                  <Td className="font-medium">
                    {u.user.name}
                    {u.user.role === "admin" && <span className="ml-1 rounded bg-muted px-1 text-xs text-muted-fg">admin</span>}
                  </Td>
                  <Td className="text-right tabular-nums">{hours(u.gpu_seconds)}</Td>
                  <Td className="text-right tabular-nums">{hours(u.cpu_seconds)}</Td>
                  <Td className="text-right tabular-nums">
                    {u.jobs_total} <span className="text-muted-fg">({u.jobs_failed} failed)</span>
                  </Td>
                  <Td className="min-w-[10rem]">
                    <Progress value={u.running_gpus} max={u.quota.max_running_gpus} />
                  </Td>
                  <Td className="min-w-[10rem]">
                    <Progress value={u.running_jobs} max={u.quota.max_running_jobs} />
                  </Td>
                  <Td className="min-w-[10rem]">
                    <Progress value={u.queued_jobs} max={u.quota.max_queued_jobs} />
                  </Td>
                  <Td className="text-right tabular-nums" title="decayed GPU-hours; lower = scheduled first among equal priority">
                    {hours(u.fair_share_usage)} h
                  </Td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}
      </Card>
    </div>
  );
}

function buildDaily(shown: UserUsage[], other: UserUsage[], days: number): { rows: Record<string, string | number>[]; series: string[] } {
  const series = shown.map((u) => u.user.name);
  if (other.length) series.push("Other");
  const byDay = new Map<string, Record<string, string | number>>();
  const start = new Date(Date.now() - (days - 1) * 86400_000);
  for (let i = 0; i < days; i++) {
    const d = new Date(start.getTime() + i * 86400_000).toISOString().slice(0, 10);
    const row: Record<string, string | number> = { day: d };
    for (const s of series) row[s] = 0;
    byDay.set(d, row);
  }
  const add = (name: string, u: UserUsage) => {
    for (const d of u.daily) {
      const row = byDay.get(d.day);
      if (row) row[name] = (row[name] as number) + d.gpu_seconds / 3600;
    }
  };
  for (const u of shown) add(u.user.name, u);
  for (const u of other) add("Other", u);
  const rows = [...byDay.values()];
  const any = rows.some((r) => series.some((s) => (r[s] as number) > 0));
  return { rows: any ? rows : [], series };
}
