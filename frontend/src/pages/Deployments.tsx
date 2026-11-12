import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../lib/api";
import { Badge, Card, ConfirmModal, Field, Modal, Spinner } from "../components/ui";
import type { DeploymentRow } from "../lib/types";

function fmtTokens(n?: number | null): string {
  if (n === null || n === undefined || n <= 0) return "—";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

export default function DeploymentsPage() {
  const qc = useQueryClient();
  const [pending, setPending] = useState<DeploymentRow | null>(null);
  const [editing, setEditing] = useState<DeploymentRow | null>(null);
  const [budgetInput, setBudgetInput] = useState("");
  const [busy, setBusy] = useState(false);
  const q = useQuery({
    queryKey: ["deployments"],
    queryFn: () => api.get<{ deployments: DeploymentRow[] }>("/api/deployments"),
    refetchInterval: 5000,
  });

  const openEdit = (d: DeploymentRow) => {
    setEditing(d);
    setBudgetInput(d.kv_budget_tokens ? String(d.kv_budget_tokens) : "");
  };

  const saveBudget = async () => {
    if (!editing) return;
    setBusy(true);
    try {
      const n = Number(budgetInput);
      await api.put(`/api/deployments/${editing.id}`, {
        kv_budget_tokens: Number.isFinite(n) && n > 0 ? Math.floor(n) : null,
      });
      qc.invalidateQueries({ queryKey: ["deployments"] });
      setEditing(null);
    } finally {
      setBusy(false);
    }
  };

  const del = async () => {
    if (!pending) return;
    setBusy(true);
    try {
      await api.del(`/api/deployments/${pending.id}?hard=true`);
      qc.invalidateQueries({ queryKey: ["deployments"] });
      setPending(null);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-5">
      <h1 className="text-lg font-semibold">Deployments</h1>
      {q.isLoading ? <Spinner /> : (
        <Card className="overflow-x-auto p-0">
          <table className="w-full">
            <thead><tr>
              <th className="th">Name</th><th className="th">Backend</th><th className="th">Model</th>
              <th className="th">Client</th><th className="th">APIs</th>
              <th className="th">Runtime</th><th className="th">Tier-2</th>
              <th className="th">KV budget</th><th className="th">Enabled</th>
              <th className="th"></th>
            </tr></thead>
            <tbody>
              {(q.data?.deployments ?? []).map((d) => {
                const caps = (d.capabilities ?? {}) as Record<string, boolean>;
                const media = [
                  caps.images && "images", caps.audio_speech && "speech",
                  caps.audio_transcription && "stt", caps.audio_translation && "translate",
                  caps.video && "video",
                ].filter(Boolean) as string[];
                return (
                <tr key={d.id}>
                  <td className="td font-medium">{d.name}</td>
                  <td className="td text-muted">{d.backend_type}</td>
                  <td className="td">{d.model_name}</td>
                  <td className="td text-muted">{d.client_name ?? "—"}</td>
                  <td className="td">
                    <div className="flex flex-wrap gap-1">
                      {(caps.chat || caps.completions || caps.embeddings) && <Badge tone="muted">text</Badge>}
                      {media.map((m) => <Badge key={m} tone="accent">{m}</Badge>)}
                      {media.length === 0 && !caps.chat && !caps.completions && !caps.embeddings && <span className="text-muted">—</span>}
                    </div>
                  </td>
                  <td className="td">
                    {d.runtime ? (
                      <span className="inline-flex items-center gap-1.5">
                        <Badge tone={d.runtime.state === "available" ? "ok" : "warn"}>{d.runtime.state}</Badge>
                        {d.runtime.pressure_gated && <Badge tone="err">pressure</Badge>}
                      </span>
                    ) : <Badge tone="muted">no report</Badge>}
                  </td>
                  <td className="td text-muted">
                    {d.runtime ? `${d.runtime.tier2_free}/${d.runtime.tier2_total}` : "—"}
                  </td>
                  <td className="td">
                    <button
                      className="text-left hover:text-accent"
                      title={d.kv_budget_tokens
                        ? `Admin override — click to edit (adapter reported ${fmtTokens(d.reported_kv_budget_tokens)})`
                        : d.reported_kv_budget_tokens
                          ? "Adapter-reported — click to override"
                          : "Not measured — click to set a budget"}
                      onClick={() => openEdit(d)}
                    >
                      {d.kv_budget_effective ? (
                        <span className="font-mono text-xs">
                          {fmtTokens(d.runtime?.kv_used)}/{fmtTokens(d.kv_budget_effective)}
                          {d.kv_budget_tokens ? <span className="text-accent">*</span> : null}
                        </span>
                      ) : <span className="text-muted">—</span>}
                    </button>
                  </td>
                  <td className="td">{d.enabled ? <Badge tone="ok">enabled</Badge> : <Badge tone="err">disabled</Badge>}</td>
                  <td className="td text-right">
                    <button className="btn-danger px-2 py-0.5 text-xs" onClick={() => setPending(d)}>Delete</button>
                  </td>
                </tr>
              );})}
              {(q.data?.deployments ?? []).length === 0 && (
                <tr><td className="td text-center text-muted" colSpan={10}>No deployments reported yet.</td></tr>
              )}
            </tbody>
          </table>
        </Card>
      )}
      <Modal open={editing !== null} title={`KV token budget — ${editing?.name ?? ""}`} onClose={() => setEditing(null)}>
        <div className="space-y-4">
          <p className="text-sm text-muted">
            Maximum KV tokens tier-2 requests may reserve concurrently on this deployment.
            Concurrency scales with the budget and live request footprints — no
            backend-side context compression.
          </p>
          <Field
            label="Budget (tokens)"
            hint={`Leave empty to ${editing?.reported_kv_budget_tokens
              ? `use the adapter-reported ${fmtTokens(editing.reported_kv_budget_tokens)}`
              : "disable token admission (slot-only)"}`}
          >
            <input
              className="input font-mono"
              type="number"
              min={0}
              value={budgetInput}
              onChange={(e) => setBudgetInput(e.target.value)}
              placeholder={editing?.reported_kv_budget_tokens ? String(editing.reported_kv_budget_tokens) : "e.g. 1250000"}
              autoFocus
            />
          </Field>
          <div className="flex justify-end gap-2">
            <button className="btn" onClick={() => setEditing(null)}>Cancel</button>
            <button className="btn-primary" onClick={saveBudget} disabled={busy}>Save</button>
          </div>
        </div>
      </Modal>
      <ConfirmModal
        open={pending !== null}
        title="Delete deployment"
        danger
        busy={busy}
        confirmLabel="Delete"
        body={pending ? `"${pending.name}" (${pending.model_name}) — the deployment is removed on the client too and cannot be undone.` : ""}
        onConfirm={del}
        onClose={() => setPending(null)}
      />
    </div>
  );
}
