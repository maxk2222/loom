import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Copy, KeyRound, Plus, RefreshCw, Trash2 } from "lucide-react";
import { useEffect, useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { Badge, Card, Field, Modal, Spinner, Toggle } from "../components/ui";
import type { Project, VirtualModel } from "../lib/types";

interface ApiKey {
  id: string;
  project_id: string;
  name: string;
  prefix: string;
  rpm_limit: number;
  tpm_limit: number;
  max_concurrent: number;
  active: boolean;
  scopes?: string[];
  allowed_models?: string[];
}

const slugify = (s: string) =>
  s.toLowerCase().trim().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "") || "user";

function ModelChips({ projectId, vms }: { projectId: string; vms: VirtualModel[] }) {
  const q = useQuery({
    queryKey: ["pmodels", projectId],
    queryFn: () => api.get<{ virtual_model_ids: string[] }>(`/api/projects/${projectId}/models`),
  });
  const ids = new Set(q.data?.virtual_model_ids ?? []);
  const names = vms.filter((m) => ids.has(m.id)).map((m) => m.api_name);
  if (!q.data) return <span className="text-xs text-muted">…</span>;
  if (names.length === 0) return <span className="text-xs text-muted">all models</span>;
  return (
    <div className="flex max-w-xs flex-wrap gap-1">
      {names.map((n) => (
        <span key={n} className="badge bg-acc/10 text-acc">{n}</span>
      ))}
    </div>
  );
}

