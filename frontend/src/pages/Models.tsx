import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, Trash2 } from "lucide-react";
import { useEffect, useState, type FormEvent } from "react";
import { Link } from "react-router-dom";
import { api } from "../lib/api";
import { Badge, Card, Field, Modal, Select, Spinner, Toggle } from "../components/ui";
import type { DeploymentRow, Member, VirtualModel } from "../lib/types";

const STRATEGIES = [
  { value: "priority", label: "Priority" },
  { value: "least_in_flight", label: "Least in flight" },
  { value: "weighted", label: "Weighted" },
  { value: "round_robin", label: "Round robin" },
  { value: "manual", label: "Manual (pinned)" },
];
const API_TYPES = [
  { value: "chat", label: "Chat completions" },
  { value: "images", label: "Image generation" },
  { value: "audio_speech", label: "Audio — speech (TTS)" },
  { value: "audio_transcription", label: "Audio — transcription (STT)" },
  { value: "audio_translation", label: "Audio — translation" },
  { value: "video", label: "Video generation" },
  { value: "completions", label: "Completions" },
  { value: "embeddings", label: "Embeddings" },
];

interface FormState {
  api_name: string;
  description: string;
  api_type: string;
  routing_strategy: string;
  allow_heterogeneous: boolean;
  fallback_virtual_model_id: string;
  req_tools: boolean;
  req_vision: boolean;
  req_streaming: boolean;
  req_json_mode: boolean;
  enabled: boolean;
  members: Member[];
}

const EMPTY: FormState = {
  api_name: "", description: "", api_type: "chat", routing_strategy: "priority",
  allow_heterogeneous: false, fallback_virtual_model_id: "",
  req_tools: false, req_vision: false, req_streaming: false, req_json_mode: false,
  enabled: true, members: [],
};

