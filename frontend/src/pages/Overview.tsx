import { useQuery } from "@tanstack/react-query";
import { Activity, Boxes, Cpu, ListOrdered, Zap } from "lucide-react";
import { AreaChart, Area, ResponsiveContainer, XAxis, YAxis, Tooltip, CartesianGrid } from "recharts";
import { useEffect, useState } from "react";
import { api, openEventStream } from "../lib/api";
import { LiveBadge, PageHeader, Stat } from "../components/ui";
import type { ClientRow, Overview } from "../lib/types";

export default function OverviewPage() {
  const ov = useQuery({ queryKey: ["overview"], queryFn: () => api.get<Overview>("/api/overview"), refetchInterval: 5000 });
  const clients = useQuery({ queryKey: ["clients"], queryFn: () => api.get<{ clients: ClientRow[] }>("/api/clients"), refetchInterval: 5000 });
  const hist = useQuery({ queryKey: ["ov-hist"], queryFn: () => api.get<{ history: any[] }>("/api/history?limit=200"), refetchInterval: 10000 });
  const chart = (() => {
    const buckets = new Map<string, { t: string; n: number; ok: number }>();
    for (const h of [...(hist.data?.history ?? [])].reverse()) {
      const t = new Date(h.created_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
      const b = buckets.get(t) ?? { t, n: 0, ok: 0 };
      b.n++; if (h.status === "done") b.ok++;
      buckets.set(t, b);
    }
    return [...buckets.values()].slice(-30);
  })();
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
      <PageHeader
        title="Overview"
        desc={`${online.length} of ${d?.clients_total ?? 0} clients online · live via event stream`}
        actions={<LiveBadge live={online.length > 0} />}
      />
      <div className="stagger grid grid-cols-2 gap-4 md:grid-cols-3 xl:grid-cols-6">
        <Stat label="Clients online" value={<span className="flex items-center gap-2"><Cpu size={18} className="text-ok" />{online.length}<span className="text-sm text-muted">/ {d?.clients_total ?? 0}</span></span>} />
        <Stat label="Virtual models" value={<span className="flex items-center gap-2"><Boxes size={18} className="text-accent" />{d?.virtual_models ?? 0}</span>} />
        <Stat label="Queued (5m)" value={<span className="flex items-center gap-2"><ListOrdered size={18} className="text-warn" />{d?.queued_5m ?? 0}</span>} />
        <Stat label="Active (5m)" value={<span className="flex items-center gap-2"><Activity size={18} className="text-accent" />{d?.active_5m ?? 0}</span>} />
        <Stat label="Requests (24h)" value={<span className="flex items-center gap-2"><Zap size={18} className="text-ok" />{d?.requests_24h ?? 0}</span>} />
      </div>
      {chart.length > 1 && (
        <div className="card p-4">
          <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">Recent requests (2min)</div>
          <ResponsiveContainer width="100%" height={160}>
            <AreaChart data={chart} margin={{ top: 4, right: 4, bottom: 0, left: -22 }}>
              <CartesianGrid stroke="#1c2740" vertical={false} />
              <XAxis dataKey="t" tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} />
              <YAxis tick={{ fontSize: 10, fill: "#5b6b8c" }} tickLine={false} axisLine={false} allowDecimals={false} />
              <Tooltip contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }} />
              <Area type="monotone" dataKey="n" stroke="#3d6fae" fill="#3d6fae" fillOpacity={0.16} strokeWidth={2} isAnimationActive={false} />
            </AreaChart>
          </ResponsiveContainer>
        </div>
      )}
      <div className="card">
        <div className="border-b border-border px-4 py-3 text-sm font-medium">Connected clients</div>
        <table className="w-full">
          <thead><tr><th className="th">Name</th><th className="th">Status</th><th className="th">Version</th><th className="th">Platform</th><th className="th">Last seen</th></tr></thead>
          <tbody>
            {(clients.data?.clients ?? []).map((c) => (
              <tr key={c.id}>
                <td className="td">{c.name}</td>
                <td className="td"><LiveBadge live={c.online} /></td>
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
