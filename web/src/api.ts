// Typed client for the mlqd REST API. Mirrors crates/core/src/api.rs.

export type JobState =
  | "queued"
  | "held"
  | "starting"
  | "pulling"
  | "running"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "timeout";

export const TERMINAL: JobState[] = ["succeeded", "failed", "cancelled", "timeout"];
export const ACTIVE: JobState[] = ["starting", "pulling", "running"];

export interface ResourceRequest {
  cpus: number;
  mem_bytes: number;
  gpus: number;
}

export interface Job {
  id: number;
  user_id: number;
  user_name: string;
  node_id: string | null;
  name: string;
  image: string;
  command: string[];
  env: Record<string, string>;
  resources: ResourceRequest;
  state: JobState;
  priority: number;
  depends_on: number[];
  array_index: number | null;
  time_limit_secs: number | null;
  submitted_at: string;
  started_at: string | null;
  finished_at: string | null;
  exit_code: number | null;
  gpu_ids: number[];
  container_id: string | null;
  workspace_path: string | null;
  reason: string | null;
  archive_path: string | null;
  workspace_deleted: boolean;
}

export interface User {
  id: number;
  name: string;
  role: "admin" | "user";
  created_at: string;
}

export interface Quota {
  max_running_jobs: number | null;
  max_running_gpus: number | null;
  max_queued_jobs: number | null;
}

export interface Me {
  user: User;
  quota: Quota;
}

export interface GpuInfo {
  index: number;
  name: string;
  uuid: string;
  mem_total: number;
  mem_used: number;
  util_percent: number;
  temp_c: number | null;
  managed: boolean;
  job_id: number | null;
}

export interface ClusterStatus {
  node: {
    id: string;
    hostname: string;
    cpus_total: number;
    mem_total: number;
    cpus_managed: number;
    mem_managed: number;
    gpus_managed: number[];
    runtime: string;
    default_image: string;
  };
  gpus: GpuInfo[];
  queued_jobs: number;
  running_jobs: number;
  free_cpus: number;
  free_mem: number;
  free_gpus: number[];
  version: string;
}

export interface DailyUsage {
  day: string;
  gpu_seconds: number;
  cpu_seconds: number;
  jobs: number;
}

export interface UserUsage {
  user: User;
  quota: Quota;
  jobs_total: number;
  jobs_succeeded: number;
  jobs_failed: number;
  running_jobs: number;
  running_gpus: number;
  queued_jobs: number;
  gpu_seconds: number;
  cpu_seconds: number;
  fair_share_usage: number;
  daily: DailyUsage[];
}

export interface LogChunk {
  job_id: number;
  content: string;
  next_offset: number;
  state: JobState;
}

export interface ArtifactEntry {
  path: string;
  size: number;
  is_dir: boolean;
}

export interface JobResult {
  job: Job;
  artifacts: ArtifactEntry[];
  log_tail: string;
}

export interface TokenCreated {
  id: number;
  user: User;
  token: string;
}

const TOKEN_KEY = "mlq.token";

export function getToken(): string | null {
  try {
    return localStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

export function setToken(token: string | null) {
  try {
    if (token) localStorage.setItem(TOKEN_KEY, token);
    else localStorage.removeItem(TOKEN_KEY);
  } catch {
    /* private mode */
  }
}

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  const token = getToken();
  if (token) headers["Authorization"] = `Bearer ${token}`;
  if (body !== undefined) headers["Content-Type"] = "application/json";
  const res = await fetch(`/api/v1${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) {
    let msg = res.statusText;
    try {
      const j = await res.json();
      if (j && typeof j.error === "string") msg = j.error;
    } catch {
      /* not json */
    }
    throw new ApiError(res.status, msg);
  }
  return (await res.json()) as T;
}

function qs(params: Record<string, string | number | boolean | undefined | null>): string {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null && v !== "") p.set(k, String(v));
  }
  const s = p.toString();
  return s ? `?${s}` : "";
}

export const api = {
  me: () => request<Me>("GET", "/me"),
  status: () => request<ClusterStatus>("GET", "/status"),
  gpus: () => request<GpuInfo[]>("GET", "/gpus"),
  jobs: (p: { state?: string; user?: string; all?: boolean; limit?: number }) =>
    request<Job[]>("GET", `/jobs${qs(p)}`),
  job: (id: number) => request<Job>("GET", `/jobs/${id}`),
  cancel: (id: number) => request<Job>("DELETE", `/jobs/${id}`),
  release: (id: number) => request<Job>("POST", `/jobs/${id}/release`),
  log: (id: number, p: { tail?: number; offset?: number }) =>
    request<LogChunk>("GET", `/jobs/${id}/log${qs(p)}`),
  result: (id: number) => request<JobResult>("GET", `/jobs/${id}/result`),
  artifactUrl: (id: number, path: string) =>
    `/api/v1/jobs/${id}/artifacts/${path
      .split("/")
      .map(encodeURIComponent)
      .join("/")}?token=${encodeURIComponent(getToken() ?? "")}`,
  usage: (p: { user?: string; from?: string; to?: string }) =>
    request<UserUsage[]>("GET", `/usage${qs(p)}`),
  users: () => request<User[]>("GET", "/users"),
  createUser: (name: string, role: "admin" | "user") =>
    request<TokenCreated>("POST", "/users", { name, role }),
  deleteUser: (name: string) => request<unknown>("DELETE", `/users/${encodeURIComponent(name)}`),
  quota: (name: string) => request<Quota>("GET", `/users/${encodeURIComponent(name)}/quota`),
  setQuota: (name: string, q: Quota) =>
    request<Quota>("PUT", `/users/${encodeURIComponent(name)}/quota`, q),
  createToken: (p: { name?: string; user?: string }) => request<TokenCreated>("POST", "/tokens", p),
  sweep: () => request<unknown>("POST", "/retention/sweep"),
  eventsUrl: () => `/api/v1/events?token=${encodeURIComponent(getToken() ?? "")}`,
};

// ---- formatting helpers ----

export function fmtBytes(b: number): string {
  const units = ["B", "K", "M", "G", "T"];
  let v = b;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return i === 0 ? `${b}B` : `${v.toFixed(1)}${units[i]}`;
}

export function fmtDuration(secs: number): string {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = Math.floor(secs % 60);
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}` : `${m}:${String(s).padStart(2, "0")}`;
}

export function runtimeSecs(j: Job, now = Date.now()): number | null {
  if (!j.started_at) return null;
  const end = j.finished_at ? Date.parse(j.finished_at) : now;
  return Math.max(0, (end - Date.parse(j.started_at)) / 1000);
}

export function fmtTime(iso: string | null): string {
  if (!iso) return "";
  const d = new Date(iso);
  return d.toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

export function hours(secs: number): string {
  return (secs / 3600).toFixed(secs >= 36000 ? 0 : 1);
}
