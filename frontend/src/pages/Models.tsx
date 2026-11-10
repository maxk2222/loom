import { useQuery, useQueryClient } from "@tanstack/react-query";
import { GripVertical, Plus, Trash2 } from "lucide-react";
import { useRef, useState, type FormEvent } from "react";
import { Link } from "react-router-dom";
import { api } from "../lib/api";
import { Badge, Card, ConfirmModal, Field, Modal, Select, Spinner, Toggle } from "../components/ui";
import type { DeploymentRow, Member, VirtualModel } from "../lib/types";

const STRATEGIES = [
  { value: "priority", label: "Priority — first reachable member wins", hint: "Requests go to the earliest member that can serve them. Drag members to order them." },
  { value: "least_in_flight", label: "Least in flight — least busy member", hint: "Requests go to the member with the most free capacity right now." },
  { value: "weighted", label: "Weighted — split by weight", hint: "Higher weight receives proportionally more requests." },
  { value: "round_robin", label: "Round robin — rotate evenly", hint: "Members take turns in order." },
  { value: "manual", label: "Manual — pinned members only", hint: "Only members marked pin can be picked (for router rules that pin a member explicitly)." },
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

function SectionTitle({ children }: { children: string }) {
  return <div className="border-t border-border pt-4 text-xs font-medium uppercase tracking-wide text-muted">{children}</div>;
}

export default function ModelsPage() {
  const qc = useQueryClient();
  const vms = useQuery({ queryKey: ["vms"], queryFn: () => api.get<{ models: VirtualModel[] }>("/api/models") });
  const deps = useQuery({ queryKey: ["deployments"], queryFn: () => api.get<{ deployments: DeploymentRow[] }>("/api/deployments") });
  const clis = useQuery({ queryKey: ["clients"], queryFn: () => api.get<{ clients: any[] }>("/api/clients") });
  const cname = (id: string) => clis.data?.clients.find((c: any) => c.id === id)?.name ?? id.slice(0, 8);
  const [open, setOpen] = useState(false);
  const [editId, setEditId] = useState<string | null>(null);
  const [f, setF] = useState<FormState>(EMPTY);
  // Drag state for member reordering (HTML5 native — no extra deps).
  const dragIdx = useRef<number | null>(null);
  const [overIdx, setOverIdx] = useState<number | null>(null);

  const refresh = () => qc.invalidateQueries({ queryKey: ["vms"] });

  const openCreate = () => { setEditId(null); setF(EMPTY); setOpen(true); };
  const openEdit = async (id: string) => {
    const d = await api.get<{ model: VirtualModel; members: Member[] }>(`/api/models/${id}`);
    const m = d.model;
    const rc = (m.required_capabilities ?? {}) as Record<string, boolean>;
    setEditId(id);
    // Sort members by stored priority so list order matches routing order.
    const members = [...(d.members ?? [])].sort((a, b) => a.priority - b.priority);
    setF({
      api_name: m.api_name, description: m.description ?? "", api_type: m.api_type,
      routing_strategy: m.routing_strategy, allow_heterogeneous: m.allow_heterogeneous,
      fallback_virtual_model_id: m.fallback_virtual_model_id ?? "",
      req_tools: !!rc.tools, req_vision: !!rc.vision,
      req_streaming: !!rc.streaming, req_json_mode: !!rc.json_mode,
      enabled: m.enabled, members,
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
      // List order IS the priority — derived on save so drag order is
      // the single source of truth (lower value = earlier pick).
      members: f.members
        .filter((m) => m.deployment_id)
        .map((m, i) => ({ ...m, priority: (i + 1) * 10 })),
      required_capabilities: {
        ...(f.req_tools ? { tools: true } : {}),
        ...(f.req_vision ? { vision: true } : {}),
        ...(f.req_streaming ? { streaming: true } : {}),
        ...(f.req_json_mode ? { json_mode: true } : {}),
      },
    };
    if (f.fallback_virtual_model_id) body.fallback_virtual_model_id = f.fallback_virtual_model_id;
    if (editId) {
      await api.put(`/api/models/${editId}`, body);
    } else {
      await api.post("/api/models", body);
    }
    setOpen(false);
    refresh();
  };

  const [pendingDel, setPendingDel] = useState<{ id: string; name: string } | null>(null);
  const del = async () => {
    if (!pendingDel) return;
    await api.del(`/api/models/${pendingDel.id}`);
    setPendingDel(null);
    refresh();
  };

  const setMember = (i: number, patch: Partial<Member>) => {
    const ms = [...f.members];
    ms[i] = { ...ms[i], ...patch };
    setF({ ...f, members: ms });
  };

  const memberDepIds = new Set(f.members.map(m => m.deployment_id));
  const availableDeps = (deps.data?.deployments ?? []).filter((d: any) => d.enabled !== false && !memberDepIds.has(d.id));
  // Group available deployments by client for the optgroup picker.
  const depsByClient = availableDeps.reduce<Record<string, any[]>>((acc, d: any) => {
    (acc[cname(d.client_id)] ??= []).push(d);
    return acc;
  }, {});

  const strat = STRATEGIES.find(s => s.value === f.routing_strategy);

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
                <button className="btn-danger py-1 text-xs" onClick={() => setPendingDel({ id: m.id, name: m.api_name })}>Delete</button>
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
          <SectionTitle>Basics</SectionTitle>
          <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
            <Field label="API name" hint="Model id used in /v1 requests">
              <input className="input font-mono" value={f.api_name} disabled={!!editId} onChange={(e) => setF({ ...f, api_name: e.target.value })} required placeholder="vm-llama3" />
            </Field>
            <Field label="API type"><Select value={f.api_type} onChange={(v) => setF({ ...f, api_type: v })} options={API_TYPES} /></Field>
          </div>
          <Field label="Description"><input className="input" value={f.description} onChange={(e) => setF({ ...f, description: e.target.value })} /></Field>

          <SectionTitle>Routing</SectionTitle>
          <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
            <Field label="Strategy" hint={strat?.hint}>
              <Select value={f.routing_strategy} onChange={(v) => setF({ ...f, routing_strategy: v })} options={STRATEGIES.map(s => ({ value: s.value, label: s.label }))} />
            </Field>
            <Field label="Fallback model" hint="Used when no member can serve">
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

          <SectionTitle>Members</SectionTitle>
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <span className="text-xs text-muted">
                {f.routing_strategy === "priority"
                  ? "Order = routing priority. Drag to reorder — earlier members are tried first."
                  : f.routing_strategy === "weighted"
                    ? "Set each member's weight — share = weight ÷ Σ weights."
                    : f.routing_strategy === "manual"
                      ? "Mark members pin to make them routable."
                      : "All members participate equally."}
              </span>
              {f.members.length > 0 && (
                <button type="button" className="btn-ghost py-1 text-xs text-muted" onClick={() => setF({ ...f, members: [] })}>Clear all</button>
              )}
            </div>
            {f.members.map((m, i) => (
              <div
                key={m.deployment_id || i}
                draggable
                onDragStart={() => { dragIdx.current = i; }}
                onDragEnter={() => {
                  const from = dragIdx.current;
                  if (from === null || from === i) return;
                  const ms = [...f.members];
                  const [mv] = ms.splice(from, 1);
                  ms.splice(i, 0, mv);
                  dragIdx.current = i;
                  setF({ ...f, members: ms });
                  setOverIdx(i);
                }}
                onDragOver={(e) => e.preventDefault()}
                onDragEnd={() => { dragIdx.current = null; setOverIdx(null); }}
                className={`flex items-center gap-2 rounded-md border bg-panel2 p-2 transition-colors ${overIdx === i ? "border-accent" : "border-border"}`}
              >
                <span className="cursor-grab text-muted hover:text-text active:cursor-grabbing" title="Drag to reorder"><GripVertical size={15} /></span>
                <span className="w-6 shrink-0 text-center text-[10px] font-semibold text-muted">#{i + 1}</span>
                <select className="input flex-1" value={m.deployment_id}
                  onChange={(e) => setMember(i, { deployment_id: e.target.value })}>
                  <option value="">— pick deployment —</option>
                  {Object.entries(depsByClient).map(([client, list]) => (
                    <optgroup key={client} label={client}>
                      {list.filter(d => d.id === m.deployment_id || !memberDepIds.has(d.id)).map(d => (
                        <option key={d.id} value={d.id}>{d.name || d.model_name}</option>
                      ))}
                    </optgroup>
                  ))}
                  {/* keep the current selection renderable even if its client group is empty */}
                  {m.deployment_id && !availableDeps.some(d => d.id === m.deployment_id) && (
                    <option value={m.deployment_id}>{(deps.data?.deployments ?? []).find(d => d.id === m.deployment_id)?.model_name ?? m.deployment_id.slice(0, 8)}</option>
                  )}
                </select>
                {f.routing_strategy === "weighted" && (
                  <input className="input w-16" type="number" min={1} title="weight" placeholder="w"
                    value={m.weight} onChange={(e) => setMember(i, { weight: +e.target.value })} />
                )}
                {f.routing_strategy === "manual" && (
                  <Toggle checked={m.manual_pinned} onChange={(v) => setMember(i, { manual_pinned: v })} label="pin" />
                )}
                <Toggle checked={m.enabled} onChange={(v) => setMember(i, { enabled: v })} label="on" />
                <button type="button" className="btn-ghost p-1.5 text-err" title="Remove"
                  onClick={() => setF({ ...f, members: f.members.filter((_, j) => j !== i) })}>
                  <Trash2 size={14} />
                </button>
              </div>
            ))}
            {/* Add member — dropdown lists deployments not already in the list */}
            {availableDeps.length > 0 ? (
              <select
                className="input w-full border-dashed text-muted"
                value=""
                onChange={(e) => {
                  if (!e.target.value) return;
                  setF({ ...f, members: [...f.members, { deployment_id: e.target.value, priority: (f.members.length + 1) * 10, weight: 1, manual_pinned: false, enabled: true }] });
                }}
              >
                <option value="">＋ Add deployment…</option>
                {Object.entries(depsByClient).map(([client, list]) => (
                  <optgroup key={client} label={client}>
                    {list.map(d => <option key={d.id} value={d.id}>{d.name || d.model_name}</option>)}
                  </optgroup>
                ))}
              </select>
            ) : f.members.length === 0 ? (
              <div className="rounded-md border border-dashed border-border p-3 text-center text-xs text-muted">
                No enabled deployments available — create one under Deployments first.
              </div>
            ) : null}
            {f.members.length === 0 && availableDeps.length > 0 && (
              <div className="text-xs text-muted">No members yet — add deployments to make this model routable.</div>
            )}
          </div>

          <button className="btn-primary w-full justify-center">{editId ? "Save" : "Create"}</button>
        </form>
      </Modal>
      <ConfirmModal
        open={pendingDel !== null}
        title="Delete virtual model"
        danger
        confirmLabel="Delete"
        body={pendingDel ? `Delete virtual model "${pendingDel.name}"?` : ""}
        onConfirm={del}
        onClose={() => setPendingDel(null)}
      />
    </div>
  );
}
