import { useQuery, useQueryClient } from "@tanstack/react-query";
import { XCircle } from "lucide-react";
import { api } from "../../lib/api";
import { Badge, Card, Spinner } from "../../components/ui";
import type { LocalRequest } from "../../lib/types";
import { fmtDur } from "../../lib/format";

export default function RequestsPage() {
  const qc = useQueryClient();
  const q = useQuery({
    queryKey: ["client-requests"],
    queryFn: () => api.get<{ requests: LocalRequest[] }>("/local/api/requests"),
    refetchInterval: 2000,
  });
  const deps = useQuery({
    queryKey: ["client-deps"],
    queryFn: () => api.get<{ deployments: { deployment: { id: string; name: string; model_name: string } }[] }>("/local/api/deployments"),
    refetchInterval: 10000,
  });
  const depName = (id: string) => {
    const d = deps.data?.deployments.find((x) => x.deployment.id === id)?.deployment;
    return d ? (d.name || d.model_name) : id.slice(0, 8);
  };

  const cancel = async (id: string) => {
    await api.del(`/local/api/requests/${id}`);
    qc.invalidateQueries({ queryKey: ["client-requests"] });
  };

  if (q.isLoading) return <Spinner />;
  const rows = q.data?.requests ?? [];

  return (
    <div className="space-y-5">
      <h1 className="text-lg font-semibold">Active requests</h1>
      <Card className="overflow-x-auto p-0">
        <table className="w-full">
          <thead><tr><th className="th">Request</th><th className="th">Class</th><th className="th">API</th><th className="th">Deployment</th><th className="th">Running</th><th className="th" /></tr></thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.request_id}>
                <td className="td font-mono text-xs">{r.request_id.slice(0, 8)}…</td>
                <td className="td"><Badge tone={r.class === "local" ? "accent" : "warn"}>{r.class}</Badge></td>
                <td className="td text-muted">{r.api}</td>
                <td className="td text-muted">{depName(r.deployment_id)}</td>
                <td className="td text-muted">{fmtDur(Math.max(0, (Date.now() - r.started_unix_ms) / 1000))}</td>
                <td className="td text-right"><button className="btn-ghost p-1 text-err" onClick={() => cancel(r.request_id)}><XCircle size={15} /></button></td>
              </tr>
            ))}
            {rows.length === 0 && <tr><td className="td text-center text-muted" colSpan={6}>No active requests.</td></tr>}
          </tbody>
        </table>
      </Card>
    </div>
  );
}
