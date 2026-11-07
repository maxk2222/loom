import { useQuery } from "@tanstack/react-query";
import { api } from "../../lib/api";
import { Badge, Card, Stat } from "../../components/ui";
import type { ClientStatus } from "../../lib/types";

function fmtUptime(s: number): string {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m ${s % 60}s`;
}

export default function ClientStatusPage() {
  const q = useQuery({
    queryKey: ["client-status"],
    queryFn: () => api.get<ClientStatus>("/local/api/status"),
    refetchInterval: 3000,
  });
  const d = q.data;
  const totalFree = (d?.capacities ?? []).reduce((a, c) => a + c.tier2_free, 0);
  const totalCap = (d?.capacities ?? []).reduce((a, c) => a + c.tier2_total, 0);

  return (
    <div className="space-y-6">
      <h1 className="text-lg font-semibold">Status</h1>
      <div className="grid grid-cols-2 gap-4 md:grid-cols-4">
        <Stat label="Server connection" value={<Badge tone={d?.server_connected ? "ok" : "err"}>{d?.server_connected ? "connected" : "offline"}</Badge>} />
        <Stat label="Uptime" value={fmtUptime(d?.uptime_secs ?? 0)} />
        <Stat label="Sharing" value={<Badge tone={d?.sharing_enabled ? "ok" : "muted"}>{d?.sharing_enabled ? "enabled" : "off"}</Badge>} />
        <Stat label="Tier-2 slots free" value={`${totalFree}/${totalCap}`} />
      </div>
      <Card className="space-y-2 text-sm">
        <div className="flex justify-between"><span className="text-muted">Client name</span><span>{d?.name ?? "—"}</span></div>
        <div className="flex justify-between"><span className="text-muted">Version</span><span>{d?.version ?? "—"}</span></div>
        <div className="flex justify-between"><span className="text-muted">Server</span><span className="font-mono text-xs">{d?.server_url || "not configured"}</span></div>
      </Card>
      <Card className="overflow-hidden p-0">
        <div className="border-b border-border px-4 py-3 text-sm font-medium">Per-deployment capacity</div>
        <table className="w-full">
          <thead><tr><th className="th">Deployment</th><th className="th">Mode</th><th className="th">Tier-2</th><th className="th">Local inflight</th><th className="th">Tier-2 inflight</th><th className="th">Accepting</th></tr></thead>
          <tbody>
            {(d?.capacities ?? []).map((c) => (
              <tr key={c.deployment_id}>
                <td className="td font-mono text-xs">{c.deployment_id.slice(0, 8)}…</td>
                <td className="td text-muted">{c.sharing_mode}</td>
                <td className="td">{c.tier2_free}/{c.tier2_total}</td>
                <td className="td text-muted">{c.inflight_local}</td>
                <td className="td text-muted">{c.inflight_tier2}</td>
                <td className="td">{c.accepting ? <Badge tone="ok">yes</Badge> : <Badge tone="warn">no</Badge>}</td>
              </tr>
            ))}
            {(d?.capacities ?? []).length === 0 && <tr><td className="td text-center text-muted" colSpan={6}>No deployments configured.</td></tr>}
          </tbody>
        </table>
      </Card>
    </div>
  );
}
