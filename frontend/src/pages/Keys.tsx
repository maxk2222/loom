import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus } from "lucide-react";
import { useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { Badge, Card, Field, Modal, Select, Spinner } from "../components/ui";
import type { ApiKey, Project } from "../lib/types";

const SCOPES = ["chat", "completions", "embeddings", "models"];

export default function KeysPage() {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["keys"], queryFn: () => api.get<{ keys: ApiKey[] }>("/api/keys") });
  const projects = useQuery({ queryKey: ["projects"], queryFn: () => api.get<{ projects: Project[] }>("/api/projects") });
  const [open, setOpen] = useState(false);
  const [token, setToken] = useState("");
  const [f, setF] = useState({ project_id: "", name: "", rpm_limit: 60, tpm_limit: 100000, max_concurrent: 4, scopes: new Set(SCOPES) });

  const refresh = () => qc.invalidateQueries({ queryKey: ["keys"] });

  const create = async (e: FormEvent) => {
    e.preventDefault();
    const r = await api.post<{ token: string }>("/api/keys", {
      project_id: f.project_id, name: f.name, rpm_limit: f.rpm_limit,
      tpm_limit: f.tpm_limit, max_concurrent: f.max_concurrent, scopes: [...f.scopes],
    });
    setToken(r.token);
    refresh();
  };

  const revoke = async (id: string) => {
    await api.post(`/api/keys/${id}/revoke`);
    refresh();
  };

  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between">
        <h1 className="text-lg font-semibold">API keys</h1>
        <button className="btn-primary" onClick={() => { setOpen(true); setToken(""); }}><Plus size={16} /> New key</button>
      </div>
      {q.isLoading ? <Spinner /> : (
        <Card className="overflow-hidden p-0">
          <table className="w-full">
            <thead><tr><th className="th">Name</th><th className="th">Prefix</th><th className="th">Project</th><th className="th">Limits (rpm/tpm/conc)</th><th className="th">Status</th><th className="th" /></tr></thead>
            <tbody>
              {(q.data?.keys ?? []).map((k) => (
                <tr key={k.id}>
                  <td className="td">{k.name || "—"}</td>
                  <td className="td font-mono text-muted">{k.prefix}…</td>
                  <td className="td text-muted font-mono text-xs">{k.project_id.slice(0, 8)}…</td>
                  <td className="td text-muted">{k.rpm_limit}/{k.tpm_limit}/{k.max_concurrent}</td>
                  <td className="td">{k.active ? <Badge tone="ok">active</Badge> : <Badge tone="err">revoked</Badge>}</td>
                  <td className="td text-right">{k.active && <button className="btn-danger py-1 text-xs" onClick={() => revoke(k.id)}>Revoke</button>}</td>
                </tr>
              ))}
              {(q.data?.keys ?? []).length === 0 && <tr><td className="td text-center text-muted" colSpan={6}>No API keys.</td></tr>}
            </tbody>
          </table>
        </Card>
      )}

      <Modal open={open} title="New API key" onClose={() => setOpen(false)}>
        {token ? (
          <div className="space-y-3">
            <p className="text-sm text-muted">Store this key now — it will not be shown again.</p>
            <code className="block break-all rounded-md bg-panel2 p-3 text-xs">{token}</code>
            <button className="btn-primary w-full justify-center" onClick={() => setOpen(false)}>Done</button>
          </div>
        ) : (
          <form onSubmit={create} className="space-y-4">
            <Field label="Project">
              <Select value={f.project_id} onChange={(v) => setF({ ...f, project_id: v })}
                options={[{ value: "", label: "— select —" }, ...(projects.data?.projects ?? []).map((p) => ({ value: p.id, label: p.slug }))]} />
            </Field>
            <Field label="Name"><input className="input" value={f.name} onChange={(e) => setF({ ...f, name: e.target.value })} placeholder="batch-jobs" /></Field>
            <div className="grid grid-cols-3 gap-3">
              <Field label="RPM limit"><input className="input" type="number" min={0} value={f.rpm_limit} onChange={(e) => setF({ ...f, rpm_limit: +e.target.value })} /></Field>
              <Field label="TPM limit"><input className="input" type="number" min={0} value={f.tpm_limit} onChange={(e) => setF({ ...f, tpm_limit: +e.target.value })} /></Field>
              <Field label="Concurrency"><input className="input" type="number" min={0} value={f.max_concurrent} onChange={(e) => setF({ ...f, max_concurrent: +e.target.value })} /></Field>
            </div>
            <Field label="Scopes">
              <div className="flex gap-4 pt-1">
                {SCOPES.map((s) => (
                  <label key={s} className="flex items-center gap-1.5 text-sm text-muted">
                    <input type="checkbox" checked={f.scopes.has(s)}
                      onChange={(e) => { const set = new Set(f.scopes); e.target.checked ? set.add(s) : set.delete(s); setF({ ...f, scopes: set }); }} />
                    {s}
                  </label>
                ))}
              </div>
            </Field>
            <button className="btn-primary w-full justify-center">Create key</button>
          </form>
        )}
      </Modal>
    </div>
  );
}
