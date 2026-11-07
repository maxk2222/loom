import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Megaphone, Pin, Plus, Trash2 } from "lucide-react";
import { useState, type FormEvent } from "react";
import { api } from "../lib/api";
import { Badge, Card, Field, Modal, Select, Spinner, Toggle } from "../components/ui";
import type { ClientRow } from "../lib/types";

interface AdminNote {
  id: string;
  client_id: string | null;
  client_name: string | null;
  title: string;
  body: string;
  severity: string;
  persistent: boolean;
  expires_at: string | null;
  created_at: string;
}

const SEV_TONE: Record<string, "muted" | "warn" | "err"> = { info: "muted", warning: "warn", critical: "err" };

export default function NotesPage() {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["notes"], queryFn: () => api.get<{ notes: AdminNote[] }>("/api/notes"), refetchInterval: 10000 });
  const clients = useQuery({ queryKey: ["clients"], queryFn: () => api.get<{ clients: ClientRow[] }>("/api/clients") });
  const [open, setOpen] = useState(false);
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [severity, setSeverity] = useState("info");
  const [target, setTarget] = useState("");
  const [persistent, setPersistent] = useState(false);
  const [expiresHours, setExpiresHours] = useState(0);
  const [busy, setBusy] = useState(false);

  const create = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    try {
      await api.post("/api/notes", {
        title, body, severity, persistent,
        client_id: target,
        expires_hours: expiresHours,
      });
      setOpen(false);
      setTitle(""); setBody(""); setSeverity("info"); setTarget(""); setPersistent(false); setExpiresHours(0);
      qc.invalidateQueries({ queryKey: ["notes"] });
    } finally {
      setBusy(false);
    }
  };

  const del = async (id: string) => {
    await api.del(`/api/notes/${id}`);
    qc.invalidateQueries({ queryKey: ["notes"] });
  };

  if (q.isLoading) return <Spinner />;
  const notes = q.data?.notes ?? [];
  const targetOpts = [
    { value: "", label: "All clients (broadcast)" },
    ...(clients.data?.clients ?? []).map(c => ({ value: c.id, label: c.name })),
  ];

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-lg font-semibold">Notices</h1>
          <p className="mt-0.5 text-sm text-muted">Notes and briefings pushed to client dashboards. Persistent notes stay pinned and cannot be dismissed.</p>
        </div>
        <button className="btn-primary flex items-center gap-1.5" onClick={() => setOpen(true)}><Plus size={15} /> New notice</button>
      </div>

      <div className="space-y-3">
        {notes.length === 0 && (
          <Card className="py-10 text-center text-sm text-muted">
            <Megaphone className="mx-auto mb-2 h-6 w-6 opacity-40" />
            No notices yet — connected clients see new ones within seconds.
          </Card>
        )}
        {notes.map(n => (
          <Card key={n.id} className={n.severity === "critical" ? "border-err/40" : n.severity === "warning" ? "border-warn/40" : ""}>
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-sm font-medium">{n.title}</span>
                  <Badge tone={SEV_TONE[n.severity] ?? "muted"}>{n.severity}</Badge>
                  {n.persistent && <Badge tone="muted"><Pin className="mr-0.5 inline h-3 w-3" />pinned</Badge>}
                  <Badge tone="muted">{n.client_name ?? "all clients"}</Badge>
                </div>
                {n.body && <div className="mt-1.5 whitespace-pre-wrap text-xs text-muted">{n.body}</div>}
                <div className="mt-1.5 text-[10px] text-muted/60">
                  {new Date(n.created_at).toLocaleString()}
                  {n.expires_at ? ` · expires ${new Date(n.expires_at).toLocaleString()}` : " · no expiry"}
                </div>
              </div>
              <button className="shrink-0 rounded p-1.5 text-muted hover:bg-err/10 hover:text-err" onClick={() => del(n.id)} title="delete (removes from all clients)">
                <Trash2 size={15} />
              </button>
            </div>
          </Card>
        ))}
      </div>

      <Modal open={open} onClose={() => setOpen(false)} title="New notice">
        <form onSubmit={create} className="space-y-4">
          <Field label="Title">
            <input className="input" value={title} onChange={(e) => setTitle(e.target.value)} required maxLength={200} placeholder="Maintenance briefing Friday" />
          </Field>
          <Field label="Body" hint="shown under the title on client dashboards">
            <textarea className="input min-h-[90px]" value={body} onChange={(e) => setBody(e.target.value)} placeholder="Details, instructions, links…" />
          </Field>
          <div className="grid grid-cols-2 gap-4">
            <Field label="Severity">
              <Select value={severity} onChange={setSeverity} options={[
                { value: "info", label: "info" },
                { value: "warning", label: "warning" },
                { value: "critical", label: "critical" },
              ]} />
            </Field>
            <Field label="Target">
              <Select value={target} onChange={setTarget} options={targetOpts} />
            </Field>
          </div>
          <div className="grid grid-cols-2 gap-4">
            <Field label="Expires after (hours)" hint="0 = never">
              <input className="input" type="number" min={0} value={expiresHours} onChange={(e) => setExpiresHours(+e.target.value)} />
            </Field>
            <Field label="Persistent briefing" hint="pinned, cannot be dismissed">
              <div className="pt-1.5"><Toggle checked={persistent} onChange={setPersistent} label={persistent ? "pinned" : "dismissible"} /></div>
            </Field>
          </div>
          <div className="flex justify-end gap-2">
            <button type="button" className="btn-ghost" onClick={() => setOpen(false)}>Cancel</button>
            <button className="btn-primary" disabled={busy || !title.trim()}>{busy ? "Sending…" : "Send notice"}</button>
          </div>
        </form>
      </Modal>
    </div>
  );
}