export default function ModelsPage() {
  const qc = useQueryClient();
  const vms = useQuery({ queryKey: ["vms"], queryFn: () => api.get<{ models: VirtualModel[] }>("/api/models") });
  const deps = useQuery({ queryKey: ["deployments"], queryFn: () => api.get<{ deployments: DeploymentRow[] }>("/api/deployments") });
  const clis = useQuery({ queryKey: ["clients"], queryFn: () => api.get<{ clients: any[] }>("/api/clients") });
  const cname = (id: string) => clis.data?.clients.find((c: any) => c.id === id)?.name ?? id.slice(0, 8);
  const [open, setOpen] = useState(false);
  const [editId, setEditId] = useState<string | null>(null);
  const [f, setF] = useState<FormState>(EMPTY);

  const refresh = () => qc.invalidateQueries({ queryKey: ["vms"] });

  const openCreate = () => { setEditId(null); setF(EMPTY); setOpen(true); };
  const openEdit = async (id: string) => {
    const d = await api.get<{ model: VirtualModel; members: Member[] }>(`/api/models/${id}`);
    const m = d.model;
    const rc = (m.required_capabilities ?? {}) as Record<string, boolean>;
    setEditId(id);
    setF({
      api_name: m.api_name, description: m.description ?? "", api_type: m.api_type,
      routing_strategy: m.routing_strategy, allow_heterogeneous: m.allow_heterogeneous,
      fallback_virtual_model_id: m.fallback_virtual_model_id ?? "",
      req_tools: !!rc.tools, req_vision: !!rc.vision,
      req_streaming: !!rc.streaming, req_json_mode: !!rc.json_mode,
      enabled: m.enabled, members: d.members ?? [],
    });
    setOpen(true);
  };

  const save = async (e: FormEvent) => {
    e.preventDefault();
    const body: Record<string, unknown> = {
      api_name: f.api_name,
      description: f.description,
      api_type: f.api_type,
      routing_strategy: f.routing_strategy,
      allow_heterogeneous: f.allow_heterogeneous,
      enabled: f.enabled,
      required_capabilities: {
        ...(f.req_tools ? { tools: true } : {}),
        ...(f.req_vision ? { vision: true } : {}),
        ...(f.req_streaming ? { streaming: true } : {}),
        ...(f.req_json_mode ? { json_mode: true } : {}),
      },
    };
    if (f.fallback_virtual_model_id) body.fallback_virtual_model_id = f.fallback_virtual_model_id;
    if (editId) {
      body.members = f.members;
      await api.put(`/api/models/${editId}`, body);
    } else {
      await api.post("/api/models", body);
    }
    setOpen(false);
    refresh();
  };

  const del = async (id: string) => {
    if (!confirm("Delete this virtual model?")) return;
    await api.del(`/api/models/${id}`);
    refresh();
  };

  useEffect(() => { /* form unchanged */ }, [f]);

  const addMember = () =>
    setF({ ...f, members: [...f.members, { deployment_id: "", priority: 100, weight: 1, manual_pinned: false, enabled: true }] });

  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between">
        <h1 className="text-lg font-semibold">Virtual models</h1>
        <button className="btn-primary" onClick={openCreate}><Plus size={16} /> New model</button>
      </div>
      {vms.isLoading ? <Spinner /> : (
        <div className="grid gap-4 md:grid-cols-2">
          {(vms.data?.models ?? []).map((m) => (
            <Card key={m.id} className="space-y-2">
              <div className="flex items-start justify-between">
                <div>
                  <div className="font-mono text-sm font-semibold">{m.api_name}</div>
                  <div className="mt-0.5 text-xs text-muted">{m.description || "—"}</div>
                </div>
                <Badge tone={m.enabled ? "ok" : "err"}>{m.enabled ? "enabled" : "disabled"}</Badge>
              </div>
              <div className="flex flex-wrap gap-2 text-xs text-muted">
                <Badge tone="accent">{m.api_type}</Badge>
                <Badge tone="muted">{m.routing_strategy}</Badge>
                {m.kind === "router" && <Badge tone="warn">auto-router</Badge>}
                {m.allow_heterogeneous && <Badge tone="warn">heterogeneous</Badge>}
              </div>
              <div className="flex gap-2 pt-1">
                {m.kind === "router" ? (
                  <Link className="btn-outline flex-1 justify-center py-1 text-xs" to="/routers">Manage in Auto Routers</Link>
                ) : (
                  <button className="btn-outline flex-1 justify-center py-1 text-xs" onClick={() => openEdit(m.id)}>Edit</button>
                )}
                <button className="btn-danger py-1 text-xs" onClick={() => del(m.id)}>Delete</button>
              </div>
            </Card>
          ))}
          {(vms.data?.models ?? []).length === 0 && (
            <Card className="col-span-2 text-center text-sm text-muted">No virtual models yet.</Card>
          )}
        </div>
      )}

      <Modal open={open} title={editId ? `Edit ${f.api_name}` : "New virtual model"} onClose={() => setOpen(false)} wide>
        <form onSubmit={save} className="space-y-4">
          <div className="grid grid-cols-2 gap-4">
            <Field label="API name" hint="Model id used in /v1 requests">
              <input className="input font-mono" value={f.api_name} disabled={!!editId} onChange={(e) => setF({ ...f, api_name: e.target.value })} required placeholder="vm-llama3" />
            </Field>
            <Field label="API type"><Select value={f.api_type} onChange={(v) => setF({ ...f, api_type: v })} options={API_TYPES} /></Field>
          </div>
          <Field label="Description"><input className="input" value={f.description} onChange={(e) => setF({ ...f, description: e.target.value })} /></Field>
          <div className="grid grid-cols-2 gap-4">
            <Field label="Routing strategy"><Select value={f.routing_strategy} onChange={(v) => setF({ ...f, routing_strategy: v })} options={STRATEGIES} /></Field>
            <Field label="Fallback model">
              <Select value={f.fallback_virtual_model_id} onChange={(v) => setF({ ...f, fallback_virtual_model_id: v })}
                options={[{ value: "", label: "— none —" }, ...(vms.data?.models ?? []).filter((m) => m.id !== editId).map((m) => ({ value: m.id, label: m.api_name }))]} />
            </Field>
          </div>
          <Field label="Required capabilities" hint="Requests needing these are only routed to capable members">
            <div className="flex gap-6 pt-1">
              <Toggle checked={f.req_tools} onChange={(v) => setF({ ...f, req_tools: v })} label="tools" />
              <Toggle checked={f.req_vision} onChange={(v) => setF({ ...f, req_vision: v })} label="vision" />
              <Toggle checked={f.req_streaming} onChange={(v) => setF({ ...f, req_streaming: v })} label="streaming" />
              <Toggle checked={f.req_json_mode} onChange={(v) => setF({ ...f, req_json_mode: v })} label="json_mode" />
            </div>
          </Field>
          <div className="flex gap-6">
            <Toggle checked={f.allow_heterogeneous} onChange={(v) => setF({ ...f, allow_heterogeneous: v })} label="allow mixed model names" />
            <Toggle checked={f.enabled} onChange={(v) => setF({ ...f, enabled: v })} label="enabled" />
          </div>

          {editId && (
            <div className="space-y-2">
              <div className="flex items-center justify-between">
                <label className="label mb-0">Members (deployments)</label>
                <button type="button" className="btn-outline py-1 text-xs" onClick={addMember}><Plus size={14} /> Add</button>
              </div>
              {f.members.map((m, i) => (
                <div key={i} className="flex items-center gap-2 rounded-md border border-border bg-panel2 p-2">
                  <select className="input flex-1" value={m.deployment_id}
                    onChange={(e) => { const ms = [...f.members]; ms[i] = { ...m, deployment_id: e.target.value }; setF({ ...f, members: ms }); }}>
                    <option value="">— deployment —</option>
                    {(deps.data?.deployments ?? []).filter((d: any) => d.enabled !== false).map((d: any) => (
                      <option key={d.id} value={d.id}>{cname(d.client_id)} · {d.model_name}</option>
                    ))}
                  </select>
                  <input className="input w-20" type="number" title="priority" value={m.priority}
                    onChange={(e) => { const ms = [...f.members]; ms[i] = { ...m, priority: +e.target.value }; setF({ ...f, members: ms }); }} />
                  <input className="input w-16" type="number" title="weight" value={m.weight}
                    onChange={(e) => { const ms = [...f.members]; ms[i] = { ...m, weight: +e.target.value }; setF({ ...f, members: ms }); }} />
                  <Toggle checked={m.manual_pinned} onChange={(v) => { const ms = [...f.members]; ms[i] = { ...m, manual_pinned: v }; setF({ ...f, members: ms }); }} label="pin" />
                  <button type="button" className="btn-ghost p-1.5 text-err" onClick={() => setF({ ...f, members: f.members.filter((_, j) => j !== i) })}>
                    <Trash2 size={14} />
                  </button>
                </div>
              ))}
              {f.members.length === 0 && <div className="text-xs text-muted">No members — add deployments to make this model routable.</div>}
            </div>
          )}

          <button className="btn-primary w-full justify-center">{editId ? "Save" : "Create"}</button>
        </form>
      </Modal>
    </div>
  );
}
