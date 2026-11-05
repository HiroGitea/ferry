import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, inviteLink, type InviteCreated, type Quota, type TokenCreated, type User } from "../api";
import { Button, Card, Empty, ErrorBox, Input, Select, Table, Td, Th } from "../components/ui";

export default function Users() {
  const qc = useQueryClient();
  const users = useQuery({ queryKey: ["users"], queryFn: api.users });
  const [name, setName] = useState("");
  const [role, setRole] = useState<"user" | "admin">("user");
  const [created, setCreated] = useState<TokenCreated | null>(null);
  const create = useMutation({
    mutationFn: () => api.createUser(name.trim(), role),
    onSuccess: (t) => {
      setCreated(t);
      setName("");
      qc.invalidateQueries({ queryKey: ["users"] });
      qc.invalidateQueries({ queryKey: ["usage"] });
    },
  });

  return (
    <div className="space-y-4">
      <Card title="Create user">
        <form
          className="flex flex-wrap items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            create.mutate();
          }}
        >
          <Input placeholder="name" value={name} onChange={(e) => setName(e.target.value)} />
          <Select value={role} onChange={(e) => setRole(e.target.value as "user" | "admin")}>
            <option value="user">user</option>
            <option value="admin">admin</option>
          </Select>
          <Button type="submit" variant="primary" disabled={!name.trim() || create.isPending}>
            Create
          </Button>
          {create.error ? <ErrorBox error={create.error} /> : null}
        </form>
        {created && (
          <div className="mt-3 rounded-md border border-success/40 bg-success/10 p-3 text-sm">
            Token for <b>{created.user.name}</b> (shown once):
            <code className="mt-1 block select-all break-all font-mono text-xs">{created.token}</code>
            <div className="mt-1 text-xs text-muted-fg">
              Log in with: <code>ferry login --url {window.location.origin} --token {created.token}</code>
            </div>
          </div>
        )}
      </Card>
      <Invites />
      <Card title="Users">
        {users.error ? <ErrorBox error={users.error} /> : null}
        {!users.data?.length ? (
          <Empty>No users.</Empty>
        ) : (
          <Table>
            <thead>
              <tr className="border-b border-border">
                <Th>ID</Th>
                <Th>Name</Th>
                <Th>Role</Th>
                <Th>Quota (running jobs / gpus / queued)</Th>
                <Th></Th>
              </tr>
            </thead>
            <tbody>
              {users.data.map((u) => (
                <UserRow key={u.id} user={u} />
              ))}
            </tbody>
          </Table>
        )}
      </Card>
    </div>
  );
}

function UserRow({ user }: { user: User }) {
  const qc = useQueryClient();
  const quota = useQuery({ queryKey: ["quota", user.name], queryFn: () => api.quota(user.name) });
  const [edit, setEdit] = useState<Quota | null>(null);
  const save = useMutation({
    mutationFn: (q: Quota) => api.setQuota(user.name, q),
    onSuccess: () => {
      setEdit(null);
      qc.invalidateQueries({ queryKey: ["quota", user.name] });
      qc.invalidateQueries({ queryKey: ["usage"] });
    },
  });
  const del = useMutation({
    mutationFn: () => api.deleteUser(user.name),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["users"] }),
  });
  const newToken = useMutation({ mutationFn: () => api.createToken({ user: user.name, name: "dashboard" }) });
  const q = edit ?? quota.data ?? { max_running_jobs: null, max_running_gpus: null, max_queued_jobs: null };
  const num = (v: number | null) => (v === null ? "" : String(v));
  const parse = (s: string) => (s.trim() === "" ? null : Math.max(0, Number(s)));

  return (
    <tr className="border-b border-border/60">
      <Td className="tabular-nums text-muted-fg">{user.id}</Td>
      <Td className="font-medium">{user.name}</Td>
      <Td>{user.role}</Td>
      <Td>
        <span className="flex items-center gap-1">
          <Input className="w-16" value={num(q.max_running_jobs)} placeholder="∞" onChange={(e) => setEdit({ ...q, max_running_jobs: parse(e.target.value) })} />
          <Input className="w-16" value={num(q.max_running_gpus)} placeholder="∞" onChange={(e) => setEdit({ ...q, max_running_gpus: parse(e.target.value) })} />
          <Input className="w-16" value={num(q.max_queued_jobs)} placeholder="∞" onChange={(e) => setEdit({ ...q, max_queued_jobs: parse(e.target.value) })} />
          {edit && (
            <Button onClick={() => save.mutate(edit)} disabled={save.isPending}>
              Save
            </Button>
          )}
        </span>
      </Td>
      <Td>
        <span className="flex items-center gap-2">
          <Button variant="ghost" onClick={() => newToken.mutate()}>
            new token
          </Button>
          <Button variant="danger" onClick={() => confirm(`Delete user ${user.name}?`) && del.mutate()}>
            delete
          </Button>
        </span>
        {newToken.data && <code className="mt-1 block select-all break-all font-mono text-xs">{newToken.data.token}</code>}
        {(newToken.error || del.error || save.error) && <ErrorBox error={newToken.error || del.error || save.error} />}
      </Td>
    </tr>
  );
}

