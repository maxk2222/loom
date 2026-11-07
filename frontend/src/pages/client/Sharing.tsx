import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { api } from "../../lib/api";
import { Badge, Card, Field, Select, Spinner, Toggle } from "../../components/ui";

interface SharingCfg {
  enabled: boolean;
  default_mode: string;
}

export default function SharingPage() {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["sharing"], queryFn: () => api.get<SharingCfg>("/local/api/sharing"), refetchInterval: 5000 });
  const [enabled, setEnabled] = useState(false);
  const [mode, setMode] = useState("auto");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (q.data) {
      setEnabled(q.data.enabled);
      setMode(q.data.default_mode || "auto");
    }
  }, [q.data]);

  const save = async () => {
    setBusy(true);
    try {
      await api.put("/local/api/sharing", { enabled, default_mode: mode });
      qc.invalidateQueries({ queryKey: ["sharing"] });
    } finally {
      setBusy(false);
    }
  };

  if (q.isLoading) return <Spinner />;

  return (
    <div className="max-w-xl space-y-6">
      <h1 className="text-lg font-semibold">Resource sharing</h1>
      <Card className="space-y-5">
        <div className="flex items-center justify-between">
          <div>
            <div className="text-sm font-medium">Share spare capacity with the central pool</div>
            <div className="mt-0.5 text-xs text-muted">Local requests always keep priority — tier-2 slots are reduced automatically under local pressure.</div>
          </div>
          <Toggle checked={enabled} onChange={setEnabled} />
        </div>
        <Field label="Default sharing mode" hint="auto: reduce to zero under local pressure · fixed: keep tier2_max constant">
          <Select value={mode} onChange={setMode} options={[
            { value: "auto", label: "auto (pressure-aware)" },
            { value: "fixed", label: "fixed" },
            { value: "manual", label: "manual" },
          ]} />
        </Field>
        <button className="btn-primary" onClick={save} disabled={busy}>{busy ? "Saving…" : "Apply"}</button>
      </Card>
      <Card>
        <h2 className="mb-2 text-sm font-medium">How limits work</h2>
        <ul className="list-disc space-y-1 pl-5 text-sm text-muted">
          <li><code className="text-text">max_concurrent</code> — total backend parallelism per deployment.</li>
          <li><code className="text-text">local_reserve</code> — slots always kept for local callers.</li>
          <li><code className="text-text">tier2_max</code> — upper bound of slots offered to the central pool.</li>
          <li>When local load rises, the offered tier-2 capacity shrinks toward zero.</li>
        </ul>
      </Card>
    </div>
  );
}
