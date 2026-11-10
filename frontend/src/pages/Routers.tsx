import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowDown, ArrowUp, Plus, Route, Trash2 } from "lucide-react";
import { useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { Badge, Card, ConfirmModal, Field, Modal, Select, Spinner, Toggle } from "../components/ui";
import type { DeploymentRow, VirtualModel } from "../lib/types";

interface WhenRule {
  chars_lt?: number;
  chars_gte?: number;
  tokens_lt?: number;
  tokens_gte?: number;
  msgs_lt?: number;
  msgs_gte?: number;
  code?: boolean;
  images?: boolean;
  audio?: boolean;
  system?: boolean;
  tools?: boolean;
  json_mode?: boolean;
  reasoning?: boolean;
  effort_gte?: number | string; // level name or 0..7
  effort_lt?: number | string;
  any?: WhenRule[]; // OR — editable via API; rendered read-only here
  not?: WhenRule; // negation — editable via API
}
interface RouterMember {
  ref: string; // "vm:<api_name>" | "dep:<uuid>"
  name: string;
  description: string;
  when?: WhenRule; // heuristic mode: first matching member wins
  tags?: string[]; // shown to the LLM engine as [tags] in the model menu
  set?: Record<string, unknown>; // request overrides merged on dispatch
}
interface RouterConfig {
  mode: "llm" | "heuristic";
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
  mode: "llm", router_model: "", system_prompt: "", fallback_model: "", max_user_chars: 2000, members: [],
};

const TRI = [
  { value: "", label: "any" },
  { value: "true", label: "yes" },
  { value: "false", label: "no" },
];

// OpenAI reasoning_effort levels — stored as names, the backend maps them.
const EFFORTS = ["", "none", "minimal", "low", "medium", "high", "xhigh", "max"].map((v) => ({
  value: v,
  label: v || "any",
}));

export default function RoutersPage() {
  const qc = useQueryClient();
  const routers = useQuery({
    queryKey: ["routers"],
    queryFn: () => api.get<{ routers: RouterRow[] }>("/api/routers"),
    refetchInterval: 15000,
  });
  const vms = useQuery({ queryKey: ["vms"], queryFn: () => api.get<{ models: (VirtualModel & { kind?: string })[] }>("/api/models") });
  const deps = useQuery({ queryKey: ["deployments"], queryFn: () => api.get<{ deployments: DeploymentRow[] }>("/api/deployments") });
  // Engines must answer chat completions; members are routed chat traffic.
  // Routers and non-chat models (media/embedding only) are excluded.
  const engines = (vms.data?.models ?? []).filter(
    (m) => (m.kind ?? "vm") === "vm" && ["chat", "multi", "completions"].includes(m.api_type),
  );

  const [open, setOpen] = useState(false);
  const [editId, setEditId] = useState<string | null>(null);
  const [f, setF] = useState({ api_name: "", description: "", enabled: true, config: EMPTY_CFG });
  const [testId, setTestId] = useState<string | null>(null);
  const [testPrompt, setTestPrompt] = useState("");
  const [testRes, setTestRes] = useState<any>(null);
  const [testing, setTesting] = useState(false);
  const [saveErr, setSaveErr] = useState("");

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
    setSaveErr("");
    const body = { api_name: f.api_name, description: f.description, enabled: f.enabled, config: f.config };
    try {
      if (editId) await api.put(`/api/routers/${editId}`, body);
      else await api.post("/api/routers", body);
      setOpen(false);
      refresh();
    } catch (err: any) {
      setSaveErr(err?.message ?? "save failed");
    }
  };

  const [pendingDel, setPendingDel] = useState<{ id: string; name: string } | null>(null);
  const del = async () => {
    if (!pendingDel) return;
    await api.del(`/api/routers/${pendingDel.id}`);
    setPendingDel(null);
    refresh();
  };

  const isHeuristic = f.config.mode === "heuristic";
  const addMember = () =>
    setC({ members: [...f.config.members, { ref: "", name: "", description: "" }] });
  const setMember = (i: number, patch: Partial<RouterMember>) => {
    const members = f.config.members.slice();
    members[i] = { ...members[i], ...patch };
    setC({ members });
  };
  const setWhen = (i: number, patch: Partial<WhenRule>) => {
    const members = f.config.members.slice();
    const w = { ...(members[i].when ?? {}), ...patch } as Record<string, unknown>;
    for (const k of Object.keys(w)) if (w[k] === undefined || w[k] === "") delete w[k];
    members[i] = { ...members[i], when: w as WhenRule };
    setC({ members });
  };
  const moveMember = (i: number, dir: -1 | 1) => {
    const j = i + dir;
    if (j < 0 || j >= f.config.members.length) return;
    const members = f.config.members.slice();
    [members[i], members[j]] = [members[j], members[i]];
    setC({ members });
  };
  const setSet = (i: number, patch: Record<string, unknown>) => {
    const members = f.config.members.slice();
    const s = { ...(members[i].set ?? {}), ...patch };
    for (const k of Object.keys(s)) if (s[k] === undefined || s[k] === "") delete s[k];
    members[i] = { ...members[i], set: Object.keys(s).length ? s : undefined };
    setC({ members });
  };
  const whenSummary = (w?: WhenRule) => {
    if (!w || Object.keys(w).length === 0) return "catch-all";
    const fmt = (v: unknown) =>
      typeof v === "boolean" ? (v ? "yes" : "no")
        : typeof v === "object" && v !== null ? JSON.stringify(v)
        : String(v);
    return Object.entries(w)
      .map(([k, v]) => `${k.replace("_lt", "<").replace("_gte", "≥")} ${fmt(v)}`)
      .join(" · ");
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
                <Badge tone={r.config?.mode === "heuristic" ? "warn" : "accent"}>
                  {r.config?.mode === "heuristic" ? "rules" : `engine: ${r.config?.router_model || "—"}`}
                </Badge>
                <Badge tone="muted">{(r.config?.members ?? []).length} members</Badge>
                {r.config?.fallback_model && <Badge tone="warn">fallback: {r.config.fallback_model}</Badge>}
              </div>
              <div className="space-y-1 border-t border-border/60 pt-2">
                {(r.config?.members ?? []).map((m, i) => (
                  <div key={i} className="flex items-baseline gap-2 text-xs">
                    <span className="font-mono text-text/90">{m.name}</span>
                    {r.config?.mode === "heuristic" && (
                      <span className="rounded bg-panel2 px-1 font-mono text-[10px] text-accent/90">{whenSummary(m.when)}</span>
                    )}
                    <span className="truncate text-muted/80">{memberLabel(m.ref)} — {m.description}</span>
                  </div>
                ))}
              </div>
              <div className="flex gap-2 border-t border-border/60 pt-2.5">
                <button className="btn-outline py-1 text-xs" onClick={() => { setTestId(r.id); setTestRes(null); }}>Test</button>
                <button className="btn-outline py-1 text-xs" onClick={() => openEdit(r)}>Edit</button>
                <button className="btn-ghost ml-auto py-1 text-xs text-err" onClick={() => setPendingDel({ id: r.id, name: r.api_name })}><Trash2 size={14} /></button>
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
                    {testRes.pick?.set && Object.keys(testRes.pick.set).length > 0 && (
                      <div><span className="text-muted">overrides:</span> <code className="text-accent">{JSON.stringify(testRes.pick.set)}</code></div>
                    )}
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
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
              <Field label="API name">
                <input className="input font-mono" required value={f.api_name} placeholder="loom-auto"
                  onChange={(e) => setF({ ...f, api_name: e.target.value })} />
              </Field>
              <Field label="Routing mode" hint={isHeuristic ? "first matching member wins — order matters" : undefined}>
                <Select value={f.config.mode}
                  options={[
                    { value: "llm", label: "LLM classifier" },
                    { value: "heuristic", label: "Heuristic rules" },
                  ]}
                  onChange={(v) => setC({ mode: v as RouterConfig["mode"] })} />
              </Field>
            </div>
            {!isHeuristic && (
              <Field label="Router engine (classifier model)">
                <Select value={f.config.router_model}
                  options={[{ value: "", label: "choose…" }, ...engines.map((m) => ({ value: m.api_name, label: m.api_name }))]}
                  onChange={(v) => setC({ router_model: v })} />
              </Field>
            )}
            <Field label="Description">
              <input className="input" value={f.description} onChange={(e) => setF({ ...f, description: e.target.value })} />
            </Field>

            <div className="space-y-2">
              <div className="flex items-center justify-between">
                <label className="label mb-0">Member models</label>
                <button type="button" className="btn-ghost px-2 py-0.5 text-xs" onClick={addMember}><Plus size={13} /> add</button>
              </div>
              {f.config.members.map((m, i) => (
                <Card key={i} className="space-y-2 bg-panel2/50 p-2.5">
                  <div className="grid grid-cols-[1fr,140px,1fr,52px] items-center gap-2">
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
                    <div className="flex items-center gap-0.5">
                      {isHeuristic && (
                        <>
                          <button type="button" className="btn-ghost p-1" disabled={i === 0}
                            onClick={() => moveMember(i, -1)}><ArrowUp size={13} /></button>
                          <button type="button" className="btn-ghost p-1" disabled={i === f.config.members.length - 1}
                            onClick={() => moveMember(i, 1)}><ArrowDown size={13} /></button>
                        </>
                      )}
                      <button type="button" className="btn-ghost p-1 text-err"
                        onClick={() => setC({ members: f.config.members.filter((_, j) => j !== i) })}>
                        <Trash2 size={14} />
                      </button>
                    </div>
                  </div>
                  {isHeuristic && (
                    <div className="space-y-1.5 border-t border-border/40 pt-2">
                      <div className="grid grid-cols-[repeat(4,minmax(70px,1fr)),repeat(4,minmax(60px,1fr))] items-end gap-1.5">
                        {(["chars_lt", "chars_gte", "tokens_lt", "tokens_gte"] as const).map((k) => (
                          <label key={k} className="text-[10px] text-muted">
                            {k.replace("_", " ")}
                            <input className="input mt-0.5 px-1.5 py-1 font-mono text-[11px]" type="number" min={0}
                              value={m.when?.[k] ?? ""} placeholder="—"
                              onChange={(e) => setWhen(i, { [k]: e.target.value === "" ? undefined : Number(e.target.value) })} />
                          </label>
                        ))}
                        {(["code", "images", "audio", "system"] as const).map((k) => (
                          <label key={k} className="text-[10px] text-muted">
                            {k}
                            <Select value={m.when?.[k] === undefined ? "" : String(m.when[k])}
                              options={TRI}
                              onChange={(v) => setWhen(i, { [k]: v === "" ? undefined : v === "true" })} />
                          </label>
                        ))}
                      </div>
                      <div className="grid grid-cols-[repeat(2,minmax(70px,1fr)),repeat(4,minmax(60px,1fr)),minmax(64px,1fr)] items-end gap-1.5">
                        <label className="text-[10px] text-muted">effort ≥
                          <Select value={String(m.when?.effort_gte ?? "")} options={EFFORTS}
                            onChange={(v) => setWhen(i, { effort_gte: v === "" ? undefined : v })} />
                        </label>
                        <label className="text-[10px] text-muted">effort &lt;
                          <Select value={String(m.when?.effort_lt ?? "")} options={EFFORTS}
                            onChange={(v) => setWhen(i, { effort_lt: v === "" ? undefined : v })} />
                        </label>
                        <label className="text-[10px] text-muted">msgs ≥
                          <input className="input mt-0.5 px-1.5 py-1 font-mono text-[11px]" type="number" min={0}
                            value={m.when?.msgs_gte ?? ""} placeholder="—"
                            onChange={(e) => setWhen(i, { msgs_gte: e.target.value === "" ? undefined : Number(e.target.value) })} />
                        </label>
                        {(["reasoning", "tools", "json_mode"] as const).map((k) => (
                          <label key={k} className="text-[10px] text-muted">
                            {k.replace("_", " ")}
                            <Select value={m.when?.[k] === undefined ? "" : String(m.when[k])}
                              options={TRI}
                              onChange={(v) => setWhen(i, { [k]: v === "" ? undefined : v === "true" })} />
                          </label>
                        ))}
                        <label className="text-[10px] text-muted">msgs &lt;
                          <input className="input mt-0.5 px-1.5 py-1 font-mono text-[11px]" type="number" min={0}
                            value={m.when?.msgs_lt ?? ""} placeholder="—"
                            onChange={(e) => setWhen(i, { msgs_lt: e.target.value === "" ? undefined : Number(e.target.value) })} />
                        </label>
                      </div>
                    </div>
                  )}
                  {/* member extras: engine tags + request overrides (escalation) */}
                  <div className="grid grid-cols-[1fr,minmax(90px,120px),minmax(70px,90px),minmax(80px,100px)] items-end gap-1.5 border-t border-border/40 pt-2">
                    <label className="text-[10px] text-muted">
                      tags {!isHeuristic && <span className="text-muted/60">(engine hint)</span>}
                      <input className="input mt-0.5 px-1.5 py-1 font-mono text-[11px]"
                        placeholder="reasoning, vision…" value={(m.tags ?? []).join(",")}
                        onChange={(e) => setMember(i, {
                          tags: e.target.value.split(",").map((t) => t.trim()).filter(Boolean),
                        })} />
                    </label>
                    <label className="text-[10px] text-muted" title="Override merged into the request when this member is picked — reasoning escalation">
                      set effort
                      <Select value={String(m.set?.reasoning_effort ?? "")} options={EFFORTS}
                        onChange={(v) => setSet(i, { reasoning_effort: v === "" ? undefined : v })} />
                    </label>
                    <label className="text-[10px] text-muted">set temp
                      <input className="input mt-0.5 px-1.5 py-1 font-mono text-[11px]" type="number" step={0.1}
                        value={m.set?.temperature === undefined ? "" : String(m.set.temperature)} placeholder="—"
                        onChange={(e) => setSet(i, { temperature: e.target.value === "" ? undefined : Number(e.target.value) })} />
                    </label>
                    <label className="text-[10px] text-muted">set max tok
                      <input className="input mt-0.5 px-1.5 py-1 font-mono text-[11px]" type="number" min={0}
                        value={m.set?.max_tokens === undefined ? "" : String(m.set.max_tokens)} placeholder="—"
                        onChange={(e) => setSet(i, { max_tokens: e.target.value === "" ? undefined : Number(e.target.value) })} />
                    </label>
                  </div>
                </Card>
              ))}
              {f.config.members.length === 0 && (
                <div className="text-xs text-muted">Add at least two members — the router picks one per request.</div>
              )}
            </div>

            <div className={`grid gap-3 ${isHeuristic ? "" : "grid-cols-1 sm:grid-cols-2"}`}>
              <Field label="Fallback model" hint={isHeuristic ? "used when no rule matches (else a catch-all member is required)" : undefined}>
                <Select value={f.config.fallback_model}
                  options={[{ value: "", label: "none" }, ...engines.map((m) => ({ value: m.api_name, label: m.api_name }))]}
                  onChange={(v) => setC({ fallback_model: v })} />
              </Field>
              {!isHeuristic && (
                <Field label="Max chars classified">
                  <input className="input" type="number" min={100} max={8000} value={f.config.max_user_chars}
                    onChange={(e) => setC({ max_user_chars: Number(e.target.value) || 2000 })} />
                </Field>
              )}
            </div>

            {!isHeuristic && (
              <Field label="Prompt template (optional — {{models}} and {{request}} are substituted)">
                <textarea className="input min-h-24 font-mono text-xs" value={f.config.system_prompt}
                  placeholder="You are a request router…\nAvailable models:\n{{models}}"
                  onChange={(e) => setC({ system_prompt: e.target.value })} />
              </Field>
            )}

            <div className="flex items-center justify-between pt-1">
              <Toggle checked={f.enabled} onChange={(v) => setF({ ...f, enabled: v })} label="Enabled" />
              <button className="btn-primary" disabled={!f.api_name || (!isHeuristic && !f.config.router_model) || f.config.members.length === 0}>
                {editId ? "Save" : "Create router"}
              </button>
            </div>
            {saveErr && <div className="text-xs text-err">{saveErr}</div>}
          </form>
        </Modal>
      )}
      <ConfirmModal
        open={pendingDel !== null}
        title="Delete router"
        danger
        confirmLabel="Delete"
        body={pendingDel ? `Delete router "${pendingDel.name}"?` : ""}
        onConfirm={del}
        onClose={() => setPendingDel(null)}
      />
    </div>
  );
}
