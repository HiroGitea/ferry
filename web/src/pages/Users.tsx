import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type Quota, type TokenCreated, type User } from "../api";
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
