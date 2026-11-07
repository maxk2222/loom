import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, Route, Trash2 } from "lucide-react";
import { useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { Badge, Card, Field, Modal, Select, Spinner, Toggle } from "../components/ui";
import type { DeploymentRow, VirtualModel } from "../lib/types";

interface RouterMember {
  ref: string; // "vm:<api_name>" | "dep:<uuid>"
  name: string;
  description: string;
}
interface RouterConfig {
  router_model: string;
  system_prompt: string;
  fallback_model: string;
  max_user_chars: number;
  members: RouterMember[];
}
interface RouterRow {
  id: string;
  api_name: string;
  description: string;
  enabled: boolean;
  config: Partial<RouterConfig>;
}

const EMPTY_CFG: RouterConfig = {
  router_model: "", system_prompt: "", fallback_model: "", max_user_chars: 2000, members: [],
};

export default function RoutersPage() {
  const qc = useQueryClient();
  const routers = useQuery({
    queryKey: ["routers"],
    queryFn: () => api.get<{ routers: RouterRow[] }>("/api/routers"),
    refetchInterval: 15000,
  });
  const vms = useQuery({ queryKey: ["vms"], queryFn: () => api.get<{ models: (VirtualModel & { kind?: string })[] }>("/api/models") });
  const deps = useQuery({ queryKey: ["deployments"], queryFn: () => api.get<{ deployments: DeploymentRow[] }>("/api/deployments") });
  const engines = (vms.data?.models ?? []).filter((m) => (m.kind ?? "vm") === "vm" && m.api_type !== "embeddings");

  const [open, setOpen] = useState(false);
  const [editId, setEditId] = useState<string | null>(null);
  const [f, setF] = useState({ api_name: "", description: "", enabled: true, config: EMPTY_CFG });
  const [testId, setTestId] = useState<string | null>(null);
  const [testPrompt, setTestPrompt] = useState("");
  const [testRes, setTestRes] = useState<any>(null);
  const [testing, setTesting] = useState(false);

  const refresh = () => qc.invalidateQueries({ queryKey: ["routers"] });
  const setC = (patch: Partial<RouterConfig>) => setF({ ...f, config: { ...f.config, ...patch } });

  const openCreate = () => { setEditId(null); setF({ api_name: "", description: "", enabled: true, config: EMPTY_CFG }); setOpen(true); };
  const openEdit = (r: RouterRow) => {
    setEditId(r.id);
    setF({
      api_name: r.api_name, description: r.description ?? "", enabled: r.enabled,
      config: { ...EMPTY_CFG, ...(r.config as RouterConfig), members: r.config?.members ?? [] },
    });
    setOpen(true);
  };

  const save = async (e: FormEvent) => {
    e.preventDefault();
    const body = { api_name: f.api_name, description: f.description, enabled: f.enabled, config: f.config };
    if (editId) await api.put(`/api/routers/${editId}`, body);
    else await api.post("/api/routers", body);
    setOpen(false);
    refresh();
  };

  const del = async (id: string) => {
    if (!confirm("Delete this router?")) return;
    await api.del(`/api/routers/${id}`);
    refresh();
  };

  const addMember = () =>
    setC({ members: [...f.config.members, { ref: "", name: "", description: "" }] });
  const setMember = (i: number, patch: Partial<RouterMember>) => {
    const members = f.config.members.slice();
    members[i] = { ...members[i], ...patch };
    setC({ members });
  };
  const memberLabel = (ref: string) => {
    if (ref.startsWith("vm:")) return `virtual · ${ref.slice(3)}`;
    if (ref.startsWith("dep:")) {
      const d = (deps.data?.deployments ?? []).find((x) => x.id === ref.slice(4));
      return d ? `deployment · ${d.name} (${d.client_name ?? "?"})` : `deployment · ${ref.slice(4, 12)}`;
    }
    return "pick a model…";
  };

  const runTest = async () => {
    if (!testId) return;
    setTesting(true);
    setTestRes(await api.post(`/api/routers/${testId}/test`, { prompt: testPrompt }).catch(() => ({ ok: false, error: "request failed" })));
    setTesting(false);
  };

  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-lg font-semibold">Auto Router</h1>
          <p className="mt-0.5 text-xs text-muted">
            Virtual models that classify each request with a small router model and forward it
            to the best-matching member — by speed, cost or task fit.
          </p>
        </div>
        <button className="btn-primary" onClick={openCreate}><Plus size={16} /> New router</button>
      </div>

      {routers.isLoading ? <Spinner /> : (routers.data?.routers ?? []).length === 0 ? (
        <Card className="py-10 text-center text-sm text-muted">
          No routers yet — create one and call it like any model (e.g. <code>model: "loom-auto"</code>).
        </Card>
      ) : (
        <div className="grid gap-3 md:grid-cols-2">
          {(routers.data?.routers ?? []).map((r) => (
            <Card key={r.id} className="space-y-3">
              <div className="flex items-center justify-between">
                <div className="flex items-center gap-2 font-medium"><Route size={15} className="text-accent" />{r.api_name}</div>
                <Badge tone={r.enabled ? "ok" : "muted"}>{r.enabled ? "enabled" : "disabled"}</Badge>
              </div>
              {r.description && <div className="text-xs text-muted">{r.description}</div>}
              <div className="flex flex-wrap gap-1.5 text-[11px]">
                <Badge tone="accent">engine: {r.config?.router_model || "—"}</Badge>
                <Badge tone="muted">{(r.config?.members ?? []).length} members</Badge>
                {r.config?.fallback_model && <Badge tone="warn">fallback: {r.config.fallback_model}</Badge>}
              </div>
              <div className="space-y-1 border-t border-border/60 pt-2">
                {(r.config?.members ?? []).map((m, i) => (
                  <div key={i} className="flex items-baseline gap-2 text-xs">
                    <span className="font-mono text-text/90">{m.name}</span>
                    <span className="truncate text-muted/80">{memberLabel(m.ref)} — {m.description}</span>
                  </div>
                ))}
              </div>
              <div className="flex gap-2 border-t border-border/60 pt-2.5">
                <button className="btn-outline py-1 text-xs" onClick={() => { setTestId(r.id); setTestRes(null); }}>Test</button>
                <button className="btn-outline py-1 text-xs" onClick={() => openEdit(r)}>Edit</button>
                <button className="btn-ghost ml-auto py-1 text-xs text-err" onClick={() => del(r.id)}><Trash2 size={14} /></button>
              </div>
            </Card>
          ))}
        </div>
      )}

      {/* test console */}
      {testId && (
        <Modal open title="Test router classification" onClose={() => setTestId(null)}>
          <div className="space-y-3">
            <Field label="User prompt">
              <textarea className="input min-h-24" value={testPrompt} onChange={(e) => setTestPrompt(e.target.value)}
                placeholder="e.g. refactor this function to use async iterators…" />
            </Field>
            <button className="btn-primary" disabled={testing || !testPrompt.trim()} onClick={runTest}>
              {testing ? "Classifying…" : "Run router"}
            </button>
            {testRes && (
              <Card className="space-y-1.5 bg-panel2/60 text-xs">
                {testRes.ok ? (
                  <>
                    <div><span className="text-muted">router said:</span> <code className="text-accent">{testRes.raw || "∅"}</code></div>
                    <div><span className="text-muted">picked:</span>{" "}
                      {testRes.pick ? <Badge tone="ok">{testRes.pick.model} ({testRes.pick.reason})</Badge> : <Badge tone="warn">no member matched → fallback</Badge>}
                    </div>
                  </>
                ) : (
                  <div className="text-err">{testRes.error}</div>
                )}
              </Card>
            )}
          </div>
        </Modal>
      )}

      {/* create/edit modal */}
      {open && (
        <Modal open title={editId ? "Edit router" : "New router"} onClose={() => setOpen(false)}>
          <form onSubmit={save} className="space-y-4">
            <div className="grid grid-cols-2 gap-3">
              <Field label="API name">
                <input className="input font-mono" required value={f.api_name} placeholder="loom-auto"
                  onChange={(e) => setF({ ...f, api_name: e.target.value })} />
              </Field>
              <Field label="Router engine (classifier model)">
                <Select value={f.config.router_model}
                  options={[{ value: "", label: "choose…" }, ...engines.map((m) => ({ value: m.api_name, label: m.api_name }))]}
                  onChange={(v) => setC({ router_model: v })} />
              </Field>
            </div>
            <Field label="Description">
              <input className="input" value={f.description} onChange={(e) => setF({ ...f, description: e.target.value })} />
            </Field>

            <div className="space-y-2">
              <div className="flex items-center justify-between">
                <label className="label mb-0">Member models</label>
                <button type="button" className="btn-ghost px-2 py-0.5 text-xs" onClick={addMember}><Plus size={13} /> add</button>
              </div>
              {f.config.members.map((m, i) => (
                <Card key={i} className="grid grid-cols-[1fr,140px,1fr,28px] items-center gap-2 bg-panel2/50 p-2.5">
                  <Select value={m.ref} onChange={(ref) => {
                    const auto = ref.startsWith("vm:") ? ref.slice(3)
                      : (deps.data?.deployments ?? []).find((d) => d.id === ref.slice(4))?.model_name ?? "";
                    setMember(i, { ref, name: m.name || auto });
                  }}
                    options={[
                      { value: "", label: memberLabel(m.ref) },
                      ...engines.map((v) => ({ value: `vm:${v.api_name}`, label: `vm · ${v.api_name}` })),
                      ...(deps.data?.deployments ?? []).filter((d) => d.enabled).map((d) => ({
                        value: `dep:${d.id}`, label: `dep · ${d.name} · ${d.model_name} (${d.client_name})`,
                      })),
                    ]} />
                  <input className="input font-mono py-1.5" placeholder="choice name" value={m.name}
                    onChange={(e) => setMember(i, { name: e.target.value })} />
                  <input className="input py-1.5" placeholder="what it's good at…" value={m.description}
                    onChange={(e) => setMember(i, { description: e.target.value })} />
                  <button type="button" className="btn-ghost p-1 text-err"
                    onClick={() => setC({ members: f.config.members.filter((_, j) => j !== i) })}>
                    <Trash2 size={14} />
                  </button>
                </Card>
              ))}
              {f.config.members.length === 0 && (
                <div className="text-xs text-muted">Add at least two members — the router picks one per request.</div>
              )}
            </div>

            <div className="grid grid-cols-2 gap-3">
              <Field label="Fallback model">
                <Select value={f.config.fallback_model}
                  options={[{ value: "", label: "none" }, ...engines.map((m) => ({ value: m.api_name, label: m.api_name }))]}
                  onChange={(v) => setC({ fallback_model: v })} />
              </Field>
              <Field label="Max chars classified">
                <input className="input" type="number" min={100} max={8000} value={f.config.max_user_chars}
                  onChange={(e) => setC({ max_user_chars: Number(e.target.value) || 2000 })} />
              </Field>
            </div>

            <Field label="Prompt template (optional — {{models}} and {{request}} are substituted)">
              <textarea className="input min-h-24 font-mono text-xs" value={f.config.system_prompt}
                placeholder="You are a request router…\nAvailable models:\n{{models}}"
                onChange={(e) => setC({ system_prompt: e.target.value })} />
            </Field>

            <div className="flex items-center justify-between pt-1">
              <Toggle checked={f.enabled} onChange={(v) => setF({ ...f, enabled: v })} label="Enabled" />
              <button className="btn-primary" disabled={!f.api_name || !f.config.router_model || f.config.members.length === 0}>
                {editId ? "Save" : "Create router"}
              </button>
            </div>
          </form>
        </Modal>
      )}
    </div>
  );
}
