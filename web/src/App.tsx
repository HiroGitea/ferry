import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { NavLink, Navigate, Route, Routes } from "react-router-dom";
import { Activity, BarChart3, Cpu, ListTodo, LogOut, Moon, Sun, Users as UsersIcon } from "lucide-react";
import { api, getToken, setToken } from "./api";
import { useLiveUpdates } from "./hooks";
import { cn } from "./components/ui";
import Login from "./pages/Login";
import Usage from "./pages/Usage";
import Jobs from "./pages/Jobs";
import Gpus from "./pages/Gpus";
import Users from "./pages/Users";

function useTheme() {
  const [theme, setTheme] = useState<string>(() => {
    try {
      return localStorage.getItem("mlq.theme") ?? "auto";
    } catch {
      return "auto";
    }
  });
  useEffect(() => {
    if (theme === "auto") document.documentElement.removeAttribute("data-theme");
    else document.documentElement.setAttribute("data-theme", theme);
    try {
      localStorage.setItem("mlq.theme", theme);
    } catch {
      /* ignore */
    }
  }, [theme]);
  return [theme, setTheme] as const;
}

export default function App() {
  const [authed, setAuthed] = useState(!!getToken());
  if (!authed) return <Login onLogin={() => setAuthed(true)} />;
  return <Shell onLogout={() => { setToken(null); setAuthed(false); }} />;
}

function Shell({ onLogout }: { onLogout: () => void }) {
  const me = useQuery({ queryKey: ["me"], queryFn: api.me, retry: false });
  const status = useQuery({ queryKey: ["status"], queryFn: api.status, refetchInterval: 10000 });
  const live = useLiveUpdates();
  const [theme, setTheme] = useTheme();

  useEffect(() => {
    if (me.error && (me.error as { status?: number }).status === 401) onLogout();
  }, [me.error, onLogout]);

  const isAdmin = me.data?.user.role === "admin";
  const nav = [
    { to: "/usage", label: "Usage", icon: BarChart3 },
    { to: "/jobs", label: "Jobs", icon: ListTodo },
    { to: "/gpus", label: "GPUs", icon: Cpu },
    ...(isAdmin ? [{ to: "/users", label: "Users", icon: UsersIcon }] : []),
  ];
  const s = status.data;

  return (
    <div className="flex min-h-screen bg-bg text-fg">
      <aside className="hidden w-52 shrink-0 flex-col border-r border-border bg-card p-3 sm:flex">
        <div className="mb-4 px-2 text-lg font-semibold tracking-tight">mlq</div>
        <nav className="space-y-0.5">
          {nav.map((n) => (
            <NavLink
              key={n.to}
              to={n.to}
              className={({ isActive }) =>
                cn("flex items-center gap-2 rounded-md px-2 py-1.5 text-sm", isActive ? "bg-muted font-medium" : "text-muted-fg hover:bg-muted/60")
              }
            >
              <n.icon size={16} /> {n.label}
            </NavLink>
          ))}
        </nav>
        <div className="mt-auto space-y-2 px-2 text-xs text-muted-fg">
          <div className="flex items-center gap-1.5">
            <Activity size={12} className={cn(live === "live" ? "text-success" : live === "polling" ? "text-warning" : "text-muted-fg")} />
            {live === "live" ? "live" : live === "polling" ? "polling" : "connecting"}
          </div>
          {s && (
            <div>
              {s.node.hostname} · v{s.version}
            </div>
          )}
        </div>
      </aside>
      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex items-center justify-between gap-4 border-b border-border bg-card px-4 py-2">
          <nav className="flex gap-3 text-sm sm:hidden">
            {nav.map((n) => (
              <NavLink key={n.to} to={n.to} className={({ isActive }) => cn(isActive ? "font-medium" : "text-muted-fg")}>
                {n.label}
              </NavLink>
            ))}
          </nav>
          <div className="hidden items-center gap-4 text-sm text-muted-fg sm:flex">
            {s && (
              <>
                <span>
                  <b className="text-fg tabular-nums">{s.free_gpus.length}</b>/{s.node.gpus_managed.length} GPUs free
                </span>
                <span>
                  <b className="text-fg tabular-nums">{s.running_jobs}</b> running
                </span>
                <span>
                  <b className="text-fg tabular-nums">{s.queued_jobs}</b> queued
                </span>
                <span>
                  <b className="text-fg tabular-nums">{s.free_cpus.toFixed(0)}</b> CPUs free
                </span>
              </>
            )}
          </div>
          <div className="flex items-center gap-2 text-sm">
            <button
              className="rounded-md p-1.5 text-muted-fg hover:bg-muted"
              title={`theme: ${theme}`}
              onClick={() => setTheme(theme === "dark" ? "light" : theme === "light" ? "auto" : "dark")}
            >
              {theme === "dark" ? <Moon size={16} /> : <Sun size={16} />}
            </button>
            <span className="text-muted-fg">
              {me.data?.user.name}
              {isAdmin && <span className="ml-1 rounded bg-muted px-1 text-xs">admin</span>}
            </span>
            <button className="rounded-md p-1.5 text-muted-fg hover:bg-muted" title="sign out" onClick={onLogout}>
              <LogOut size={16} />
            </button>
          </div>
        </header>
        <main className="flex-1 overflow-y-auto p-4">
          <Routes>
            <Route path="/" element={<Navigate to="/usage" replace />} />
            <Route path="/usage" element={<Usage isAdmin={!!isAdmin} />} />
            <Route path="/jobs" element={<Jobs isAdmin={!!isAdmin} />} />
            <Route path="/gpus" element={<Gpus />} />
            {isAdmin && <Route path="/users" element={<Users />} />}
            <Route path="*" element={<Navigate to="/usage" replace />} />
          </Routes>
        </main>
      </div>
    </div>
  );
}