export default function ApiUsersPage() {
  const qc = useQueryClient();
  const projects = useQuery({ queryKey: ["projects"], queryFn: () => api.get<{ projects: Project[] }>("/api/projects") });
  const keys = useQuery({ queryKey: ["keys"], queryFn: () => api.get<{ keys: ApiKey[] }>("/api/keys") });
  const vms = useQuery({ queryKey: ["vms"], queryFn: () => api.get<{ models: VirtualModel[] }>("/api/models") });

  const [createOpen, setCreateOpen] = useState(false);
  const [edit, setEdit] = useState<Project | null>(null);
  const [freshKey, setFreshKey] = useState<{ name: string; token: string } | null>(null);
  const [err, setErr] = useState("");

  const [f, setF] = useState({ name: "", rpm_limit: 60, tpm_limit: 100000, max_concurrent: 4 });
  const [selModels, setSelModels] = useState<Set<string>>(new Set());
  const [editModels, setEditModels] = useState<Set<string>>(new Set());
  const [editLimits, setEditLimits] = useState({ rpm_limit: 60, tpm_limit: 100000, max_concurrent: 4 });
  const [editScopes, setEditScopes] = useState<Set<string>>(new Set());
  const [editName, setEditName] = useState("");

  const SCOPES = [
    "chat", "completions", "embeddings", "models",
    "images", "audio_speech", "audio_transcription", "audio_translation", "video",
  ];

  const refresh = () => {
    qc.invalidateQueries({ queryKey: ["projects"] });
    qc.invalidateQueries({ queryKey: ["keys"] });
    qc.invalidateQueries({ queryKey: ["pmodels"] });
  };

  const keysFor = (pid: string) => (keys.data?.keys ?? []).filter((k) => k.project_id === pid);
  const primaryKey = (pid: string) => keysFor(pid).find((k) => k.active);

  // --- create flow: project → model grants → auto key → show once ---
  const create = async (e: FormEvent) => {
    e.preventDefault();
    setErr("");
    try {
      await api.post("/api/projects", { slug: slugify(f.name), name: f.name, priority_class: 1, weight: 1 });
      const plist = await api.get<{ projects: Project[] }>("/api/projects");
      const p = plist.projects.find((x) => x.slug === slugify(f.name));
      if (!p) throw new Error("project not created");
      if (selModels.size > 0) {
        await api.put(`/api/projects/${p.id}/models`, { virtual_model_ids: [...selModels] });
      }
      const k = await api.post<{ token: string }>("/api/keys", {
        project_id: p.id,
        name: "primary",
        rpm_limit: f.rpm_limit,
        tpm_limit: f.tpm_limit,
        max_concurrent: f.max_concurrent,
      });
      setFreshKey({ name: f.name, token: k.token });
      setCreateOpen(false);
      setF({ name: "", rpm_limit: 60, tpm_limit: 100000, max_concurrent: 4 });
      setSelModels(new Set());
      refresh();
    } catch (ex) {
      setErr(ex instanceof Error ? ex.message : "failed");
    }
  };

  // --- edit flow ---
  const openEdit = async (p: Project) => {
    setEdit(p);
    setEditName(p.name);
    const pk = primaryKey(p.id);
    setEditLimits({
      rpm_limit: pk?.rpm_limit ?? 60,
      tpm_limit: pk?.tpm_limit ?? 100000,
      max_concurrent: pk?.max_concurrent ?? 4,
    });
    setEditScopes(new Set(pk?.scopes ?? ["chat", "completions", "embeddings", "models"]));
    try {
      const d = await api.get<{ virtual_model_ids: string[] }>(`/api/projects/${p.id}/models`);
      setEditModels(new Set(d.virtual_model_ids));
    } catch {
      setEditModels(new Set());
    }
  };

  const saveEdit = async (e: FormEvent) => {
    e.preventDefault();
    if (!edit) return;
    setErr("");
    try {
      await api.put(`/api/projects/${edit.id}`, { name: editName });
      await api.put(`/api/projects/${edit.id}/models`, { virtual_model_ids: [...editModels] });
      const pk = primaryKey(edit.id);
      if (pk) {
        await api.put(`/api/keys/${pk.id}`, { name: pk.name, ...editLimits, scopes: [...editScopes] });
      }
      setEdit(null);
      refresh();
    } catch (ex) {
      setErr(ex instanceof Error ? ex.message : "failed");
    }
  };

  const regenerate = async (p: Project) => {
    const pk = primaryKey(p.id);
    if (!pk) return;
    const k = await api.post<{ token: string }>("/api/keys", {
      project_id: p.id,
      name: pk.name || "primary",
      rpm_limit: pk.rpm_limit,
      tpm_limit: pk.tpm_limit,
      max_concurrent: pk.max_concurrent,
    });
    await api.post(`/api/keys/${pk.id}/revoke`);
    setFreshKey({ name: p.name, token: k.token });
    refresh();
  };

  const del = async (p: Project) => {
    if (!confirm(`Delete API user "${p.name}" and its keys?`)) return;
    await api.del(`/api/projects/${p.id}`);
    refresh();
  };

  const toggle = async (p: Project) => {
    await api.put(`/api/projects/${p.id}`, { enabled: !p.enabled });
    refresh();
  };

  const vmList = vms.data?.models ?? [];

  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-lg font-semibold">API Users</h1>
          <p className="text-xs text-muted">Each user gets one API key plus model grants and rate limits.</p>
        </div>
        <button className="btn-primary" onClick={() => setCreateOpen(true)}><Plus size={16} /> New API user</button>
      </div>

      {projects.isLoading ? <Spinner /> : (
        <Card className="overflow-hidden p-0">
          <table className="w-full">
            <thead>
              <tr>
                <th className="th">User</th>
                <th className="th">API key</th>
                <th className="th">Model grants</th>
                <th className="th">Limits (rpm / tpm / conc.)</th>
                <th className="th">Status</th>
                <th className="th" />
              </tr>
            </thead>
            <tbody>
              {(projects.data?.projects ?? []).map((p) => {
                const pk = primaryKey(p.id);
                return (
                  <tr key={p.id}>
                    <td className="td">
                      <div className="font-medium">{p.name}</div>
                      <div className="text-xs text-muted">{p.slug}</div>
                    </td>
                    <td className="td font-mono text-xs">
                      {pk ? <span>{pk.prefix}…</span> : <span className="text-muted">revoked</span>}
                    </td>
                    <td className="td"><ModelChips projectId={p.id} vms={vmList} /></td>
                    <td className="td text-xs text-muted">
                      {pk ? `${pk.rpm_limit} / ${pk.tpm_limit.toLocaleString()} / ${pk.max_concurrent}` : "—"}
                    </td>
                    <td className="td">
                      <Badge tone={p.enabled ? "ok" : "warn"}>{p.enabled ? "enabled" : "disabled"}</Badge>
                    </td>
                    <td className="td">
                      <div className="flex items-center justify-end gap-1">
                        <button className="btn-ghost px-2 py-1 text-xs" onClick={() => openEdit(p)}>edit</button>
                        <button className="btn-ghost p-1" title="regenerate key" onClick={() => regenerate(p)}>
                          <RefreshCw size={14} />
                        </button>
                        <button className="btn-ghost p-1" title={p.enabled ? "disable" : "enable"} onClick={() => toggle(p)}>
                          <KeyRound size={14} />
                        </button>
                        <button className="btn-ghost p-1 text-red-400" title="delete" onClick={() => del(p)}>
                          <Trash2 size={14} />
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </Card>
      )}

      {/* create modal */}
      <Modal open={createOpen} title="New API user" onClose={() => setCreateOpen(false)}>
        <form className="space-y-4" onSubmit={create}>
          <Field label="Name">
            <input className="input w-full" value={f.name} onChange={(e) => setF({ ...f, name: e.target.value })} required placeholder="e.g. research-bot" />
          </Field>
          <Field label="Model grants (empty = all)">
            <div className="card max-h-44 space-y-1 overflow-y-auto p-3">
              {vmList.map((m) => (
                <Toggle key={m.id} checked={selModels.has(m.id)} label={m.api_name}
                  onChange={(v) => { const s = new Set(selModels); v ? s.add(m.id) : s.delete(m.id); setSelModels(s); }} />
              ))}
            </div>
          </Field>
          <div className="grid grid-cols-3 gap-3">
            <Field label="rpm"><input className="input w-full" type="number" min={1} value={f.rpm_limit} onChange={(e) => setF({ ...f, rpm_limit: +e.target.value })} /></Field>
            <Field label="tpm"><input className="input w-full" type="number" min={1} value={f.tpm_limit} onChange={(e) => setF({ ...f, tpm_limit: +e.target.value })} /></Field>
            <Field label="concurrent"><input className="input w-full" type="number" min={1} value={f.max_concurrent} onChange={(e) => setF({ ...f, max_concurrent: +e.target.value })} /></Field>
          </div>
          {err && <p className="text-xs text-red-400">{err}</p>}
          <button className="btn-primary w-full justify-center">Create user + key</button>
        </form>
      </Modal>

      {/* edit modal */}
      <Modal open={!!edit} title={edit ? `Edit ${edit.name}` : ""} onClose={() => setEdit(null)}>
        <form className="space-y-4" onSubmit={saveEdit}>
          <Field label="Name">
            <input className="input w-full" value={editName} onChange={(e) => setEditName(e.target.value)} required />
          </Field>
          <Field label="Model grants (empty = all)">
            <div className="card max-h-44 space-y-1 overflow-y-auto p-3">
              {vmList.map((m) => (
                <Toggle key={m.id} checked={editModels.has(m.id)} label={m.api_name}
                  onChange={(v) => { const s = new Set(editModels); v ? s.add(m.id) : s.delete(m.id); setEditModels(s); }} />
              ))}
            </div>
          </Field>
          <div className="grid grid-cols-3 gap-3">
            <Field label="rpm"><input className="input w-full" type="number" min={1} value={editLimits.rpm_limit} onChange={(e) => setEditLimits({ ...editLimits, rpm_limit: +e.target.value })} /></Field>
            <Field label="tpm"><input className="input w-full" type="number" min={1} value={editLimits.tpm_limit} onChange={(e) => setEditLimits({ ...editLimits, tpm_limit: +e.target.value })} /></Field>
            <Field label="concurrent"><input className="input w-full" type="number" min={1} value={editLimits.max_concurrent} onChange={(e) => setEditLimits({ ...editLimits, max_concurrent: +e.target.value })} /></Field>
          </div>
          <Field label="Scopes">
            <div className="flex flex-wrap gap-3">
              {SCOPES.map((sc) => (
                <Toggle key={sc} checked={editScopes.has(sc)} label={sc}
                  onChange={(v) => { const s = new Set(editScopes); v ? s.add(sc) : s.delete(sc); setEditScopes(s); }} />
              ))}
            </div>
          </Field>
          {err && <p className="text-xs text-red-400">{err}</p>}
          <button className="btn-primary w-full justify-center">Save</button>
        </form>
      </Modal>

      {/* one-time key reveal */}
      <Modal open={!!freshKey} title="API key created" onClose={() => setFreshKey(null)}>
        <div className="space-y-4">
          <p className="text-sm text-muted">
            Key for <span className="font-medium text-text">{freshKey?.name}</span> — shown once, store it now:
          </p>
          <div className="card flex items-center gap-2 p-3">
            <code className="flex-1 break-all font-mono text-xs text-acc">{freshKey?.token}</code>
            <button className="btn-ghost p-1.5" onClick={() => navigator.clipboard.writeText(freshKey?.token ?? "")}>
              <Copy size={14} />
            </button>
          </div>
          <button className="btn-primary w-full justify-center" onClick={() => setFreshKey(null)}>Done</button>
        </div>
      </Modal>
    </div>
  );
}
