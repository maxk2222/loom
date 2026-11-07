import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus } from "lucide-react";
import { useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { Badge, Card, Field, Modal, Spinner, Toggle } from "../components/ui";
import type { Project, VirtualModel } from "../lib/types";

export default function ProjectsPage() {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["projects"], queryFn: () => api.get<{ projects: Project[] }>("/api/projects") });
  const vms = useQuery({ queryKey: ["vms"], queryFn: () => api.get<{ models: VirtualModel[] }>("/api/models") });
  const [open, setOpen] = useState(false);
  const [models, setModels] = useState<Project | null>(null);
  const [selVms, setSelVms] = useState<Set<string>>(new Set());
  const [f, setF] = useState({ slug: "", name: "", priority_class: 1, weight: 1 });

  const refresh = () => qc.invalidateQueries({ queryKey: ["projects"] });

  const create = async (e: FormEvent) => {
    e.preventDefault();
    await api.post("/api/projects", f);
    setOpen(false);
    setF({ slug: "", name: "", priority_class: 1, weight: 1 });
    refresh();
  };

  const toggleEnabled = async (p: Project) => {
    await api.put(`/api/projects/${p.id}`, { enabled: !p.enabled });
    refresh();
  };

  const saveModels = async () => {
    if (!models) return;
    await api.put(`/api/projects/${models.id}/models`, { virtual_model_ids: [...selVms] });
    setModels(null);
  };

  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between">
        <h1 className="text-lg font-semibold">Projects</h1>
        <button className="btn-primary" onClick={() => setOpen(true)}><Plus size={16} /> New project</button>
      </div>
      {q.isLoading ? <Spinner /> : (
        <Card className="overflow-hidden p-0">
          <table className="w-full">
            <thead><tr><th className="th">Slug</th><th className="th">Name</th><th className="th">Priority class</th><th className="th">Weight</th><th className="th">Status</th><th className="th" /></tr></thead>
            <tbody>
              {(q.data?.projects ?? []).map((p) => (
                <tr key={p.id}>
                  <td className="td font-mono">{p.slug}</td>
                  <td className="td">{p.name}</td>
                  <td className="td text-muted">{p.priority_class}</td>
                  <td className="td text-muted">{p.weight}</td>
                  <td className="td">{p.enabled ? <Badge tone="ok">enabled</Badge> : <Badge tone="err">disabled</Badge>}</td>
                  <td className="td text-right space-x-2">
                    <button className="btn-outline py-1 text-xs" onClick={() => { setModels(p); setSelVms(new Set()); }}>Models</button>
                    <button className="btn-outline py-1 text-xs" onClick={() => toggleEnabled(p)}>{p.enabled ? "Disable" : "Enable"}</button>
                  </td>
                </tr>
              ))}
              {(q.data?.projects ?? []).length === 0 && <tr><td className="td text-center text-muted" colSpan={6}>No projects.</td></tr>}
            </tbody>
          </table>
        </Card>
      )}

      <Modal open={open} title="New project" onClose={() => setOpen(false)}>
        <form onSubmit={create} className="space-y-4">
          <Field label="Slug"><input className="input font-mono" value={f.slug} onChange={(e) => setF({ ...f, slug: e.target.value })} required placeholder="research-team-a" /></Field>
          <Field label="Name"><input className="input" value={f.name} onChange={(e) => setF({ ...f, name: e.target.value })} /></Field>
          <div className="grid grid-cols-2 gap-4">
            <Field label="Priority class (0 = highest)" hint="0–3">
              <input className="input" type="number" min={0} max={3} value={f.priority_class} onChange={(e) => setF({ ...f, priority_class: +e.target.value })} />
            </Field>
            <Field label="Weight" hint="DRR share within a class">
              <input className="input" type="number" min={1} value={f.weight} onChange={(e) => setF({ ...f, weight: +e.target.value })} />
            </Field>
          </div>
          <button className="btn-primary w-full justify-center">Create</button>
        </form>
      </Modal>

      <Modal open={!!models} title={`Model access — ${models?.slug}`} onClose={() => setModels(null)}>
        <div className="space-y-3">
          <p className="text-sm text-muted">Virtual models this project may use (when API key has no explicit allowlist):</p>
          <div className="space-y-2">
            {(vms.data?.models ?? []).map((m) => (
              <Toggle key={m.id} checked={selVms.has(m.id)} label={m.api_name}
                onChange={(v) => { const s = new Set(selVms); v ? s.add(m.id) : s.delete(m.id); setSelVms(s); }} />
            ))}
          </div>
          <button className="btn-primary w-full justify-center" onClick={saveModels}>Save</button>
        </div>
      </Modal>
    </div>
  );
}
