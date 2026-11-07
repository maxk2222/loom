import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus } from "lucide-react";
import { useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { Badge, Card, Field, Modal, Select, Spinner } from "../components/ui";
import type { UserRow } from "../lib/types";

export default function UsersPage() {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["users"], queryFn: () => api.get<{ users: UserRow[] }>("/api/users") });
  const [open, setOpen] = useState(false);
  const [f, setF] = useState({ email: "", display_name: "", password: "", role: "viewer" });

  const refresh = () => qc.invalidateQueries({ queryKey: ["users"] });

  const create = async (e: FormEvent) => {
    e.preventDefault();
    await api.post("/api/users", f);
    setOpen(false);
    setF({ email: "", display_name: "", password: "", role: "viewer" });
    refresh();
  };

  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between">
        <h1 className="text-lg font-semibold">Users</h1>
        <button className="btn-primary" onClick={() => setOpen(true)}><Plus size={16} /> New user</button>
      </div>
      {q.isLoading ? <Spinner /> : (
        <Card className="overflow-hidden p-0">
          <table className="w-full">
            <thead><tr><th className="th">Email</th><th className="th">Name</th><th className="th">Role</th><th className="th">Status</th></tr></thead>
            <tbody>
              {(q.data?.users ?? []).map((u) => (
                <tr key={u.id}>
                  <td className="td">{u.email}</td>
                  <td className="td text-muted">{u.display_name || "—"}</td>
                  <td className="td"><Badge tone={u.role === "admin" ? "accent" : "muted"}>{u.role}</Badge></td>
                  <td className="td">{u.disabled ? <Badge tone="err">disabled</Badge> : <Badge tone="ok">active</Badge>}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </Card>
      )}

      <Modal open={open} title="New user" onClose={() => setOpen(false)}>
        <form onSubmit={create} className="space-y-4">
          <Field label="Email"><input className="input" type="email" value={f.email} onChange={(e) => setF({ ...f, email: e.target.value })} required /></Field>
          <Field label="Display name"><input className="input" value={f.display_name} onChange={(e) => setF({ ...f, display_name: e.target.value })} /></Field>
          <Field label="Password (min. 8)"><input className="input" type="password" minLength={8} value={f.password} onChange={(e) => setF({ ...f, password: e.target.value })} required /></Field>
          <Field label="Role"><Select value={f.role} onChange={(v) => setF({ ...f, role: v })} options={[{ value: "admin", label: "admin" }, { value: "operator", label: "operator" }, { value: "viewer", label: "viewer" }]} /></Field>
          <button className="btn-primary w-full justify-center">Create user</button>
        </form>
      </Modal>
    </div>
  );
}
