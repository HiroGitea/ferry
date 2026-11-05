import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { api, type MetricTick } from "../api";

/** Shared hover timestamp so every chart on the page shows the same crosshair (netdata-style). */
interface SyncCtx {
  hoverT: number | null;
  setHoverT: (t: number | null) => void;
}
const Sync = createContext<SyncCtx>({ hoverT: null, setHoverT: () => {} });

export function ChartSyncProvider({ children }: { children: ReactNode }) {
  const [hoverT, setHoverT] = useState<number | null>(null);
  const value = useMemo(() => ({ hoverT, setHoverT }), [hoverT]);
  return <Sync.Provider value={value}>{children}</Sync.Provider>;
}

export function useChartSync() {
  return useContext(Sync);
}

/** One EventSource for live metric ticks, shared by every chart. */
type Listener = (tick: MetricTick) => void;
interface StreamCtx {
  subscribe: (l: Listener) => () => void;
  connected: boolean;
}
const Stream = createContext<StreamCtx>({ subscribe: () => () => {}, connected: false });

export function MetricStreamProvider({ children }: { children: ReactNode }) {
  const listeners = useRef(new Set<Listener>());
  const [connected, setConnected] = useState(false);
  useEffect(() => {
    let es: EventSource | null = null;
    let closed = false;
    let retry: number | undefined;
    const connect = () => {
      if (closed) return;
      es = new EventSource(api.metricsStreamUrl());
      es.onopen = () => setConnected(true);
      es.addEventListener("tick", (e) => {
        try {
          const tick = JSON.parse((e as MessageEvent).data) as MetricTick;
          listeners.current.forEach((l) => l(tick));
        } catch {
          /* ignore malformed */
        }
      });
      es.onerror = () => {
        setConnected(false);
        es?.close();
        retry = window.setTimeout(connect, 5000);
      };
    };
    connect();
    return () => {
      closed = true;
      es?.close();
      if (retry) window.clearTimeout(retry);
    };
  }, []);
  const subscribe = useCallback((l: Listener) => {
    listeners.current.add(l);
    return () => {
      listeners.current.delete(l);
    };
  }, []);
  const value = useMemo(() => ({ subscribe, connected }), [subscribe, connected]);
  return <Stream.Provider value={value}>{children}</Stream.Provider>;
}

export function useMetricStream() {
  return useContext(Stream);
}
