import { useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { api, setToken, type TokenCreated } from "../api";
import { Button, ErrorBox, Input } from "../components/ui";

// Public page reached from an invite link: /register?code=inv_...
export default function Register() {
  const code = new URLSearchParams(window.location.search).get("code") ?? "";
  const preview = useQuery({
    queryKey: ["invite", code],
    queryFn: () => api.previewInvite(code),
    enabled: !!code,
    retry: false,
  });
  const [name, setName] = useState("");
  const [done, setDone] = useState<TokenCreated | null>(null);
  const register = useMutation({
    mutationFn: () => api.register(code, (preview.data?.name ?? name).trim()),
    onSuccess: (t) => setDone(t),
  });

  const enter = () => {
    if (done) setToken(done.token);
    window.location.replace("/");
  };
  const origin = window.location.origin;

  return (
    <div className="flex min-h-screen items-center justify-center bg-bg px-4 py-8">
      <div className="w-full max-w-md space-y-4 rounded-xl border border-border bg-card p-6 shadow-sm">
        <div>
          <h1 className="text-lg font-semibold">ferry</h1>
          <p className="text-sm text-muted-fg">Create your account from an invite.</p>
        </div>

        {!code ? (
          <p className="text-sm">This link has no invite code. Ask your admin for a new one.</p>
        ) : preview.isLoading ? (
          <p className="text-sm text-muted-fg">Checking invite…</p>
        ) : preview.error ? (
          <ErrorBox error={preview.error} />
        ) : done ? (
          <div className="space-y-3 text-sm">
            <p>
              Welcome, <b>{done.user.name}</b>. Your API token is shown <b>once</b>; keep it somewhere safe:
            </p>
            <code className="block select-all break-all rounded-md border border-border bg-bg p-2 font-mono text-xs">{done.token}</code>
            <div className="space-y-1 text-xs text-muted-fg">
              <div>Command line:</div>
              <code className="block select-all break-all font-mono">
                ferry login --url {origin} --token {done.token}
              </code>
              <div className="pt-1">Claude Code:</div>
              <code className="block select-all break-all font-mono">
                claude mcp add --transport http ferry {origin}/mcp --header "Authorization: Bearer {done.token}"
              </code>
            </div>
            <Button variant="primary" className="w-full" onClick={enter}>
              Open the dashboard
            </Button>
          </div>
        ) : (
          <form
            className="space-y-3"
            onSubmit={(e) => {
              e.preventDefault();
              register.mutate();
            }}
          >
            <p className="text-sm">
              This invite creates a <b>{preview.data?.role}</b> account
              {preview.data && preview.data.remaining_uses > 1 ? ` (${preview.data.remaining_uses} uses left)` : ""}, valid until{" "}
              {preview.data ? new Date(preview.data.expires_at).toLocaleString() : ""}.
            </p>
            <label className="block space-y-1 text-sm">
              <span className="text-muted-fg">User name</span>
              <Input
                className="w-full"
                value={preview.data?.name ?? name}
                disabled={!!preview.data?.name}
                onChange={(e) => setName(e.target.value)}
                placeholder="letters, digits, . _ -"
                pattern="[A-Za-z0-9._\-]{1,64}"
                autoFocus
              />
            </label>
            {register.error ? <ErrorBox error={register.error} /> : null}
            <Button
              type="submit"
              variant="primary"
              className="w-full"
              disabled={register.isPending || !(preview.data?.name ?? name).trim()}
            >
              Create account
            </Button>
          </form>
        )}
      </div>
    </div>
  );
}