const EXPIRY = [
  { label: "1 day", secs: 86400 },
  { label: "7 days", secs: 7 * 86400 },
  { label: "30 days", secs: 30 * 86400 },
];

function Invites() {
  const qc = useQueryClient();
  const invites = useQuery({ queryKey: ["invites"], queryFn: api.invites });
  const [role, setRole] = useState<"user" | "admin">("user");
  const [name, setName] = useState("");
  const [uses, setUses] = useState("1");
  const [expiry, setExpiry] = useState(String(7 * 86400));
  const [note, setNote] = useState("");
  const [created, setCreated] = useState<InviteCreated | null>(null);
  const [copied, setCopied] = useState(false);
  const create = useMutation({
    mutationFn: () =>
      api.createInvite({
        role,
        name: name.trim() || undefined,
        note: note.trim() || undefined,
        max_uses: name.trim() ? 1 : Math.max(1, Number(uses) || 1),
        expires_in_secs: Number(expiry),
      }),
    onSuccess: (c) => {
      setCreated(c);
      setCopied(false);
      setName("");
      setNote("");
      qc.invalidateQueries({ queryKey: ["invites"] });
    },
  });
  const revoke = useMutation({
    mutationFn: (id: number) => api.deleteInvite(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["invites"] }),
  });
  const link = created ? inviteLink(created.code) : "";
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(link);
      setCopied(true);
    } catch {
      /* clipboard unavailable over plain http: the link is selectable */
    }
  };
  const now = Date.now();

  return (
    <Card title="Invite links">
      <form
        className="flex flex-wrap items-center gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          create.mutate();
        }}
      >
        <Select value={role} onChange={(e) => setRole(e.target.value as "user" | "admin")}>
          <option value="user">user</option>
          <option value="admin">admin</option>
        </Select>
        <Input className="w-36" placeholder="name (optional)" value={name} onChange={(e) => setName(e.target.value)} />
        <label className="flex items-center gap-1 text-sm text-muted-fg">
          uses
          <Input className="w-16" type="number" min={1} max={1000} value={name.trim() ? "1" : uses} disabled={!!name.trim()} onChange={(e) => setUses(e.target.value)} />
        </label>
        <Select value={expiry} onChange={(e) => setExpiry(e.target.value)}>
          {EXPIRY.map((x) => (
            <option key={x.secs} value={x.secs}>
              expires in {x.label}
            </option>
          ))}
        </Select>
        <Input className="w-40" placeholder="note (optional)" value={note} onChange={(e) => setNote(e.target.value)} />
        <Button type="submit" variant="primary" disabled={create.isPending}>
          Create link
        </Button>
        {create.error ? <ErrorBox error={create.error} /> : null}
      </form>
      {created && (
        <div className="mt-3 rounded-md border border-success/40 bg-success/10 p-3 text-sm">
          Invite link (shown once; send it to the new user):
          <div className="mt-1 flex items-center gap-2">
            <code className="block min-w-0 flex-1 select-all break-all font-mono text-xs">{link}</code>
            <Button variant="ghost" onClick={copy}>
              {copied ? "copied" : "copy"}
            </Button>
          </div>
          <div className="mt-1 text-xs text-muted-fg">
            Or from a terminal: <code>ferry register &apos;{link}&apos;</code>
          </div>
        </div>
      )}
      {invites.error ? <ErrorBox error={invites.error} /> : null}
      {!!invites.data?.length && (
        <div className="mt-3">
          <Table>
            <thead>
              <tr className="border-b border-border">
                <Th>ID</Th>
                <Th>Role</Th>
                <Th>Name</Th>
                <Th>Uses</Th>
                <Th>Expires</Th>
                <Th>Registered</Th>
                <Th>Note</Th>
                <Th></Th>
              </tr>
            </thead>
            <tbody>
              {invites.data.map((i) => {
                const expired = new Date(i.expires_at).getTime() <= now;
                const usedUp = i.uses >= i.max_uses;
                return (
                  <tr key={i.id} className={"border-b border-border/60" + (expired || usedUp ? " text-muted-fg" : "")}>
                    <Td className="tabular-nums">{i.id}</Td>
                    <Td>{i.role}</Td>
                    <Td>{i.name ?? "any"}</Td>
                    <Td className="tabular-nums">
                      {i.uses}/{i.max_uses}
                    </Td>
                    <Td>{expired ? "expired" : new Date(i.expires_at).toLocaleString()}</Td>
                    <Td>{i.used_by.join(", ")}</Td>
                    <Td>{i.note}</Td>
                    <Td>
                      <Button variant="ghost" onClick={() => revoke.mutate(i.id)} disabled={revoke.isPending}>
                        {expired || usedUp ? "remove" : "revoke"}
                      </Button>
                    </Td>
                  </tr>
                );
              })}
            </tbody>
          </Table>
        </div>
      )}
    </Card>
  );
}
