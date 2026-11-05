import { useState } from "react";
import { api, setToken } from "../api";
import { Button, Input, ErrorBox } from "../components/ui";

export default function Login({ onLogin }: { onLogin: () => void }) {
  const [token, setTok] = useState("");
  const [err, setErr] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    setToken(token.trim());
    try {
      await api.me();
      onLogin();
    } catch (e) {
      setToken(null);
      setErr(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex min-h-screen items-center justify-center bg-bg px-4">
      <form onSubmit={submit} className="w-full max-w-sm space-y-4 rounded-xl border border-border bg-card p-6 shadow-sm">
        <div>
          <h1 className="text-lg font-semibold">ferry</h1>
          <p className="text-sm text-muted-fg">Paste your API token (from <code>ferry token create</code> or your admin).</p>
        </div>
        <Input type="password" placeholder="ferry_…" value={token} onChange={(e) => setTok(e.target.value)} className="w-full" autoFocus />
        {err ? <ErrorBox error={err} /> : null}
        <Button type="submit" variant="primary" className="w-full" disabled={busy || !token.trim()}>
          Sign in
        </Button>
      </form>
    </div>
  );
}
