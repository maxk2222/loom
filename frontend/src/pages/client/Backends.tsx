import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { PlugZap, Plus, RefreshCw, Trash2 } from "lucide-react";
import { useState } from "react";
import { api } from "../../lib/api";
import { Badge, Card, Empty, Field, Modal, PageHeader, Select, Spinner, Toggle } from "../../components/ui";

interface DepForm {
  auto_load?: boolean;
  id: string;
  name: string;
  model_name: string;
  resolved_model?: string | null;
  max_concurrent: number;
  local_reserve: number;
  tier2_max: number;
  sharing_mode: string;
  enabled: boolean;
}

interface BackendForm {
  id: string;
  name: string;
  backend_type: string;
  endpoint: string;
  api_key: string;
  has_api_key?: boolean;
  enabled: boolean;
  remote?: boolean;
  api_version?: string;
  health_interval_secs: number;
  deployments: DepForm[];
  detected_models?: { model: string; loaded: boolean }[];
}

const EMPTY_DEP: DepForm = {
  id: "",
  name: "",
  model_name: "",
  max_concurrent: 4,
  local_reserve: 1,
  tier2_max: 2,
  sharing_mode: "shared",
  enabled: true,
  auto_load: false,
};

const EMPTY_BACKEND: BackendForm = {
  id: "",
  name: "",
  backend_type: "ollama",
  endpoint: "",
  api_key: "",
  enabled: true,
  remote: false,
  api_version: "2024-10-21",
  health_interval_secs: 15,
  deployments: [{ ...EMPTY_DEP }],
};

const TYPE_OPTS = [
  { value: "ollama", label: "Ollama" },
  { value: "vllm", label: "vLLM" },
  { value: "generic_open_ai", label: "OpenAI-compatible" },
  { value: "llama_cpp", label: "llama.cpp" },
  { value: "azure_open_ai", label: "Azure OpenAI" },
  { value: "anthropic", label: "Anthropic" },
];

// Hosted provider presets: endpoint + type + remote flag only —
// name/key/models stay user-supplied.
const PRESETS = [
  { id: "", label: "custom / self-hosted" },
  { id: "openrouter", label: "OpenRouter", name: "openrouter", endpoint: "https://openrouter.ai/api", backend_type: "generic_open_ai" },
  { id: "groq", label: "Groq", name: "groq", endpoint: "https://api.groq.com/openai", backend_type: "generic_open_ai" },
  { id: "together", label: "Together AI", name: "together", endpoint: "https://api.together.xyz", backend_type: "generic_open_ai" },
  { id: "fireworks", label: "Fireworks AI", name: "fireworks", endpoint: "https://api.fireworks.ai/inference", backend_type: "generic_open_ai" },
  { id: "deepinfra", label: "DeepInfra", name: "deepinfra", endpoint: "https://api.deepinfra.com", backend_type: "generic_open_ai" },
  { id: "openai", label: "OpenAI", name: "openai", endpoint: "https://api.openai.com", backend_type: "generic_open_ai" },
  { id: "azure", label: "Azure OpenAI", name: "azure", endpoint: "https://YOUR-RESOURCE.openai.azure.com", backend_type: "azure_open_ai" },
  { id: "anthropic", label: "Anthropic", name: "anthropic", endpoint: "https://api.anthropic.com", backend_type: "anthropic" },
];
const SHARE_OPTS = [
  { value: "shared", label: "shared" },
  { value: "opportunistic", label: "opportunistic" },
  { value: "private", label: "private" },
];

