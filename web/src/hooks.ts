import { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { api, getToken } from "./api";

export type LiveState = "connecting" | "live" | "polling";

/**
 * Subscribe to the daemon's SSE stream. Every job event invalidates the job-related queries;
 * if the stream is unavailable we fall back to polling every 5 s.
 */
export function useLiveUpdates(): LiveState {
  const qc = useQueryClient();
  const [state, setState] = useState<LiveState>("connecting");
  const timer = useRef<number | null>(null);

  useEffect(() => {
    if (!getToken()) return;
    let es: EventSource | null = null;
    let closed = false;
    const invalidate = () => {
      qc.invalidateQueries({ queryKey: ["jobs"] });
      qc.invalidateQueries({ queryKey: ["status"] });
      qc.invalidateQueries({ queryKey: ["usage"] });
      qc.invalidateQueries({ queryKey: ["tasks"] });
    };
    const startPolling = () => {
      if (timer.current === null) {
        timer.current = window.setInterval(invalidate, 5000);
      }
      setState("polling");
    };
    const stopPolling = () => {
      if (timer.current !== null) {
        window.clearInterval(timer.current);
        timer.current = null;
      }
    };
    const connect = () => {
      if (closed) return;
      es = new EventSource(api.eventsUrl());
      es.onopen = () => {
        stopPolling();
        setState("live");
        invalidate();
      };
      es.addEventListener("job", invalidate);
      es.addEventListener("task", invalidate);
      es.addEventListener("gpus", () => qc.invalidateQueries({ queryKey: ["status"] }));
      es.onerror = () => {
        es?.close();
        startPolling();
        window.setTimeout(connect, 10000);
      };
    };
    connect();
    return () => {
      closed = true;
      es?.close();
      stopPolling();
    };
  }, [qc]);

  return state;
}

/** Re-render every `ms` so running-time columns tick. */
export function useTick(ms: number): number {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), ms);
    return () => window.clearInterval(t);
  }, [ms]);
  return now;
}
