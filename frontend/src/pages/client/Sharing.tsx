import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { api } from "../../lib/api";
import { Badge, Card, Field, Spinner, Toggle } from "../../components/ui";

interface PressureCfg {
  enabled: boolean;
  cpu_pct: number;
  ram_pct: number;
  unload_models: boolean;
  cooldown_secs: number;
}

interface SharingCfg {
  sharing_enabled: boolean;
  pressure: PressureCfg;
}

export default function SharingPage() {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["sharing"], queryFn: () => api.get<SharingCfg>("/local/api/sharing"), refetchInterval: 5000 });
  const [enabled, setEnabled] = useState(false);
  const [p, setP] = useState<PressureCfg>({ enabled: true, cpu_pct: 0, ram_pct: 0, unload_models: false, cooldown_secs: 30 });
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (q.data) {
      setEnabled(q.data.sharing_enabled);
      if (q.data.pressure) setP(q.data.pressure);
    }
  }, [q.data]);

  const save = async () => {
    setBusy(true);
    try {
      await api.put("/local/api/sharing", { sharing_enabled: enabled, pressure: p });
      qc.invalidateQueries({ queryKey: ["sharing"] });
    } finally {
      setBusy(false);
    }
  };

  if (q.isLoading) return <Spinner />;
  const limitsOn = p.enabled && (p.cpu_pct > 0 || p.ram_pct > 0);

  return (
    <div className="max-w-xl space-y-6">
      <h1 className="text-lg font-semibold">Resource sharing</h1>
      <Card className="space-y-5">
        <div className="flex items-center justify-between">
          <div>
            <div className="text-sm font-medium">Share spare capacity with the central pool</div>
            <div className="mt-0.5 text-xs text-muted">Local requests always keep priority — tier-2 slots are reserved for them first.</div>
          </div>
          <Toggle checked={enabled} onChange={setEnabled} />
        </div>
      </Card>
      <Card className="space-y-5">
        <div className="flex items-center justify-between">
          <div>
            <div className="text-sm font-medium">Pressure gate</div>
            <div className="mt-0.5 text-xs text-muted">
              Stop accepting central requests when this machine is busy. Gates reopen once load stays below the limits for the cooldown window.
              {!limitsOn && p.enabled && <span className="text-warn"> Set at least one limit above 0 to activate.</span>}
            </div>
          </div>
          <Toggle checked={p.enabled} onChange={(v) => setP({ ...p, enabled: v })} />
        </div>
        <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
          <Field label="CPU limit %" hint="0 = ignore cpu">
            <input className="input" type="number" min={0} max={100} value={p.cpu_pct} onChange={(e) => setP({ ...p, cpu_pct: +e.target.value })} />
          </Field>
          <Field label="RAM limit %" hint="0 = ignore ram">
            <input className="input" type="number" min={0} max={100} value={p.ram_pct} onChange={(e) => setP({ ...p, ram_pct: +e.target.value })} />
          </Field>
        </div>
        <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
          <Field label="Cooldown (seconds)" hint="hysteresis against flapping">
            <input className="input" type="number" min={0} value={p.cooldown_secs} onChange={(e) => setP({ ...p, cooldown_secs: +e.target.value })} />
          </Field>
          <Field label="Also unload models" hint="free memory on idle deployments">
            <div className="pt-1.5"><Toggle checked={p.unload_models} onChange={(v) => setP({ ...p, unload_models: v })} label={p.unload_models ? "on" : "off"} /></div>
          </Field>
        </div>
        {limitsOn && <div className="text-xs text-muted">Active: tier-2 pauses above <span className="text-text">{p.cpu_pct > 0 ? `cpu ${p.cpu_pct}%` : ""}{p.cpu_pct > 0 && p.ram_pct > 0 ? " or " : ""}{p.ram_pct > 0 ? `ram ${p.ram_pct}%` : ""}</span>.</div>}
        <button className="btn-primary" onClick={save} disabled={busy}>{busy ? "Saving…" : "Apply"}</button>
      </Card>
      <Card>
        <h2 className="mb-2 text-sm font-medium">How limits work</h2>
        <ul className="list-disc space-y-1 pl-5 text-sm text-muted">
          <li><code className="text-text">max_concurrent</code> — total backend parallelism per deployment.</li>
          <li><code className="text-text">local_reserve</code> — slots always kept for local callers.</li>
          <li><code className="text-text">tier2_max</code> — upper bound of slots offered to the central pool.</li>
          <li><Badge tone="err">pressure</Badge> — tier-2 admissions stop entirely while cpu/ram exceed the limits; running requests are never interrupted.</li>
          <li>Unload frees model memory only when a deployment has zero in-flight requests; <code className="text-text">auto-load</code> models re-warm automatically afterwards.</li>
        </ul>
      </Card>
    </div>
  );
}
