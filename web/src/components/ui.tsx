import type { ReactNode } from "react";
import type { JobState } from "../api";

export function cn(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(" ");
}

export function Card({ title, action, children, className }: { title?: ReactNode; action?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <section className={cn("rounded-xl border border-border bg-card text-card-fg shadow-sm", className)}>
      {(title || action) && (
        <header className="flex items-center justify-between gap-3 border-b border-border px-4 py-3">
          <h2 className="text-sm font-semibold tracking-tight">{title}</h2>
          {action}
        </header>
      )}
      <div className="p-4">{children}</div>
    </section>
  );
}

export function Stat({ label, value, hint }: { label: string; value: ReactNode; hint?: ReactNode }) {
  return (
    <div className="rounded-xl border border-border bg-card px-4 py-3">
      <div className="text-xs font-medium uppercase tracking-wide text-muted-fg">{label}</div>
      <div className="mt-1 text-2xl font-semibold tabular-nums">{value}</div>
      {hint && <div className="mt-0.5 text-xs text-muted-fg">{hint}</div>}
    </div>
  );
}

const STATE_STYLE: Record<JobState, string> = {
  queued: "bg-muted text-muted-fg",
  held: "bg-muted text-muted-fg",
  starting: "bg-info/15 text-info",
  pulling: "bg-info/15 text-info",
  running: "bg-info/15 text-info",
  succeeded: "bg-success/15 text-success",
  failed: "bg-danger/15 text-danger",
  timeout: "bg-warning/15 text-warning",
  cancelled: "bg-muted text-muted-fg",
};

export function StateBadge({ state }: { state: JobState }) {
  return (
    <span className={cn("inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-xs font-medium", STATE_STYLE[state])}>
      {state === "running" && <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-current" />}
      {state}
    </span>
  );
}

export function Button({ children, variant = "default", className, ...rest }: React.ButtonHTMLAttributes<HTMLButtonElement> & { variant?: "default" | "ghost" | "danger" | "primary" }) {
  const v = {
    default: "border border-border bg-card hover:bg-muted",
    ghost: "hover:bg-muted",
    danger: "border border-danger/40 text-danger hover:bg-danger/10",
    primary: "bg-fg text-bg hover:opacity-90",
  }[variant];
  return (
    <button className={cn("inline-flex h-8 items-center gap-1.5 rounded-md px-3 text-sm font-medium transition disabled:opacity-50", v, className)} {...rest}>
      {children}
    </button>
  );
}

export function Input(props: React.InputHTMLAttributes<HTMLInputElement>) {
  return <input {...props} className={cn("h-8 rounded-md border border-border bg-bg px-2.5 text-sm outline-none focus:ring-2 focus:ring-info/40", props.className)} />;
}

export function Select(props: React.SelectHTMLAttributes<HTMLSelectElement>) {
  return <select {...props} className={cn("h-8 rounded-md border border-border bg-bg px-2 text-sm outline-none focus:ring-2 focus:ring-info/40", props.className)} />;
}

export function Table({ children }: { children: ReactNode }) {
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-sm">{children}</table>
    </div>
  );
}

export function Th({ children, className }: { children?: ReactNode; className?: string }) {
  return <th className={cn("px-3 py-2 text-left text-xs font-medium uppercase tracking-wide text-muted-fg", className)}>{children}</th>;
}

export function Td({ children, className, title }: { children?: ReactNode; className?: string; title?: string }) {
  return (
    <td className={cn("px-3 py-2 align-middle", className)} title={title}>
      {children}
    </td>
  );
}

export function Progress({ value, max, className }: { value: number; max: number | null; className?: string }) {
  if (max === null || max === 0) {
    return <div className={cn("text-xs text-muted-fg", className)}>{value} / ∞</div>;
  }
  const pct = Math.min(100, (value / max) * 100);
  const tone = pct >= 100 ? "bg-danger" : pct >= 75 ? "bg-warning" : "bg-info";
  return (
    <div className={cn("flex items-center gap-2", className)}>
      <div className="h-2 flex-1 rounded-full bg-muted">
        <div className={cn("h-2 rounded-full", tone)} style={{ width: `${pct}%` }} />
      </div>
      <span className="w-14 text-right text-xs tabular-nums text-muted-fg">
        {value} / {max}
      </span>
    </div>
  );
}

export function Empty({ children }: { children: ReactNode }) {
  return <div className="py-8 text-center text-sm text-muted-fg">{children}</div>;
}

export function ErrorBox({ error }: { error: unknown }) {
  const msg = error instanceof Error ? error.message : String(error);
  return <div className="rounded-md border border-danger/40 bg-danger/10 px-3 py-2 text-sm text-danger">{msg}</div>;
}
