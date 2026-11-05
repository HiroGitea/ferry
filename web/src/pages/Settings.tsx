import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, fmtTime, type NotifySettings, type TokenCreated, type WebhookFormat } from "../api";
import { Button, Card, Empty, ErrorBox, Input, Select, Table, Td, Th } from "../components/ui";

const FORMATS: { value: WebhookFormat; label: string }[] = [
  { value: "json", label: "ferry JSON" },
  { value: "feishu", label: "Feishu / Lark bot" },
  { value: "dingtalk", label: "DingTalk bot" },
  { value: "wecom", label: "WeCom bot" },
  { value: "slack", label: "Slack / Discord (Slack-compatible)" },
];

export default function Settings() {
  return (
    <div className="space-y-4">
      <Notifications />
      <Tokens />
    </div>
  );
}

function Notifications() {
  const qc = useQueryClient();
  const info = useQuery({ queryKey: ["notify"], queryFn: api.notify });
  const [s, setS] = useState<NotifySettings | null>(null);
  useEffect(() => {
    if (info.data && !s) setS(info.data.settings);
  }, [info.data, s]);
  const save = useMutation({
    mutationFn: (v: NotifySettings) => api.setNotify(v),
    onSuccess: (r) => {
      setS(r.settings);
      qc.setQueryData(["notify"], r);
    },
  });
  const test = useMutation({ mutationFn: api.testNotify });
  const dirty = !!s && !!info.data && JSON.stringify(s) !== JSON.stringify(info.data.settings);
  const tg = info.data?.telegram_available ?? false;

  return (
    <Card title="Notifications">
      {info.error ? <ErrorBox error={info.error} /> : null}
      {s && (
        <form
          className="space-y-3 text-sm"
          onSubmit={(e) => {
            e.preventDefault();
            save.mutate(s);
          }}
        >
          <label className="block space-y-1">
            <span className="text-muted-fg">Webhook URL</span>
            <div className="flex flex-wrap gap-2">
              <Input
                className="min-w-0 flex-1"
                placeholder="https://open.feishu.cn/open-apis/bot/v2/hook/…"
                value={s.webhook_url ?? ""}
                onChange={(e) => setS({ ...s, webhook_url: e.target.value || null })}
              />
              <Select value={s.webhook_format} onChange={(e) => setS({ ...s, webhook_format: e.target.value as WebhookFormat })}>
                {FORMATS.map((f) => (
                  <option key={f.value} value={f.value}>
                    {f.label}
                  </option>
                ))}
              </Select>
            </div>
          </label>
          <label className="block space-y-1">
            <span className="text-muted-fg">Telegram chat id</span>
            <Input
              className="w-full max-w-xs"
              placeholder={tg ? "123456789" : "no Telegram bot on this server"}
              disabled={!tg}
              value={s.telegram_chat_id ?? ""}
              onChange={(e) => setS({ ...s, telegram_chat_id: e.target.value || null })}
            />
            {tg && <span className="block text-xs text-muted-fg">Send any message to the server's bot first; @userinfobot tells you your id.</span>}
          </label>
          <div className="flex flex-wrap gap-4">
            {(
              [
                ["on_success", "when a job succeeds"],
                ["on_failure", "when a job fails or times out"],
                ["task_steps", "also for every task step"],
              ] as const
            ).map(([k, label]) => (
              <label key={k} className="flex items-center gap-2">
                <input type="checkbox" checked={s[k]} onChange={(e) => setS({ ...s, [k]: e.target.checked })} />
                {label}
              </label>
            ))}
          </div>
          <p className="text-xs text-muted-fg">Idle-GPU warnings and preemptions are always sent.</p>
          <div className="flex flex-wrap items-center gap-2">
            <Button type="submit" variant="primary" disabled={!dirty || save.isPending}>
              Save
            </Button>
            <Button type="button" onClick={() => test.mutate()} disabled={dirty || test.isPending || (!s.webhook_url && !s.telegram_chat_id)}>
              Send test
            </Button>
            {test.isSuccess && <span className="text-xs text-success">sent</span>}
          </div>
          {(save.error || test.error) && <ErrorBox error={save.error || test.error} />}
        </form>
      )}
    </Card>
  );
}

function Tokens() {
  const qc = useQueryClient();
  const me = useQuery({ queryKey: ["me"], queryFn: api.me });
  const tokens = useQuery({ queryKey: ["tokens"], queryFn: api.tokens });
  const [name, setName] = useState("");
  const [created, setCreated] = useState<TokenCreated | null>(null);
  const create = useMutation({
    mutationFn: () => api.createToken({ name: name.trim() || undefined }),
    onSuccess: (t) => {
      setCreated(t);
      setName("");
      qc.invalidateQueries({ queryKey: ["tokens"] });
    },
  });
  const del = useMutation({
    mutationFn: (id: number) => api.deleteToken(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["tokens"] }),
  });
  const mine = (tokens.data ?? []).filter((t) => t.user_id === me.data?.user.id);

  return (
    <Card title="My API tokens">
      <p className="mb-3 text-sm text-muted-fg">Use a separate token per machine or for Claude, so each can be revoked on its own.</p>
      <form
        className="flex flex-wrap items-center gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          create.mutate();
        }}
      >
        <Input placeholder="name, e.g. laptop or claude" value={name} onChange={(e) => setName(e.target.value)} />
        <Button type="submit" variant="primary" disabled={create.isPending}>
          New token
        </Button>
        {create.error ? <ErrorBox error={create.error} /> : null}
      </form>
      {created && (
        <div className="mt-3 rounded-md border border-success/40 bg-success/10 p-3 text-sm">
          New token (shown once):
          <code className="mt-1 block select-all break-all font-mono text-xs">{created.token}</code>
        </div>
      )}
      {!mine.length ? (
        <Empty>No tokens.</Empty>
      ) : (
        <div className="mt-3">
          <Table>
            <thead>
              <tr className="border-b border-border">
                <Th>ID</Th>
                <Th>Name</Th>
                <Th>Created</Th>
                <Th>Last used</Th>
                <Th></Th>
              </tr>
            </thead>
            <tbody>
              {mine.map((t) => (
                <tr key={t.id} className="border-b border-border/60">
                  <Td className="tabular-nums">{t.id}</Td>
                  <Td>{t.name}</Td>
                  <Td>{fmtTime(t.created_at)}</Td>
                  <Td>{fmtTime(t.last_used_at) || "never"}</Td>
                  <Td>
                    <Button
                      variant="danger"
                      onClick={() => confirm(`Revoke token ${t.name ?? t.id}? Anything using it stops working.`) && del.mutate(t.id)}
                    >
                      revoke
                    </Button>
                  </Td>
                </tr>
              ))}
            </tbody>
          </Table>
        </div>
      )}
      {del.error ? <ErrorBox error={del.error} /> : null}
    </Card>
  );
}
