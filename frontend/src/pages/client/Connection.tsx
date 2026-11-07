import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { api } from "../../lib/api";
import { Badge, Card, Field, Spinner, Toggle } from "../../components/ui";

interface ConnCfg {
  server_url: string;
  enabled: boolean;
  connected: boolean;
  client_id: string;
  name: string;
}

export default function ConnectionPage() {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["conn"], queryFn: () => api.get<ConnCfg>("/local/api/connection"), refetchInterval: 3000 });
  const [enabled, setEnabled] = useState(true);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState("");

  useEffect(() => {
    if (q.data) {
      setEnabled(q.data.enabled);
      setName(q.data.name);
    }
  }, [q.data]);

  const save = async () => {
    setBusy(true);
    setSaved("");
    try {
      await api.put("/local/api/connection", { enabled, name });
      qc.invalidateQueries({ queryKey: ["conn"] });
      setSaved("applied — reconnecting with new identity");
    } finally {
      setBusy(false);
    }
  };

  if (q.isLoading) return <Spinner />;
  const d = q.data;

  return (
    <div className="max-w-xl space-y-6">
      <h1 className="text-lg font-semibold">Server connection</h1>
      <Card className="space-y-4">
        <div className="flex items-center justify-between text-sm">
          <span className="text-muted">State</span>
          {d?.connected ? <Badge tone="ok">connected</Badge> : d?.enabled ? <Badge tone="warn">reconnecting…</Badge> : <Badge tone="muted">disabled</Badge>}
        </div>
        <Field label="Workstation name (shown in the dashboard)">
          <input className="input w-full" value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. gpu-workstation-01" maxLength={128} />
        </Field>
        <div className="flex items-center justify-between text-sm">
          <span className="text-muted">Server URL</span>
          <span className="font-mono text-xs">{d?.server_url || "—"}</span>
        </div>
        <div className="flex items-center justify-between text-sm">
          <span className="text-muted">Client ID</span>
          <span className="font-mono text-xs">{d?.client_id || "not enrolled"}</span>
        </div>
        <div className="flex items-center justify-between border-t border-border pt-4">
          <div>
            <div className="text-sm font-medium">Connect to server</div>
            <div className="text-xs text-muted">Disconnect to keep this workstation fully local.</div>
          </div>
          <Toggle checked={enabled} onChange={setEnabled} />
        </div>
        <div className="flex items-center gap-3">
          <button className="btn-primary" onClick={save} disabled={busy || !name.trim()}>{busy ? "Applying…" : "Apply"}</button>
          {saved && <span className="text-xs text-muted">{saved}</span>}
        </div>
      </Card>
      <Card>
        <h2 className="mb-2 text-sm font-medium">Enrollment</h2>
        <p className="text-sm text-muted">
          Enroll this workstation once with an admin-issued token:
        </p>
        <pre className="mt-2 overflow-x-auto rounded-md bg-panel2 p-3 text-xs">loom enroll --server https://your-server:9443 --token enroll_…</pre>
      </Card>
    </div>
  );
}
