import { useQuery } from "@tanstack/react-query";
import { Activity, Boxes, Cpu, ListOrdered, Zap } from "lucide-react";
import { useEffect, useState } from "react";
import { api, openEventStream } from "../lib/api";
import { Stat } from "../components/ui";
import type { ClientRow, Overview } from "../lib/types";

export default function OverviewPage() {
  const ov = useQuery({ queryKey: ["overview"], queryFn: () => api.get<Overview>("/api/overview"), refetchInterval: 5000 });
  const clients = useQuery({ queryKey: ["clients"], queryFn: () => api.get<{ clients: ClientRow[] }>("/api/clients"), refetchInterval: 5000 });
  const [, setTick] = useState(0);

  useEffect(() => {
    const stop = openEventStream(() => {
      setTick((t) => t + 1);
      ov.refetch();
      clients.refetch();
    });
    return stop;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const d = ov.data;
  const online = (clients.data?.clients ?? []).filter((c) => c.online);

  return (
    <div className="space-y-6">
      <h1 className="text-lg font-semibold">Overview</h1>
      <div className="grid grid-cols-2 gap-4 md:grid-cols-3 xl:grid-cols-6">
        <Stat label="Clients online" value={<span className="flex items-center gap-2"><Cpu size={18} className="text-ok" />{online.length}<span className="text-sm text-muted">/ {d?.clients_total ?? 0}</span></span>} />
        <Stat label="Virtual models" value={<span className="flex items-center gap-2"><Boxes size={18} className="text-accent" />{d?.virtual_models ?? 0}</span>} />
        <Stat label="Queued (5m)" value={<span className="flex items-center gap-2"><ListOrdered size={18} className="text-warn" />{d?.queued_5m ?? 0}</span>} />
        <Stat label="Active (5m)" value={<span className="flex items-center gap-2"><Activity size={18} className="text-accent" />{d?.active_5m ?? 0}</span>} />
        <Stat label="Requests (24h)" value={<span className="flex items-center gap-2"><Zap size={18} className="text-ok" />{d?.requests_24h ?? 0}</span>} />
      </div>
      <div className="card">
        <div className="border-b border-border px-4 py-3 text-sm font-medium">Connected clients</div>
        <table className="w-full">
          <thead><tr><th className="th">Name</th><th className="th">Status</th><th className="th">Version</th><th className="th">Platform</th><th className="th">Last seen</th></tr></thead>
          <tbody>
            {(clients.data?.clients ?? []).map((c) => (
              <tr key={c.id}>
                <td className="td">{c.name}</td>
                <td className="td">{c.online ? <span className="text-ok">online</span> : <span className="text-muted">offline</span>}</td>
                <td className="td text-muted">{c.version || "—"}</td>
                <td className="td text-muted">{c.platform || "—"}</td>
                <td className="td text-muted">{c.last_seen_at ? new Date(c.last_seen_at).toLocaleString() : "—"}</td>
              </tr>
            ))}
            {(clients.data?.clients ?? []).length === 0 && (
              <tr><td className="td text-center text-muted" colSpan={5}>No clients registered</td></tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
}