export default function BackendsPage() {
  const qc = useQueryClient();
  const q = useQuery({
    queryKey: ["client-backends"],
    queryFn: () => api.get<{ backends: (BackendForm & { url?: string; kind?: string })[] }>("/local/api/backends"),
    refetchInterval: 8000,
  });
  const [editing, setEditing] = useState<BackendForm | null>(null);
  const [testing, setTesting] = useState<null | { ok?: boolean; error?: string; latency_ms?: number; models?: string[] }>(null);

  const save = useMutation({
    mutationFn: (b: BackendForm) =>
      b.id ? api.put(`/local/api/backends/${b.id}`, { backend: b }) : api.post("/local/api/backends", { backend: b }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["client-backends"] });
      setEditing(null);
    },
  });
  const del = useMutation({
    mutationFn: (id: string) => api.del(`/local/api/backends/${id}`),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["client-backends"] }),
  });

  // One-click deploy of a detected model: append a deployment with fleet
  // defaults to the backend and save it in place.
  const addModel = useMutation({
    mutationFn: (p: { backend: BackendForm; model: string }) =>
      api.put(`/local/api/backends/${p.backend.id}`, {
        backend: {
          ...p.backend,
          api_key: "",
          deployments: [
            ...p.backend.deployments,
            {
              ...EMPTY_DEP,
              id: "",
              name: p.model === "auto" ? `${p.backend.name}-auto` : p.model,
              model_name: p.model,
            },
          ],
        },
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["client-backends"] }),
  });

  const probe = async () => {
    await api.post("/local/api/reprobe");
    qc.invalidateQueries({ queryKey: ["client-backends"] });
  };

  const test = async (b: BackendForm) => {
    setTesting({});
    try {
      const r = await api.post<{ ok?: boolean; error?: string; latency_ms?: number; models?: string[] }>(
        "/local/api/backends/test",
        { endpoint: b.endpoint, api_key: b.api_key, backend_type: b.backend_type, api_version: b.api_version },
      );
      setTesting(r);
    } catch (e: any) {
      setTesting({ ok: false, error: e?.message ?? "request failed" });
    }
  };

  const upd = (b: BackendForm, patch: Partial<BackendForm>) => setEditing({ ...b, ...patch });
  const updDep = (b: BackendForm, i: number, patch: Partial<DepForm>) => {
    const ds = b.deployments.map((d, j) => (j === i ? { ...d, ...patch } : d));
    upd(b, { deployments: ds });
  };

  return (
    <div className="space-y-5">
      <PageHeader
        title="Backends & models"
        desc="Inference backends running on this workstation"
        actions={
          <>
            <button className="btn-outline" onClick={probe}>
              <RefreshCw size={15} /> Re-probe
            </button>
            <button className="btn-primary" onClick={() => setEditing({ ...EMPTY_BACKEND, deployments: [{ ...EMPTY_DEP }] })}>
              <Plus size={15} /> Add backend
            </button>
          </>
        }
      />
      {q.isLoading ? (
        <Spinner />
      ) : (q.data?.backends ?? []).length === 0 ? (
        <Card>
          <Empty msg="No backends configured — add one to expose models." />
        </Card>
      ) : (
        <div className="space-y-4">
          {(q.data?.backends ?? []).map((b) => (
            <Card key={b.id} className="space-y-3" hover>
              <div className="flex items-center justify-between">
                <div className="flex items-center gap-3">
                  <Badge tone="accent">{b.backend_type}</Badge>
                  {b.remote && <Badge tone="muted">remote</Badge>}
                  <span className="font-mono text-xs text-muted">{b.endpoint}</span>
                  {b.has_api_key && <Badge tone="muted">api key</Badge>}
                  {!b.enabled && <Badge tone="err">disabled</Badge>}
                </div>
                <div className="flex gap-1">
                  <button className="btn-ghost px-2 py-1 text-xs" onClick={() => setEditing({ ...b, api_key: "" })}>
                    Edit
                  </button>
                  <button
                    className="btn-ghost px-2 py-1 text-xs text-err"
                    onClick={() => confirm(`Delete backend "${b.name}"?`) && del.mutate(b.id)}
                  >
                    <Trash2 size={14} />
                  </button>
                </div>
              </div>
              <table className="w-full">
                <thead>
                  <tr>
                    <th className="th">Deployment</th>
                    <th className="th">Model</th>
                    <th className="th">Limits</th>
                    <th className="th">Sharing</th>
                    <th className="th">Enabled</th>
                  </tr>
                </thead>
                <tbody>
                  {b.deployments.map((d) => (
                    <tr key={d.id}>
                      <td className="td">{d.name}</td>
                      <td className="td font-mono text-xs text-muted">
                        {d.model_name === "auto" ? (
                          <>
                            <Badge tone="accent">auto</Badge>{" "}
                            {d.resolved_model ?? "—"}
                          </>
                        ) : (
                          d.model_name
                        )}
                      </td>
                      <td className="td text-xs text-muted">
                        max {d.max_concurrent} · res {d.local_reserve} · t2 {d.tier2_max}
                      </td>
                      <td className="td text-xs text-muted">{d.sharing_mode}</td>
                      <td className="td">{d.enabled ? <Badge tone="ok">on</Badge> : <Badge tone="err">off</Badge>}{d.auto_load && <Badge tone="muted"> · autoload</Badge>}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
              {(b.detected_models?.length ?? 0) > 0 && (
                <div className="flex flex-wrap items-center gap-2 border-t border-border/40 pt-3">
                  <span className="text-xs text-muted">Detected:</span>
                  {b.detected_models!.map((m) => (
                    <button
                      key={m.model}
                      className="btn-ghost flex items-center gap-1.5 border border-border/60 px-2 py-1 font-mono text-xs"
                      title={m.loaded ? "Loaded — click to add deployment" : "Listed — click to add deployment"}
                      onClick={() => addModel.mutate({ backend: b, model: m.model })}
                    >
                      <span className={m.loaded ? "text-ok" : "text-muted"}>●</span>
                      {m.model}
                      <Plus size={12} />
                    </button>
                  ))}
                  {b.detected_models!.some((m) => m.loaded) &&
                    !b.deployments.some((d) => d.model_name === "auto") && (
                    <button
                      className="btn-ghost flex items-center gap-1 border border-accent/50 px-2 py-1 text-xs text-accent"
                      title="Deployment that always uses the model currently loaded in this backend"
                      onClick={() => addModel.mutate({ backend: b, model: "auto" })}
                    >
                      <Plus size={12} /> auto (current model)
                    </button>
                  )}
                </div>
              )}
            </Card>
          ))}
        </div>
      )}

      <Modal open={!!editing} title={editing?.id ? "Edit backend" : "Add backend"} onClose={() => { setEditing(null); setTesting(null); }} wide>
        {editing && (
          <div className="space-y-4">
            <div className="grid grid-cols-3 gap-3">
              <Field label="Name">
                <input className="input" value={editing.name} onChange={(e) => upd(editing, { name: e.target.value })} placeholder="e.g. ollama-main" />
              </Field>
              <Field label="Type">
                <Select value={editing.backend_type} onChange={(v) => upd(editing, { backend_type: v })} options={TYPE_OPTS} />
              </Field>
              <Field label="Preset">
                <Select
                  value=""
                  onChange={(v) => {
                    const p = PRESETS.find(x => x.id === v);
                    if (p?.id) upd(editing, {
                      name: editing.name || p.name!,
                      endpoint: p.endpoint!,
                      backend_type: p.backend_type!,
                      remote: true,
                    });
                  }}
                  options={PRESETS.map(p => ({ value: p.id, label: p.label }))}
                />
              </Field>
            </div>
            <Field label="Endpoint URL" hint="Base URL, e.g. http://127.0.0.1:11434 or http://gpu-01:8000">
              <input className="input font-mono" value={editing.endpoint} onChange={(e) => upd(editing, { endpoint: e.target.value })} placeholder="http://…" />
            </Field>
            <div className="flex items-center gap-4">
              <label className="flex cursor-pointer items-center gap-2 text-xs text-text">
                <input
                  type="checkbox"
                  className="accent-accent"
                  checked={!!editing.remote}
                  onChange={(e) => upd(editing, { remote: e.target.checked })}
                />
                remote / hosted provider
                <span className="text-muted">(no local compute — skips auto-load warm-up)</span>
              </label>
              {editing.backend_type === "azure_open_ai" && (
                <Field label="api-version">
                  <input
                    className="input w-36 font-mono"
                    value={editing.api_version ?? "2024-10-21"}
                    onChange={(e) => upd(editing, { api_version: e.target.value })}
                  />
                </Field>
              )}
            </div>
            <div className="flex items-end gap-3">
              <Field label={editing.has_api_key || editing.id ? "API key (blank = keep existing)" : "API key (optional)"}>
                <input
                  className="input font-mono"
                  type="password"
                  value={editing.api_key}
                  onChange={(e) => upd(editing, { api_key: e.target.value })}
                  placeholder={editing.has_api_key ? "••••••••" : "sk-…"}
                  autoComplete="off"
                />
              </Field>
              <button className="btn-outline shrink-0" onClick={() => test(editing)} disabled={!editing.endpoint}>
                <PlugZap size={15} /> Test connection
              </button>
            </div>
            {testing && (
              <div className={`rounded-lg px-3 py-2 text-sm ${testing.ok === undefined ? "bg-panel2 text-muted" : testing.ok ? "bg-ok/10 text-ok" : "bg-err/10 text-err"}`}>
                {testing.ok === undefined
                  ? "Testing…"
                  : testing.ok
                    ? `Connected in ${testing.latency_ms}ms — ${testing.models?.length ?? 0} models: ${(testing.models ?? []).slice(0, 6).join(", ")}`
                    : `Failed: ${testing.error ?? `status ${(testing as any).status ?? "?"}`}`}
              </div>
            )}
            <div className="space-y-3">
              <div className="flex items-center justify-between">
                <div className="label mb-0">Deployments</div>
                <button
                  className="btn-ghost px-2 py-1 text-xs"
                  onClick={() => upd(editing, { deployments: [...editing.deployments, { ...EMPTY_DEP }] })}
                >
                  <Plus size={13} /> add
                </button>
              </div>
              {editing.deployments.map((d, i) => (
                <div key={i} className="card space-y-2 border-border/70 p-3">
                  <div className="grid grid-cols-2 gap-2">
                    <Field label="Deployment name">
                      <input className="input" value={d.name} onChange={(e) => updDep(editing, i, { name: e.target.value })} placeholder="llama-8b" />
                    </Field>
                    <Field label="Physical model" hint='Use "auto" to always use the model currently loaded in this backend'>
                      <input className="input font-mono" value={d.model_name} onChange={(e) => updDep(editing, i, { model_name: e.target.value })} placeholder='llama3.1:8b or "auto"' />
                    </Field>
                  </div>
                  <div className="grid grid-cols-4 gap-2">
                    <Field label="Max">
                      <input className="input" type="number" min={1} value={d.max_concurrent} onChange={(e) => updDep(editing, i, { max_concurrent: +e.target.value })} />
                    </Field>
                    <Field label="Reserve">
                      <input className="input" type="number" min={0} value={d.local_reserve} onChange={(e) => updDep(editing, i, { local_reserve: +e.target.value })} />
                    </Field>
                    <Field label="Tier-2">
                      <input className="input" type="number" min={0} value={d.tier2_max} onChange={(e) => updDep(editing, i, { tier2_max: +e.target.value })} />
                    </Field>
                    <Field label="Sharing">
                      <Select value={d.sharing_mode} onChange={(v) => updDep(editing, i, { sharing_mode: v })} options={SHARE_OPTS} />
                    </Field>
                  </div>
                  <div className="flex items-center justify-between">
                    <div className="flex items-center gap-4">
                      <Toggle checked={d.enabled} onChange={(v) => updDep(editing, i, { enabled: v })} label="enabled" />
                      <Toggle checked={!!d.auto_load} onChange={(v) => updDep(editing, i, { auto_load: v })} label="auto-load" />
                    </div>
                    {editing.deployments.length > 1 && (
                      <button
                        className="btn-ghost px-2 py-1 text-xs text-err"
                        onClick={() => upd(editing, { deployments: editing.deployments.filter((_, j) => j !== i) })}
                      >
                        <Trash2 size={13} />
                      </button>
                    )}
                  </div>
                </div>
              ))}
            </div>
            <div className="flex items-center justify-between border-t border-border pt-3">
              <Toggle checked={editing.enabled} onChange={(v) => upd(editing, { enabled: v })} label="backend enabled" />
              <div className="flex gap-2">
                <button className="btn-ghost" onClick={() => setEditing(null)}>Cancel</button>
                <button
                  className="btn-primary"
                  disabled={!editing.name || !editing.endpoint || editing.deployments.some((d) => !d.name || !d.model_name)}
                  onClick={() => save.mutate(editing)}
                >
                  {editing.id ? "Save" : "Add backend"}
                </button>
              </div>
            </div>
          </div>
        )}
      </Modal>
    </div>
  );
}
