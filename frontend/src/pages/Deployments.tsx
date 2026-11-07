import { useQuery } from "@tanstack/react-query";
import { api } from "../lib/api";
import { Badge, Card, Spinner } from "../components/ui";
import type { DeploymentRow } from "../lib/types";

export default function DeploymentsPage() {
  const q = useQuery({
    queryKey: ["deployments"],
    queryFn: () => api.get<{ deployments: DeploymentRow[] }>("/api/deployments"),
    refetchInterval: 5000,
  });

  return (
    <div className="space-y-5">
      <h1 className="text-lg font-semibold">Deployments</h1>
      {q.isLoading ? <Spinner /> : (
        <Card className="overflow-hidden p-0">
          <table className="w-full">
            <thead><tr>
              <th className="th">Name</th><th className="th">Backend</th><th className="th">Model</th>
              <th className="th">Client</th><th className="th">Runtime</th><th className="th">Tier-2</th><th className="th">Enabled</th>
            </tr></thead>
            <tbody>
              {(q.data?.deployments ?? []).map((d) => (
                <tr key={d.id}>
                  <td className="td font-medium">{d.name}</td>
                  <td className="td text-muted">{d.backend_type}</td>
                  <td className="td">{d.model_name}</td>
                  <td className="td text-muted">{d.client_name ?? "—"}</td>
                  <td className="td">
                    {d.runtime ? (
                      <Badge tone={d.runtime.state === "available" ? "ok" : "warn"}>{d.runtime.state}</Badge>
                    ) : <Badge tone="muted">no report</Badge>}
                  </td>
                  <td className="td text-muted">
                    {d.runtime ? `${d.runtime.tier2_free}/${d.runtime.tier2_total}` : "—"}
                  </td>
                  <td className="td">{d.enabled ? <Badge tone="ok">enabled</Badge> : <Badge tone="err">disabled</Badge>}</td>
                </tr>
              ))}
              {(q.data?.deployments ?? []).length === 0 && (
                <tr><td className="td text-center text-muted" colSpan={7}>No deployments reported yet.</td></tr>
              )}
            </tbody>
          </table>
        </Card>
      )}
    </div>
  );
}
