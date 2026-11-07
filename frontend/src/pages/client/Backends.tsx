import { useQuery, useQueryClient } from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";
import { api } from "../../lib/api";
import { Badge, Card, Spinner } from "../../components/ui";
import type { LocalDeployment } from "../../lib/types";

export default function BackendsPage() {
  const qc = useQueryClient();
  const q = useQuery({
    queryKey: ["client-backends"],
    queryFn: () => api.get<{ backends: { backend: { id: string; url: string; kind: string }; deployments: LocalDeployment[] }[] }>("/local/api/backends"),
    refetchInterval: 5000,
  });

  const probe = async () => {
    await api.post("/local/api/probe");
    qc.invalidateQueries({ queryKey: ["client-backends"] });
  };

  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between">
        <h1 className="text-lg font-semibold">Backends & models</h1>
        <button className="btn-outline" onClick={probe}><RefreshCw size={15} /> Re-probe capabilities</button>
      </div>
      {q.isLoading ? <Spinner /> : (q.data?.backends ?? []).length === 0 ? (
        <Card className="text-center text-sm text-muted">No backends configured — add them to the client config file.</Card>
      ) : (
        <div className="space-y-4">
          {(q.data?.backends ?? []).map((b) => (
            <Card key={b.backend.id} className="space-y-3">
              <div className="flex items-center gap-3">
                <Badge tone="accent">{b.backend.kind}</Badge>
                <span className="font-mono text-xs text-muted">{b.backend.url}</span>
              </div>
              <table className="w-full">
                <thead><tr><th className="th">Deployment</th><th className="th">Model</th><th className="th">Health</th><th className="th">Capabilities</th><th className="th">Limits</th><th className="th">Enabled</th></tr></thead>
                <tbody>
                  {b.deployments.map((d) => (
                    <tr key={d.deployment.id}>
                      <td className="td">{d.deployment.name}</td>
                      <td className="td text-muted">{d.deployment.model_name}</td>
                      <td className="td">{d.health === "ok" ? <Badge tone="ok">ok</Badge> : d.health ? <Badge tone="err">{d.health}</Badge> : <Badge tone="muted">unknown</Badge>}</td>
                      <td className="td text-xs text-muted">
                        {d.capabilities
                          ? Object.entries(d.capabilities).filter(([k, v]) => v === true && typeof v === "boolean").map(([k]) => k).join(", ") || "—"
                          : "—"}
                      </td>
                      <td className="td text-muted text-xs">max {d.deployment.max_concurrent} · res {d.deployment.local_reserve} · t2 {d.deployment.tier2_max}</td>
                      <td className="td">{d.deployment.enabled ? <Badge tone="ok">on</Badge> : <Badge tone="err">off</Badge>}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </Card>
          ))}
        </div>
      )}
    </div>
  );
}
